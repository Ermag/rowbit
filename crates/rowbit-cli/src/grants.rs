//! `rowbit grants diff` and `apply`: database users for scoped agents. See
//! "Database users for agents" in `docs/spec/server.md`.

use std::collections::BTreeSet;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::Context;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use rowbit_mysql::grants::{self, Account, Catalog, Compiled, Current, Diff};
use rowbit_policy::{PolicyFile, agent_user};
use rowbit_store::Store;

/// What `grants` needs: an administrator for the one run, never stored.
#[derive(clap::Args)]
pub struct Admin {
    /// An administrator's mysql:// URL, able to create users and grant
    /// privileges. Used for this run only, never stored.
    #[arg(long, env = "ROWBIT_ADMIN_DATABASE_URL", hide_env_values = true)]
    admin_url: String,
    /// Rowbit's own mysql:// URL. Agents' users get the host part of
    /// Rowbit's user, so they can connect from where Rowbit runs.
    #[arg(long, env = "ROWBIT_DATABASE_URL", hide_env_values = true)]
    url: Option<String>,
    /// The host part for agents' users, instead of rowbit-server.yml's
    /// agent_users.host or Rowbit's own
    #[arg(long)]
    agent_host: Option<String>,
    /// Server settings, for agent_users.host
    #[arg(
        long = "config",
        env = "ROWBIT_SERVER_CONFIG",
        default_value = "rowbit-server.yml"
    )]
    config: PathBuf,
    /// The policy file
    #[arg(long, env = "ROWBIT_POLICY", default_value = "rowbit.yml")]
    policy: PathBuf,
    /// Change state and the audit log (SQLite), which keeps the agents'
    /// database passwords
    #[arg(long, env = "ROWBIT_STORE", default_value = "rowbit.db")]
    store: PathBuf,
}

#[derive(clap::Args)]
pub struct ApplyArgs {
    /// Don't ask before running the statements
    #[arg(long)]
    yes: bool,
    /// Give every agent's user a new password
    #[arg(long)]
    rotate_passwords: bool,
    /// Drop the users of agents no longer scoped in rowbit.yml
    #[arg(long)]
    drop_removed: bool,
}

fn password_key(agent: &str) -> String {
    format!("agent.{agent}.password")
}

fn account_key(agent: &str) -> String {
    format!("agent.{agent}.account")
}

/// One scoped agent's user: what it holds and what it should.
struct Plan {
    agent: String,
    account: Account,
    exists: bool,
    has_password: bool,
    compiled: Compiled,
    current: Current,
    diff: Diff,
}

impl Plan {
    fn needs_password(&self, rotate: bool) -> bool {
        !self.exists || !self.has_password || rotate
    }

    fn is_empty(&self, rotate: bool) -> bool {
        !self.needs_password(rotate) && self.diff.is_empty()
    }

    fn print(&self, rotate: bool, out: &mut impl Write) -> std::io::Result<()> {
        writeln!(out, "{}  {}", self.agent, self.account)?;
        if self.is_empty(rotate) && self.current.unmanaged.is_empty() {
            writeln!(out, "  up to date")?;
        }
        if !self.exists {
            writeln!(out, "  create the user, with a new password")?;
        } else if self.needs_password(rotate) {
            let why = if rotate { "as asked" } else { "none is stored" };
            writeln!(out, "  give the user a new password ({why})")?;
        }
        for s in self.diff.statements(&self.account) {
            writeln!(out, "  {s}")?;
        }
        for n in &self.compiled.notes {
            writeln!(out, "  note: {n}")?;
        }
        for e in &self.compiled.errors {
            writeln!(out, "  error: {e}")?;
        }
        for line in &self.current.unmanaged {
            writeln!(out, "  can't fix, remove it by hand: {line}")?;
        }
        Ok(())
    }
}

