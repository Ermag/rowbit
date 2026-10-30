//! Rowbit's MCP server. An agent gets four tools: `query` to read,
//! `describe_schema` to see the tables it may use, `propose_write` to ask
//! for a write, and `change_status` to follow it.
//! There is no tool to approve, reject, apply or undo: those belong to
//! people, through the CLI and the web page.
//!
//! The agent's identity comes from how the server was started (one server
//! per agent over stdio), or over HTTP from the token the request signed in
//! with ([`SignedInAgent`]), never from tool arguments. What a tool returns
//! never includes preview row values.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{RoleServer, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use rowbit_engine::{ChangeStatus, Engine};
use serde::Deserialize;
use serde_json::{Value, json};

const INSTRUCTIONS: &str = "\
Rowbit guards writes to this database. See the tables you may use with \
`describe_schema`, and read with `query`. To change data, \
send one INSERT, UPDATE or DELETE (or a schema change) to `propose_write`. \
Rowbit previews it without running it and checks it against the team's \
rules: it may be applied at once, held for a person, or rejected with a \
reason. You cannot approve changes; a person does. When a change is held, \
give the person its `review_url`, say what it changes, and stop: don't \
retry the write in another form or split it to get under a rule. Follow \
it with `change_status`.";

/// The agent an HTTP request signed in as. The transport's sign-in puts it
/// in the request's extensions after checking the request's token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedInAgent(pub String);

#[derive(Clone)]
enum Identity {
    /// Over stdio: set by whoever configured the MCP client.
    Fixed(String),
    /// Over HTTP: each request's [`SignedInAgent`].
    FromRequest,
}

#[derive(Clone)]
pub struct RowbitMcp {
    engine: Arc<Engine>,
    identity: Identity,
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
            identity: Identity::Fixed(agent.into()),
            database: database.into(),
            tool_router: Self::tool_router(),
        }
    }

    /// The MCP streamable HTTP transport, for agents that sign in per
    /// request. Mount it behind a layer that checks each request's token and
    /// adds its [`SignedInAgent`]; a request without one gets no tool.
    /// `hosts` are the `Host` values it answers, `origins` the only browser
    /// origins it accepts (requests without an `Origin` pass).
    pub fn http_service(
        engine: Arc<Engine>,
        database: impl Into<String>,
        hosts: Vec<String>,
        origins: Vec<String>,
    ) -> StreamableHttpService<RowbitMcp, LocalSessionManager> {
        let database = database.into();
        let config = StreamableHttpServerConfig::default()
            .with_allowed_hosts(hosts)
            .with_allowed_origins(origins)
            .enforce_origin_validation();
        StreamableHttpService::new(
            move || {
                Ok(RowbitMcp {
                    engine: engine.clone(),
                    identity: Identity::FromRequest,
                    database: database.clone(),
                    tool_router: Self::tool_router(),
                })
            },
            Arc::new(LocalSessionManager::default()),
            config,
        )
    }

    /// Who is calling: never from the tool's arguments.
    fn agent(&self, ctx: &RequestContext<RoleServer>) -> Result<String, CallToolResult> {
        match &self.identity {
            Identity::Fixed(agent) => Ok(agent.clone()),
            Identity::FromRequest => ctx
                .extensions
                .get::<http::request::Parts>()
                .and_then(|p| p.extensions.get::<SignedInAgent>())
                .map(|a| a.0.clone())
                .ok_or_else(|| error("not signed in")),
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
pub struct DescribeArgs {
    #[schemars(
        description = "For an agent without scopes, the database to list; defaults to the server's"
    )]
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
    async fn query(
        &self,
        Parameters(args): Parameters<QueryArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let agent = match self.agent(&ctx) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let database = self.database_or_default(args.database);
        match self.engine.query(&agent, &database, &args.sql).await {
            Ok(result) => ok(json!({
                "columns": result.columns,
                "rows": result.rows,
                "truncated": result.truncated,
            })),
            Err(e) => error(&e.to_string()),
        }
    }

    #[tool(
        description = "List the tables you may use: their columns (name, type, nullable, primary key) and what you may do with each (read, insert, update, delete, ddl)."
    )]
    async fn describe_schema(
        &self,
        Parameters(args): Parameters<DescribeArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let agent = match self.agent(&ctx) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let database = self.database_or_default(args.database);
        match self.engine.describe(&agent, &database).await {
            Ok(tables) => ok(json!({
                "tables": tables.iter().map(|t| json!({
                    "database": t.table.database,
                    "table": t.table.name,
                    "may": t.operations.iter().map(|o| o.as_str()).collect::<Vec<_>>(),
                    "columns": t.table.columns.iter().map(|c| json!({
                        "name": c.name,
                        "type": c.column_type,
                        "nullable": c.nullable,
                        "primary_key": c.primary_key,
                    })).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            })),
            Err(e) => error(&e.to_string()),
        }
    }

    #[tool(
        description = "Propose one write. Rowbit previews it, checks it against the rules, and applies it, holds it for a person, or rejects it. Returns the change's id, state, table, row count and reason, and for a held change review_url, its page for the person who reviews it."
    )]
    async fn propose_write(
        &self,
        Parameters(args): Parameters<ProposeArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let agent = match self.agent(&ctx) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let database = self.database_or_default(args.database);
        match self.engine.propose(&agent, &database, &args.sql).await {
            Ok(status) => ok(status_json(&status)),
            Err(e) => error(&e.to_string()),
        }
    }

    #[tool(description = "The current state of a change you proposed.")]
    async fn change_status(
        &self,
        Parameters(args): Parameters<StatusArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let agent = match self.agent(&ctx) {
            Ok(a) => a,
            Err(e) => return e,
        };
        match self.engine.status(&agent, args.id) {
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
    let mut json = json!({
        "id": s.id,
        "state": s.state.as_str(),
        "table": s.table,
        "rows": s.rows,
        "reason": s.reason,
    });
    if let Some(url) = &s.review_url {
        json["review_url"] = json!(url);
    }
    json
}

fn ok(value: Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(value.to_string())])
}

fn error(message: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.to_owned())])
}
