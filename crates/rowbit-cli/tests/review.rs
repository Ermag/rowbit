//! A person reviewing changes with the `rowbit` binary: list, show,
//! approve, reject, undo, verify. Changes are proposed through the engine,
//! as the MCP server would, into the same SQLite file the binary uses.

use std::path::{Path, PathBuf};
use std::process::Command;

use mysql_async::prelude::Queryable;
use rowbit_engine::Engine;
use rowbit_mysql::MySqlAdapter;
use rowbit_policy::PolicyFile;
use rowbit_store::Store;

const POLICY: &str = "version: 0\nagents:\n  refund-bot:\n    default: hold\n";

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
}

impl Setup {
    fn rowbit(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_rowbit"))
            .args(args)
            .env("ROWBIT_DATABASE_URL", &self.url)
            .env("ROWBIT_POLICY", self.dir.join("rowbit.yml"))
            .env("ROWBIT_STORE", self.dir.join("rowbit.db"))
            .env("ROWBIT_USER", "philip")
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    async fn propose(&self, sql: &str) -> i64 {
        let engine = Engine::new(
            MySqlAdapter::new(&self.url).unwrap(),
            PolicyFile::from_yaml(POLICY).unwrap(),
            Store::open(&self.dir.join("rowbit.db")).unwrap(),
        );
        engine
            .propose("refund-bot", "rowbit_test", sql)
            .await
            .unwrap()
            .id
    }
}

async fn setup(var: &str, table: &str) -> Option<Setup> {
    let url = url(var)?;
    let dir = std::env::temp_dir().join(format!("rowbit-cli-{table}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("rowbit.yml"), POLICY).unwrap();
    let mut conn = mysql_async::Conn::from_url(&url).await.unwrap();
    conn.query_drop(format!(
        "DROP TABLE IF EXISTS {table}; \
         CREATE TABLE {table} (id INT PRIMARY KEY, status VARCHAR(20) NOT NULL) ENGINE=InnoDB; \
         INSERT INTO {table} VALUES (1, 'approved'), (2, 'approved')"
    ))
    .await
    .unwrap();
    conn.disconnect().await.unwrap();
    Some(Setup { dir, url })
}

fn review_hash(show: &str) -> String {
    show.lines()
        .find_map(|l| l.strip_prefix("Review    "))
        .expect("show prints the review hash")
        .trim()
        .to_owned()
}

async fn statuses(url: &str, table: &str) -> Vec<String> {
    let mut conn = mysql_async::Conn::from_url(url).await.unwrap();
    let s = conn
        .query(format!("SELECT status FROM {table} ORDER BY id"))
        .await
        .unwrap();
    conn.disconnect().await.unwrap();
    s
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

macro_rules! on_both {
    ($scenario:ident) => {
        mod $scenario {
            #[tokio::test]
            async fn mariadb() {
                super::$scenario("ROWBIT_TEST_MARIADB_URL", "cli_m").await
            }
            #[tokio::test]
            async fn mysql() {
                super::$scenario("ROWBIT_TEST_MYSQL_URL", "cli_y").await
            }
        }
    };
}

on_both!(a_person_reviews_approves_and_undoes);
async fn a_person_reviews_approves_and_undoes(var: &str, table: &str) {
    let t = format!("{table}_a");
    let Some(s) = setup(var, &t).await else {
        return;
    };
    let id = s
        .propose(&format!("UPDATE {t} SET status = 'refunded'"))
        .await
        .to_string();

    let (ok, list) = s.rowbit(&["changes", "--state", "held"]);
    assert!(ok, "{list}");
    assert!(
        list.contains("held") && list.contains("refund-bot"),
        "{list}"
    );

    let (ok, show) = s.rowbit(&["show", &id]);
    assert!(ok, "{show}");
    assert!(show.contains("This change updates 2 rows"), "{show}");
    assert!(
        show.contains(r#"status               "approved" -> "refunded""#),
        "{show}"
    );
    let hash = review_hash(&show);

    let (ok, out) = s.rowbit(&["approve", &id, "--review", "000000000000"]);
    assert!(!ok, "{out}");
    let (ok, out) = s.rowbit(&["approve", &id, "--review", &hash[..12]]);
    assert!(ok && out.contains("applied"), "{out}");
    assert_eq!(statuses(&s.url, &t).await, ["refunded", "refunded"]);

    let (ok, out) = s.rowbit(&["undo", &id]);
    assert!(ok && out.contains("undone"), "{out}");
    assert_eq!(statuses(&s.url, &t).await, ["approved", "approved"]);

    let (_, show) = s.rowbit(&["show", &id]);
    assert!(show.contains("human:philip"), "{show}");
    let (ok, out) = s.rowbit(&["verify"]);
    assert!(ok && out.contains("audit log ok"), "{out}");
    cleanup(&s.dir);
}

on_both!(a_person_rejects);
async fn a_person_rejects(var: &str, table: &str) {
    let t = format!("{table}_r");
    let Some(s) = setup(var, &t).await else {
        return;
    };
    let id = s
        .propose(&format!("DELETE FROM {t} WHERE id = 1"))
        .await
        .to_string();
    let (ok, out) = s.rowbit(&["reject", &id, "--reason", "keep it"]);
    assert!(ok && out.contains("rejected"), "{out}");
    let (ok, out) = s.rowbit(&["undo", &id]);
    assert!(!ok, "{out}");
    assert_eq!(statuses(&s.url, &t).await, ["approved", "approved"]);
    cleanup(&s.dir);
}

// Found in review: approving covered rows no surface ever showed, and an
// agent's escape codes reached the reviewer's terminal.
on_both!(big_changes_and_hostile_text);
async fn big_changes_and_hostile_text(var: &str, table: &str) {
    let t = format!("{table}_b");
    let Some(s) = setup(var, &t).await else {
        return;
    };
    let mut conn = mysql_async::Conn::from_url(&s.url).await.unwrap();
    for id in 3..=60 {
        conn.query_drop(format!("INSERT INTO {t} VALUES ({id}, 'approved')"))
            .await
            .unwrap();
    }
    conn.disconnect().await.unwrap();

    let id = s
        .propose(&format!(
            "UPDATE {t} SET status = 'x\u{1b}[8m' /* \u{202e} safe */ WHERE id > 0"
        ))
        .await
        .to_string();
    let (ok, show) = s.rowbit(&["show", &id]);
    assert!(ok, "{show}");
    assert!(
        !show.contains('\u{1b}') && !show.contains('\u{202e}'),
        "{show:?}"
    );
    let statement = show.lines().find(|l| l.starts_with("Statement")).unwrap();
    assert!(
        !statement.contains("safe"),
        "comments are not shown: {statement}"
    );
    assert!(
        show.contains("... and 10 more. Approving covers all 60"),
        "{show}"
    );
    // Keys may print quoted or plain; either way row 60 is not listed.
    let row_60 = |text: &str| text.contains("row 60\n") || text.contains("row \"60\"\n");
    assert!(!row_60(&show), "{show}");
    let (_, all) = s.rowbit(&["show", &id, "--all"]);
    assert!(row_60(&all), "{all}");

    let hash = review_hash(&show);
    let (ok, out) = s.rowbit(&["approve", &id, "--review", &hash]);
    assert!(!ok && out.contains("--rows 60"), "{out}");
    let (ok, out) = s.rowbit(&["approve", &id, "--review", &hash, "--rows", "59"]);
    assert!(!ok, "{out}");
    let (ok, out) = s.rowbit(&["approve", &id, "--review", &hash, "--rows", "60"]);
    assert!(ok && out.contains("applied"), "{out}");
    cleanup(&s.dir);
}
