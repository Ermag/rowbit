mod grants;
mod review;

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use rowbit_core::adapter::{Adapter, Engine};
use rowbit_core::change::{Actor, Event, State};

use rowbit_engine::Engine as RowbitEngine;
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
    /// Propose a write as an agent would, without an MCP client
    ///
    /// For trying Rowbit and for scripts. Prints what the agent would get
    /// back: the change's id, state, table, row count and reason.
    Propose {
        /// The agent's id, as listed in rowbit.yml
        #[arg(long)]
        agent: String,
        /// One INSERT, UPDATE or DELETE, or one schema change
        sql: String,
        #[command(flatten)]
        setup: Setup,
    },
    /// List changes, newest last
    Changes {
        /// Only changes in this state: held, applied, rejected, ...
        #[arg(long)]
        state: Option<String>,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Show one change: the statement, every row before and after, and its history
    Show {
        id: i64,
        /// List every row, not just the first 50
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Approve a held change and apply it
    ///
    /// --review is the review hash `rowbit show` printed (at least its first
    /// 12 characters), so you approve exactly what you looked at. A change
    /// of more rows than `show` lists by default also needs --rows with its
    /// row count, so you say how many rows you approve.
    Approve {
        id: i64,
        #[arg(long)]
        review: String,
        /// The change's row count; needed above 50 rows
        #[arg(long)]
        rows: Option<usize>,
        #[command(flatten)]
        person: Person,
        #[command(flatten)]
        setup: Setup,
    },
    /// Reject a held change
    Reject {
        id: i64,
        #[arg(long)]
        reason: String,
        #[command(flatten)]
        person: Person,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Undo an applied change, if no row changed since
    Undo {
        id: i64,
        #[command(flatten)]
        person: Person,
        #[command(flatten)]
        setup: Setup,
    },
    /// Serve the review page: held changes, approve, reject, undo
    ///
    /// Reviewers sign in with the page's password and their name. The first
    /// time, Rowbit makes up a password and prints it once; only its hash is
    /// kept, in the state file. --set-password asks for one instead.
    Serve {
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: std::net::SocketAddr,
        /// Choose the page's password (asked for on the terminal) and serve
        #[arg(long)]
        set_password: bool,
        #[command(flatten)]
        setup: Setup,
    },
    /// Check the audit log for tampering
    Verify {
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Database users for scoped agents: diff, apply
    ///
    /// Each agent with scopes in rowbit.yml gets a database user of its
    /// own, rowbit_<agent>, with grants compiled from its scopes. These
    /// commands connect as an administrator, for this run only.
    Grants {
        #[command(subcommand)]
        command: GrantsCommand,
    },
    /// Check that Rowbit can reach a database
    Ping {
        /// mysql://user:password@host:port/database
        #[arg(long, env = "ROWBIT_DATABASE_URL", hide_env_values = true)]
        url: String,
    },
}

/// Only Rowbit's state file: for commands that don't touch the database,
/// so reviewers can look without its credentials.
#[derive(clap::Args)]
struct StoreArgs {
    /// Change state and the audit log (SQLite)
    #[arg(long, env = "ROWBIT_STORE", default_value = "rowbit.db")]
    store: PathBuf,
}

impl StoreArgs {
    fn open(&self) -> anyhow::Result<Store> {
        anyhow::ensure!(
            self.store.exists(),
            "{} does not exist; run this where Rowbit keeps its state, or pass --store",
            self.store.display()
        );
        Store::open(&self.store).with_context(|| format!("opening {}", self.store.display()))
    }
}

/// Who is acting. Recorded in the audit log as `human:<name>`.
#[derive(clap::Args)]
struct Person {
    /// Your name in the audit log; defaults to your login name
    #[arg(long = "as", env = "ROWBIT_USER")]
    name: Option<String>,
}

impl Person {
    fn name(&self) -> anyhow::Result<String> {
        let name = self
            .name
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .or_else(|| std::env::var("USERNAME").ok())
            .unwrap_or_default();
        anyhow::ensure!(!name.trim().is_empty(), "say who you are with --as <name>");
        Ok(name)
    }
}

/// The full review hash, from the prefix a person typed. Refuses short or
/// mismatched prefixes, so a typo never approves the wrong change.
fn review_hash(stored: Option<&str>, typed: &str) -> anyhow::Result<String> {
    let stored = stored.context("this change has nothing to review")?;
    anyhow::ensure!(
        typed.len() >= 12,
        "give at least the first 12 characters of the review hash"
    );
    anyhow::ensure!(
        stored.starts_with(&typed.to_ascii_lowercase()),
        "that is not this change's review hash; run `rowbit show` again"
    );
    Ok(stored.to_owned())
}

/// Above the rows `show` lists by default, the person must name the total,
/// so an approval never covers rows they didn't know about.
fn confirm_rows(change: &rowbit_store::Change, rows: Option<usize>) -> anyhow::Result<()> {
    let total = match &change.review {
        Some(rowbit_store::Review::Rows(p)) => p.rows.len(),
        _ => return Ok(()),
    };
    match rows {
        Some(n) if n == total => Ok(()),
        Some(n) => anyhow::bail!("this change writes {}, not {n}", review::count(total)),
        None if total <= review::SHOWN_ROWS => Ok(()),
        None => anyhow::bail!(
            "this change writes {total} rows and `rowbit show` lists {} of them; \
             check them with `rowbit show {} --all`, then approve with --rows {total}",
            review::SHOWN_ROWS,
            change.id
        ),
    }
}

/// The review page's password hash, kept in the state file. Makes one up
/// and prints it once if there is none; asks for one with `set`.
fn page_password(store: &std::path::Path, set: bool) -> anyhow::Result<String> {
    const KEY: &str = "web.password";
    let mut store = Store::open(store).with_context(|| format!("opening {}", store.display()))?;
    if set {
        let password = rpassword::prompt_password("New review page password: ")?;
        anyhow::ensure!(password.chars().count() >= 12, "use at least 12 characters");
        let again = rpassword::prompt_password("Again: ")?;
        anyhow::ensure!(password == again, "the two passwords differ");
        let hash = rowbit_web::hash_password(&password);
        store.set_setting(KEY, &hash)?;
        eprintln!("rowbit: review page password set");
        return Ok(hash);
    }
    if let Some(hash) = store.setting(KEY)? {
        return Ok(hash);
    }
    let password = rowbit_web::new_password();
    let hash = rowbit_web::hash_password(&password);
    store.set_setting(KEY, &hash)?;
    eprintln!(
        "rowbit: review page password, shown this once (change it with --set-password):\n\n    {password}\n"
    );
    Ok(hash)
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
    fn engine(&self) -> anyhow::Result<(RowbitEngine, String)> {
        let src = std::fs::read_to_string(&self.policy)
            .with_context(|| format!("reading {}", self.policy.display()))?;
        let policy = PolicyFile::from_yaml(&src)?;
        // Fail closed until enforcement lands: nobody should believe a
        // scope is enforced when it is not.
        anyhow::ensure!(
            !policy.has_scopes(),
            "{} has scopes, which `rowbit grants` compiles but this build doesn't enforce yet; \
             remove them to run",
            self.policy.display()
        );
        let database = mysql_database(&self.url)?;
        let adapter = rowbit_mysql::MySqlAdapter::new(&self.url)?;
        let store = Store::open(&self.store)
            .with_context(|| format!("opening {}", self.store.display()))?;
        Ok((RowbitEngine::new(adapter, policy, store), database))
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
enum GrantsCommand {
    /// Print what would change for each agent's user; changes nothing
    Diff {
        #[command(flatten)]
        admin: grants::Admin,
    },
    /// Create agents' users and bring their grants in line with rowbit.yml
    Apply {
        #[command(flatten)]
        admin: grants::Admin,
        #[command(flatten)]
        args: grants::ApplyArgs,
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
                "{}: ok ({} agent(s), {rules} rule(s))",
                path.display(),
                policy.agents.len()
            );
            for warning in policy.warnings() {
                println!("warning: {warning}");
            }
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
        Command::Propose { agent, sql, setup } => {
            let (engine, database) = setup.engine()?;
            let status = engine.propose(&agent, &database, &sql).await?;
            println!("{}", rowbit_mcp::status_json(&status));
        }
        Command::Changes { state, store } => {
            let store = store.open()?;
            let states = match state {
                Some(s) => vec![State::parse(&s).with_context(|| format!("unknown state {s}"))?],
                None => State::ALL.to_vec(),
            };
            let mut ids = Vec::new();
            for state in states {
                ids.extend(store.list(state)?);
            }
            ids.sort_unstable();
            for id in ids {
                println!("{}", review::line(&store.get(id)?));
            }
        }
        Command::Show { id, all, store } => {
            let store = store.open()?;
            print!("{}", review::full(&store.get(id)?, &store.events(id)?, all));
        }
        Command::Approve {
            id,
            review,
            rows,
            person,
            setup,
        } => {
            let (engine, _) = setup.engine()?;
            let change = engine.change(id)?;
            let hash = review_hash(change.review_hash.as_deref(), &review)?;
            confirm_rows(&change, rows)?;
            let change = engine.approve(&person.name()?, id, &hash).await?;
            println!("{}", review::line(&change));
        }
        Command::Reject {
            id,
            reason,
            person,
            store,
        } => {
            let mut store = store.open()?;
            store.transition(
                id,
                &Event::Reject { reason },
                &Actor::Human(person.name()?),
                None,
            )?;
            println!("{}", review::line(&store.get(id)?));
        }
        Command::Undo { id, person, setup } => {
            let (engine, _) = setup.engine()?;
            let change = engine.undo(&person.name()?, id).await?;
            println!("{}", review::line(&change));
        }
        Command::Serve {
            listen,
            set_password,
            setup,
        } => {
            let hash = page_password(&setup.store, set_password)?;
            let (engine, _) = setup.engine()?;
            if !listen.ip().is_loopback() {
                eprintln!(
                    "rowbit: the page is served over plain HTTP on {listen}; \
                     put it behind HTTPS before anyone signs in over a network"
                );
            }
            eprintln!("rowbit: review page on http://{listen}/");
            rowbit_web::serve(std::sync::Arc::new(engine), hash, listen).await?;
        }
        Command::Verify { store } => {
            let store = store.open()?;
            let changes: usize = State::ALL
                .iter()
                .map(|s| store.list(*s).map(|ids| ids.len()))
                .sum::<Result<usize, _>>()?;
            match store.verify()? {
                Ok(()) => println!("audit log ok ({changes} changes checked)"),
                Err(problem) => anyhow::bail!("audit log tampered: {problem}"),
            }
        }
        Command::Grants {
            command: GrantsCommand::Diff { admin },
        } => grants::diff(admin).await?,
        Command::Grants {
            command: GrantsCommand::Apply { admin, args },
        } => grants::apply(admin, args).await?,
        Command::Ping { url } => {
            let adapter = rowbit_mysql::MySqlAdapter::new(&url)?;
            let info = adapter.server_info().await?;
            let engine = match info.engine {
                Engine::MariaDb => "MariaDB",
                Engine::MySql => "MySQL",
            };
            println!("{engine} {}", info.version);
            adapter.close().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_hash_needs_a_real_prefix() {
        let stored = "ab12cd34ef56ab12cd34ef56ab12cd34ef56ab12cd34ef56ab12cd34ef56ab12";
        assert_eq!(review_hash(Some(stored), "AB12CD34EF56").unwrap(), stored);
        assert!(review_hash(Some(stored), "ab12cd").is_err());
        assert!(review_hash(Some(stored), "ab12cd34ef57").is_err());
        assert!(review_hash(None, "ab12cd34ef56").is_err());
    }

    #[test]
    fn database_comes_from_the_url() {
        assert_eq!(
            mysql_database("mysql://u:p@h:3306/app?ssl-mode=required").unwrap(),
            "app"
        );
        assert!(mysql_database("mysql://u:p@h:3306").is_err());
        assert!(mysql_database("mysql://u:p@h:3306/").is_err());
    }
}