async fn connect(url: &str) -> anyhow::Result<Conn> {
    let opts = Opts::from_url(url).context("the database URL is not valid")?;
    Ok(Conn::new(opts).await?)
}

/// The host part for agents' users.
async fn agent_host(admin: &Admin) -> anyhow::Result<String> {
    let from_file = if admin.config.exists() {
        let src = std::fs::read_to_string(&admin.config)
            .with_context(|| format!("reading {}", admin.config.display()))?;
        rowbit_web::config::ServerConfig::agent_users_host(&src)?
    } else {
        None
    };
    let host = match (admin.agent_host.as_ref().or(from_file.as_ref()), &admin.url) {
        (Some(host), _) => host.clone(),
        (None, Some(url)) => {
            let mut conn = connect(url)
                .await
                .context("connecting with ROWBIT_DATABASE_URL to find Rowbit's user")?;
            let me: Option<String> = conn.query_first("SELECT CURRENT_USER()").await?;
            let _ = conn.disconnect().await;
            Account::parse_current(&me.unwrap_or_default())
                .context("the server did not say who Rowbit's user is")?
                .host
        }
        (None, None) => anyhow::bail!(
            "set ROWBIT_DATABASE_URL (agents' users get the host part of Rowbit's user) or pass --agent-host"
        ),
    };
    anyhow::ensure!(
        !host.is_empty() && !host.contains('\\') && !host.contains('\''),
        "the host part {host:?} is empty or has a quote or backslash, which Rowbit refuses"
    );
    Ok(host)
}

async fn plans(
    admin: &Admin,
    conn: &mut Conn,
    store: Option<&Store>,
) -> anyhow::Result<(Vec<Plan>, Vec<String>)> {
    let src = std::fs::read_to_string(&admin.policy)
        .with_context(|| format!("reading {}", admin.policy.display()))?;
    let policy = PolicyFile::from_yaml(&src)?;
    let host = agent_host(admin).await?;
    let mut plans = Vec::new();
    let mut unscoped = Vec::new();
    for (agent, p) in &policy.agents {
        let Some(scopes) = &p.scopes else {
            unscoped.push(agent.clone());
            continue;
        };
        let account = Account {
            user: agent_user(agent),
            host: host.clone(),
        };
        let catalog = Catalog::read(conn, &grants::databases(scopes)).await?;
        let compiled = grants::compile(scopes, &catalog);
        let exists = grants::exists(conn, &account).await?;
        let current = if exists {
            grants::current(conn, &account).await?
        } else {
            Current::default()
        };
        let diff = Diff::between(&compiled.grants, &current.grants);
        let has_password = match store {
            Some(s) => s.setting(&password_key(agent))?.is_some(),
            None => false,
        };
        plans.push(Plan {
            agent: agent.clone(),
            account,
            exists,
            has_password,
            compiled,
            current,
            diff,
        });
    }
    Ok((plans, unscoped))
}

fn print_unscoped(unscoped: &[String]) {
    for agent in unscoped {
        println!("{agent}  no scopes: uses Rowbit's own database user");
    }
}

pub async fn diff(admin: Admin) -> anyhow::Result<()> {
    let store = if admin.store.exists() {
        Some(
            Store::open(&admin.store)
                .with_context(|| format!("opening {}", admin.store.display()))?,
        )
    } else {
        None
    };
    let mut conn = connect(&admin.admin_url)
        .await
        .context("connecting as the administrator")?;
    let (plans, unscoped) = plans(&admin, &mut conn, store.as_ref()).await?;
    let _ = conn.disconnect().await;
    let mut out = std::io::stdout().lock();
    for plan in &plans {
        plan.print(false, &mut out)?;
    }
    let errors: usize = plans.iter().map(|p| p.compiled.errors.len()).sum();
    drop(out);
    print_unscoped(&unscoped);
    if plans.is_empty() {
        println!("No agent in {} has scopes.", admin.policy.display());
    }
    anyhow::ensure!(
        errors == 0,
        "{} has {errors} error(s) above",
        admin.policy.display()
    );
    Ok(())
}

