//! Proposes changes end to end against the test databases from
//! `compose.yaml`: classify, preview, policy, store. Every scenario runs on
//! both MariaDB 11.4 and MySQL 8.4.

use mysql_async::prelude::Queryable;
use rowbit_core::change::State;
use rowbit_engine::{ChangeStatus, Engine};
use rowbit_mysql::MySqlAdapter;
use rowbit_policy::PolicyFile;
use rowbit_store::Store;

const POLICY: &str = r#"
version: 0
agents:
  refund-bot:
    default: block
    rules:
      - name: small-refund-batches
        when:
          statement: update
          table: rowbit_test.en_refunds_*
          columns: status
          rows: { max: 2 }
          sum: { column: amount, max: 500 }
        then: allow
      - name: notes-have-no-sum
        when:
          table: rowbit_test.en_refunds_*
          columns: note
          sum: { column: note, max: 1 }
        then: allow
      - name: refunds-need-a-person
        when:
          table: rowbit_test.en_refunds_*
        then: hold
        reason: Large or unusual refund batches are reviewed by billing.
"#;

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
    ($scenario:ident) => {
        mod $scenario {
            #[tokio::test]
            async fn mariadb() {
                super::$scenario("ROWBIT_TEST_MARIADB_URL", "en_refunds_m").await
            }
            #[tokio::test]
            async fn mysql() {
                super::$scenario("ROWBIT_TEST_MYSQL_URL", "en_refunds_y").await
            }
        }
    };
}

/// An engine, and a table of refunds named `table` (unique per scenario).
async fn setup(var: &str, table: &str) -> Option<(Engine, String)> {
    let url = url(var)?;
    let mut conn = mysql_async::Conn::from_url(&url).await.unwrap();
    conn.query_drop(format!(
        "DROP TABLE IF EXISTS {table}; \
         CREATE TABLE {table} (id INT PRIMARY KEY, status VARCHAR(20) NOT NULL, \
           amount DECIMAL(10,2) NOT NULL, note TEXT NULL) ENGINE=InnoDB; \
         INSERT INTO {table} VALUES (1, 'approved', 120.00, NULL), (2, 'approved', 80.00, NULL), \
           (3, 'approved', 999.00, NULL), (4, 'refunded', 5.00, NULL)"
    ))
    .await
    .unwrap();
    conn.disconnect().await.unwrap();
    let engine = Engine::new(
        MySqlAdapter::new(&url).unwrap(),
        PolicyFile::from_yaml(POLICY).unwrap(),
        Store::open_in_memory().unwrap(),
    );
    Some((engine, table.to_owned()))
}

async fn propose(engine: &Engine, agent: &str, sql: &str) -> ChangeStatus {
    engine
        .propose(agent, "rowbit_test", sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

on_both!(small_batch_is_approved_large_one_is_held);
async fn small_batch_is_approved_large_one_is_held(var: &str, table: &str) {
    let Some((engine, t)) = setup(var, &format!("{table}_a")).await else {
        return;
    };
    let s = propose(
        &engine,
        "refund-bot",
        &format!("UPDATE {t} SET status = 'refunded' WHERE id IN (1, 2)"),
    )
    .await;
    assert_eq!(s.state, State::Approved, "{s:?}");
    assert_eq!(s.rows, Some(2));
    assert_eq!(
        s.table.as_deref(),
        Some(format!("rowbit_test.{t}").as_str())
    );
    assert_eq!(s.reason, None);

    let s = propose(
        &engine,
        "refund-bot",
        &format!("UPDATE {t} SET status = 'refunded' WHERE status = 'approved'"),
    )
    .await;
    assert_eq!(s.state, State::Held, "{s:?}");
    assert_eq!(s.rows, Some(3));
    assert_eq!(
        s.reason.as_deref(),
        Some("Large or unusual refund batches are reviewed by billing.")
    );
}

on_both!(sum_limit_holds_two_rows_over_budget);
async fn sum_limit_holds_two_rows_over_budget(var: &str, table: &str) {
    let Some((engine, t)) = setup(var, &format!("{table}_s")).await else {
        return;
    };
    let s = propose(
        &engine,
        "refund-bot",
        &format!("UPDATE {t} SET status = 'refunded' WHERE id IN (1, 3)"),
    )
    .await;
    assert_eq!(s.state, State::Held, "{s:?}");
}

on_both!(failures_before_a_decision_block);
async fn failures_before_a_decision_block(var: &str, table: &str) {
    let Some((engine, t)) = setup(var, &format!("{table}_f")).await else {
        return;
    };
    let cases = [
        ("stranger", format!("DELETE FROM {t}"), "not listed"),
        (
            "refund-bot",
            format!("UPDATE {t} SET a = 'x\\\\'"),
            "backslash",
        ),
        (
            "refund-bot",
            "DELETE FROM en_missing".to_owned(),
            "not found",
        ),
        (
            "refund-bot",
            format!("DELETE FROM {t} WHERE RAND() < 1"),
            "RAND",
        ),
        // A rule needs the sum of a text column, which has none.
        (
            "refund-bot",
            format!("UPDATE {t} SET note = 'x' WHERE id = 1"),
            "sum of note",
        ),
    ];
    for (agent, sql, needle) in cases {
        let s = propose(&engine, agent, &sql).await;
        assert_eq!(s.state, State::Rejected, "{sql}: {s:?}");
        let reason = s.reason.unwrap_or_default();
        assert!(reason.contains(needle), "{sql}: {reason:?}");
    }
}

on_both!(ddl_is_held_on_its_statement);
async fn ddl_is_held_on_its_statement(var: &str, table: &str) {
    let Some((engine, t)) = setup(var, &format!("{table}_d")).await else {
        return;
    };
    let s = propose(
        &engine,
        "refund-bot",
        &format!("alter table {t} add column extra int"),
    )
    .await;
    assert_eq!(s.state, State::Held, "{s:?}");
    assert_eq!(s.rows, None);
}

on_both!(agents_see_only_their_own_changes);
async fn agents_see_only_their_own_changes(var: &str, table: &str) {
    let Some((engine, t)) = setup(var, &format!("{table}_o")).await else {
        return;
    };
    let s = propose(
        &engine,
        "refund-bot",
        &format!("UPDATE {t} SET status = 'refunded' WHERE id = 1"),
    )
    .await;
    assert_eq!(engine.status("refund-bot", s.id).unwrap(), s);
    assert!(engine.status("support-bot", s.id).is_err());
    assert!(engine.status("refund-bot", 999).is_err());
}
