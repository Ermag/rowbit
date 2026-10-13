//! Connects to the test databases from `compose.yaml`.
//!
//! Each test reads its URL from the environment and skips when it is unset,
//! unless `ROWBIT_REQUIRE_DB=1` (set in CI), which turns a missing URL into
//! a failure so the integration job can never pass by skipping.

use rowbit_core::adapter::{Adapter, Engine};
use rowbit_mysql::MySqlAdapter;

fn url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(url) => Some(url),
        Err(_) if std::env::var("ROWBIT_REQUIRE_DB").as_deref() == Ok("1") => {
            panic!("{var} is not set and ROWBIT_REQUIRE_DB=1")
        }
        Err(_) => {
            eprintln!("skipping: {var} is not set (run `docker compose up -d --wait`)");
            None
        }
    }
}

async fn check(var: &str, engine: Engine, version_prefix: &str) {
    let Some(url) = url(var) else { return };
    let adapter = MySqlAdapter::new(&url).expect("valid URL");
    let info = adapter.server_info().await.expect("server answers");
    assert_eq!(info.engine, engine, "{}", info.version);
    assert!(info.version.starts_with(version_prefix), "{}", info.version);
    adapter.close().await.unwrap();
}

#[tokio::test]
async fn connects_to_mariadb_11_4() {
    check("ROWBIT_TEST_MARIADB_URL", Engine::MariaDb, "11.4.").await;
}

#[tokio::test]
async fn connects_to_mysql_8_4() {
    check("ROWBIT_TEST_MYSQL_URL", Engine::MySql, "8.4.").await;
}
