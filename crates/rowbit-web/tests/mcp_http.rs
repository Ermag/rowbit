//! Agents over MCP's streamable HTTP transport at `/mcp`, each signing in
//! with its own token, against the MariaDB test database from
//! `compose.yaml`.

use std::sync::Arc;

use mysql_async::prelude::Queryable;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rowbit_engine::Engine;
use rowbit_mysql::MySqlAdapter;
use rowbit_policy::PolicyFile;
use rowbit_store::Store;
use serde_json::{Value, json};

const POLICY: &str =
    "version: 0\nagents:\n  refund-bot:\n    default: hold\n  support-bot:\n    default: hold\n";

struct Server {
    base: String,
    refund: String,
    support: String,
    ghost: String,
}

async fn server(url: &str) -> Server {
    let mut store = Store::open_in_memory().unwrap();
    let refund = store.issue_agent_token("refund-bot").unwrap();
    let support = store.issue_agent_token("support-bot").unwrap();
    // A token for an agent no longer in rowbit.yml.
    let ghost = store.issue_agent_token("ghost-bot").unwrap();
    let engine = Arc::new(Engine::new(
        MySqlAdapter::new(url).unwrap(),
        PolicyFile::from_yaml(POLICY).unwrap(),
        store,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = listener.local_addr().unwrap().to_string();
    let router = rowbit_web::router(
        engine,
        rowbit_web::Web {
            sign_in: rowbit_web::SignIn::Password {
                hash: rowbit_web::hash_password(&rowbit_web::new_password()),
            },
            hosts: vec![host.clone()],
            secure: false,
            mcp: Some("rowbit_test".into()),
        },
    );
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Server {
        base: format!("http://{host}"),
        refund,
        support,
        ghost,
    }
}

async fn client(s: &Server, token: &str) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{}/mcp", s.base)).auth_header(token),
    );
    ().serve(transport).await.expect("signed in")
}

async fn call(
    c: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tool: &'static str,
    args: Value,
) -> (bool, String) {
    let r = c
        .call_tool(
            CallToolRequestParams::new(tool).with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    let text = r
        .content
        .iter()
        .map(|c| match c {
            ContentBlock::Text(t) => t.text.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    (r.is_error == Some(true), text)
}

/// A raw `initialize` POST, to see what the transport answers.
async fn raw(s: &Server, token: Option<&str>, origin: Option<&str>) -> u16 {
    let mut r = reqwest::Client::new()
        .post(format!("{}/mcp", s.base))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-06-18","capabilities":{},
                "clientInfo":{"name":"t","version":"0"}}})
            .to_string(),
        );
    if let Some(t) = token {
        r = r.bearer_auth(t);
    }
    if let Some(o) = origin {
        r = r.header("origin", o);
    }
    r.send().await.unwrap().status().as_u16()
}

#[tokio::test]
async fn agents_sign_in_with_their_own_token() {
    let Ok(url) = std::env::var("ROWBIT_TEST_MARIADB_URL") else {
        assert!(std::env::var("ROWBIT_REQUIRE_DB").as_deref() != Ok("1"));
        return;
    };
    let mut conn = mysql_async::Conn::from_url(&url).await.unwrap();
    conn.query_drop(
        "DROP TABLE IF EXISTS mcp_http_refunds; \
         CREATE TABLE mcp_http_refunds (id INT PRIMARY KEY, status VARCHAR(20) NOT NULL) ENGINE=InnoDB; \
         INSERT INTO mcp_http_refunds VALUES (1, 'approved'), (2, 'approved')",
    )
    .await
    .unwrap();
    conn.disconnect().await.unwrap();
    let s = server(&url).await;

    // No token, a made-up one, or one for an agent no longer listed.
    assert_eq!(raw(&s, None, None).await, 401);
    assert_eq!(raw(&s, Some("rbt_made-up"), None).await, 401);
    assert_eq!(raw(&s, Some(&s.ghost), None).await, 401);
    // A web page in someone's browser can't reach it, even with a token.
    assert_eq!(
        raw(&s, Some(&s.refund), Some("https://evil.example")).await,
        403
    );
    assert_eq!(raw(&s, Some(&s.refund), None).await, 200);

    let refund = client(&s, &s.refund).await;
    let mut tools: Vec<String> = refund
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.into_owned())
        .collect();
    tools.sort();
    assert_eq!(tools, ["change_status", "propose_write", "query"]);

    let (failed, text) = call(
        &refund,
        "propose_write",
        json!({ "sql": "UPDATE mcp_http_refunds SET status = 'refunded'" }),
    )
    .await;
    assert!(!failed, "{text}");
    let held: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(held["state"], "held");
    let (failed, _) = call(&refund, "change_status", json!({ "id": held["id"] })).await;
    assert!(!failed);

    // The identity is the token's, whatever session the request names: the
    // other agent can't see this change.
    let support = client(&s, &s.support).await;
    let (failed, text) = call(&support, "change_status", json!({ "id": held["id"] })).await;
    assert!(failed && text.contains("not found"), "{text}");
    refund.cancel().await.unwrap();
    support.cancel().await.unwrap();
}
