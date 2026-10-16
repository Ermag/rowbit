//! A scoped agent's own database user. Rowbit runs on it what the database
//! should check against the agent's grants: its reads, an `EXPLAIN` of each
//! write, and approved schema changes. See "How scopes are checked" in
//! `docs/spec/policy.md`.

use std::time::Duration;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder};

use crate::grants::{self, Account, Current};
use crate::preview::{PreviewError, SQL_MODE};
use crate::query::{self, QueryLimits, QueryResult};
use crate::{ApplyError, MySqlAdapter};

/// How long an `EXPLAIN` may take.
const EXPLAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Server errors that mean the user lacks a privilege.
pub const DENIED: [u16; 4] = [
    1142, // command denied for table
    1143, // command denied for column
    1044, // access denied to database
    1227, // needs a privilege the user doesn't have
];

pub struct AgentDb {
    opts: Opts,
}

/// Why the database refused an `EXPLAIN`.
#[derive(Debug, thiserror::Error)]
pub enum ExplainError {
    /// The agent's user lacks a privilege the statement needs.
    #[error("outside the agent's scope: database error {0}")]
    Denied(u16),
    /// Anything else: an unknown table, a time-out, no connection.
    #[error("{0}")]
    Failed(String),
}

impl MySqlAdapter {
    /// The same server, as an agent's own user.
    pub fn agent(&self, user: &str, password: &str) -> AgentDb {
        AgentDb {
            opts: OptsBuilder::from_opts(self.opts.clone())
                .user(Some(user))
                .pass(Some(password))
                .into(),
        }
    }
}

/// A connection with `database` as the default, or with none if the user
/// holds nothing on it: names in other databases still resolve, and the
/// database refuses the rest.
pub(crate) async fn connect_default(
    opts: &Opts,
    database: &str,
) -> Result<Conn, mysql_async::Error> {
    match Conn::new(OptsBuilder::from_opts(opts.clone()).db_name(Some(database))).await {
        Err(mysql_async::Error::Server(e)) if e.code == 1044 => {
            Conn::new(OptsBuilder::from_opts(opts.clone()).db_name(None::<String>)).await
        }
        other => other,
    }
}

impl AgentDb {
    /// Runs a read on the agent's user. See [`MySqlAdapter::query`].
    pub async fn query(
        &self,
        database: &str,
        sql: &str,
        limits: QueryLimits,
    ) -> Result<QueryResult, PreviewError> {
        query::query(&self.opts, database, sql, limits).await
    }

    /// Asks the server to plan `sql` as the agent's user. The server checks
    /// every privilege the statement needs and runs nothing.
    pub async fn explain(&self, database: &str, sql: &str) -> Result<(), ExplainError> {
        let work = async {
            let mut conn = connect_default(&self.opts, database).await?;
            conn.query_drop(format!("SET SESSION sql_mode = '{SQL_MODE}'"))
                .await?;
            let result = conn.query_drop(format!("EXPLAIN {sql}")).await;
            let _ = conn.disconnect().await;
            result
        };
        match tokio::time::timeout(EXPLAIN_TIMEOUT, work).await {
            Err(_) => Err(ExplainError::Failed(format!(
                "the scope check took longer than {EXPLAIN_TIMEOUT:?}"
            ))),
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => match server_code(&e) {
                Some(code) if DENIED.contains(&code) => Err(ExplainError::Denied(code)),
                Some(code) => Err(ExplainError::Failed(format!(
                    "the scope check failed: database error {code}"
                ))),
                None => Err(ExplainError::Failed(
                    "the scope check could not reach the database".into(),
                )),
            },
        }
    }

    /// Runs an approved schema change on the agent's user, so the database
    /// enforces `scopes.ddl`. See [`MySqlAdapter::run_ddl`].
    pub async fn run_ddl(&self, database: &str, sql: &str) -> Result<(), ApplyError> {
        crate::apply::run_ddl(&self.opts, database, sql).await
    }

    /// Who the user is and what it holds, as the server reports it to the
    /// user itself.
    pub async fn grants(&self) -> Result<(Account, Current), mysql_async::Error> {
        let mut conn =
            Conn::new(OptsBuilder::from_opts(self.opts.clone()).db_name(None::<String>)).await?;
        let me: Option<String> = conn.query_first("SELECT CURRENT_USER()").await?;
        let account = Account::parse_current(&me.unwrap_or_default()).ok_or_else(|| {
            mysql_async::Error::Other("the server did not say who the user is".into())
        })?;
        let current = grants::current(&mut conn, &account).await;
        let _ = conn.disconnect().await;
        Ok((account, current?))
    }
}

fn server_code(e: &mysql_async::Error) -> Option<u16> {
    match e {
        mysql_async::Error::Server(e) => Some(e.code),
        _ => None,
    }
}
