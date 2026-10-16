//! Runs an agent's read for the MCP `query` tool: one checked `SELECT`,
//! in a read-only transaction, with a time limit and a row cap.

use std::time::Duration;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, OptsBuilder, Value};

use crate::preview::{PreviewError, SQL_MODE};

/// What a read returned. Values are the server's text, `None` for `NULL`;
/// bytes that are not UTF-8 are shown as `0x` and hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    /// More rows matched than were returned.
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct QueryLimits {
    pub max_rows: usize,
    pub timeout: Duration,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            max_rows: 1_000,
            timeout: Duration::from_secs(10),
        }
    }
}

pub(crate) async fn query(
    opts: &mysql_async::Opts,
    database: &str,
    sql: &str,
    limits: QueryLimits,
) -> Result<QueryResult, PreviewError> {
    let work = async {
        let opts = OptsBuilder::from_opts(opts.clone()).db_name(Some(database));
        let mut conn = Conn::new(opts).await?;
        let result = run(&mut conn, sql, limits).await;
        let _ = conn.query_drop("ROLLBACK").await;
        let _ = conn.disconnect().await;
        result
    };
    tokio::time::timeout(limits.timeout, work)
        .await
        .unwrap_or_else(|_| {
            Err(PreviewError::Refused(format!(
                "the query took longer than {:?}",
                limits.timeout
            )))
        })
}

async fn run(conn: &mut Conn, sql: &str, limits: QueryLimits) -> Result<QueryResult, PreviewError> {
    let version: Option<String> = conn.query_first("SELECT VERSION()").await?;
    let statement_limit = if version
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("mariadb")
    {
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
    conn.query_drop("START TRANSACTION READ ONLY").await?;

    let mut result = conn.query_iter(sql).await?;
    let columns = result
        .columns_ref()
        .iter()
        .map(|c| c.name_str().into_owned())
        .collect();
    let mut rows = Vec::new();
    let mut truncated = false;
    while let Some(row) = result.next().await? {
        if rows.len() == limits.max_rows {
            truncated = true;
            result.drop_result().await?;
            break;
        }
        rows.push(row.unwrap().into_iter().map(show).collect());
    }
    Ok(QueryResult {
        columns,
        rows,
        truncated,
    })
}

fn show(value: Value) -> Option<String> {
    match value {
        Value::NULL => None,
        Value::Bytes(bytes) => Some(String::from_utf8(bytes).unwrap_or_else(|e| {
            let hex: String = e.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
            format!("0x{hex}")
        })),
        other => Some(other.as_sql(true)),
    }
}
