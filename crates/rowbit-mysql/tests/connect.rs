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

/// A server that requires TLS, with a certificate from the test CA in
/// scripts/test-tls: refused in plain text, reached when the URL asks for
/// TLS, its certificate refused unless the URL names that CA, and accepted,
/// host name and all, when it does.
#[tokio::test]
async fn connects_over_tls_when_the_url_asks() {
    let Some(url) = common::url(common::TLS) else {
        return;
    };
    let ca = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/test-tls/ca.pem")
        .canonicalize()
        .unwrap();
    let ca = ca.to_str().unwrap();
    let connect = |tls: String| {
        let url = format!("{url}?{tls}");
        async move {
            let adapter = MySqlAdapter::new(&url).expect("valid URL");
            let result = adapter.server_info().await;
            let _ = adapter.close().await;
            result
        }
    };

    let e = connect("ssl-mode=DISABLED".into()).await.unwrap_err();
    assert!(e.to_string().contains("secure transport"), "{e}");

    for tls in [
        "ssl-mode=REQUIRED",
        "ssl-mode=PREFERRED",
        "sslaccept=accept_invalid_certs",
    ] {
        let info = connect(tls.into())
            .await
            .unwrap_or_else(|e| panic!("{tls}: {e}"));
        assert_eq!(info.engine, Engine::MariaDb);
    }

    // Mozilla's roots don't vouch for the test CA: refused for the
    // certificate, not for want of TLS.
    for tls in [
        "ssl-mode=VERIFY_IDENTITY",
        "sslaccept=strict",
        "require_ssl=true",
    ] {
        let e = connect(tls.into()).await.unwrap_err();
        let e = e.to_string();
        assert!(
            e.contains("certificate") && !e.contains("secure transport"),
            "{tls}: {e}"
        );
    }

    for tls in [
        format!("ssl-mode=VERIFY_IDENTITY&ssl-ca={ca}"),
        format!("ssl-mode=VERIFY_CA&ssl-ca={ca}"),
        format!("ssl-ca={ca}"),
    ] {
        let info = connect(tls.clone())
            .await
            .unwrap_or_else(|e| panic!("{tls}: {e}"));
        assert_eq!(info.engine, Engine::MariaDb);
    }
}
