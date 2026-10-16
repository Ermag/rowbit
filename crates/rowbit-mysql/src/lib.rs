//! Rowbit adapter for MariaDB and MySQL. Both speak the same wire protocol,
//! so one adapter serves both and tells them apart by the server version.

mod apply;
mod preview;
mod query;
pub mod statement;

pub use apply::ApplyError;
pub use preview::{PreviewError, PreviewLimits, db_error_code};
pub use query::{QueryLimits, QueryResult};

use mysql_async::prelude::Queryable;
use rowbit_core::adapter::{Adapter, Engine, ServerInfo};
use rowbit_core::preview::{Applied, Preview};

use crate::statement::Classified;

pub use mysql_async::Error;

pub struct MySqlAdapter {
    opts: mysql_async::Opts,
    pool: mysql_async::Pool,
}

impl MySqlAdapter {
    /// `url` is a `mysql://user:password@host:port/database` URL.
    pub fn new(url: &str) -> Result<Self, Error> {
        let opts = mysql_async::Opts::from_url(url)?;
        Ok(Self {
            pool: mysql_async::Pool::new(opts.clone()),
            opts,
        })
    }

    /// Previews a write without running it. `database` is the default for
    /// names the statement does not qualify.
    pub async fn preview(
        &self,
        database: &str,
        statement: &Classified,
        limits: PreviewLimits,
    ) -> Result<Preview, PreviewError> {
        preview::preview(&self.opts, database, statement, limits).await
    }

    /// Writes an approved preview by primary key, after checking that the
    /// rows still match it. `statement` is the classified statement the
    /// preview came from. Writes nothing unless every check passes.
    pub async fn apply(
        &self,
        database: &str,
        statement: &Classified,
        preview: &Preview,
    ) -> Result<Applied, ApplyError> {
        apply::apply(&self.opts, database, statement, preview).await
    }

    /// Restores the before images of an applied preview, if every row still
    /// holds what apply wrote.
    pub async fn undo(
        &self,
        database: &str,
        preview: &Preview,
        applied: &Applied,
    ) -> Result<(), ApplyError> {
        apply::undo(&self.opts, database, preview, applied).await
    }

    /// Runs a read for an agent: `sql` must come from
    /// [`statement::classify_read`]. Read only, time-limited, row-capped.
    pub async fn query(
        &self,
        database: &str,
        sql: &str,
        limits: QueryLimits,
    ) -> Result<QueryResult, PreviewError> {
        query::query(&self.opts, database, sql, limits).await
    }

    /// Runs an approved DDL statement as reviewed.
    pub async fn run_ddl(&self, database: &str, sql: &str) -> Result<(), ApplyError> {
        apply::run_ddl(&self.opts, database, sql).await
    }

    /// Closes every pooled connection.
    pub async fn close(self) -> Result<(), Error> {
        self.pool.disconnect().await
    }
}

impl Adapter for MySqlAdapter {
    type Error = Error;

    async fn server_info(&self) -> Result<ServerInfo, Error> {
        let mut conn = self.pool.get_conn().await?;
        let version: Option<String> = conn.query_first("SELECT VERSION()").await?;
        let version = version.unwrap_or_default();
        let engine = if version.to_ascii_lowercase().contains("mariadb") {
            Engine::MariaDb
        } else {
            Engine::MySql
        };
        Ok(ServerInfo { engine, version })
    }
}
