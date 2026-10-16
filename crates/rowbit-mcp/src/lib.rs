//! Rowbit's MCP server. An agent gets three tools: `query` to read,
//! `propose_write` to ask for a write, and `change_status` to follow it.
//! There is no tool to approve, reject, apply or undo: those belong to
//! people, through the CLI and the web page.
//!
//! The agent's identity comes from how the server was started (one server
//! per agent over stdio), never from tool arguments. What a tool returns
//! never includes preview row values.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use rowbit_engine::{ChangeStatus, Engine};
use serde::Deserialize;
use serde_json::{Value, json};

const INSTRUCTIONS: &str = "\
Rowbit guards writes to this database. Read with `query`. To change data, \
send one INSERT, UPDATE or DELETE (or a schema change) to `propose_write`. \
Rowbit previews it without running it and checks it against the team's \
rules: it may be applied at once, held for a person, or rejected with a \
reason. Follow a held change with `change_status`. You cannot approve \
changes; a person does.";

#[derive(Clone)]
pub struct RowbitMcp {
    engine: Arc<Engine>,
    agent: String,
    database: String,
    #[allow(dead_code, reason = "used by the tool_handler macro")]
    tool_router: ToolRouter<Self>,
}

impl RowbitMcp {
    /// A server for one agent. `agent` must be the identity the operator
    /// configured for this server, never something the agent supplied.
    /// `database` is the default for tools that do not name one.
    pub fn new(engine: Arc<Engine>, agent: impl Into<String>, database: impl Into<String>) -> Self {
        Self {
            engine,
            agent: agent.into(),
            database: database.into(),
            tool_router: Self::tool_router(),
        }
    }

    /// Serves over stdin and stdout until the client disconnects.
    pub async fn serve_stdio(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let running = self.serve(rmcp::transport::stdio()).await?;
        running.waiting().await?;
        Ok(())
    }

    fn database_or_default(&self, database: Option<String>) -> String {
        database.unwrap_or_else(|| self.database.clone())
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryArgs {
    #[schemars(description = "One SELECT statement, MySQL syntax")]
    pub sql: String,
    #[schemars(description = "Database to read from; defaults to the server's")]
    pub database: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProposeArgs {
    #[schemars(
        description = "One INSERT, UPDATE or DELETE on one table with a primary key, or one schema change. MySQL syntax"
    )]
    pub sql: String,
    #[schemars(description = "Database to write to; defaults to the server's")]
    pub database: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StatusArgs {
    #[schemars(description = "The change id propose_write returned")]
    pub id: i64,
}

#[tool_router]
impl RowbitMcp {
    #[tool(description = "Run one read-only SELECT. Returns columns and up to 1,000 rows.")]
    async fn query(&self, Parameters(args): Parameters<QueryArgs>) -> CallToolResult {
        let database = self.database_or_default(args.database);
        match self.engine.query(&self.agent, &database, &args.sql).await {
            Ok(result) => ok(json!({
                "columns": result.columns,
                "rows": result.rows,
                "truncated": result.truncated,
            })),
            Err(e) => error(&e.to_string()),
        }
    }

    #[tool(
        description = "Propose one write. Rowbit previews it, checks it against the rules, and applies it, holds it for a person, or rejects it. Returns the change's id, state, table, row count and reason."
    )]
    async fn propose_write(&self, Parameters(args): Parameters<ProposeArgs>) -> CallToolResult {
        let database = self.database_or_default(args.database);
        match self.engine.propose(&self.agent, &database, &args.sql).await {
            Ok(status) => ok(status_json(&status)),
            Err(e) => error(&e.to_string()),
        }
    }

    #[tool(description = "The current state of a change you proposed.")]
    async fn change_status(&self, Parameters(args): Parameters<StatusArgs>) -> CallToolResult {
        match self.engine.status(&self.agent, args.id) {
            Ok(status) => ok(status_json(&status)),
            Err(e) => error(&e.to_string()),
        }
    }
}

#[tool_handler]
impl ServerHandler for RowbitMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rowbit", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Everything an agent learns about a change. Built field by field from
/// [`ChangeStatus`], which carries no row values.
pub fn status_json(s: &ChangeStatus) -> Value {
    json!({
        "id": s.id,
        "state": s.state.as_str(),
        "table": s.table,
        "rows": s.rows,
        "reason": s.reason,
    })
}

fn ok(value: Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(value.to_string())])
}

fn error(message: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.to_owned())])
}