pub async fn apply(admin: Admin, args: ApplyArgs) -> anyhow::Result<()> {
    let mut store =
        Store::open(&admin.store).with_context(|| format!("opening {}", admin.store.display()))?;
    let mut conn = connect(&admin.admin_url)
        .await
        .context("connecting as the administrator")?;
    let (plans, unscoped) = plans(&admin, &mut conn, Some(&store)).await?;

    let scoped: BTreeSet<&str> = plans.iter().map(|p| p.agent.as_str()).collect();
    let mut removed = Vec::new();
    if args.drop_removed {
        for (key, value) in store.settings_with_prefix("agent.")? {
            let Some(agent) = key
                .strip_prefix("agent.")
                .and_then(|k| k.strip_suffix(".account"))
            else {
                continue;
            };
            if !scoped.contains(agent) {
                let account = Account::parse_current(&value)
                    .with_context(|| format!("{key} in the state file is not user@host"))?;
                removed.push((agent.to_owned(), account));
            }
        }
    }

    let mut out = std::io::stdout().lock();
    for plan in &plans {
        plan.print(args.rotate_passwords, &mut out)?;
    }
    for (agent, account) in &removed {
        writeln!(
            out,
            "{agent}  {account}\n  drop the user (no longer scoped in rowbit.yml)"
        )?;
    }
    drop(out);
    print_unscoped(&unscoped);

    // A deny entry that matches nothing would grant what it meant to deny.
    let errors: usize = plans.iter().map(|p| p.compiled.errors.len()).sum();
    anyhow::ensure!(
        errors == 0,
        "{} has {errors} error(s) above; nothing changed",
        admin.policy.display()
    );
    let work = plans.iter().any(|p| !p.is_empty(args.rotate_passwords)) || !removed.is_empty();
    if work && !args.yes && !confirm()? {
        anyhow::bail!("nothing changed");
    }

    for plan in &plans {
        if plan.needs_password(args.rotate_passwords) {
            let password = new_password();
            // Kept before the server has it: if the statement fails, a
            // rerun with --rotate-passwords sets a fresh one, whereas a
            // password the server has but Rowbit lost locks Rowbit out.
            store.set_setting(&password_key(&plan.agent), &password)?;
            store.set_setting(&account_key(&plan.agent), &plan.account.to_string())?;
            if plan.exists {
                grants::set_password(&mut conn, &plan.account, &password).await?;
            } else {
                grants::create_user(&mut conn, &plan.account, &password).await?;
            }
        }
        for statement in plan.diff.statements(&plan.account) {
            conn.query_drop(&statement)
                .await
                .with_context(|| format!("running {statement}"))?;
        }
    }
    for (agent, account) in &removed {
        grants::drop_user(&mut conn, account).await?;
        store.delete_setting(&password_key(agent))?;
        store.delete_setting(&account_key(agent))?;
    }
    let _ = conn.disconnect().await;

    let unmanaged: usize = plans.iter().map(|p| p.current.unmanaged.len()).sum();
    anyhow::ensure!(
        unmanaged == 0,
        "{unmanaged} grant(s) above can't be fixed by Rowbit; remove them by hand, \
         or Rowbit won't serve those agents"
    );
    if work {
        eprintln!(
            "rowbit: agents' database users are in line with {}",
            admin.policy.display()
        );
    }
    Ok(())
}

fn confirm() -> anyhow::Result<bool> {
    let stdin = std::io::stdin();
    anyhow::ensure!(
        stdin.is_terminal(),
        "not asking without a terminal; review `rowbit grants diff` and pass --yes"
    );
    eprint!("Run these? [y/N] ");
    let mut answer = String::new();
    stdin.lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// 32 random bytes as hex: no character needs quoting anywhere.
fn new_password() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the system has a random source");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
