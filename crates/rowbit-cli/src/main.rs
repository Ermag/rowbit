use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use rowbit_core::adapter::Adapter;
use rowbit_engine::Engine;
use rowbit_policy::PolicyFile;
use rowbit_store::Store;

#[derive(Parser)]
#[command(
    name = "rowbit",
    version,
    about = "A safety net for AI agents that write to your database"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Work with rowbit.yml
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
    /// Serve one agent over MCP on stdin and stdout
    ///
    /// The agent's identity is --agent, set by whoever configures the MCP
    /// client, never by the agent. Run one server per agent.
    Mcp {
        /// The agent's id, as listed in rowbit.yml
        #[arg(long)]
        agent: String,
        #[command(flatten)]
        setup: Setup,
    },
    /// Check that Rowbit can reach a database
    Ping {
        /// mysql://user:password@host:port/database
        #[arg(long, env = "ROWBIT_DATABASE_URL", hide_env_values = true)]
        url: String,
    },
}

/// Where Rowbit's database, policy and state are.
#[derive(clap::Args)]
struct Setup {
    /// mysql://user:password@host:port/database
    #[arg(long, env = "ROWBIT_DATABASE_URL", hide_env_values = true)]
    url: String,
    /// The policy file
    #[arg(long, env = "ROWBIT_POLICY", default_value = "rowbit.yml")]
    policy: PathBuf,
    /// Change state and the audit log (SQLite)
    #[arg(long, env = "ROWBIT_STORE", default_value = "rowbit.db")]
    store: PathBuf,
}

impl Setup {
    /// The engine, and the database named in the URL.
    fn engine(&self) -> anyhow::Result<(Engine, String)> {
        let src = std::fs::read_to_string(&self.policy)
            .with_context(|| format!("reading {}", self.policy.display()))?;
        let policy = PolicyFile::from_yaml(&src)?;
        let database = mysql_database(&self.url)?;
        let adapter = rowbit_mysql::MySqlAdapter::new(&self.url)?;
        let store = Store::open(&self.store)
            .with_context(|| format!("opening {}", self.store.display()))?;
        Ok((Engine::new(adapter, policy, store), database))
    }
}

/// The database a `mysql://` URL names; Rowbit needs one as the default.
fn mysql_database(url: &str) -> anyhow::Result<String> {
    let path = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split_once('/'))
        .map(|(_, path)| path.split(['?', '#']).next().unwrap_or(""))
        .unwrap_or("");
    anyhow::ensure!(
        !path.is_empty(),
        "the database URL must name a database, as in mysql://user@host:3306/app"
    );
    Ok(path.to_owned())
}

#[derive(Subcommand)]
enum PolicyCommand {
    /// Parse and validate a policy file
    Check {
        #[arg(default_value = "rowbit.yml")]
        path: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Policy {
            command: PolicyCommand::Check { path },
        } => {
            let src = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let policy = PolicyFile::from_yaml(&src)?;
            let rules: usize = policy.agents.values().map(|a| a.rules.len()).sum();
            println!(
                "{}: ok ({} agents, {rules} rules)",
                path.display(),
                policy.agents.len()
            );
        }
        Command::Mcp { agent, setup } => {
            let (engine, database) = setup.engine()?;
            anyhow::ensure!(
                !agent.is_empty(),
                "--agent must name an agent from rowbit.yml"
            );
            // stdout carries MCP; everything for people goes to stderr.
            let interrupted = engine.interrupted()?;
            if !interrupted.is_empty() {
                eprintln!(
                    "rowbit: changes {interrupted:?} were interrupted while applying or undoing; \
                     check their rows against the preview before anything else"
                );
            }
            rowbit_mcp::RowbitMcp::new(std::sync::Arc::new(engine), agent, database)
                .serve_stdio()
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
        }
        Command::Ping { url } => {
            let adapter = rowbit_mysql::MySqlAdapter::new(&url)?;
            let info = adapter.server_info().await?;
            println!("{:?} {}", info.engine, info.version);
            adapter.close().await?;
        }
    }
    Ok(())
}
