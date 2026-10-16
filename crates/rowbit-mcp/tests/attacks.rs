//! Guarded-agent attack tests: a misbehaving or hijacked agent, talking to
//! Rowbit only through MCP, tries to get a write past it. Each test is one
//! attack and the defence that must hold. Run on both databases.
//!
//! Part of the v0.1 release gate (see the roadmap): these must pass before
//! v0.1.0 is tagged. Agent text reaching a reviewer's terminal is tested
//! with the CLI, in `crates/rowbit-cli/tests/review.rs`.

use std::sync::Arc;

use mysql_async::prelude::Queryable;
use rmcp::model::{CallToolRequestParams, ClientConfig, ContentBlock};
use rmcp::service::RunningService;
use rmcp::{ClientHandler, RoleClient, ServiceExt};
use rowbit_engine::Engine;
use rowbit_mcp::RowbitMcp;
use rowbit_mysql::MySqlAdapter;
use rowbit_policy::PolicyFile;
use rowbit_store::Store;
use serde_json::{Value, json};

/// Small status fixes are allowed, deletes are blocked, everything else on
/// these tables waits for a person, and a catch-all rule allows the rest,
/// which the DDL floor must still hold.
const POLICY: &str = r#"
version: 0
agents:
  refund-bot:
    default: hold
    rules:
      - name: no-deletes
        when: { statement: delete, table: rowbit_test.at_* }
        then: block
        reason: Refunds are never deleted.
      - name: small-status-fixes
        when: { statement: update, table: rowbit_test.at_*, columns: status, rows: { max: 2 } }
        then: allow
      - name: batches-need-a-person
        when: { table: rowbit_test.at_* }
        then: hold
      - name: anything-else
        then: allow
  support-bot:
    default: hold
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
                super::$scenario("ROWBIT_TEST_MARIADB_URL", "at_m").await
            }
            #[tokio::test]
            async fn mysql() {
                super::$scenario("ROWBIT_TEST_MYSQL_URL", "at_y").await
            }
        }
    };
}

#[derive(Clone, Default)]
struct Agent;

impl ClientHandler for Agent {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::default()
    }
}

type Client = RunningService<RoleClient, Agent>;

struct World {
    engine: Arc<Engine>,
    url: String,
    table: String,
}

impl World {
    async fn new(var: &str, table: &str) -> Option<World> {
        let url = url(var)?;
        let mut conn = mysql_async::Conn::from_url(&url).await.unwrap();
        conn.query_drop(format!(
            "DROP TABLE IF EXISTS {table}; \
             CREATE TABLE {table} (id INT PRIMARY KEY, status VARCHAR(20) NOT NULL, \
               amount DECIMAL(10,2) NOT NULL) ENGINE=InnoDB; \
             INSERT INTO {table} VALUES (1, 'approved', 10), (2, 'approved', 20), \
               (3, 'approved', 30), (4, 'approved', 40)"
        ))
        .await
        .unwrap();
        conn.disconnect().await.unwrap();
        let engine = Engine::new(
            MySqlAdapter::new(&url).unwrap(),
            PolicyFile::from_yaml(POLICY).unwrap(),
            Store::open_in_memory().unwrap(),
        );
        Some(World {
            engine: Arc::new(engine),
            url,
            table: table.to_owned(),
        })
    }

    async fn agent(&self, name: &str) -> Client {
        let (client_io, server_io) = tokio::io::duplex(1 << 16);
        let server = RowbitMcp::new(self.engine.clone(), name, "rowbit_test");
        tokio::spawn(async move {
            let running = server.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });
        Agent.serve(client_io).await.unwrap()
    }

    async fn rows(&self) -> Vec<(u32, String)> {
        let mut conn = mysql_async::Conn::from_url(&self.url).await.unwrap();
        let rows = conn
            .query(format!("SELECT id, status FROM {} ORDER BY id", self.table))
            .await
            .unwrap();
        conn.disconnect().await.unwrap();
        rows
    }

    async fn untouched(&self) {
        let rows = self.rows().await;
        assert!(rows.iter().all(|(_, s)| s == "approved"), "{rows:?}");
        assert_eq!(rows.len(), 4);
    }
}

