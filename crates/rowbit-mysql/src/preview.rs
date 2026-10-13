//! Runs a preview: checks the target table, then runs the read that
//! [`classify`](crate::statement::classify) derived, without ever running
//! the write. See "Preview" in `docs/spec/change-lifecycle.md`.

use std::time::Duration;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, OptsBuilder, Row, Value};
use rowbit_core::preview::{Cell, Preview, RowChange, Table, WriteKind};
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

fn refuse<T>(reason: impl Into<String>) -> Result<T, PreviewError> {
    Err(PreviewError::Refused(reason.into()))
}

#[derive(Debug, Clone, Copy)]
pub struct PreviewLimits {
    /// More matching rows than this and the preview is refused.
    pub max_rows: usize,
    /// How long the preview read may run.
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
    let opts = OptsBuilder::from_opts(opts.clone()).db_name(Some(database));
    let mut conn = Conn::new(opts).await?;
    let result = run(&mut conn, database, statement, kind, read, limits).await;
    // Read only, so there is nothing to undo; this just ends the snapshot.
    let _ = conn.query_drop("ROLLBACK").await;
    let _ = conn.disconnect().await;
    result
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
    let timeout = if version
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("mariadb")
    {
        format!("max_statement_time = {}", limits.timeout.as_secs_f64())
    } else {
        format!("max_execution_time = {}", limits.timeout.as_millis())
    };
    conn.query_drop(format!(
        "SET SESSION {timeout}, SESSION sql_select_limit = {}",
        limits.max_rows + 1
    ))
    .await?;
    conn.query_drop("SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .await?;
    conn.query_drop("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY")
        .await?;

    let db = statement.table.database.as_deref().unwrap_or(database);
    let info = table_info(conn, db, &statement.table.table).await?;
    let table = &info.table;

    let written: Vec<usize> = match kind {
        WriteKind::Delete => (0..table.columns.len()).collect(),
        WriteKind::Insert | WriteKind::Update => statement
            .columns
            .iter()
            .map(|c| {
                table
                    .columns
                    .iter()
                    .position(|t| t.eq_ignore_ascii_case(c))
                    .map_or_else(
                        || refuse(format!("{} has no column {c}", table.qualified_name())),
                        Ok,
                    )
            })
            .collect::<Result<_, _>>()?,
    };
    if kind == WriteKind::Insert {
        let key_given = table.primary_key.iter().all(|k| written.contains(k));
        let key_assigned =
            matches!(table.primary_key.as_slice(), [k] if Some(*k) == info.auto_increment);
        if !key_given && !key_assigned {
            return refuse(format!(
                "give every primary key column of {}",
                table.qualified_name()
            ));
        }
    }
    refuse_side_effects(conn, table, kind, &written).await?;

    let mut result = conn.query_iter(read.sql()).await?;
    let mut rows = Vec::new();
    while let Some(row) = result.next().await? {
        if rows.len() == limits.max_rows {
            result.drop_result().await?;
            return refuse(format!(
                "the write matches more than {} rows, the most Rowbit previews",
                limits.max_rows
            ));
        }
        rows.push(row);
    }
    drop(result);

    let n = table.columns.len();
    let changes = rows
        .into_iter()
        .map(|row| {
            let names: Vec<String> = row
                .columns_ref()
                .iter()
                .map(|c| c.name_str().into_owned())
                .collect();
            let cells = cells(row)?;
            match kind {
                WriteKind::Update | WriteKind::Delete => {
                    let extra = if kind == WriteKind::Update {
                        written.len()
                    } else {
                        0
                    };
                    if cells.len() != n + extra || names[..n] != table.columns[..] {
                        return refuse("preview columns do not match the table");
                    }
                    let mut cells = cells;
                    let after = cells.split_off(n);
                    Ok(RowChange {
                        before: Some(cells),
                        after: (kind == WriteKind::Update).then_some(after),
                    })
                }
                WriteKind::Insert => {
                    if cells.len() != written.len() {
                        return refuse("preview columns do not match the INSERT columns");
                    }
                    Ok(RowChange {
                        before: None,
                        after: Some(cells),
                    })
                }
            }
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Preview {
        kind,
        table: info.table,
        written,
        rows: changes,
    })
}

struct TableInfo {
    table: Table,
    /// Index of the `AUTO_INCREMENT` column, if any.
    auto_increment: Option<usize>,
}

async fn table_info(conn: &mut Conn, db: &str, name: &str) -> Result<TableInfo, PreviewError> {
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

    let columns: Vec<(String, String)> = conn
        .exec(
            "SELECT COLUMN_NAME, EXTRA FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
            (&database, &name),
        )
        .await?;
    let auto_increment = columns
        .iter()
        .position(|(_, extra)| extra.to_ascii_lowercase().contains("auto_increment"));
    let columns: Vec<String> = columns.into_iter().map(|(c, _)| c).collect();

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
        .map(|k| columns.iter().position(|c| c == k))
        .collect::<Option<Vec<_>>>()
        .map_or_else(
            || refuse(format!("cannot read the primary key of {qualified}")),
            Ok,
        )?;

    Ok(TableInfo {
        table: Table {
            database,
            name,
            columns,
            primary_key,
        },
        auto_increment,
    })
}

/// Refuses writes whose effects would reach beyond the previewed rows:
/// triggers on the table, and foreign keys from other tables that cascade.
async fn refuse_side_effects(
    conn: &mut Conn,
    table: &Table,
    kind: WriteKind,
    written: &[usize],
) -> Result<(), PreviewError> {
    let qualified = table.qualified_name();
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

    let references: Vec<(String, String, String, String)> = conn
        .exec(
            "SELECT k.TABLE_NAME, k.REFERENCED_COLUMN_NAME, rc.UPDATE_RULE, rc.DELETE_RULE \
             FROM information_schema.KEY_COLUMN_USAGE k \
             JOIN information_schema.REFERENTIAL_CONSTRAINTS rc \
               ON rc.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA \
              AND rc.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
              AND rc.TABLE_NAME = k.TABLE_NAME \
             WHERE k.REFERENCED_TABLE_SCHEMA = ? AND k.REFERENCED_TABLE_NAME = ?",
            (&table.database, &table.name),
        )
        .await?;
    let acts = |rule: &str| matches!(rule, "CASCADE" | "SET NULL" | "SET DEFAULT");
    for (child, column, on_update, on_delete) in references {
        let touched = match kind {
            WriteKind::Insert => false,
            WriteKind::Delete => acts(&on_delete),
            WriteKind::Update => {
                acts(&on_update)
                    && written
                        .iter()
                        .any(|&w| table.columns[w].eq_ignore_ascii_case(&column))
            }
        };
        if touched {
            return refuse(format!(
                "{child} references {qualified} with a cascading foreign key, \
                 whose effects a preview cannot show"
            ));
        }
    }
    Ok(())
}

/// A text-protocol row as cells. Any other value shape is refused, so a
/// value is never converted on its way to the reviewer.
fn cells(row: Row) -> Result<Vec<Cell>, PreviewError> {
    row.unwrap()
        .into_iter()
        .map(|v| match v {
            Value::NULL => Ok(None),
            Value::Bytes(b) => Ok(Some(b)),
            other => refuse(format!("unexpected value {other:?} in preview")),
        })
        .collect()
}
