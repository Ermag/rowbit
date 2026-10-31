//! Drives the MCP server as an agent would, through an MCP client, against
//! the test databases from `compose.yaml`. Every scenario runs on both
//! MariaDB 11.4 and MySQL 8.4.

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

const SECRET: &str = "s3cr3t-card-4242";

const POLICY: &str = r#"
version: 0
agents:
  refund-bot:
    default: block
    rules:
      - name: small-refund-batches
        when: { statement: update, table: rowbit_test.mc_*, columns: status, rows: { max: 2 } }
        then: allow
      - name: refunds-need-a-person
        when: { table: rowbit_test.mc_* }
        then: hold
        reason: Large refund batches are reviewed by billing.
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
                super::$scenario("ROWBIT_TEST_MARIADB_URL", "mc_m").await
            }
            #[tokio::test]
            async fn mysql() {
                super::$scenario("ROWBIT_TEST_MYSQL_URL", "mc_y").await
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

/// A connected MCP client for `agent`, served by one shared engine.
async fn connect(engine: &Arc<Engine>, agent: &str) -> RunningService<RoleClient, Agent> {
    let (client_io, server_io) = tokio::io::duplex(1 << 16);
    let server = RowbitMcp::new(engine.clone(), agent, "rowbit_test");
    tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    Agent.serve(client_io).await.unwrap()
}

/// Calls a tool; returns whether it reported an error, and its text.
async fn call(
    client: &RunningService<RoleClient, Agent>,
    tool: &'static str,
    args: Value,
) -> (bool, String) {
    let result = client
        .call_tool(
            CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    let text = result
        .content
        .iter()
        .map(|c| match c {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("unexpected content {other:?}"),
        })
        .collect::<String>();
    (result.is_error == Some(true), text)
}

async fn setup(var: &str, table: &str) -> Option<(Arc<Engine>, String)> {
    let url = url(var)?;
    let mut conn = mysql_async::Conn::from_url(&url).await.unwrap();
    conn.query_drop(format!(
        "DROP TABLE IF EXISTS {table}; \
         CREATE TABLE {table} (id INT PRIMARY KEY, status VARCHAR(20) NOT NULL, \
           card TEXT NOT NULL) ENGINE=InnoDB; \
         INSERT INTO {table} VALUES (1, 'approved', '{SECRET}'), (2, 'approved', '{SECRET}'), \
           (3, 'approved', '{SECRET}')"
    ))
    .await
    .unwrap();
    conn.disconnect().await.unwrap();
    let engine = Engine::new(
        MySqlAdapter::new(&url).unwrap(),
        PolicyFile::from_yaml(POLICY).unwrap(),
        Store::open_in_memory().unwrap(),
    );
    Some((Arc::new(engine), table.to_owned()))
}

#[tokio::test]
async fn agents_get_four_tools_and_none_approves() {
    // Needs no database: listing tools never reaches it.
    let engine = Arc::new(Engine::new(
        MySqlAdapter::new("mysql://nobody@127.0.0.1:1/none").unwrap(),
        PolicyFile::from_yaml(POLICY).unwrap(),
        Store::open_in_memory().unwrap(),
    ));
    let client = connect(&engine, "refund-bot").await;
    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["change_status", "describe_schema", "propose_write", "query"]
    );
    client.cancel().await.unwrap();
}

on_both!(propose_and_follow_a_change);
async fn propose_and_follow_a_change(var: &str, table: &str) {
    let Some((engine, t)) = setup(var, &format!("{table}_p")).await else {
        return;
    };
    let client = connect(&engine, "refund-bot").await;

    let (failed, text) = call(
        &client,
        "propose_write",
        json!({ "sql": format!("UPDATE {t} SET status = 'refunded' WHERE id IN (1, 2)") }),
    )
    .await;
    assert!(!failed, "{text}");
    let status: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(status["state"], "applied", "{text}");
    assert_eq!(status["rows"], 2);
    assert!(status.get("review_url").is_none(), "{text}");

    let (_, text) = call(
        &client,
        "propose_write",
        json!({ "sql": format!("UPDATE {t} SET status = 'refunded' WHERE status <> 'x'") }),
    )
    .await;
    let held: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(held["state"], "held", "{text}");
    assert_eq!(
        held["reason"],
        "Large refund batches are reviewed by billing."
    );
    assert_eq!(
        held["review_url"],
        format!("http://127.0.0.1:8080/changes/{}", held["id"])
    );

    let (_, text) = call(&client, "change_status", json!({ "id": held["id"] })).await;
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), held);
    client.cancel().await.unwrap();
}

