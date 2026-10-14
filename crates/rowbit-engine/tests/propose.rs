//! Proposes changes end to end against the test databases from
//! `compose.yaml`: classify, preview, policy, store. Every scenario runs on
//! both MariaDB 11.4 and MySQL 8.4.

use mysql_async::prelude::Queryable;
use rowbit_core::change::State;
use rowbit_engine::{ChangeStatus, Engine};
use rowbit_mysql::MySqlAdapter;
use rowbit_policy::PolicyFile;
use rowbit_store::{Review, Store};

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
    // Allowed by policy, so applied straight away.
    assert_eq!(s.state, State::Applied, "{s:?}");
    assert_eq!(s.rows, Some(2));
    assert_eq!(
        s.table.as_deref(),
        Some(format!("rowbit_test.{t}").as_str())
    );
    assert_eq!(s.reason, None);

    let s = propose(
        &engine,
        "refund-bot",
        &format!("UPDATE {t} SET status = 'refunded' WHERE status <> 'paid'"),
    )
    .await;
    assert_eq!(s.state, State::Held, "{s:?}");
    assert_eq!(s.rows, Some(4));
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

// Found in review: the server's error message reached the agent, quoting a
// value from a table it should not read.
on_both!(database_errors_do_not_leak_values_to_the_agent);
async fn database_errors_do_not_leak_values_to_the_agent(var: &str, table: &str) {
    let Some((engine, t)) = setup(var, &format!("{table}_l")).await else {
        return;
    };
    let url = std::env::var(var).unwrap();
    let mut conn = mysql_async::Conn::from_url(&url).await.unwrap();
    conn.query_drop(
        "DROP TABLE IF EXISTS en_secrets; \
         CREATE TABLE en_secrets (id INT PRIMARY KEY, pw TEXT) ENGINE=InnoDB; \
         INSERT INTO en_secrets VALUES (1, 'hunter2')",
    )
    .await
    .unwrap();
    conn.disconnect().await.unwrap();

    // A text value into a DECIMAL column: the server's error quotes it.
    let sql = format!("UPDATE {t} SET amount = (SELECT MAX(pw) FROM en_secrets) WHERE id = 1");
    let s = propose(&engine, "refund-bot", &sql).await;
    assert_eq!(s.state, State::Rejected, "{s:?}");
    let reason = s.reason.unwrap_or_default();
    assert!(
        reason.starts_with("preview failed: database error "),
        "{reason:?}"
    );
    assert!(
        !reason.contains("hunter2") && !reason.contains('\''),
        "{reason:?}"
    );
    // People still see the whole error in the log.
    let history = engine.history(s.id).unwrap();
    assert!(
        history.iter().any(|e| e.event == "ErrorDetail"
            && e.detail.as_deref().unwrap_or_default().contains("hunter2")),
        "{history:?}"
    );
}

on_both!(zero_row_changes_are_rejected);
async fn zero_row_changes_are_rejected(var: &str, table: &str) {
    let Some((engine, t)) = setup(var, &format!("{table}_z")).await else {
        return;
    };
    let s = propose(
        &engine,
        "refund-bot",
        &format!("UPDATE {t} SET status = 'refunded' WHERE id = 999"),
    )
    .await;
    assert_eq!(s.state, State::Rejected);
    assert_eq!(s.reason.as_deref(), Some("the statement matches no rows"));
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

// The demo's story: a large batch is held, a person approves exactly what
// they saw, it applies, and one undo puts every row back.
on_both!(held_change_is_approved_applied_and_undone);
async fn held_change_is_approved_applied_and_undone(var: &str, table: &str) {
    let Some((engine, t)) = setup(var, &format!("{table}_h")).await else {
        return;
    };
    let s = propose(
        &engine,
        "refund-bot",
        &format!("UPDATE {t} SET status = 'refunded' WHERE status = 'approved'"),
    )
    .await;
    assert_eq!(s.state, State::Held);
    assert_eq!(engine.held().unwrap(), [s.id]);

    let change = engine.change(s.id).unwrap();
    let Some(Review::Rows(preview)) = &change.review else {
        panic!("{change:?}")
    };
    assert_eq!(preview.rows.len(), 3);
    let hash = change.review_hash.clone().unwrap();

    assert!(
        engine
            .approve("philip", s.id, &"0".repeat(64))
            .await
            .is_err()
    );
    let change = engine.approve("philip", s.id, &hash).await.unwrap();
    assert_eq!(change.state, State::Applied);

    let change = engine.undo("philip", s.id).await.unwrap();
    assert_eq!(change.state, State::Undone);
    assert!(engine.undo("philip", s.id).await.is_err());
}

on_both!(rejected_and_drifted_changes_write_nothing);
async fn rejected_and_drifted_changes_write_nothing(var: &str, table: &str) {
    let Some((engine, t)) = setup(var, &format!("{table}_r")).await else {
        return;
    };
    let sql = format!("UPDATE {t} SET status = 'refunded' WHERE status = 'approved'");
    let s = propose(&engine, "refund-bot", &sql).await;
    let change = engine.reject("philip", s.id, "not today").unwrap();
    assert_eq!(change.state, State::Rejected);
    assert_eq!(
        engine.status("refund-bot", s.id).unwrap().reason.as_deref(),
        Some("not today")
    );

    // Held, then a row changes before the approval: apply fails, nothing written.
    let s = propose(&engine, "refund-bot", &sql).await;
    let hash = engine.change(s.id).unwrap().review_hash.unwrap();
    let url = std::env::var(var).unwrap();
    let mut conn = mysql_async::Conn::from_url(&url).await.unwrap();
    conn.query_drop(format!("UPDATE {t} SET amount = 1 WHERE id = 1"))
        .await
        .unwrap();
    let change = engine.approve("philip", s.id, &hash).await.unwrap();
    assert_eq!(change.state, State::Failed);
    let status = engine.status("refund-bot", s.id).unwrap();
    assert!(
        status
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("drift"),
        "{status:?}"
    );
    let refunded: Vec<u32> = conn
        .query(format!("SELECT id FROM {t} WHERE status = 'refunded'"))
        .await
        .unwrap();
    assert_eq!(refunded, [4]);
    conn.disconnect().await.unwrap();
}
