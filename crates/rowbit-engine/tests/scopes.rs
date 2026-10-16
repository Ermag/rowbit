//! Scoped agents end to end on the test databases from `compose.yaml`: the
//! parser check, the database's own check through the agent's user, the
//! startup check, and what `describe_schema` shows. Every scenario runs on
//! both MariaDB 11.4 and MySQL 8.4.

use mysql_async::prelude::Queryable;
use rowbit_core::change::State;
use rowbit_engine::{ChangeStatus, Engine, EngineError};
use rowbit_mysql::MySqlAdapter;
use rowbit_mysql::grants::{self, Account, Diff};
use rowbit_policy::{Operation, PolicyFile};
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

macro_rules! on_both {
    ($scenario:ident, $prefix:literal) => {
        mod $scenario {
            #[tokio::test]
            async fn mariadb() {
                super::$scenario("ROWBIT_TEST_MARIADB_URL", concat!($prefix, "_m")).await
            }
            #[tokio::test]
            async fn mysql() {
                super::$scenario("ROWBIT_TEST_MYSQL_URL", concat!($prefix, "_y")).await
            }
        }
    };
}

/// A per-run password for the agents' users, not written in the code.
fn password() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("t{nanos:x}")
}

struct Setup {
    engine: Engine,
    admin: mysql_async::Conn,
    account: Account,
    /// Table names for this scenario.
    orders: String,
    cards: String,
    view: String,
    agent: String,
}

