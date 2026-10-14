//! Runs a preview: checks the target table, reads the rows a write would
//! touch, and replays the write on a private temporary copy of them to see
//! exactly what the table would store. The real table is only ever read.
//! See "Preview" in `docs/spec/change-lifecycle.md`.

use std::collections::HashMap;
use std::time::Duration;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, OptsBuilder, Params, Row, Value};
use rowbit_core::preview::{Cell, Column, Preview, RowChange, Table, WriteKind};
use thiserror::Error;

use crate::statement::{Classified, Kind, PreviewRead};

#[derive(Debug, Error)]
pub enum PreviewError {
    /// Rowbit will not preview this write. The reason is for people.
    #[error("{0}")]
    Refused(String),
    #[error("database error during preview: {0}")]
    Database(#[from] mysql_async::Error),
}

impl PreviewError {
    /// What the agent may be told. Rowbit's own refusals carry no values;
    /// a database error is reduced to its code, because server messages
    /// quote values, from any table Rowbit's user can read.
    pub fn agent_reason(&self) -> String {
        match self {
            PreviewError::Refused(reason) => reason.clone(),
            PreviewError::Database(e) => format!("preview failed: {}", db_error_code(e)),
        }
    }
}

/// A database error without its message: `database error 1366`.
pub fn db_error_code(e: &mysql_async::Error) -> String {
    match e {
        mysql_async::Error::Server(e) => format!("database error {}", e.code),
        mysql_async::Error::Io(_) => "database unreachable".to_owned(),
        _ => "database error".to_owned(),
    }
}

fn refuse<T>(reason: impl Into<String>) -> Result<T, PreviewError> {
    Err(PreviewError::Refused(reason.into()))
}

#[derive(Debug, Clone, Copy)]
pub struct PreviewLimits {
    /// More matching rows than this and the preview is refused.
    pub max_rows: usize,
    /// How long the whole preview may take.
    pub timeout: Duration,
}

impl Default for PreviewLimits {
    fn default() -> Self {
        Self {
            max_rows: 10_000,
            timeout: Duration::from_secs(10),
        }
    }
}

/// The session's SQL mode: strict, so a value that would not fit its column
/// is an error instead of being truncated quietly, and with backslashes
/// taken literally, the way Rowbit prints string literals.
pub(crate) const SQL_MODE: &str = "STRICT_ALL_TABLES,NO_BACKSLASH_ESCAPES,NO_ENGINE_SUBSTITUTION";

/// The private copy a write is replayed on. A temporary table is visible to
/// this connection only, and dropped with it.
const SCRATCH: &str = "`__rowbit_preview`";

/// Previews `statement` against `database` (the default database for
/// unqualified names) on a new connection of its own.
pub(crate) async fn preview(
    opts: &mysql_async::Opts,
    database: &str,
    statement: &Classified,
    limits: PreviewLimits,
) -> Result<Preview, PreviewError> {
    let (kind, read) = match (statement.kind, &statement.preview) {
        (Kind::Insert, Some(read)) => (WriteKind::Insert, read),
        (Kind::Update, Some(read)) => (WriteKind::Update, read),
        (Kind::Delete, Some(read)) => (WriteKind::Delete, read),
        _ => return refuse("DDL is not previewed"),
    };
    let work = async {
        let opts = OptsBuilder::from_opts(opts.clone()).db_name(Some(database));
        let mut conn = Conn::new(opts).await?;
        let result = run(&mut conn, database, statement, kind, read, limits).await;
        // The transaction is read only, so this only ends it. Disconnecting
        // drops the temporary table.
        let _ = conn.query_drop("ROLLBACK").await;
        let _ = conn.disconnect().await;
        result
    };
    tokio::time::timeout(limits.timeout, work)
        .await
        .unwrap_or_else(|_| refuse(format!("the preview took longer than {:?}", limits.timeout)))
}

async fn run(
    conn: &mut Conn,
    database: &str,
    statement: &Classified,
    kind: WriteKind,
    read: &PreviewRead,
    limits: PreviewLimits,
) -> Result<Preview, PreviewError> {
    let version: Option<String> = conn.query_first("SELECT VERSION()").await?;
    let mariadb = version
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("mariadb");
    let statement_limit = if mariadb {
        format!("max_statement_time = {}", limits.timeout.as_secs_f64())
    } else {
        format!("max_execution_time = {}", limits.timeout.as_millis())
    };
    conn.query_drop(format!(
        "SET SESSION sql_mode = '{SQL_MODE}', SESSION {statement_limit}, \
         SESSION sql_select_limit = {}",
        limits.max_rows + 1
    ))
    .await?;
    // Each statement reads its own consistent snapshot, without locks, even
    // when it fills the scratch table from other tables.
    conn.query_drop("SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .await?;
    // Pin NOW() and friends for everything this preview evaluates.
    let as_of: Option<String> = conn
        .query_first("SELECT CAST(UNIX_TIMESTAMP(NOW(6)) AS CHAR)")
        .await?;
    let as_of = as_of.unwrap_or_default();
    conn.query_drop(format!("SET SESSION timestamp = {as_of}"))
        .await?;

    let db = statement.table.database.as_deref().unwrap_or(database);
    let table = table_info(conn, db, &statement.table.table).await?;

    let written: Vec<usize> = match kind {
        WriteKind::Delete => (0..table.columns.len()).collect(),
        WriteKind::Insert | WriteKind::Update => statement
            .columns
            .iter()
            .map(|c| {
                table
                    .columns
                    .iter()
                    .position(|t| t.name.eq_ignore_ascii_case(c))
                    .map_or_else(
                        || refuse(format!("{} has no column {c}", table.qualified_name())),
                        Ok,
                    )
            })
            .collect::<Result<_, _>>()?,
    };
    if kind != WriteKind::Delete {
        if let Some(&g) = written.iter().find(|&&w| table.columns[w].generated) {
            return refuse(format!(
                "{} is a generated column and cannot be written",
                table.columns[g].name
            ));
        }
    }
    if kind == WriteKind::Update {
        if let Some(&k) = written.iter().find(|w| table.primary_key.contains(w)) {
            return refuse(format!(
                "{} is part of the primary key; v0.1 does not change keys",
                table.columns[k].name
            ));
        }
    }
    if kind == WriteKind::Insert {
        let key_given = table.primary_key.iter().all(|k| written.contains(k));
        let key_assigned =
            matches!(table.primary_key.as_slice(), [k] if table.columns[*k].auto_increment);
        if !key_given && !key_assigned {
            return refuse(format!(
                "give every primary key column of {}",
                table.qualified_name()
            ));
        }
    }
    refuse_side_effects(conn, mariadb, &table, kind, &written).await?;

    if kind != WriteKind::Delete {
        conn.query_drop(format!(
            "CREATE TEMPORARY TABLE {SCRATCH} LIKE {}.{}",
            quote(&table.database),
            quote(&table.name)
        ))
        .await?;
    }
    // Read only: the server refuses any write to a real table from here on.
    conn.query_drop("START TRANSACTION READ ONLY").await?;

    let columns = table
        .columns
        .iter()
        .map(|c| quote(&c.name))
        .collect::<Vec<_>>();
    let rows = match read {
        PreviewRead::Existing {
            from,
            qualifier,
            filter,
            assignments,
        } => {
            let select = columns
                .iter()
                .map(|c| format!("{}.{c}", quote(qualifier)))
                .collect::<Vec<_>>()
                .join(", ");
            let before = read_capped(
                conn,
                &format!("SELECT {select} FROM {from}{filter}"),
                limits,
            )
            .await?;
            if kind == WriteKind::Delete {
                before
                    .into_iter()
                    .map(|b| RowChange {
                        before: Some(b),
                        after: None,
                    })
                    .collect()
            } else {
                replay_update(conn, &table, &columns, qualifier, assignments, before).await?
            }
        }
        PreviewRead::New { source } => {
            let named = written
                .iter()
                .map(|&w| columns[w].as_str())
                .collect::<Vec<_>>()
                .join(", ");
            conn.query_drop(format!("INSERT INTO {SCRATCH} ({named}) {source}"))
                .await?;
            let after = read_capped(
                conn,
                &format!("SELECT {} FROM {SCRATCH}", columns.join(", ")),
                limits,
            )
            .await?;
            // The scratch table numbers AUTO_INCREMENT keys from 1, which
            // says nothing about the keys the real table will assign.
            let assigned: Vec<usize> = table
                .primary_key
                .iter()
                .copied()
                .filter(|k| !written.contains(k))
                .collect();
            after
                .into_iter()
                .map(|mut a| {
                    for &k in &assigned {
                        a[k] = None;
                    }
                    RowChange {
                        before: None,
                        after: Some(a),
                    }
                })
                .collect()
        }
    };

    Ok(Preview {
        kind,
        table,
        written,
        rows,
        as_of,
    })
}

/// Copies the rows into the scratch table, runs the statement's `SET` list
/// there, and reads back what the table would store, including columns the
/// server changes itself (`ON UPDATE CURRENT_TIMESTAMP`, generated columns).
async fn replay_update(
    conn: &mut Conn,
    table: &Table,
    columns: &[String],
    qualifier: &str,
    assignments: &str,
    before: Vec<Vec<Cell>>,
) -> Result<Vec<RowChange>, PreviewError> {
    let insertable: Vec<usize> = (0..table.columns.len())
        .filter(|&i| !table.columns[i].generated)
        .collect();
    let insert = format!(
        "INSERT INTO {SCRATCH} ({}) VALUES ({})",
        insertable
            .iter()
            .map(|&i| columns[i].as_str())
            .collect::<Vec<_>>()
            .join(", "),
        vec!["?"; insertable.len()].join(", ")
    );
    for row in &before {
        let params: Vec<Value> = insertable.iter().map(|&i| to_value(&row[i])).collect();
        conn.exec_drop(&insert, Params::Positional(params)).await?;
    }
    conn.query_drop(format!(
        "UPDATE {SCRATCH} AS {} SET {assignments}",
        quote(qualifier)
    ))
    .await?;

    let mut after: HashMap<Vec<Cell>, Vec<Cell>> = HashMap::new();
    let rows: Vec<Row> = conn
        .query(format!("SELECT {} FROM {SCRATCH}", columns.join(", ")))
        .await?;
    for row in rows {
        let cells = cells(row)?;
        after.insert(key_of(table, &cells), cells);
    }
    before
        .into_iter()
        .map(|b| {
            let a = after
                .remove(&key_of(table, &b))
                .map_or_else(|| refuse("a previewed row went missing"), Ok)?;
            Ok(RowChange {
                before: Some(b),
                after: Some(a),
            })
        })
        .collect()
}

fn key_of(table: &Table, image: &[Cell]) -> Vec<Cell> {
    table
        .primary_key
        .iter()
        .map(|&k| image[k].clone())
        .collect()
}

/// Runs a read and refuses if it returns more than `max_rows` rows.
async fn read_capped(
    conn: &mut Conn,
    sql: &str,
    limits: PreviewLimits,
) -> Result<Vec<Vec<Cell>>, PreviewError> {
    let mut result = conn.query_iter(sql).await?;
    let mut rows = Vec::new();
    while let Some(row) = result.next().await? {
        if rows.len() == limits.max_rows {
            result.drop_result().await?;
            return refuse(format!(
                "the write matches more than {} rows, the most Rowbit previews",
                limits.max_rows
            ));
        }
        rows.push(cells(row)?);
    }
    Ok(rows)
}

async fn table_info(conn: &mut Conn, db: &str, name: &str) -> Result<Table, PreviewError> {
    let found: Option<(String, String, String, Option<String>)> = conn
        .exec_first(
            "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE, ENGINE FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
            (db, name),
        )
        .await?;
    let Some((database, name, table_type, engine)) = found else {
        return refuse(format!("table {db}.{name} not found"));
    };
    let qualified = format!("{database}.{name}");
    if table_type != "BASE TABLE" {
        return refuse(format!("{qualified} is not a base table"));
    }
    if !engine.is_some_and(|e| e.eq_ignore_ascii_case("InnoDB")) {
        return refuse(format!(
            "{qualified} is not InnoDB, so a write to it cannot be rolled back"
        ));
    }

    let columns: Vec<(String, String, String)> = conn
        .exec(
            "SELECT COLUMN_NAME, COLUMN_TYPE, EXTRA FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
            (&database, &name),
        )
        .await?;
    let columns: Vec<Column> = columns
        .into_iter()
        .map(|(name, column_type, extra)| {
            let extra = extra.to_ascii_lowercase();
            Column {
                name,
                column_type,
                generated: extra.contains("generated"),
                auto_increment: extra.contains("auto_increment"),
            }
        })
        .collect();

    let key: Vec<String> = conn
        .exec(
            "SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND CONSTRAINT_NAME = 'PRIMARY' \
             ORDER BY ORDINAL_POSITION",
            (&database, &name),
        )
        .await?;
    if key.is_empty() {
        return refuse(format!("{qualified} has no primary key"));
    }
    let primary_key = key
        .iter()
        .map(|k| columns.iter().position(|c| &c.name == k))
        .collect::<Option<Vec<_>>>()
        .map_or_else(
            || refuse(format!("cannot read the primary key of {qualified}")),
            Ok,
        )?;

    Ok(Table {
        database,
        name,
        columns,
        primary_key,
    })
}

/// Refuses writes whose effects would reach beyond the previewed rows:
/// triggers on the table, and foreign keys from other tables that cascade.
/// Both checks fail closed when Rowbit's database user cannot see enough to
/// be sure.
async fn refuse_side_effects(
    conn: &mut Conn,
    mariadb: bool,
    table: &Table,
    kind: WriteKind,
    written: &[usize],
) -> Result<(), PreviewError> {
    let qualified = table.qualified_name();

    // information_schema.TRIGGERS only lists triggers on tables where the
    // user holds TRIGGER, so an empty answer means nothing without it.
    // Database-level grants are LIKE patterns (`rowbit\_test`). The session
    // runs with NO_BACKSLASH_ESCAPES, so '\' below is one backslash.
    let grantee = "CONCAT('''', SUBSTRING_INDEX(CURRENT_USER(), '@', 1), '''@''', \
                   SUBSTRING_INDEX(CURRENT_USER(), '@', -1), '''')";
    let can_see_triggers: Option<u64> = conn
        .exec_first(
            format!(
                "SELECT COUNT(*) FROM ( \
                 SELECT 1 FROM information_schema.USER_PRIVILEGES \
                  WHERE GRANTEE = {grantee} AND PRIVILEGE_TYPE = 'TRIGGER' \
                 UNION ALL SELECT 1 FROM information_schema.SCHEMA_PRIVILEGES \
                  WHERE GRANTEE = {grantee} AND PRIVILEGE_TYPE = 'TRIGGER' \
                    AND ? LIKE TABLE_SCHEMA ESCAPE '\\' \
                 UNION ALL SELECT 1 FROM information_schema.TABLE_PRIVILEGES \
                  WHERE GRANTEE = {grantee} AND PRIVILEGE_TYPE = 'TRIGGER' \
                    AND TABLE_SCHEMA = ? AND TABLE_NAME = ?) AS grants"
            ),
            (&table.database, &table.database, &table.name),
        )
        .await?;
    if can_see_triggers.unwrap_or(0) == 0 {
        return refuse(format!(
            "Rowbit's database user needs the TRIGGER privilege on {qualified} \
             to check it for triggers"
        ));
    }
    let triggers: Option<u64> = conn
        .exec_first(
            "SELECT COUNT(*) FROM information_schema.TRIGGERS \
             WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ?",
            (&table.database, &table.name),
        )
        .await?;
    if triggers.unwrap_or(0) > 0 {
        return refuse(format!(
            "{qualified} has triggers, whose effects a preview cannot show"
        ));
    }

    if kind == WriteKind::Insert {
        return Ok(());
    }
    // InnoDB's own foreign key list shows every foreign key, whatever the
    // user may see of the referencing tables. It needs PROCESS. Compared
    // without case: with lower_case_table_names=2 (macOS) InnoDB stores
    // the names in lower case.
    let (foreign, columns) = if mariadb {
        ("INNODB_SYS_FOREIGN", "INNODB_SYS_FOREIGN_COLS")
    } else {
        ("INNODB_FOREIGN", "INNODB_FOREIGN_COLS")
    };
    let Some(ref_name) = innodb_name(&table.database, &table.name) else {
        return refuse(format!(
            "v0.1 cannot check foreign keys of {qualified}: use ASCII letters, digits and _ \
             in its database and table names"
        ));
    };
    let references: Result<Vec<(String, u64, String)>, _> = conn
        .exec(
            format!(
                "SELECT f.FOR_NAME, f.TYPE, c.REF_COL_NAME \
                 FROM information_schema.{foreign} f \
                 JOIN information_schema.{columns} c ON c.ID = f.ID \
                 WHERE LOWER(f.REF_NAME) = LOWER(?)"
            ),
            (ref_name,),
        )
        .await;
    let references = match references {
        Ok(r) => r,
        Err(mysql_async::Error::Server(e)) if e.code == 1227 => {
            return refuse(
                "Rowbit's database user needs the PROCESS privilege to check foreign keys",
            );
        }
        Err(e) => return Err(e.into()),
    };
    // InnoDB's flags: 1 ON DELETE CASCADE, 2 ON DELETE SET NULL,
    // 4 ON UPDATE CASCADE, 8 ON UPDATE SET NULL.
    for (child, flags, column) in references {
        let touched = match kind {
            WriteKind::Insert => false,
            WriteKind::Delete => flags & (1 | 2) != 0,
            WriteKind::Update => {
                flags & (4 | 8) != 0
                    && written
                        .iter()
                        .any(|&w| table.columns[w].name.eq_ignore_ascii_case(&column))
            }
        };
        if touched {
            return refuse(format!(
                "{} references {qualified} with a cascading foreign key, \
                 whose effects a preview cannot show",
                child.replace('/', ".")
            ));
        }
    }
    Ok(())
}

/// The `database/table` name InnoDB uses, for names that need no encoding.
fn innodb_name(database: &str, table: &str) -> Option<String> {
    let plain =
        |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    (plain(database) && plain(table)).then(|| format!("{database}/{table}"))
}

/// A text-protocol row as cells. Any other value shape is refused, so a
/// value is never converted on its way to the reviewer.
fn cells(row: Row) -> Result<Vec<Cell>, PreviewError> {
    row.unwrap()
        .into_iter()
        .map(|v| match v {
            Value::NULL => Ok(None),
            Value::Bytes(b) => Ok(Some(b)),
            _ => refuse("unexpected value type in preview"),
        })
        .collect()
}

fn to_value(cell: &Cell) -> Value {
    match cell {
        None => Value::NULL,
        Some(b) => Value::Bytes(b.clone()),
    }
}

/// Backquotes an identifier for MySQL.
fn quote(id: &str) -> String {
    format!("`{}`", id.replace('`', "``"))
}