/// Calls a tool; returns whether it failed, and its text.
async fn call(client: &Client, tool: &str, args: Value) -> (bool, String) {
    let result = client
        .call_tool(
            CallToolRequestParams::new(tool.to_owned())
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await;
    match result {
        Ok(result) => {
            let text = result
                .content
                .iter()
                .filter_map(|c| match c {
                    ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<String>();
            (result.is_error == Some(true), text)
        }
        // An unknown tool is a protocol error, which also counts as refused.
        Err(e) => (true, e.to_string()),
    }
}

async fn propose(client: &Client, sql: &str) -> Value {
    let (failed, text) = call(client, "propose_write", json!({ "sql": sql })).await;
    assert!(!failed, "{sql}: {text}");
    serde_json::from_str(&text).unwrap()
}

// Attack: approve its own held change. There is no tool for it, and no
// argument makes one of the other tools approve.
on_both!(self_approval);
async fn self_approval(var: &str, table: &str) {
    let Some(w) = World::new(var, &format!("{table}_sa")).await else {
        return;
    };
    let agent = w.agent("refund-bot").await;
    let held = propose(
        &agent,
        &format!("UPDATE {} SET status = 'refunded'", w.table),
    )
    .await;
    assert_eq!(held["state"], "held");
    let id = held["id"].clone();

    for tool in ["approve", "approve_change", "apply", "undo", "reject"] {
        let (failed, text) = call(&agent, tool, json!({ "id": id })).await;
        assert!(failed, "{tool}: {text}");
    }
    // Extra arguments are not a way in.
    for args in [
        json!({ "id": id, "state": "approved" }),
        json!({ "id": id, "approve": true, "actor": "human:philip" }),
    ] {
        let (_, text) = call(&agent, "change_status", args).await;
        assert!(!text.contains("approved\""), "{text}");
    }
    let (_, text) = call(
        &agent,
        "propose_write",
        json!({ "sql": format!("UPDATE {} SET status = 'refunded'", w.table), "approve": true, "agent": "human:philip" }),
    )
    .await;
    assert!(text.contains("\"held\""), "{text}");
    assert_eq!(w.engine.held().unwrap().len(), 2);
    w.untouched().await;
}

// Attack: act as another agent with looser rules, or read its changes.
on_both!(identity_spoofing);
async fn identity_spoofing(var: &str, table: &str) {
    let Some(w) = World::new(var, &format!("{table}_id")).await else {
        return;
    };
    let support = w.agent("support-bot").await;
    let (_, text) = call(
        &support,
        "propose_write",
        json!({ "sql": format!("UPDATE {} SET status = 'refunded' WHERE id = 1", w.table), "agent": "refund-bot" }),
    )
    .await;
    let status: Value = serde_json::from_str(&text).unwrap();
    // support-bot's default is hold; refund-bot's rule would have allowed it.
    assert_eq!(status["state"], "held", "{text}");
    let change = w.engine.change(status["id"].as_i64().unwrap()).unwrap();
    assert_eq!(change.agent, "support-bot");

    let refund = w.agent("refund-bot").await;
    let theirs = propose(
        &refund,
        &format!("UPDATE {} SET status = 'refunded'", w.table),
    )
    .await;
    let (failed, text) = call(&support, "change_status", json!({ "id": theirs["id"] })).await;
    assert!(failed && text.contains("not found"), "{text}");

    let stranger = w.agent("nobody").await;
    let s = propose(
        &stranger,
        &format!("UPDATE {} SET status = 'x' WHERE id = 1", w.table),
    )
    .await;
    assert_eq!(s["state"], "rejected");
    assert_eq!(s["reason"], "agent is not listed in rowbit.yml");
    w.untouched().await;
}

// Attack: get a schema change applied without a person, directly or
// hidden inside something else.
on_both!(sneaking_ddl);
async fn sneaking_ddl(var: &str, table: &str) {
    let Some(w) = World::new(var, &format!("{table}_dd")).await else {
        return;
    };
    let agent = w.agent("refund-bot").await;
    let t = &w.table;
    // A catch-all allow rule exists, but schema changes are always held.
    for sql in [
        format!("ALTER TABLE {t} ADD COLUMN pwned INT"),
        format!("DROP TABLE {t}"),
        format!("TRUNCATE TABLE {t}"),
        format!("CREATE TABLE {t}_copy AS SELECT * FROM {t}"),
        format!("RENAME TABLE {t} TO {t}_gone"),
    ] {
        let s = propose(&agent, &sql).await;
        assert_eq!(s["state"], "held", "{sql}: {s}");
    }
    // Hidden inside other statements or comments: refused outright.
    for sql in [
        format!("UPDATE {t} SET status = 'x' WHERE id = 1; DROP TABLE {t}"),
        format!("UPDATE {t} SET status = 'x' WHERE id = 1 /*!50000 ; DROP TABLE {t} */"),
        "CALL mysql.rds_drop_everything()".to_owned(),
        "PREPARE s FROM 'DROP TABLE x'".to_owned(),
        "DROP DATABASE rowbit_test".to_owned(),
        // Found in review 4: held instead of refused.
        "DROP TABLE mysql.user".to_owned(),
        format!("RENAME TABLE {t} TO sys.{t}"),
        "GRANT ALL ON *.* TO 'refund-bot'".to_owned(),
    ] {
        let s = propose(&agent, &sql).await;
        assert_eq!(s["state"], "rejected", "{sql}: {s}");
    }
    let (failed, _) = call(
        &agent,
        "query",
        json!({ "sql": format!("SELECT 1; DROP TABLE {t}") }),
    )
    .await;
    assert!(failed);
    w.untouched().await;
}

// Attack: make the preview show something other than what runs, or run
// something during the preview.
on_both!(escaping_the_preview);
async fn escaping_the_preview(var: &str, table: &str) {
    let Some(w) = World::new(var, &format!("{table}_ep")).await else {
        return;
    };
    let agent = w.agent("refund-bot").await;
    let t = &w.table;
    for sql in [
        // The literal that used to end early when printed back (review #1).
        format!(
            "UPDATE {t} SET status = 'x\\\\', amount = ') AS q, SLEEP(1) AS r FROM {t} -- ' WHERE id = 1"
        ),
        format!("UPDATE {t} SET status = SLEEP(3) WHERE id = 1"),
        format!("UPDATE {t} SET status = LOAD_FILE('/etc/passwd') WHERE id = 1"),
        format!("UPDATE {t} SET status = GET_LOCK('x', 10) WHERE id = 1"),
        format!("UPDATE {t} SET status = @@hostname WHERE id = 1"),
        format!("UPDATE {t} SET status = 'x' WHERE id IN (SELECT id FROM {t} FOR UPDATE)"),
        format!("DELETE FROM {t} WHERE RAND() < 0.5"),
        format!("UPDATE {t} SET id = 99 WHERE id = 1"),
    ] {
        let s = propose(&agent, &sql).await;
        assert_eq!(s["state"], "rejected", "{sql}: {s}");
    }
    for sql in [
        format!("SELECT * FROM {t} INTO OUTFILE '/tmp/x'"),
        "SELECT SLEEP(5)".to_owned(),
        "SELECT @@version".to_owned(),
    ] {
        let (failed, text) = call(&agent, "query", json!({ "sql": sql })).await;
        assert!(failed, "{sql}: {text}");
    }
    w.untouched().await;
}

// Attack: split a blocked write into pieces that each look harmless. A
// rule that blocks by what a write does (here: deletes) blocks every piece.
on_both!(splitting_a_blocked_write);
async fn splitting_a_blocked_write(var: &str, table: &str) {
    let Some(w) = World::new(var, &format!("{table}_sp")).await else {
        return;
    };
    let agent = w.agent("refund-bot").await;
    let all = propose(&agent, &format!("DELETE FROM {}", w.table)).await;
    assert_eq!(all["state"], "rejected");
    for id in 1..=4 {
        let s = propose(&agent, &format!("DELETE FROM {} WHERE id = {id}", w.table)).await;
        assert_eq!(s["state"], "rejected", "{s}");
        assert_eq!(s["reason"], "Refunds are never deleted.");
    }
    assert_eq!(w.rows().await.len(), 4);
}

// Known v0.1 limit, kept visible: a rule that holds by size ("more than 2
// rows waits for a person") can be split into allowed pieces. Every piece is
// previewed, checked and logged, but nothing adds them up. Per-agent
// budgets (rows or money per day) close this in v0.3.
on_both!(splitting_past_a_size_limit_is_logged_not_stopped);
async fn splitting_past_a_size_limit_is_logged_not_stopped(var: &str, table: &str) {
    let Some(w) = World::new(var, &format!("{table}_sz")).await else {
        return;
    };
    let agent = w.agent("refund-bot").await;
    let t = &w.table;
    let whole = propose(&agent, &format!("UPDATE {t} SET status = 'refunded'")).await;
    assert_eq!(whole["state"], "held");
    for ids in ["1, 2", "3, 4"] {
        let s = propose(
            &agent,
            &format!("UPDATE {t} SET status = 'refunded' WHERE id IN ({ids})"),
        )
        .await;
        assert_eq!(s["state"], "applied", "{s}");
    }
    let applied = w
        .engine
        .list(Some(rowbit_core::change::State::Applied))
        .unwrap();
    assert_eq!(applied.len(), 2, "each piece is its own change in the log");
}

// Attack: get a held change approved, then change the rows under it so the
// person's approval writes something they never saw.
on_both!(racing_an_approval);
async fn racing_an_approval(var: &str, table: &str) {
    let Some(w) = World::new(var, &format!("{table}_ra")).await else {
        return;
    };
    let agent = w.agent("refund-bot").await;
    let t = &w.table;
    let held = propose(
        &agent,
        &format!("UPDATE {t} SET status = 'refunded' WHERE amount < 35"),
    )
    .await;
    assert_eq!(held["state"], "held");
    // While it waits, a small allowed change moves a row into its filter.
    let sneak = propose(
        &agent,
        &format!("UPDATE {t} SET status = 'paid' WHERE id = 1"),
    )
    .await;
    assert_eq!(sneak["state"], "applied");

    let id = held["id"].as_i64().unwrap();
    let hash = w.engine.change(id).unwrap().review_hash.unwrap();
    let change = w.engine.approve("philip", id, &hash).await.unwrap();
    assert_eq!(change.state.as_str(), "failed");
    let (_, text) = call(&agent, "change_status", json!({ "id": id })).await;
    assert!(text.contains("drift since preview"), "{text}");
    assert!(!text.contains("(row"), "{text}");
    let rows = w.rows().await;
    assert_eq!(rows[0].1, "paid");
    assert!(rows[1..].iter().all(|(_, s)| s == "approved"), "{rows:?}");
}

// Attack (review 3): read other sessions' SQL through the server's own
// schemas, which the PROCESS grant Rowbit needs would otherwise expose.
on_both!(reading_the_server_s_own_schemas);
async fn reading_the_server_s_own_schemas(var: &str, table: &str) {
    let Some(w) = World::new(var, &format!("{table}_ss")).await else {
        return;
    };
    let agent = w.agent("refund-bot").await;
    for (sql, database) in [
        (
            "SELECT USER, HOST, DB, INFO FROM information_schema.PROCESSLIST",
            None,
        ),
        ("SELECT trx_query FROM information_schema.INNODB_TRX", None),
        (
            "SELECT * FROM performance_schema.events_statements_current",
            None,
        ),
        ("SELECT user, authentication_string FROM mysql.user", None),
        ("SELECT * FROM sys.session", None),
        ("TABLE information_schema.PROCESSLIST", None),
        ("SELECT INFO FROM PROCESSLIST", Some("information_schema")),
    ] {
        let mut args = json!({ "sql": sql });
        if let Some(db) = database {
            args["database"] = json!(db);
        }
        let (failed, text) = call(&agent, "query", args).await;
        assert!(failed, "{sql}: {text}");
    }
    let s = propose(
        &agent,
        &format!(
            "UPDATE {} SET status = (SELECT MAX(INFO) FROM information_schema.PROCESSLIST) WHERE id = 1",
            w.table
        ),
    )
    .await;
    assert_eq!(s["state"], "rejected", "{s}");
    w.untouched().await;
}

// Attack (review 3): an agent on the same host as the review page fetches
// it and posts an approval, with a stolen form token and guessed password.
on_both!(approving_through_the_review_page);
async fn approving_through_the_review_page(var: &str, table: &str) {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    let Some(w) = World::new(var, &format!("{table}_rp")).await else {
        return;
    };
    let agent = w.agent("refund-bot").await;
    let held = propose(
        &agent,
        &format!("UPDATE {} SET status = 'refunded'", w.table),
    )
    .await;
    let id = held["id"].as_i64().unwrap();
    let review = w.engine.change(id).unwrap().review_hash.unwrap();
    let page = rowbit_web::router(
        w.engine.clone(),
        rowbit_web::hash_password(&rowbit_web::new_password()),
        vec!["127.0.0.1:8080".to_owned()],
    );
    let send = |request: Request<Body>| {
        let page = page.clone();
        async move { page.oneshot(request).await.unwrap().status() }
    };
    let get = Request::get("/")
        .header(header::HOST, "127.0.0.1:8080")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(get).await,
        StatusCode::SEE_OTHER,
        "signed out sees nothing"
    );
    // Each wrong try waits longer; two are enough to show they fail.
    for password in [String::new(), rowbit_web::new_password()] {
        let login = Request::post("/login")
            .header(header::HOST, "127.0.0.1:8080")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("name=agent&password={password}")))
            .unwrap();
        assert_eq!(send(login).await, StatusCode::FORBIDDEN);
    }
    let approve = Request::post(format!("/changes/{id}/approve"))
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::COOKIE, format!("rowbit_session={}", "0".repeat(64)))
        .body(Body::from(format!(
            "token={}&review={review}",
            "0".repeat(64)
        )))
        .unwrap();
    assert_eq!(send(approve).await, StatusCode::FORBIDDEN);
    assert_eq!(w.engine.change(id).unwrap().state.as_str(), "held");
    w.untouched().await;
}