// The spec's promise: an agent never sees preview values. The table's card
// column is in every before and after image, so any leak would show it.
on_both!(agents_never_see_preview_values);
async fn agents_never_see_preview_values(var: &str, table: &str) {
    let Some((engine, t)) = setup(var, &format!("{table}_v")).await else {
        return;
    };
    let client = connect(&engine, "refund-bot").await;
    let mut seen = String::new();
    for sql in [
        format!("UPDATE {t} SET status = 'refunded' WHERE id = 1"),
        format!("UPDATE {t} SET status = 'refunded' WHERE status = 'approved'"),
        format!("DELETE FROM {t} WHERE id = 3"),
        format!("UPDATE {t} SET status = (SELECT MAX(card) FROM {t}) WHERE id = 2"),
        format!("UPDATE {t} SET id = (SELECT MAX(card) FROM {t}) + 0 WHERE id = 2"),
    ] {
        let (_, text) = call(&client, "propose_write", json!({ "sql": sql })).await;
        if let Ok(status) = serde_json::from_str::<Value>(&text) {
            let (_, again) = call(&client, "change_status", json!({ "id": status["id"] })).await;
            seen.push_str(&again);
        }
        seen.push_str(&text);
    }
    assert!(!seen.contains(SECRET), "{seen}");
    assert!(!seen.contains("s3cr3t"), "{seen}");
    client.cancel().await.unwrap();
}

on_both!(agents_see_only_their_own_changes);
async fn agents_see_only_their_own_changes(var: &str, table: &str) {
    let Some((engine, t)) = setup(var, &format!("{table}_o")).await else {
        return;
    };
    let refund = connect(&engine, "refund-bot").await;
    let support = connect(&engine, "support-bot").await;
    let (_, text) = call(
        &refund,
        "propose_write",
        json!({ "sql": format!("UPDATE {t} SET status = 'refunded' WHERE id = 1") }),
    )
    .await;
    let id = serde_json::from_str::<Value>(&text).unwrap()["id"].clone();
    let (failed, text) = call(&support, "change_status", json!({ "id": id })).await;
    assert!(failed, "{text}");
    assert!(text.contains("not found"), "{text}");

    let stranger = connect(&engine, "stranger").await;
    let (failed, text) = call(
        &stranger,
        "query",
        json!({ "sql": format!("SELECT * FROM {t}") }),
    )
    .await;
    assert!(failed && text.contains("not listed"), "{text}");
    refund.cancel().await.unwrap();
    support.cancel().await.unwrap();
    stranger.cancel().await.unwrap();
}

on_both!(describe_lists_tables_and_columns);
async fn describe_lists_tables_and_columns(var: &str, table: &str) {
    let Some((engine, t)) = setup(var, &format!("{table}_d")).await else {
        return;
    };
    let client = connect(&engine, "refund-bot").await;
    let (failed, text) = call(&client, "describe_schema", json!({})).await;
    assert!(!failed, "{text}");
    let schema: Value = serde_json::from_str(&text).unwrap();
    let ours = schema["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["table"] == t.as_str())
        .unwrap_or_else(|| panic!("{t} missing from {text}"));
    assert_eq!(ours["database"], "rowbit_test");
    assert_eq!(
        ours["may"],
        json!(["read", "insert", "update", "delete", "ddl"])
    );
    // MariaDB writes `int(11)`, MySQL `int`.
    let id = &ours["columns"][0];
    assert_eq!(id["name"], "id");
    assert!(id["type"].as_str().unwrap().starts_with("int"), "{id}");
    assert_eq!(
        (&id["nullable"], &id["primary_key"]),
        (&json!(false), &json!(true))
    );
    assert!(!text.contains(SECRET), "describe never reads rows");
    let (failed, text) = call(&client, "describe_schema", json!({ "database": "mysql" })).await;
    assert!(failed && text.contains("system schema"), "{text}");
    client.cancel().await.unwrap();
}

on_both!(query_reads_and_refuses_writes);
async fn query_reads_and_refuses_writes(var: &str, table: &str) {
    let Some((engine, t)) = setup(var, &format!("{table}_q")).await else {
        return;
    };
    let client = connect(&engine, "refund-bot").await;
    let (failed, text) = call(
        &client,
        "query",
        json!({ "sql": format!("SELECT id, status FROM {t} WHERE id < 3 ORDER BY id") }),
    )
    .await;
    assert!(!failed, "{text}");
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        json!({
            "columns": ["id", "status"],
            "rows": [["1", "approved"], ["2", "approved"]],
            "truncated": false,
        })
    );
    for sql in [
        format!("UPDATE {t} SET status = 'x'"),
        format!("SELECT * FROM {t} FOR UPDATE"),
        "SELECT SLEEP(5)".to_owned(),
        "SELECT INFO FROM information_schema.PROCESSLIST".to_owned(),
    ] {
        let (failed, text) = call(&client, "query", json!({ "sql": sql })).await;
        assert!(failed, "{sql}: {text}");
    }
    let (failed, text) = call(
        &client,
        "query",
        json!({ "sql": "SELECT INFO FROM PROCESSLIST", "database": "information_schema" }),
    )
    .await;
    assert!(failed && text.contains("system schema"), "{text}");
    let (failed, text) = call(
        &client,
        "query",
        json!({ "sql": "SELECT * FROM mc_missing" }),
    )
    .await;
    assert!(
        failed && text.starts_with("database error: ERROR 1146"),
        "{text}"
    );
    client.cancel().await.unwrap();
}
