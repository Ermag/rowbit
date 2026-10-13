use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use rowbit_core::adapter::Adapter;
use rowbit_policy::PolicyFile;

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
    /// Check that Rowbit can reach a database
    Ping {
        /// mysql://user:password@host:port/database
        #[arg(long, env = "ROWBIT_DATABASE_URL", hide_env_values = true)]
        url: String,
    },
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
        Command::Ping { url } => {
            let adapter = rowbit_mysql::MySqlAdapter::new(&url)?;
            let info = adapter.server_info().await?;
            println!("{:?} {}", info.engine, info.version);
            adapter.close().await?;
        }
    }
    Ok(())
}
