//! Rowbit adapter for MariaDB and MySQL. Both speak the same wire protocol,
//! so one adapter serves both and tells them apart by the server version.

pub mod statement;

use mysql_async::prelude::Queryable;
use rowbit_core::adapter::{Adapter, Engine, ServerInfo};

pub use mysql_async::Error;

pub struct MySqlAdapter {
    pool: mysql_async::Pool,
}

impl MySqlAdapter {
    /// `url` is a `mysql://user:password@host:port/database` URL.
    pub fn new(url: &str) -> Result<Self, Error> {
        let opts = mysql_async::Opts::from_url(url)?;
        Ok(Self {
            pool: mysql_async::Pool::new(opts),
        })
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
