//! `rowbit grants diff` and `apply` with the binary, as an operator runs
//! them, on both test databases.

use std::path::PathBuf;
use std::process::Command;

use mysql_async::prelude::Queryable;
use rowbit_store::Store;

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

struct Setup {
    dir: PathBuf,
    url: String,
    admin: String,
}

impl Setup {
    fn rowbit(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_rowbit"))
            .args(args)
            .env("ROWBIT_DATABASE_URL", &self.url)
            .env("ROWBIT_ADMIN_DATABASE_URL", &self.admin)
            .env("ROWBIT_POLICY", self.dir.join("rowbit.yml"))
            .env("ROWBIT_STORE", self.dir.join("rowbit.db"))
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    fn policy(&self, yaml: &str) {
        std::fs::write(self.dir.join("rowbit.yml"), yaml).unwrap();
    }

    fn password(&self, agent: &str) -> Option<String> {
        Store::open(&self.dir.join("rowbit.db"))
            .unwrap()
            .setting(&format!("agent.{agent}.password"))
            .unwrap()
    }
}

async fn admin(url: &str) -> mysql_async::Conn {
    mysql_async::Conn::new(mysql_async::Opts::from_url(url).unwrap())
        .await
        .unwrap()
}

async fn user_count(conn: &mut mysql_async::Conn) -> i64 {
    conn.query_first("SELECT COUNT(*) FROM mysql.user WHERE User = 'rowbit_cli-grants'")
        .await
        .unwrap()
        .unwrap()
}

async fn operator_runs_grants(var: &str) {
    let Some(url) = url(var) else { return };
    let admin_url = url.replacen("//rowbit:rowbit@", "//root:rowbit@", 1);
    let dir = std::env::temp_dir().join(format!("rowbit-cli-grants-{var}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s = Setup {
        dir,
        url: url.clone(),
        admin: admin_url.clone(),
    };
    let mut conn = admin(&admin_url).await;
    for sql in [
        "DROP USER IF EXISTS 'rowbit_cli-grants'@'%'",
        "DROP DATABASE IF EXISTS rg_cli",
        "CREATE DATABASE rg_cli",
        "CREATE TABLE rg_cli.orders (id INT PRIMARY KEY, status VARCHAR(10), email VARCHAR(50))",
    ] {
        conn.query_drop(sql).await.unwrap();
    }

    s.policy(
        "version: 0\nagents:\n  cli-grants:\n    scopes:\n      read: [rg_cli.orders]\n      update: [rg_cli.orders]\n      deny_read: { columns: [rg_cli.orders.email] }\n  plain: {}\n",
    );
    let (ok, out) = s.rowbit(&["grants", "diff"]);
    assert!(ok, "{out}");
    assert!(out.contains("create the user"), "{out}");
    assert!(
        out.contains("GRANT SELECT (`id`, `status`), UPDATE ON `rg_cli`.`orders`"),
        "{out}"
    );
    assert!(out.contains("plain  no scopes"), "{out}");

    // A misspelled deny entry would deny nothing: refused, and nothing runs.
    s.policy(
        "version: 0\nagents:\n  cli-grants:\n    scopes:\n      read: [rg_cli.orders]\n      deny_read: { columns: [rg_cli.orders.emial] }\n",
    );
    let (ok, out) = s.rowbit(&["grants", "diff"]);
    assert!(!ok && out.contains("emial matches no column"), "{out}");
    let (ok, out) = s.rowbit(&["grants", "apply", "--yes"]);
    assert!(!ok && out.contains("nothing changed"), "{out}");
    s.policy(
        "version: 0\nagents:\n  cli-grants:\n    scopes:\n      read: [rg_cli.orders]\n      update: [rg_cli.orders]\n      deny_read: { columns: [rg_cli.orders.email] }\n  plain: {}\n",
    );

    // Without a terminal, apply refuses to ask and changes nothing.
    let (ok, out) = s.rowbit(&["grants", "apply"]);
    assert!(!ok, "{out}");
    assert!(out.contains("--yes"), "{out}");

    let (ok, out) = s.rowbit(&["grants", "apply", "--yes"]);
    assert!(ok, "{out}");
    let first = s.password("cli-grants").expect("password kept");
    let (ok, out) = s.rowbit(&["grants", "diff"]);
    assert!(ok && out.contains("up to date"), "{out}");

    // The agent's user works with the kept password, and only within scope.
    let agent_url = url
        .replacen(
            "//rowbit:rowbit@",
            &format!("//rowbit_cli-grants:{first}@"),
            1,
        )
        .replace("/rowbit_test", "/rg_cli");
    let mut agent = admin(&agent_url).await;
    agent
        .query_drop("SELECT id, status FROM orders")
        .await
        .unwrap();
    assert!(agent.query_drop("SELECT email FROM orders").await.is_err());
    let _ = agent.disconnect().await;

    let (ok, out) = s.rowbit(&["grants", "apply", "--yes", "--rotate-passwords"]);
    assert!(ok && out.contains("new password"), "{out}");
    assert_ne!(s.password("cli-grants").unwrap(), first);

    // Serving agents checks each scoped agent's user first. Rowbit's own
    // user can't see rg_cli, so the grants look like more than rowbit.yml
    // gives: the operator is told, and the agent is refused.
    let (ok, out) = s.rowbit(&[
        "propose",
        "--agent",
        "cli-grants",
        "UPDATE rg_cli.orders SET status = 'x' WHERE id = 1",
    ]);
    assert!(ok, "{out}");
    assert!(out.contains("agent cli-grants is refused"), "{out}");
    assert!(out.contains(r#""state":"rejected""#), "{out}");

    // An agent that loses its scopes keeps its user until asked.
    s.policy("version: 0\nagents:\n  plain: {}\n");
    let (ok, out) = s.rowbit(&["grants", "apply", "--yes"]);
    assert!(ok, "{out}");
    assert_eq!(user_count(&mut conn).await, 1);
    let (ok, out) = s.rowbit(&["grants", "apply", "--yes", "--drop-removed"]);
    assert!(ok && out.contains("drop the user"), "{out}");
    assert_eq!(user_count(&mut conn).await, 0);
    assert_eq!(s.password("cli-grants"), None);

    conn.query_drop("DROP DATABASE rg_cli").await.unwrap();
    let _ = conn.disconnect().await;
    let _ = std::fs::remove_dir_all(&s.dir);
}

#[tokio::test]
async fn mariadb_operator_runs_grants() {
    operator_runs_grants("ROWBIT_TEST_MARIADB_URL").await;
}

#[tokio::test]
async fn mysql_operator_runs_grants() {
    operator_runs_grants("ROWBIT_TEST_MYSQL_URL").await;
}
