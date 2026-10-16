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
        /// Only changes from agents in shadow mode that a rule would have
        /// held or blocked
        #[arg(long)]
        shadowed: bool,
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
    /// With reviewers.oidc in rowbit-server.yml, reviewers sign in through
    /// that identity provider. Without it, Rowbit serves on a loopback
    /// address only, and reviewers sign in with the page's password and
    /// their name: the first time, Rowbit makes up a password and prints it
    /// once, keeping only its hash in the state file; --set-password asks
    /// for one instead.
    Serve {
        /// Where to listen; overrides rowbit-server.yml (default 127.0.0.1:8080)
        #[arg(long)]
        listen: Option<std::net::SocketAddr>,
        /// Choose the page's password (asked for on the terminal) and serve
        #[arg(long)]
        set_password: bool,
        #[command(flatten)]
        server: ServerArgs,
        #[command(flatten)]
        setup: Setup,
    },
    /// Work with rowbit-server.yml
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    /// Check the audit log for tampering
    Verify {
        #[command(flatten)]
        store: StoreArgs,
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
        let database = mysql_database(&self.url)?;
        let adapter = rowbit_mysql::MySqlAdapter::new(&self.url)?;
        let store = Store::open(&self.store)
            .with_context(|| format!("opening {}", self.store.display()))?;
        Ok((RowbitEngine::new(adapter, policy, store), database))
    }
}

fn decision_name(d: rowbit_policy::Decision) -> &'static str {
    match d {
        rowbit_policy::Decision::Allow => "allowed",
        rowbit_policy::Decision::Hold => "held",
        rowbit_policy::Decision::Block => "blocked",
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
enum ServerCommand {
    /// Parse and validate rowbit-server.yml, secrets included, without
    /// starting anything
    Check {
        #[command(flatten)]
        server: ServerArgs,
    },
}

/// Where rowbit-server.yml is.
#[derive(clap::Args)]
struct ServerArgs {
    /// Server settings: sign-in, public URL. Optional: without it, Rowbit
    /// serves on a loopback address with a page password
    #[arg(
        long = "config",
        env = "ROWBIT_SERVER_CONFIG",
        default_value = "rowbit-server.yml"
    )]
    config: PathBuf,
}

impl ServerArgs {
    /// The settings, or the defaults if the default file doesn't exist. A
    /// file named on purpose must exist.
    fn load(&self) -> anyhow::Result<rowbit_web::config::ServerConfig> {
        let named = self.config != std::path::Path::new("rowbit-server.yml");
        if !self.config.exists() && !named {
            return Ok(Default::default());
        }
        let src = std::fs::read_to_string(&self.config)
            .with_context(|| format!("reading {}", self.config.display()))?;
        Ok(rowbit_web::config::ServerConfig::from_yaml(&src, |name| {
            std::env::var(name).ok()
        })?)
    }
}

#[derive(Subcommand)]
enum PolicyCommand {
    /// Parse and validate a policy file
    Check {
        #[arg(default_value = "rowbit.yml")]
        path: PathBuf,
    },
    /// Replay the changes Rowbit has recorded against a policy file, and
    /// list every decision that would differ
    ///
    /// Uses each change's stored preview, so it needs no database. Scopes
    /// aren't replayed: they need the database's own check.
    Test {
        /// The policy to try
        path: PathBuf,
        #[command(flatten)]
        store: StoreArgs,
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
        }
        Command::Policy {
            command: PolicyCommand::Test { path, store },
        } => {
            let src = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let policy = PolicyFile::from_yaml(&src)?;
            let store = store.open()?;
            let mut ids = Vec::new();
            for state in State::ALL {
                ids.extend(store.list(state)?);
            }
            ids.sort_unstable();
            let (mut checked, mut differ) = (0, 0);
            for id in ids {
                let change = store.get(id)?;
                let Some(was) = rowbit_engine::recorded_decision(&store.events(id)?) else {
                    continue;
                };
                let Some(now) = rowbit_engine::replay(&policy, &change) else {
                    continue;
                };
                checked += 1;
                if now.decision == was {
                    continue;
                }
                differ += 1;
                let by = now.rule.map(|r| format!("rule {r}")).unwrap_or_else(|| {
                    if now.floor {
                        "a floor".into()
                    } else {
                        "the default".into()
                    }
                });
                println!(
                    "{}  was {}, would be {} by {by}",
                    review::line(&change),
                    decision_name(was),
                    decision_name(now.decision)
                );
            }
            println!(
                "{checked} recorded change(s) replayed against {}: {differ} would be decided differently",
                path.display()
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
        Command::Propose { agent, sql, setup } => {
            let (engine, database) = setup.engine()?;
            let status = engine.propose(&agent, &database, &sql).await?;
            println!("{}", rowbit_mcp::status_json(&status));
        }
        Command::Changes {
            state,
            shadowed,
            store,
        } => {
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
                let events = store.events(id)?;
                let shadow = events.iter().find(|e| e.event == "PolicyShadowed");
                if shadowed && shadow.is_none() {
                    continue;
                }
                let line = review::line(&store.get(id)?);
                match shadow {
                    Some(e) if shadowed => println!(
                        "{line}  {} ({})",
                        rowbit_core::text::reveal(e.detail.as_deref().unwrap_or_default()),
                        rowbit_core::text::reveal(&e.actor)
                    ),
                    _ => println!("{line}"),
                }
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
            server,
            setup,
        } => {
            let config = server.load()?;
            let listen = config.listen(listen);
            config.check_listen(listen)?;
            let sign_in = match (&config.oidc, &config.public_url) {
                (Some(oidc), Some(public_url)) => {
                    anyhow::ensure!(
                        !set_password,
                        "reviewers sign in through {}; there is no page password",
                        oidc.issuer
                    );
                    let login = rowbit_web::oidc::OidcLogin::discover(oidc, public_url)
                        .await
                        .map_err(|e| anyhow::anyhow!(e))?;
                    rowbit_web::SignIn::Oidc(Box::new(login))
                }
                _ => rowbit_web::SignIn::Password {
                    hash: page_password(&setup.store, set_password)?,
                },
            };
            let (engine, _) = setup.engine()?;
            // Every process that proposes changes queues notifications from
            // this setting; only this one sends them, with the secrets.
            engine.set_notify(&rowbit_engine::NotifySetting {
                review_url: config.public_url.clone(),
                targets: config
                    .notify
                    .iter()
                    .map(|n| rowbit_engine::NotifyTarget {
                        name: n.name.clone(),
                        agents: n.agents.clone(),
                    })
                    .collect(),
            })?;
            let engine = std::sync::Arc::new(engine);
            tokio::spawn(
                rowbit_web::notify::Notifier::new(engine.clone(), config.notify.clone()).run(),
            );
            let web = rowbit_web::Web {
                sign_in,
                hosts: config.public_host().into_iter().collect(),
                secure: config.https(),
            };
            let address = config
                .public_url
                .clone()
                .unwrap_or_else(|| format!("http://{listen}"));
            eprintln!("rowbit: review page on {address}/");
            rowbit_web::serve(engine, web, listen).await?;
        }
        Command::Server {
            command: ServerCommand::Check { server },
        } => {
            anyhow::ensure!(
                server.config.exists(),
                "{} does not exist",
                server.config.display()
            );
            let config = server.load()?;
            config.check_listen(config.listen(None))?;
            println!(
                "{}: ok (sign-in: {})",
                server.config.display(),
                match &config.oidc {
                    Some(o) => format!("OIDC through {}", o.issuer),
                    None => "page password, loopback only".to_owned(),
                }
            );
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
