//! The interface each database engine implements.
//!
//! v0.0 only needs a connection check. Preview, apply and undo are added in
//! v0.1, and the interface is frozen in v0.3.

use std::future::Future;

/// Database engines Rowbit knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    MariaDb,
    MySql,
}

/// What an adapter reports about the server it is connected to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    pub engine: Engine,
    /// The server's own version string, for example `11.4.3-MariaDB`.
    pub version: String,
}

pub trait Adapter {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Round-trips to the server and reports what it is.
    fn server_info(&self) -> impl Future<Output = Result<ServerInfo, Self::Error>> + Send;
}
