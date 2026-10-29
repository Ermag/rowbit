//! Compiles scopes into grants and applies them on the test databases from
//! `compose.yaml`, as an administrator, then checks what the agent's user
//! can and can't do. Every scenario runs on both MariaDB 11.4 and MySQL 8.4.

mod common;

use std::collections::BTreeSet;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use rowbit_mysql::grants::{self, Account, Catalog, Diff, Grant, Object};
use rowbit_policy::Scopes;

/// The agents' users' password: made up per run, not written in the code.
fn password() -> String {
    static PASSWORD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PASSWORD
        .get_or_init(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            format!("t{nanos:x}{:x}", std::process::id())
        })
        .clone()
}

/// The same server as root, whose password in `compose.yaml` is `rowbit`.
fn admin_url(url: &str) -> String {
    url.replacen("//rowbit:rowbit@", "//root:rowbit@", 1)
}

async fn connect(url: &str) -> Conn {
    Conn::new(Opts::from_url(url).unwrap())
        .await
        .expect("server answers")
}

async fn run(conn: &mut Conn, sql: &str) {
    conn.query_drop(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// A fresh database of `name` with tables, and an account for `agent`
/// that doesn't exist yet.
async fn setup(admin: &mut Conn, db: &str, agent: &str) -> Account {
    let account = Account {
        user: format!("rowbit_{agent}"),
        host: "%".into(),
    };
    grants::drop_user(admin, &account).await.unwrap();
    for sql in [
        format!("DROP DATABASE IF EXISTS `{db}`"),
        format!("CREATE DATABASE `{db}`"),
        format!(
            "CREATE TABLE `{db}`.orders (id INT PRIMARY KEY, status VARCHAR(10), email VARCHAR(50))"
        ),
        format!("CREATE TABLE `{db}`.cards (id INT PRIMARY KEY, number VARCHAR(20))"),
        format!("INSERT INTO `{db}`.orders VALUES (1, 'open', 'a@example.com')"),
        format!("INSERT INTO `{db}`.cards VALUES (1, '4111')"),
    ] {
        run(admin, &sql).await;
    }
    account
}

async fn compile(admin: &mut Conn, yaml: &str) -> BTreeSet<Grant> {
    let scopes: Scopes = serde_norway::from_str(yaml).unwrap();
    let catalog = Catalog::read(admin, &grants::databases(&scopes))
        .await
        .unwrap();
    grants::compile(&scopes, &catalog).grants
}

async fn converge(admin: &mut Conn, account: &Account, want: &BTreeSet<Grant>) {
    let have = grants::current(admin, account).await.unwrap();
    for sql in Diff::between(want, &have.grants).statements(account) {
        run(admin, &sql).await;
    }
    let have = grants::current(admin, account).await.unwrap();
    assert_eq!(
        &have.grants, want,
        "the user holds exactly what was compiled"
    );
    assert!(have.unmanaged.is_empty(), "{:?}", have.unmanaged);
}

/// The agent's user, with no default database: it may hold nothing on the
/// test database.
fn agent_url(url: &str, account: &Account) -> String {
    let url = url.replacen(
        "//rowbit:rowbit@",
        &format!("//{}:{}@", account.user, password()),
        1,
    );
    let (base, _db) = url.rsplit_once('/').unwrap();
    format!("{base}/")
}

async fn denied(conn: &mut Conn, sql: &str) -> bool {
    match conn.query_drop(sql).await {
        Err(mysql_async::Error::Server(e)) => matches!(e.code, 1142 | 1143 | 1044 | 1356),
        Err(e) => panic!("{sql}: {e}"),
        Ok(()) => false,
    }
}

async fn scopes_become_exact_grants(var: &str, db: &str, agent: &str) {
    let Some(url) = common::url(var) else { return };
    let mut admin = connect(&admin_url(&url)).await;
    let account = setup(&mut admin, db, agent).await;
    grants::create_user(&mut admin, &account, &password())
        .await
        .unwrap();
    assert!(grants::exists(&mut admin, &account).await.unwrap());

    let want = compile(
        &mut admin,
        &format!(
            "read: [{db}.*]\nupdate: [{db}.orders]\ndeny_read: {{ tables: [{db}.cards], columns: [{db}.orders.email] }}"
        ),
    )
    .await;
    converge(&mut admin, &account, &want).await;

    let mut agent_conn = connect(&agent_url(&url, &account)).await;
    // The agent sees its own grants the same way.
    let own = grants::current(&mut agent_conn, &account).await.unwrap();
    assert_eq!(own.grants, want);

    assert!(
        !denied(
            &mut agent_conn,
            &format!("SELECT id, status FROM `{db}`.orders")
        )
        .await
    );
    assert!(denied(&mut agent_conn, &format!("SELECT * FROM `{db}`.orders")).await);
    assert!(denied(&mut agent_conn, &format!("SELECT number FROM `{db}`.cards")).await);
    assert!(
        !denied(
            &mut agent_conn,
            &format!("EXPLAIN UPDATE `{db}`.orders SET status = 'x' WHERE id = 1")
        )
        .await
    );
    assert!(
        denied(
            &mut agent_conn,
            &format!("EXPLAIN UPDATE `{db}`.orders SET status = (SELECT number FROM `{db}`.cards LIMIT 1)")
        )
        .await
    );
    assert!(
        denied(
            &mut agent_conn,
            &format!("EXPLAIN DELETE FROM `{db}`.orders")
        )
        .await
    );
    let _ = agent_conn.disconnect().await;

    // Anything added by hand, including the right to pass grants on, is
    // revoked by the next run.
    run(
        &mut admin,
        &format!(
            "GRANT DELETE ON `{}`.* TO {} WITH GRANT OPTION",
            grants::escape_database(db),
            account.sql()
        ),
    )
    .await;
    let have = grants::current(&mut admin, &account).await.unwrap();
    let diff = Diff::between(&want, &have.grants);
    assert!(diff.missing.is_empty());
    assert!(diff.extra.contains(&Grant {
        object: Object::Database(grants::escape_database(db)),
        privilege: "GRANT OPTION".into(),
        column: None,
    }));
    converge(&mut admin, &account, &want).await;

    // Narrowing the scopes takes privileges away.
    let narrower = compile(
        &mut admin,
        &format!("read: [{db}.orders]\ndeny_read: {{ columns: [{db}.orders.email] }}"),
    )
    .await;
    converge(&mut admin, &account, &narrower).await;

    grants::drop_user(&mut admin, &account).await.unwrap();
    run(&mut admin, &format!("DROP DATABASE `{db}`")).await;
    let _ = admin.disconnect().await;
}

#[tokio::test]
async fn mariadb_scopes_become_exact_grants() {
    scopes_become_exact_grants(common::MARIADB, "rg_exact", "grants-exact").await;
}

#[tokio::test]
async fn mysql_scopes_become_exact_grants() {
    scopes_become_exact_grants(common::MYSQL, "rg_exact", "grants-exact").await;
}

/// `_` in a database-level grant is a wildcard unless escaped, so a scope on
/// `rg_wild` must not reach `rgXwild`.
async fn underscores_are_not_wildcards(var: &str) {
    let Some(url) = common::url(var) else { return };
    let mut admin = connect(&admin_url(&url)).await;
    let account = setup(&mut admin, "rg_wild", "grants-wild").await;
    setup(&mut admin, "rgXwild", "grants-wild-unused").await;
    grants::create_user(&mut admin, &account, &password())
        .await
        .unwrap();
    let want = compile(&mut admin, "read: [rg_wild.*]").await;
    converge(&mut admin, &account, &want).await;

    let mut agent_conn = connect(&agent_url(&url, &account)).await;
    assert!(!denied(&mut agent_conn, "SELECT * FROM rg_wild.cards").await);
    assert!(denied(&mut agent_conn, "SELECT * FROM rgXwild.cards").await);
    let _ = agent_conn.disconnect().await;

    // A wildcard grant made by hand is seen as extra and revoked.
    run(
        &mut admin,
        &format!("GRANT SELECT ON `rg_wil_`.* TO {}", account.sql()),
    )
    .await;
    converge(&mut admin, &account, &want).await;

    grants::drop_user(&mut admin, &account).await.unwrap();
    run(&mut admin, "DROP DATABASE rg_wild").await;
    run(&mut admin, "DROP DATABASE rgXwild").await;
    let _ = admin.disconnect().await;
}

#[tokio::test]
async fn mariadb_underscores_are_not_wildcards() {
    underscores_are_not_wildcards(common::MARIADB).await;
}

#[tokio::test]
async fn mysql_underscores_are_not_wildcards() {
    underscores_are_not_wildcards(common::MYSQL).await;
}

/// A role can carry any privilege, and `information_schema` doesn't show
/// it, so it is reported for a person to remove.
async fn roles_are_unmanaged(var: &str) {
    let Some(url) = common::url(var) else { return };
    let mut admin = connect(&admin_url(&url)).await;
    let account = setup(&mut admin, "rg_role", "grants-role").await;
    grants::create_user(&mut admin, &account, &password())
        .await
        .unwrap();
    run(&mut admin, "DROP ROLE IF EXISTS rowbit_test_role").await;
    run(&mut admin, "CREATE ROLE rowbit_test_role").await;
    run(&mut admin, "GRANT SELECT ON rg_role.* TO rowbit_test_role").await;
    run(
        &mut admin,
        &format!("GRANT rowbit_test_role TO {}", account.sql()),
    )
    .await;
    let have = grants::current(&mut admin, &account).await.unwrap();
    assert_eq!(have.unmanaged.len(), 1, "{:?}", have.unmanaged);
    assert!(have.unmanaged[0].contains("rowbit_test_role"));

    grants::drop_user(&mut admin, &account).await.unwrap();
    run(&mut admin, "DROP ROLE rowbit_test_role").await;
    run(&mut admin, "DROP DATABASE rg_role").await;
    let _ = admin.disconnect().await;
}

#[tokio::test]
async fn mariadb_roles_are_unmanaged() {
    roles_are_unmanaged(common::MARIADB).await;
}

#[tokio::test]
async fn mysql_roles_are_unmanaged() {
    roles_are_unmanaged(common::MYSQL).await;
}

/// Found in review: views run with their definer's rights by default, so
/// one covered by `db.*` read a denied table for the agent. Only views
/// that run with the reader's rights are granted by a pattern.
async fn views_dont_lend_their_definers_rights(var: &str) {
    let Some(url) = common::url(var) else { return };
    let mut admin = connect(&admin_url(&url)).await;
    let account = setup(&mut admin, "rg_views", "grants-views").await;
    grants::create_user(&mut admin, &account, &password())
        .await
        .unwrap();
    for sql in [
        "CREATE VIEW rg_views.card_report AS SELECT id, number FROM rg_views.cards",
        "CREATE SQL SECURITY INVOKER VIEW rg_views.card_invoker AS SELECT id, number FROM rg_views.cards",
        "CREATE VIEW rg_views.contacts AS SELECT id, email FROM rg_views.orders",
    ] {
        run(&mut admin, sql).await;
    }
    let want = compile(
        &mut admin,
        "read: [rg_views.*]\ndeny_read: { tables: [rg_views.cards], columns: [rg_views.orders.email] }",
    )
    .await;
    assert!(
        !want.iter().any(
            |g| matches!(&g.object, Object::Table(_, t) if t == "card_report" || t == "contacts")
        ),
        "{want:?}"
    );
    converge(&mut admin, &account, &want).await;

    let mut agent_conn = connect(&agent_url(&url, &account)).await;
    for sql in [
        "SELECT number FROM rg_views.card_report",
        "SELECT email FROM rg_views.contacts",
        // Granted, but it reads with the agent's own rights.
        "SELECT number FROM rg_views.card_invoker",
        "SELECT number FROM rg_views.cards",
    ] {
        assert!(denied(&mut agent_conn, sql).await, "{sql}");
    }
    assert!(!denied(&mut agent_conn, "SELECT id, status FROM rg_views.orders").await);
    let _ = agent_conn.disconnect().await;

    grants::drop_user(&mut admin, &account).await.unwrap();
    run(&mut admin, "DROP DATABASE rg_views").await;
    let _ = admin.disconnect().await;
}

#[tokio::test]
async fn mariadb_views_dont_lend_their_definers_rights() {
    views_dont_lend_their_definers_rights(common::MARIADB).await;
}

#[tokio::test]
async fn mysql_views_dont_lend_their_definers_rights() {
    views_dont_lend_their_definers_rights(common::MYSQL).await;
}