impl Setup {
    async fn run(&mut self, sql: &str) {
        self.admin
            .query_drop(sql)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    async fn propose(&self, sql: &str) -> ChangeStatus {
        self.engine
            .propose(&self.agent, "rowbit_test", sql)
            .await
            .unwrap()
    }

    async fn query(&self, sql: &str) -> Result<usize, String> {
        match self.engine.query(&self.agent, "rowbit_test", sql).await {
            Ok(r) => Ok(r.rows.len()),
            Err(EngineError::Query(reason)) => Err(reason),
            Err(e) => panic!("{e}"),
        }
    }

    fn actor(&self, id: i64) -> String {
        self.engine
            .history(id)
            .unwrap()
            .last()
            .unwrap()
            .actor
            .clone()
    }

    async fn cleanup(mut self) {
        grants::drop_user(&mut self.admin, &self.account)
            .await
            .unwrap();
        let sql = format!(
            "DROP VIEW IF EXISTS {}; DROP TABLE IF EXISTS {}, {}, {}_new",
            self.view,
            self.orders,
            self.cards,
            self.agent.replace('-', "_")
        );
        self.run(&sql).await;
        let _ = self.admin.disconnect().await;
    }
}

/// Tables `<prefix>_orders` and `<prefix>_cards`, a view over the cards,
/// and agent `<prefix>` (with `_` as `-`) scoped to read and update the
/// orders without their `email`, its user created as `rowbit grants apply`
/// would.
async fn setup(var: &str, prefix: &str) -> Option<Setup> {
    let url = url(var)?;
    let admin_url = url.replacen("//rowbit:rowbit@", "//root:rowbit@", 1);
    let mut admin = mysql_async::Conn::from_url(&admin_url).await.unwrap();
    let orders = format!("{prefix}_orders");
    let cards = format!("{prefix}_cards");
    let view = format!("{prefix}_cardview");
    let agent = prefix.replace('_', "-");
    admin
        .query_drop(format!(
            "USE rowbit_test; DROP VIEW IF EXISTS {view}; DROP TABLE IF EXISTS {orders}, {cards}, {prefix}_new; \
             CREATE TABLE {orders} (id INT PRIMARY KEY, status VARCHAR(20) NOT NULL, \
               email VARCHAR(50) NOT NULL, card_id INT NOT NULL) ENGINE=InnoDB; \
             CREATE TABLE {cards} (id INT PRIMARY KEY, number VARCHAR(20) NOT NULL) ENGINE=InnoDB; \
             CREATE VIEW {view} AS SELECT number FROM {cards}; \
             INSERT INTO {orders} VALUES (1, 'open', 'a@example.com', 1), (2, 'open', 'b@example.com', 1), \
               (3, 'open', 'c@example.com', 1); \
             INSERT INTO {cards} VALUES (1, '4111111111111111')"
        ))
        .await
        .unwrap();
    let policy = PolicyFile::from_yaml(&format!(
        r#"
version: 0
agents:
  {agent}:
    scopes:
      read: [rowbit_test.{orders}]
      update: [rowbit_test.{orders}]
      ddl: [rowbit_test.{prefix}_new]
      deny_read: {{ columns: [rowbit_test.{orders}.email] }}
    rules:
      - name: one-row-updates
        when: {{ statement: update, rows: {{ max: 1 }} }}
        then: allow
"#
    ))
    .unwrap();

    let account = Account {
        user: format!("rowbit_{agent}"),
        host: "%".into(),
    };
    grants::drop_user(&mut admin, &account).await.unwrap();
    let password = password();
    grants::create_user(&mut admin, &account, &password)
        .await
        .unwrap();
    let scopes = policy.agents[&agent].scopes.clone().unwrap();
    let catalog = grants::Catalog::read(&mut admin, &grants::databases(&scopes))
        .await
        .unwrap();
    let want = grants::compile(&scopes, &catalog).grants;
    for sql in Diff::between(&want, &Default::default()).statements(&account) {
        admin.query_drop(&sql).await.unwrap();
    }

    let mut store = Store::open_in_memory().unwrap();
    store
        .set_setting(&format!("agent.{agent}.password"), &password)
        .unwrap();
    let engine = Engine::new(MySqlAdapter::new(&url).unwrap(), policy, store);
    Some(Setup {
        engine,
        admin,
        account,
        orders,
        cards,
        view,
        agent,
    })
}

on_both!(scopes_hold_against_an_agent, "sc_hold");
async fn scopes_hold_against_an_agent(var: &str, prefix: &str) {
    let Some(s) = setup(var, prefix).await else {
        return;
    };
    let (orders, cards, view) = (s.orders.clone(), s.cards.clone(), s.view.clone());

    // Until the startup check has passed, the agent is refused.
    let refused = s
        .query(&format!("SELECT id FROM {orders}"))
        .await
        .unwrap_err();
    assert!(refused.contains("rowbit grants apply"), "{refused}");
    let blocked = s
        .propose(&format!("UPDATE {orders} SET status = 'x' WHERE id = 1"))
        .await;
    assert_eq!(blocked.state, State::Rejected);
    assert_eq!(s.actor(blocked.id), "policy:scope");

    assert!(s.engine.check_scopes().await.is_empty());

    // Reads: what's in scope, and nothing else.
    assert_eq!(
        s.query(&format!("SELECT id, status FROM {orders}")).await,
        Ok(3)
    );
    let star = s
        .query(&format!("SELECT * FROM {orders}"))
        .await
        .unwrap_err();
    assert!(star.contains("name the columns"), "{star}");
    let email = s
        .query(&format!("SELECT email FROM {orders}"))
        .await
        .unwrap_err();
    assert!(email.contains("may not read column email"), "{email}");
    for sql in [
        format!("SELECT number FROM {cards}"),
        format!("SELECT * FROM {view}"),
        format!("SELECT id FROM {orders} WHERE card_id IN (SELECT id FROM {cards})"),
    ] {
        let reason = s.query(&sql).await.unwrap_err();
        assert!(reason.contains("outside"), "{sql}: {reason}");
    }

    // Writes: in scope it applies; reading out of scope, or another
    // operation, is blocked by policy:scope before any preview.
    let ok = s
        .propose(&format!(
            "UPDATE {orders} SET status = 'shipped' WHERE id = 1"
        ))
        .await;
    assert_eq!(ok.state, State::Applied, "{ok:?}");
    for sql in [
        format!("UPDATE {orders} SET status = (SELECT number FROM {cards} LIMIT 1) WHERE id = 2"),
        format!("UPDATE {orders} SET status = (SELECT number FROM {view} LIMIT 1) WHERE id = 2"),
        // Copying a denied column into a readable one.
        format!("UPDATE {orders} SET status = email WHERE id = 2"),
        format!("UPDATE {orders} SET status = 'x' WHERE email LIKE 'b%'"),
        format!("DELETE FROM {orders} WHERE id = 2"),
        format!(
            "INSERT INTO {orders} (id, status, email, card_id) VALUES (9, 'open', 'z@example.com', 1)"
        ),
        format!("UPDATE {cards} SET number = '0' WHERE id = 1"),
        format!("ALTER TABLE {orders} ADD COLUMN x INT"),
    ] {
        let st = s.propose(&sql).await;
        assert_eq!(st.state, State::Rejected, "{sql}: {st:?}");
        assert_eq!(s.actor(st.id), "policy:scope", "{sql}");
    }

    // Writing a column the agent may not read is allowed.
    let write_email = s
        .propose(&format!(
            "UPDATE {orders} SET email = 'new@example.com' WHERE id = 3"
        ))
        .await;
    assert_eq!(write_email.state, State::Applied, "{write_email:?}");

    // describe_schema shows the orders without email, and not the cards.
    let tables = s.engine.describe(&s.agent, "rowbit_test").await.unwrap();
    assert_eq!(tables.len(), 1, "{tables:?}");
    assert_eq!(tables[0].table.name, orders);
    assert_eq!(tables[0].operations, [Operation::Read, Operation::Update]);
    let columns: Vec<&str> = tables[0]
        .table
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(columns, ["id", "status", "card_id"]);
    s.cleanup().await;
}

on_both!(the_database_has_the_last_word, "sc_last");
async fn the_database_has_the_last_word(var: &str, prefix: &str) {
    let Some(mut s) = setup(var, prefix).await else {
        return;
    };
    let orders = s.orders.clone();
    assert!(s.engine.check_scopes().await.is_empty());
    let user = s.account.sql();

    // A two-row update is held. Its grant is revoked before a person
    // approves: apply asks the database again and writes nothing.
    let held = s
        .propose(&format!(
            "UPDATE {orders} SET status = 'held' WHERE id IN (1, 2)"
        ))
        .await;
    assert_eq!(held.state, State::Held, "{held:?}");
    s.run(&format!(
        "REVOKE UPDATE ON rowbit_test.{orders} FROM {user}"
    ))
    .await;
    let hash = s.engine.change(held.id).unwrap().review_hash.unwrap();
    let after = s.engine.approve("philip", held.id, &hash).await.unwrap();
    assert_eq!(after.state, State::Failed);
    let status = s.engine.status(&s.agent, held.id).unwrap();
    assert!(status.reason.unwrap().contains("outside the agent's scope"));

    // With the grant gone, the parser check passes but the database
    // refuses the EXPLAIN.
    let st = s
        .propose(&format!("UPDATE {orders} SET status = 'x' WHERE id = 1"))
        .await;
    assert_eq!(st.state, State::Rejected);
    assert_eq!(s.actor(st.id), "policy:scope");
    assert!(st.reason.unwrap().contains("database error 1142"));
    s.run(&format!("GRANT UPDATE ON rowbit_test.{orders} TO {user}"))
        .await;

    // A privilege beyond the scopes refuses the agent at the next startup
    // check, until it is taken away again.
    s.run(&format!("GRANT DELETE ON rowbit_test.{orders} TO {user}"))
        .await;
    let notes = s.engine.check_scopes().await;
    assert!(
        notes.len() == 1 && notes[0].contains("holds more"),
        "{notes:?}"
    );
    assert!(s.query(&format!("SELECT id FROM {orders}")).await.is_err());
    s.run(&format!(
        "REVOKE DELETE ON rowbit_test.{orders} FROM {user}"
    ))
    .await;
    assert!(s.engine.check_scopes().await.is_empty());
    assert_eq!(s.query(&format!("SELECT id FROM {orders}")).await, Ok(3));

    // An approved schema change runs on the agent's user, so the database
    // enforces the ddl scope at apply too.
    let new = format!("{prefix}_new");
    let create = s
        .propose(&format!("CREATE TABLE {new} (id INT PRIMARY KEY)"))
        .await;
    assert_eq!(create.state, State::Held, "{create:?}");
    s.run(&format!("REVOKE CREATE ON rowbit_test.{new} FROM {user}"))
        .await;
    let hash = s.engine.change(create.id).unwrap().review_hash.unwrap();
    let r = s.engine.approve("philip", create.id, &hash).await;
    assert_eq!(
        r.unwrap().state,
        State::Failed,
        "a refused privilege is a clean failure"
    );
    let exists: Option<i64> = s
        .admin
        .query_first(format!(
            "SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'rowbit_test' AND TABLE_NAME = '{new}'"
        ))
        .await
        .unwrap();
    assert_eq!(exists, Some(0), "the database refused the agent's CREATE");
    s.run(&format!("GRANT CREATE ON rowbit_test.{new} TO {user}"))
        .await;
    let create = s
        .propose(&format!("CREATE TABLE {new} (id INT PRIMARY KEY)"))
        .await;
    let hash = s.engine.change(create.id).unwrap().review_hash.unwrap();
    let done = s.engine.approve("philip", create.id, &hash).await.unwrap();
    assert_eq!(done.state, State::Applied);

    // Fewer privileges than compiled: served, with a warning. The new
    // table now exists, so its ALTER, DROP and INDEX are missing too until
    // `rowbit grants apply` runs.
    s.run(&format!(
        "REVOKE UPDATE ON rowbit_test.{orders} FROM {user}"
    ))
    .await;
    let notes = s.engine.check_scopes().await;
    assert!(
        notes.len() == 1 && notes[0].contains("lacks 4"),
        "{notes:?}"
    );
    assert_eq!(s.query(&format!("SELECT id FROM {orders}")).await, Ok(3));
    s.cleanup().await;
}
