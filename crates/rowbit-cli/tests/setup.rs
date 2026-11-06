//! `rowbit setup` against the test databases, with a home directory of its
//! own, and other commands finding the setup it made. See "rowbit setup"
//! and "Where commands find their files" in docs/spec/setup.md.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use mysql_async::prelude::Queryable;

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

struct Case {
    home: PathBuf,
    work: PathBuf,
}

impl Case {
    fn new(name: &str) -> Case {
        let root = std::env::temp_dir().join(format!("rowbit-setup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (home, work) = (root.join("home"), root.join("work"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        Case { home, work }
    }

    fn setup_dir(&self) -> PathBuf {
        self.home.join(".rowbit/default")
    }

    fn run_in(&self, cwd: &Path, args: &[&str]) -> (bool, String) {
        self.run_with(cwd, args, &[])
    }

    fn run_with(&self, cwd: &Path, args: &[&str], vars: &[(&str, &str)]) -> (bool, String) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_rowbit"));
        cmd.args(args)
            .current_dir(cwd)
            .env("HOME", &self.home)
            .env("PATH", "/bin")
            .env("ROWBIT_USER", "philip")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("ROWBIT_DATABASE_URL")
            .env_remove("DATABASE_URL")
            .env_remove("ROWBIT_DIR")
            .env_remove("ROWBIT_SERVER_CONFIG")
            .env_remove("ROWBIT_POLICY")
            .env_remove("ROWBIT_STORE");
        for (k, v) in vars {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    fn run(&self, args: &[&str]) -> (bool, String) {
        self.run_in(&self.work, args)
    }
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

macro_rules! on_both {
    ($scenario:ident) => {
        mod $scenario {
            #[tokio::test]
            async fn mariadb() {
                super::$scenario("ROWBIT_TEST_MARIADB_URL", "setup_maria").await
            }
            #[tokio::test]
            async fn mysql() {
                super::$scenario("ROWBIT_TEST_MYSQL_URL", "setup_mysql").await
            }
        }
    };
}

on_both!(sets_up_and_other_commands_find_it);

async fn sets_up_and_other_commands_find_it(var: &str, table: &str) {
    let Some(url) = url(var) else { return };
    let mut conn = mysql_async::Conn::from_url(&url).await.unwrap();
    conn.query_drop(format!(
        "DROP TABLE IF EXISTS {table}; \
         CREATE TABLE {table} (id INT PRIMARY KEY, status VARCHAR(20) NOT NULL) ENGINE=InnoDB; \
         INSERT INTO {table} VALUES (1, 'new'), (2, 'new')"
    ))
    .await
    .unwrap();
    conn.disconnect().await.unwrap();
    let c = Case::new(table);

    let (ok, out) = c.run(&["setup", "--database-url", &url]);
    assert!(
        !ok && out.contains("without a terminal, pass --yes"),
        "{out}"
    );
    let (ok, out) = c.run(&["setup", "--yes", "--no-client"]);
    assert!(
        !ok && out.contains("put it in ROWBIT_DATABASE_URL"),
        "{out}"
    );
    // The demo's URL left in the shell isn't taken for yours.
    let demo = "mysql://rowbit:rowbit-quickstart@127.0.0.1:33070/shop";
    let (ok, out) = c.run_with(
        &c.work,
        &["setup", "--yes", "--no-client"],
        &[("ROWBIT_DATABASE_URL", demo)],
    );
    assert!(!ok && out.contains("the demo's database"), "{out}");

    // The URL from the environment, not the command line; a Cursor to connect.
    std::fs::create_dir_all(c.home.join(".cursor")).unwrap();
    let (ok, out) = c.run_with(
        &c.work,
        &["setup", "--yes", "--client", "cursor", "--no-service"],
        &[("ROWBIT_DATABASE_URL", &url)],
    );
    assert!(ok, "{out}");
    assert!(out.contains("(from ROWBIT_DATABASE_URL)"), "{out}");
    assert!(
        out.contains("Added rowbit to ") && out.contains(".cursor/mcp.json as cursor"),
        "{out}"
    );
    assert!(out.contains("Connected to "), "{out}");
    assert!(out.contains("Review page: http://127.0.0.1:"), "{out}");
    assert!(out.contains("password: "), "{out}");
    let dir = c.setup_dir();
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("env")), 0o600);
    let env = std::fs::read_to_string(dir.join("env")).unwrap();
    assert_eq!(env, format!("ROWBIT_DATABASE_URL={url}\n"));
    let policy = std::fs::read_to_string(dir.join("rowbit.yml")).unwrap();
    assert!(
        policy.contains("cursor:") && policy.contains("# Tables in rowbit_test"),
        "{policy}"
    );
    assert!(
        std::fs::read_to_string(dir.join("rowbit-server.yml"))
            .unwrap()
            .contains("http: true")
    );

    // Elsewhere, commands find ~/.rowbit/default, and the URL in its env file.
    let (ok, out) = c.run(&[
        "propose",
        "--agent",
        "cursor",
        &format!("UPDATE {table} SET status = 'done' WHERE id = 1"),
    ]);
    assert!(ok && out.contains("\"state\":\"held\""), "{out}");
    let (ok, out) = c.run(&["changes"]);
    assert!(ok && out.contains("held"), "{out}");
    let elsewhere = c.home.clone();
    let (ok, out) = c.run_in(
        &elsewhere,
        &["changes", "--dir", dir.to_str().unwrap(), "--state", "held"],
    );
    assert!(ok && out.contains("cursor"), "{out}");

    // A URL left in the shell doesn't beat the setup's own.
    let (ok, out) = c.run_with(
        &c.work,
        &[
            "propose",
            "--agent",
            "cursor",
            &format!("UPDATE {table} SET status = 'x' WHERE id = 2"),
        ],
        &[("ROWBIT_DATABASE_URL", demo)],
    );
    assert!(
        ok && out.contains("not ROWBIT_DATABASE_URL from your shell"),
        "{out}"
    );

    // Running it again keeps the files and the page password, leaves the
    // connected client alone and adds a new one as its own agent.
    std::fs::create_dir_all(c.home.join(".codeium/windsurf")).unwrap();
    let (ok, out) = c.run(&[
        "setup",
        "--yes",
        "--client",
        "cursor",
        "--client",
        "windsurf",
        "--no-service",
    ]);
    assert!(ok && out.contains("Found the setup in"), "{out}");
    assert!(out.contains("(from this setup)"), "{out}");
    assert!(out.contains("password as set before"), "{out}");
    assert!(out.contains("Cursor already has Rowbit"), "{out}");
    assert!(out.contains("Added agent windsurf"), "{out}");
    assert!(out.contains("mcp_config.json as windsurf"), "{out}");
    assert!(!out.contains("Couldn't connect"), "{out}");
    let policy = std::fs::read_to_string(dir.join("rowbit.yml")).unwrap();
    assert!(
        policy.contains("cursor:") && policy.contains("windsurf:"),
        "{policy}"
    );

    // ROWBIT_DATABASE_URL set on purpose wins over the setup's own URL, so a
    // re-run switches users. An application's user gets no grants printed
    // for it, a user of Rowbit's own suggested, and setup ends not ready.
    let app = url.replacen("//rowbit:rowbit@", "//limited:limited@", 1);
    let (ok, out) = c.run_with(
        &c.work,
        &["setup", "--yes", "--no-client", "--no-service"],
        &[("ROWBIT_DATABASE_URL", &app)],
    );
    assert!(!ok, "{out}");
    assert!(out.contains("(from ROWBIT_DATABASE_URL)"), "{out}");
    assert!(
        !out.contains("TO 'limited'"),
        "no grants for the app's user: {out}"
    );
    assert!(out.contains("CREATE USER 'rowbit'@'%'"), "{out}");
    assert!(out.contains("<a new password>"), "{out}");
    assert!(out.contains("not a user of its own"), "{out}");
    assert!(
        out.contains("not ready: limited@% lacks TRIGGER, PROCESS"),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("env")).unwrap(),
        format!("ROWBIT_DATABASE_URL={app}\n")
    );
    let (ok, out) = c.run_with(
        &c.work,
        &["setup", "--yes", "--no-client", "--no-service"],
        &[("ROWBIT_DATABASE_URL", &url)],
    );
    assert!(ok && !out.contains("not ready"), "{out}");

    // An env file others can read is refused.
    std::fs::set_permissions(dir.join("env"), std::fs::Permissions::from_mode(0o644)).unwrap();
    let (ok, out) = c.run(&["propose", "--agent", "cursor", "UPDATE x SET y = 1"]);
    assert!(!ok && out.contains("chmod 600"), "{out}");

    let _ = std::fs::remove_dir_all(c.home.parent().unwrap());
}

#[test]
fn refuses_what_it_cant_use() {
    let c = Case::new("refuses");
    let (ok, out) = c.run(&[
        "setup",
        "--yes",
        "--database-url",
        "postgres://a:b@h/app",
        "--no-client",
    ]);
    assert!(!ok && out.contains("PostgreSQL is planned"), "{out}");
    let (ok, out) = c.run(&[
        "setup",
        "--yes",
        "--database-url",
        "mysql://a:b@h:3306",
        "--no-client",
    ]);
    assert!(!ok && out.contains("must name a database"), "{out}");

    // A directory of yours under that name is left alone.
    let mine = c.home.join(".rowbit/default");
    std::fs::create_dir_all(&mine).unwrap();
    std::fs::write(mine.join("notes.txt"), "mine").unwrap();
    let (ok, out) = c.run(&[
        "setup",
        "--yes",
        "--database-url",
        "mysql://a:b@h/app",
        "--no-client",
    ]);
    assert!(!ok && out.contains("holds none of Rowbit's files"), "{out}");
    let (ok, out) = c.run(&["--dir", "/tmp", "setup", "--yes"]);
    assert!(!ok && out.contains("doesn't take --dir before it"), "{out}");
    let _ = std::fs::remove_dir_all(c.home.parent().unwrap());
}
