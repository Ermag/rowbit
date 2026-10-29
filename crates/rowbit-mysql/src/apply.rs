//! Applies an approved preview, and undoes an applied one. See "Apply" and
//! "Undo" in `docs/spec/change-lifecycle.md`.
//!
//! Neither runs the agent's statement. Apply writes the stored after images
//! by primary key, and undo writes the before images back, each in one
//! transaction that checks every row first and reads every row back after.
//! Any mismatch rolls the whole transaction back.

use std::collections::HashSet;
use std::time::Duration;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, OptsBuilder, Params, Row, Value};
use rowbit_core::preview::{Applied, AppliedKey, Cell, Preview, RowChange, Table, WriteKind};
use thiserror::Error;

use crate::preview::{SQL_MODE, db_error_code, quote, to_value};
use crate::statement::{Classified, PreviewRead};

#[derive(Debug, Error)]
pub enum ApplyError {
    /// The database no longer matches what was reviewed, or what was written
    /// is not what was reviewed. Nothing was written. `reason` holds no
    /// values; `row` names the row for people.
    #[error("{reason}{}", row.as_ref().map(|r| format!(" (row {r})")).unwrap_or_default())]
    Conflict { reason: String, row: Option<String> },
    /// Nothing was written.
    #[error("database error: {0}")]
    Database(#[from] mysql_async::Error),
    /// The outcome is unknown: the work timed out, or the connection failed
    /// while committing. A person has to look at the rows.
    #[error("outcome unknown: {0}")]
    Uncertain(String),
}

impl ApplyError {
    /// What the agent may be told: no row values or keys, and a database
    /// error only by its code.
    pub fn agent_reason(&self) -> String {
        match self {
            ApplyError::Conflict { reason, .. } => reason.clone(),
            ApplyError::Database(e) => db_error_code(e),
            ApplyError::Uncertain(_) => "the outcome is unknown; a person will check".to_owned(),
        }
    }
}

fn conflict<T>(reason: impl Into<String>) -> Result<T, ApplyError> {
    Err(ApplyError::Conflict {
        reason: reason.into(),
        row: None,
    })
}

fn conflict_at<T>(reason: impl Into<String>, key: &[Cell]) -> Result<T, ApplyError> {
    Err(ApplyError::Conflict {
        reason: reason.into(),
        row: Some(show_key(key)),
    })
}

/// How long apply and undo wait for a row lock before giving up.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// Writes `preview` to the database. `statement` is the classified
/// statement the preview came from; its filter is read again to make sure
/// it matches no rows the reviewer did not see.
pub(crate) async fn apply(
    opts: &mysql_async::Opts,
    database: &str,
    statement: &Classified,
    preview: &Preview,
) -> Result<Applied, ApplyError> {
    bounded(async {
        let mut conn = connect(opts, database, &preview.as_of).await?;
        let result = async {
            conn.query_drop("START TRANSACTION").await?;
            let applied = write_preview(&mut conn, statement, preview).await?;
            commit(&mut conn).await?;
            Ok(applied)
        }
        .await;
        finish(conn, result).await
    })
    .await
}

/// Restores the before images of an applied preview.
pub(crate) async fn undo(
    opts: &mysql_async::Opts,
    database: &str,
    preview: &Preview,
    applied: &Applied,
) -> Result<(), ApplyError> {
    bounded(async {
        let mut conn = connect(opts, database, &preview.as_of).await?;
        let result = async {
            conn.query_drop("START TRANSACTION").await?;
            restore(&mut conn, preview, applied).await?;
            commit(&mut conn).await?;
            Ok(())
        }
        .await;
        finish(conn, result).await
    })
    .await
}

/// Runs a reviewed DDL statement as it is.
pub(crate) async fn run_ddl(
    opts: &mysql_async::Opts,
    database: &str,
    sql: &str,
) -> Result<(), ApplyError> {
    bounded(async {
        let mut conn = connect(opts, database, "0").await?;
        // A DDL statement that fails partway can leave the schema changed,
        // so any error is reported as an unknown outcome, except a missing
        // privilege, which the server checks before it starts.
        let result = conn.query_drop(sql).await.map_err(|e| match &e {
            mysql_async::Error::Server(se) if crate::agent::DENIED.contains(&se.code) => {
                ApplyError::Conflict {
                    reason: format!("outside the agent's scope: database error {}", se.code),
                    row: None,
                }
            }
            _ => ApplyError::Uncertain(e.to_string()),
        });
        finish(conn, result).await
    })
    .await
}

/// How long apply, undo or DDL may take in all.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Runs `work` with an overall time limit. Running out of time leaves the
/// outcome unknown, since a commit may already have been sent.
async fn bounded<T>(
    work: impl std::future::Future<Output = Result<T, ApplyError>>,
) -> Result<T, ApplyError> {
    tokio::time::timeout(TIMEOUT, work)
        .await
        .unwrap_or_else(|_| {
            Err(ApplyError::Uncertain(format!(
                "timed out after {TIMEOUT:?}"
            )))
        })
}

/// A failed COMMIT may or may not have been applied by the server.
async fn commit(conn: &mut Conn) -> Result<(), ApplyError> {
    conn.query_drop("COMMIT")
        .await
        .map_err(|e| ApplyError::Uncertain(format!("COMMIT failed: {e}")))
}

async fn connect(
    opts: &mysql_async::Opts,
    database: &str,
    as_of: &str,
) -> Result<Conn, ApplyError> {
    let opts = OptsBuilder::from_opts(opts.clone()).db_name(Some(database));
    let mut conn = Conn::new(opts).await?;
    // Strict, literal backslashes, READ COMMITTED (locks on the rows by key,
    // no gap locks), and short row and metadata lock waits, so a busy row
    // fails the apply, and DDL behind a long transaction fails instead of
    // queueing every query on the table behind it.
    conn.query_drop(format!(
        "SET SESSION sql_mode = '{SQL_MODE}', SESSION innodb_lock_wait_timeout = {0}, \
         SESSION lock_wait_timeout = {0}",
        LOCK_WAIT.as_secs()
    ))
    .await?;
    conn.query_drop("SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .await?;
    if as_of != "0" {
        // NOW() means what it meant when the preview ran.
        if !as_of.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
            return conflict("the stored preview time is malformed");
        }
        conn.query_drop(format!("SET SESSION timestamp = {as_of}"))
            .await?;
    }
    Ok(conn)
}

async fn finish<T>(mut conn: Conn, result: Result<T, ApplyError>) -> Result<T, ApplyError> {
    if result.is_err() {
        let _ = conn.query_drop("ROLLBACK").await;
    }
    let _ = conn.disconnect().await;
    result
}

async fn write_preview(
    conn: &mut Conn,
    statement: &Classified,
    preview: &Preview,
) -> Result<Applied, ApplyError> {
    let table = &preview.table;
    let target = format!("{}.{}", quote(&table.database), quote(&table.name));

    // 1 and 2: lock each row by key and compare it to its before image.
    // For an insert with a known key, the key must still be free.
    for row in &preview.rows {
        let Some(key) = preview.key(row) else {
            continue;
        };
        let current = lock_row(conn, table, &target, &key).await?;
        match (&row.before, current) {
            (Some(before), Some(current)) if &current == before => {}
            (Some(_), Some(_)) => {
                return conflict_at("drift since preview: a previewed row changed", &key);
            }
            (Some(_), None) => {
                return conflict_at("drift since preview: a previewed row is gone", &key);
            }
            (None, Some(_)) => {
                return conflict_at("drift since preview: a row to insert now exists", &key);
            }
            (None, None) => {}
        }
    }

    // 3: the filter must not match rows the reviewer did not see.
    if let Some(PreviewRead::Existing {
        from,
        qualifier,
        filter,
        ..
    }) = &statement.preview
    {
        let keys = table
            .primary_key
            .iter()
            .map(|&k| format!("{}.{}", quote(qualifier), quote(&table.columns[k].name)))
            .collect::<Vec<_>>()
            .join(", ");
        let previewed: HashSet<Vec<Cell>> =
            preview.rows.iter().filter_map(|r| preview.key(r)).collect();
        let mut result = conn
            .query_iter(format!("SELECT {keys} FROM {from}{filter}"))
            .await?;
        let mut extra = None;
        while let Some(row) = result.next().await? {
            let key = cells(row)?;
            if extra.is_none() && !previewed.contains(&key) {
                extra = Some(key);
            }
        }
        drop(result);
        if let Some(key) = extra {
            return conflict_at(
                "drift since preview: the statement now matches a row nobody reviewed",
                &key,
            );
        }
    }

    // 4 and 5: write each row, then read it back.
    let mut applied = Vec::with_capacity(preview.rows.len());
    for row in &preview.rows {
        let key = match preview.kind {
            WriteKind::Update => update_row(conn, table, &target, row).await?,
            WriteKind::Delete => delete_row(conn, table, &target, row).await?,
            WriteKind::Insert => insert_row(conn, table, &target, preview, row).await?,
        };
        applied.push(AppliedKey { key: Some(key) });
    }
    Ok(Applied { keys: applied })
}

async fn restore(conn: &mut Conn, preview: &Preview, applied: &Applied) -> Result<(), ApplyError> {
    let table = &preview.table;
    let target = format!("{}.{}", quote(&table.database), quote(&table.name));
    if applied.keys.len() != preview.rows.len() {
        return conflict("the apply record does not match the preview");
    }
    for (row, applied) in preview.rows.iter().zip(&applied.keys) {
        let Some(key) = &applied.key else {
            return conflict("the apply record is missing a key");
        };
        let expected = after_with_key(table, row, key);
        let current = lock_row(conn, table, &target, key).await?;
        if current != expected {
            return conflict_at(
                "a row changed since apply, so undo would overwrite someone else's change",
                key,
            );
        }
        match preview.kind {
            WriteKind::Update => {
                let swapped = RowChange {
                    before: row.after.clone(),
                    after: row.before.clone(),
                };
                update_row(conn, table, &target, &swapped).await?;
            }
            WriteKind::Delete => {
                let reinsert = RowChange {
                    before: None,
                    after: row.before.clone(),
                };
                let all_given = Preview {
                    written: (0..table.columns.len()).collect(),
                    ..preview.clone()
                };
                insert_row(conn, table, &target, &all_given, &reinsert).await?;
            }
            WriteKind::Insert => {
                let inserted = RowChange {
                    before: expected,
                    after: None,
                };
                delete_row(conn, table, &target, &inserted).await?;
            }
        }
    }
    Ok(())
}

/// The after image with the applied key filled in (for an inserted row
/// whose key the server assigned). `None` for a delete.
fn after_with_key(table: &Table, row: &RowChange, key: &[Cell]) -> Option<Vec<Cell>> {
    let mut after = row.after.clone()?;
    for (&k, cell) in table.primary_key.iter().zip(key) {
        after[k] = cell.clone();
    }
    Some(after)
}

async fn update_row(
    conn: &mut Conn,
    table: &Table,
    target: &str,
    row: &RowChange,
) -> Result<Vec<Cell>, ApplyError> {
    let (Some(before), Some(after)) = (&row.before, &row.after) else {
        return conflict("an update row needs both images");
    };
    let key = pick(table, before);
    // Every column that is not generated and that changes, including
    // ON UPDATE columns, which then hold the reviewed value.
    let changed: Vec<usize> = (0..table.columns.len())
        .filter(|&i| !table.columns[i].generated && before[i] != after[i])
        .collect();
    if !changed.is_empty() {
        let set = changed
            .iter()
            .map(|&i| format!("{} = ?", quote(&table.columns[i].name)))
            .collect::<Vec<_>>()
            .join(", ");
        let mut params: Vec<Value> = changed.iter().map(|&i| to_value(&after[i])).collect();
        params.extend(key.iter().map(to_value));
        conn.exec_drop(
            format!("UPDATE {target} SET {set} WHERE {}", key_match(table)),
            Params::Positional(params),
        )
        .await?;
        expect_one_row(conn, &key)?;
    }
    read_back(conn, table, target, &key, Some(after)).await?;
    Ok(key)
}

async fn delete_row(
    conn: &mut Conn,
    table: &Table,
    target: &str,
    row: &RowChange,
) -> Result<Vec<Cell>, ApplyError> {
    let Some(before) = &row.before else {
        return conflict("a deleted row needs its before image");
    };
    let key = pick(table, before);
    conn.exec_drop(
        format!("DELETE FROM {target} WHERE {}", key_match(table)),
        Params::Positional(key.iter().map(to_value).collect()),
    )
    .await?;
    expect_one_row(conn, &key)?;
    read_back(conn, table, target, &key, None).await?;
    Ok(key)
}

async fn insert_row(
    conn: &mut Conn,
    table: &Table,
    target: &str,
    preview: &Preview,
    row: &RowChange,
) -> Result<Vec<Cell>, ApplyError> {
    let Some(after) = &row.after else {
        return conflict("an inserted row needs its after image");
    };
    // Every column the server does not compute: generated columns, and a
    // key it assigns.
    let assigned: Vec<usize> = table
        .primary_key
        .iter()
        .copied()
        .filter(|k| !preview.written.contains(k))
        .collect();
    let given: Vec<usize> = (0..table.columns.len())
        .filter(|i| !table.columns[*i].generated && !assigned.contains(i))
        .collect();
    let columns = given
        .iter()
        .map(|&i| quote(&table.columns[i].name))
        .collect::<Vec<_>>()
        .join(", ");
    conn.exec_drop(
        format!(
            "INSERT INTO {target} ({columns}) VALUES ({})",
            vec!["?"; given.len()].join(", ")
        ),
        Params::Positional(given.iter().map(|&i| to_value(&after[i])).collect()),
    )
    .await?;
    let key = match assigned.as_slice() {
        [] => pick(table, after),
        [_] => {
            let id = conn
                .last_insert_id()
                .map_or_else(|| conflict("the server did not report the new key"), Ok)?;
            vec![Some(id.to_string().into_bytes())]
        }
        _ => return conflict("only a single AUTO_INCREMENT key can be assigned"),
    };
    let expected = after_with_key(table, row, &key);
    read_back(conn, table, target, &key, expected.as_ref()).await?;
    Ok(key)
}

fn expect_one_row(conn: &Conn, key: &[Cell]) -> Result<(), ApplyError> {
    match conn.affected_rows() {
        1 => Ok(()),
        n => conflict_at(format!("a write affected {n} rows instead of 1"), key),
    }
}

/// Reads the row back and checks it is exactly `expected` (`None`: gone).
async fn read_back(
    conn: &mut Conn,
    table: &Table,
    target: &str,
    key: &[Cell],
    expected: Option<&Vec<Cell>>,
) -> Result<(), ApplyError> {
    let actual = select_row(conn, table, target, key, "").await?;
    if actual.as_ref() != expected {
        return conflict_at(
            "a row does not hold the reviewed values after writing it",
            key,
        );
    }
    Ok(())
}

async fn lock_row(
    conn: &mut Conn,
    table: &Table,
    target: &str,
    key: &[Cell],
) -> Result<Option<Vec<Cell>>, ApplyError> {
    select_row(conn, table, target, key, " FOR UPDATE").await
}

/// Reads one row by key over the text protocol, so its cells compare
/// byte for byte with the preview's.
async fn select_row(
    conn: &mut Conn,
    table: &Table,
    target: &str,
    key: &[Cell],
    suffix: &str,
) -> Result<Option<Vec<Cell>>, ApplyError> {
    let columns = table
        .columns
        .iter()
        .map(|c| quote(&c.name))
        .collect::<Vec<_>>()
        .join(", ");
    let condition = table
        .primary_key
        .iter()
        .zip(key)
        .map(|(&k, cell)| format!("{} = {}", quote(&table.columns[k].name), literal(cell)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let rows: Vec<Row> = conn
        .query(format!(
            "SELECT {columns} FROM {target} WHERE {condition}{suffix}"
        ))
        .await?;
    match rows.len() {
        0 => Ok(None),
        1 => Ok(Some(cells(rows.into_iter().next().expect("one row"))?)),
        n => conflict_at(format!("{n} rows share a primary key"), key),
    }
}

fn key_match(table: &Table) -> String {
    table
        .primary_key
        .iter()
        .map(|&k| format!("{} = ?", quote(&table.columns[k].name)))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn pick(table: &Table, image: &[Cell]) -> Vec<Cell> {
    table
        .primary_key
        .iter()
        .map(|&k| image[k].clone())
        .collect()
}

/// A value as a SQL literal that cannot be misread: hex bytes, as a binary
/// string the server converts to the column's type.
fn literal(cell: &Cell) -> String {
    match cell {
        None => "NULL".to_owned(),
        Some(bytes) => {
            let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
            format!("_binary X'{hex}'")
        }
    }
}

fn show_key(key: &[Cell]) -> String {
    let parts: Vec<String> = key
        .iter()
        .map(|c| match c {
            None => "NULL".to_owned(),
            Some(b) => String::from_utf8_lossy(b).into_owned(),
        })
        .collect();
    parts.join(", ")
}

fn cells(row: Row) -> Result<Vec<Cell>, ApplyError> {
    row.unwrap()
        .into_iter()
        .map(|v| match v {
            Value::NULL => Ok(None),
            Value::Bytes(b) => Ok(Some(b)),
            _ => conflict("unexpected value type"),
        })
        .collect()
}
