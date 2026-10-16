//! `rowbit changes --shadowed` and `rowbit policy test`, on changes an agent
//! in shadow mode proposed through the engine into the binary's state file.

use std::process::Command;

use mysql_async::prelude::Queryable;
use rowbit_engine::Engine;
use rowbit_mysql::MySqlAdapter;
use rowbit_policy::PolicyFile;
use rowbit_store::Store;

fn policy(table: &str, mode: &str, max: u32) -> String {
    format!(
        "version: 0\nagents:\n  support-bot:\n    mode: {mode}\n    rules:\n      - name: small\n        when: {{ table: rowbit_test.{table}, rows: {{ max: {max} }} }}\n        then: allow\n"
    )
}

async fn shadow_then_replay(var: &str, table: &str) {
    let Ok(url) = std::env::var(var) else {
        assert!(
            std::env::var("ROWBIT_REQUIRE_DB").as_deref() != Ok("1"),
            "{var} is not set"
        );
        return;
    };
    let mut conn = mysql_async::Conn::from_url(&url).await.unwrap();
    conn.query_drop(format!(
        "DROP TABLE IF EXISTS {table}; CREATE TABLE {table} (id INT PRIMARY KEY, s VARCHAR(9)) ENGINE=InnoDB; \
         INSERT INTO {table} VALUES (1, 'a'), (2, 'a'), (3, 'a')"
    ))
    .await
    .unwrap();
    conn.disconnect().await.unwrap();
    let dir = std::env::temp_dir().join(format!("rowbit-cli-{table}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = dir.join("rowbit.db");
    let engine = Engine::new(
        MySqlAdapter::new(&url).unwrap(),
        PolicyFile::from_yaml(&policy(table, "shadow", 1)).unwrap(),
        Store::open(&store).unwrap(),
    );
    for sql in [
        format!("UPDATE {table} SET s = 'b' WHERE id = 1"),
        format!("UPDATE {table} SET s = 'c' WHERE id IN (2, 3)"),
    ] {
        engine
            .propose("support-bot", "rowbit_test", &sql)
            .await
            .unwrap();
    }
    drop(engine);

    let rowbit = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_rowbit"))
            .args(args)
            .env("ROWBIT_STORE", &store)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    let shadowed = rowbit(&["changes", "--shadowed"]);
    assert_eq!(shadowed.lines().count(), 1, "{shadowed}");
    assert!(
        shadowed.contains("would hold: no rule matched (policy:default)"),
        "{shadowed}"
    );

    let looser = dir.join("looser.yml");
    std::fs::write(&looser, policy(table, "enforce", 2)).unwrap();
    let out = rowbit(&["policy", "test", looser.to_str().unwrap()]);
    assert!(
        out.contains("was held, would be allowed by rule small"),
        "{out}"
    );
    assert!(out.contains("2 recorded change(s) replayed"), "{out}");
    assert!(out.contains("1 would be decided differently"), "{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn mariadb_shadow_then_replay() {
    shadow_then_replay("ROWBIT_TEST_MARIADB_URL", "cli_shadow_m").await;
}

#[tokio::test]
async fn mysql_shadow_then_replay() {
    shadow_then_replay("ROWBIT_TEST_MYSQL_URL", "cli_shadow_y").await;
}
