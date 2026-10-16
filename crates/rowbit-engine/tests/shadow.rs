//! Shadow mode end to end on the test databases from `compose.yaml`: what a
//! rule would hold or block is applied and logged, floors still hold, and
//! the log replays against another policy. Every scenario runs on both
//! MariaDB 11.4 and MySQL 8.4.

use mysql_async::prelude::Queryable;
use rowbit_core::change::State;
use rowbit_engine::{ChangeStatus, Engine, recorded_decision, replay};
use rowbit_mysql::MySqlAdapter;
use rowbit_policy::{Decision, PolicyFile};
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

fn policy(table: &str, mode: &str) -> String {
    format!(
        r#"
version: 0
agents:
  support-bot:
    mode: {mode}
    default: hold
    rules:
      - name: one-row-fixes
        when: {{ statement: update, table: rowbit_test.{table}, rows: {{ max: 1 }} }}
        then: allow
      - name: no-deletes
        when: {{ statement: delete }}
        then: block
        reason: Support doesn't delete.
"#
    )
}

async fn setup(var: &str, table: &str) -> Option<(Engine, String)> {
    let url = url(var)?;
    let mut conn = mysql_async::Conn::from_url(&url).await.unwrap();
    conn.query_drop(format!(
        "DROP TABLE IF EXISTS {table}, {table}_nokey; \
         CREATE TABLE {table} (id INT PRIMARY KEY, status VARCHAR(20) NOT NULL) ENGINE=InnoDB; \
         CREATE TABLE {table}_nokey (status VARCHAR(20) NOT NULL) ENGINE=InnoDB; \
         INSERT INTO {table} VALUES (1, 'open'), (2, 'open'), (3, 'open'); \
         INSERT INTO {table}_nokey VALUES ('open')"
    ))
    .await
    .unwrap();
    conn.disconnect().await.unwrap();
    let engine = Engine::new(
        MySqlAdapter::new(&url).unwrap(),
        PolicyFile::from_yaml(&policy(table, "shadow")).unwrap(),
        Store::open_in_memory().unwrap(),
    );
    Some((engine, url))
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

async fn propose(engine: &Engine, sql: &str) -> ChangeStatus {
    engine
        .propose("support-bot", "rowbit_test", sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn last_policy_event(engine: &Engine, id: i64) -> (String, String, Option<String>) {
    let e = engine
        .history(id)
        .unwrap()
        .into_iter()
        .find(|e| e.event.starts_with("Policy"))
        .unwrap();
    (e.event, e.actor, e.detail)
}

on_both!(shadow_applies_what_rules_would_stop, "sh_apply");
async fn shadow_applies_what_rules_would_stop(var: &str, t: &str) {
    let Some((engine, url)) = setup(var, t).await else {
        return;
    };

    // Allowed: as in enforce mode.
    let s = propose(
        &engine,
        &format!("UPDATE {t} SET status = 'a' WHERE id = 1"),
    )
    .await;
    assert_eq!(s.state, State::Applied);
    assert_eq!(last_policy_event(&engine, s.id).0, "PolicyAllowed");

    // Would be held by the default: applied, and logged as such. The agent
    // sees `applied` and no reason.
    let held = propose(
        &engine,
        &format!("UPDATE {t} SET status = 'b' WHERE id IN (2, 3)"),
    )
    .await;
    assert_eq!(held.state, State::Applied, "{held:?}");
    assert_eq!(held.reason, None);
    assert_eq!(
        last_policy_event(&engine, held.id),
        (
            "PolicyShadowed".into(),
            "policy:default".into(),
            Some("would hold: no rule matched".into())
        )
    );

    // Would be blocked by a rule: applied too.
    let blocked = propose(&engine, &format!("DELETE FROM {t} WHERE id = 3")).await;
    assert_eq!(blocked.state, State::Applied, "{blocked:?}");
    assert_eq!(
        last_policy_event(&engine, blocked.id),
        (
            "PolicyShadowed".into(),
            "policy:no-deletes".into(),
            Some("would block: Support doesn't delete.".into())
        )
    );
    assert_eq!(statuses(&url, t).await, ["a", "b"]);

    // Floors still hold: no primary key is refused, a schema change waits.
    let nokey = propose(&engine, &format!("UPDATE {t}_nokey SET status = 'x'")).await;
    assert_eq!(nokey.state, State::Rejected, "{nokey:?}");
    let ddl = propose(&engine, &format!("ALTER TABLE {t} ADD COLUMN x INT")).await;
    assert_eq!(ddl.state, State::Held, "{ddl:?}");

    // A shadowed change undoes like any other, and the log checks out.
    engine.undo("philip", blocked.id).await.unwrap();
    engine.undo("philip", held.id).await.unwrap();
    assert_eq!(statuses(&url, t).await, ["a", "open", "open"]);
    assert_eq!(engine.verify().unwrap(), Ok(()));

    // Replaying the log against the same rules in enforce mode: what was
    // shadowed is what enforcing would have done.
    let enforce = PolicyFile::from_yaml(&policy(t, "enforce")).unwrap();
    for id in [s.id, held.id, blocked.id, ddl.id] {
        let change = engine.change(id).unwrap();
        let was = recorded_decision(&engine.history(id).unwrap()).unwrap();
        assert_eq!(replay(&enforce, &change).unwrap().decision, was, "#{id}");
    }
    assert_eq!(
        recorded_decision(&engine.history(blocked.id).unwrap()),
        Some(Decision::Block)
    );
    // A rule allowing two rows would have let the held one through.
    let looser = PolicyFile::from_yaml(&policy(t, "enforce").replace("max: 1", "max: 2")).unwrap();
    let change = engine.change(held.id).unwrap();
    assert_eq!(replay(&looser, &change).unwrap().decision, Decision::Allow);
    // A change refused before preview has nothing to replay.
    assert!(recorded_decision(&engine.history(nokey.id).unwrap()).is_none());
}
