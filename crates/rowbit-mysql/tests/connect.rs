//! Connects to the test databases from `compose.yaml`.

mod common;

use rowbit_core::adapter::{Adapter, Engine};
use rowbit_mysql::MySqlAdapter;

async fn check(var: &str, engine: Engine, version_prefix: &str) {
    let Some(url) = common::url(var) else { return };
    let adapter = MySqlAdapter::new(&url).expect("valid URL");
    let info = adapter.server_info().await.expect("server answers");
    assert_eq!(info.engine, engine, "{}", info.version);
    assert!(info.version.starts_with(version_prefix), "{}", info.version);
    adapter.close().await.unwrap();
}

#[tokio::test]
async fn connects_to_mariadb_11_4() {
    check(common::MARIADB, Engine::MariaDb, "11.4.").await;
}

#[tokio::test]
async fn connects_to_mysql_8_4() {
    check(common::MYSQL, Engine::MySql, "8.4.").await;
}
