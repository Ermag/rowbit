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

/// A server that requires TLS: refused in plain text, reached with the
/// TLS parameters other clients use, and its self-signed certificate
/// refused wherever the URL asks for it to be checked.
#[tokio::test]
async fn connects_over_tls_when_the_url_asks() {
    let Some(url) = common::url(common::TLS) else {
        return;
    };
    let plain = MySqlAdapter::new(&url).unwrap();
    let e = plain.server_info().await.unwrap_err();
    assert!(e.to_string().contains("secure"), "{e}");

    for tls in ["ssl-mode=REQUIRED", "sslaccept=accept_invalid_certs"] {
        let adapter = MySqlAdapter::new(&format!("{url}?{tls}")).expect("valid URL");
        let info = adapter
            .server_info()
            .await
            .unwrap_or_else(|e| panic!("{tls}: {e}"));
        assert_eq!(info.engine, Engine::MariaDb);
        adapter.close().await.unwrap();
    }

    for tls in [
        "ssl-mode=VERIFY_IDENTITY",
        "sslaccept=strict",
        "require_ssl=true",
    ] {
        let adapter = MySqlAdapter::new(&format!("{url}?{tls}")).expect("valid URL");
        assert!(
            adapter.server_info().await.is_err(),
            "{tls}: a self-signed certificate was accepted"
        );
    }
}
