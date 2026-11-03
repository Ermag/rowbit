//! `rowbit setup`: Rowbit in front of a database of your own, or the
//! sample one. See "rowbit setup" in docs/spec/setup.md.

use std::collections::BTreeSet;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use rowbit_mysql::grants::{self, Account, Grant, Object};

use crate::connect::{self, Client, Env, Scope};
use crate::place;

/// What Rowbit's own user needs on each database, as in docs/privileges.md.
const NEEDED: [&str; 6] = [
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE TEMPORARY TABLES",
    "TRIGGER",
];

/// `rowbit setup`'s arguments: one flag per question.
#[derive(clap::Args)]
pub struct SetupArgs {
    /// The setup's name; its files go in ~/.rowbit/<name>
    #[arg(long, default_value = "default")]
    name: String,
    /// Try Rowbit on the sample shop database instead (runs `rowbit demo`)
    #[arg(long, conflicts_with = "database_url")]
    sample: bool,
    /// Your database: mysql://user:password@host:port/database
    #[arg(long)]
    database_url: Option<String>,
    /// An agent client to connect; repeat for more
    #[arg(long = "client", value_enum)]
    clients: Vec<Client>,
    /// Connect no agent client
    #[arg(long, conflicts_with = "clients")]
    no_client: bool,
    /// Claude Code: local (this directory) or user (everywhere)
    #[arg(long, value_enum)]
    scope: Option<Scope>,
    /// Install a service that runs Rowbit while you're logged in
    #[arg(long, conflicts_with = "no_service")]
    service: bool,
    /// Don't install a service
    #[arg(long)]
    no_service: bool,
    /// Take the default answer to every question not given by a flag
    #[arg(long)]
    yes: bool,
}

/// Questions on the terminal, or their defaults with `--yes`.
struct Ask {
    asking: bool,
}

impl Ask {
    fn line(&self, question: &str, default: &str) -> anyhow::Result<String> {
        if !self.asking {
            return Ok(default.to_owned());
        }
        if default.is_empty() {
            print!("{question} ");
        } else {
            print!("{question} [{default}] ");
        }
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        let answer = answer.trim();
        Ok(if answer.is_empty() { default } else { answer }.to_owned())
    }

    fn yes(&self, question: &str, default: bool) -> anyhow::Result<bool> {
        let answer = self.line(question, if default { "Y/n" } else { "y/N" })?;
        Ok(match answer.to_ascii_lowercase().as_str() {
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => default,
        })
    }

    /// Not echoed: it holds a password.
    fn secret(&self, question: &str) -> anyhow::Result<String> {
        anyhow::ensure!(self.asking, "no terminal to ask on: {question}");
        Ok(rpassword::prompt_password(format!("{question} "))?
            .trim()
            .to_owned())
    }
}

pub async fn run(a: SetupArgs) -> anyhow::Result<()> {
    let terminal = std::io::stdin().is_terminal();
    anyhow::ensure!(
        terminal || (a.yes && (a.sample || a.database_url.is_some())),
        "rowbit setup asks questions; without a terminal, pass --yes and --sample or --database-url <url>"
    );
    let ask = Ask {
        asking: terminal && !a.yes,
    };
    let sample = if a.sample {
        true
    } else if a.database_url.is_some() {
        false
    } else if ask.asking {
        say("Set Rowbit up for:");
        say("  1. Your database (MariaDB or MySQL)");
        say("  2. A sample shop database, to try it (needs Docker)");
        ask.line("Which?", "1")? == "2"
    } else {
        false
    };
    if sample {
        return run_demo(&a);
    }

    connect::check_name(&a.name)?;
    let dir = place::home()?.join(&a.name);
    let resuming = place::is_setup(&dir);
    if resuming {
        say(&format!(
            "Found the setup in {}; keeping its files.",
            dir.display()
        ));
    } else if dir.exists() {
        anyhow::ensure!(
            std::fs::read_dir(&dir)?.next().is_none(),
            "{} isn't empty and holds no setup; pick another --name",
            dir.display()
        );
    }
    make_private_dir(&dir)?;

    println!();
    let (url, found) = database(&a, &ask, &dir).await?;
    let database = crate::mysql_database(&url)?;

    println!();
    let clients = pick_clients(&a, &ask)?;
    let agents: Vec<String> = if clients.is_empty() {
        vec!["coding-agent".into()]
    } else {
        clients.iter().map(|c| c.id().to_owned()).collect()
    };

    crate::connect::write_private(
        &dir.join(place::ENV_FILE),
        &format!("ROWBIT_DATABASE_URL={url}\n"),
    )?;
    let policy = dir.join("rowbit.yml");
    if policy.exists() {
        let src = std::fs::read_to_string(&policy)?;
        let known = rowbit_policy::PolicyFile::from_yaml(&src)?;
        for agent in agents.iter().filter(|a| !known.agents.contains_key(*a)) {
            say(&format!(
                "{} has no agent {agent}; add it under agents: to connect it:\n\n  {agent}:\n    default: hold\n",
                policy.display()
            ));
        }
    } else {
        std::fs::write(&policy, starter_policy(&database, &found.tables, &agents))?;
        say(&format!(
            "Wrote {}: every write waits for a person until you add rules.",
            policy.display()
        ));
    }
    let server = dir.join("rowbit-server.yml");
    if !server.exists() {
        let listen = free_port()?;
        std::fs::write(
            &server,
            format!("version: 0\nlisten: {listen}\nmcp:\n  http: true\n"),
        )?;
    }
    let address = crate::ServerArgs {
        config: server.clone(),
    }
    .address()?
    .base();
    let store_path = dir.join("rowbit.db");
    let password = {
        let mut store = rowbit_store::Store::open(&store_path)?;
        match store.setting("web.password")? {
            Some(_) => None,
            None => {
                let password = rowbit_web::new_password();
                store.set_setting("web.password", &rowbit_web::hash_password(&password))?;
                Some(password)
            }
        }
    };

    let cwd = std::env::current_dir()?;
    let mut connected = Vec::new();
    for client in &clients {
        let scope = if *client == Client::ClaudeCode {
            claude_scope(&a, &ask, &cwd)?
        } else {
            Scope::Local
        };
        let args = connect::ConnectArgs::for_setup(&dir, &cwd, *client, client.id(), scope, &url);
        match connect::run(args) {
            Ok(Some(c)) => {
                let mut env = Env::current()?;
                env.cwd = cwd.clone();
                if !connect::exists(c.client, &env, &c.name).unwrap_or(false) {
                    say(&format!(
                        "{} doesn't show {} yet; once Rowbit runs, `rowbit connect {} --check --dir {}` says why.",
                        c.client.label(),
                        c.name,
                        c.client.id(),
                        dir.display()
                    ));
                }
                connected.push((c.client, scope));
            }
            Ok(None) => {}
            Err(e) => say(&format!(
                "Couldn't connect {}: {e:#}\nLater: rowbit connect {} --dir {}",
                client.label(),
                client.id(),
                dir.display()
            )),
        }
    }
    if connected.iter().any(|(c, _)| *c == Client::ClaudeCode) {
        println!();
        say("In Claude Code, add the skill that teaches the agent to use Rowbit well:");
        say("  /plugin marketplace add Ermag/rowbit");
        say("  /plugin install rowbit@rowbit");
    }

    println!();
    let serve = format!("rowbit serve --dir {}", tilde(&dir));
    let service = if a.service {
        true
    } else if a.no_service {
        false
    } else {
        ask.yes(
            "Install a service that runs Rowbit while you're logged in?",
            false,
        )?
    };
    let start = if service {
        Some(install_service(&a.name, &dir)?)
    } else {
        None
    };

    println!();
    say(&format!("Rowbit is set up in {}.", tilde(&dir)));
    match &start {
        Some(start) => say(&format!("Start it: {start}")),
        None => say(&format!("Start it: {serve}")),
    }
    match &password {
        Some(p) => say(&format!(
            "Review page: {address}    password: {p}    (shown this once)"
        )),
        None => say(&format!(
            "Review page: {address}    (password as set before)"
        )),
    }
    for (client, scope) in &connected {
        match (client, scope) {
            (Client::ClaudeCode, Scope::Local) => say(&format!(
                "Claude Code has Rowbit in {}; start or restart it there.",
                cwd.display()
            )),
            (c, _) => say(&format!(
                "{} has Rowbit; restart it if it's open.",
                c.label()
            )),
        }
    }
    match found.tables.first() {
        Some(table) if !connected.is_empty() => say(&format!(
            "Then ask your agent: \"Using Rowbit, how many rows does {table} have?\""
        )),
        _ if connected.is_empty() => say(&format!(
            "Connect an agent: rowbit connect <client> --dir {} (rowbit connect lists them).",
            tilde(&dir)
        )),
        _ => {}
    }
    say("Writes wait for you on the review page until you add rules to rowbit.yml.");
    if dir != place::home()?.join("default") {
        say(&format!(
            "Other commands find this setup with --dir {}.",
            tilde(&dir)
        ));
    }
    Ok(())
}

/// The sample: `rowbit demo`, as if typed.
fn run_demo(a: &SetupArgs) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new(exe()?);
    cmd.arg("demo");
    match (a.no_client, a.clients.as_slice()) {
        (true, _) => {
            cmd.arg("--no-connect");
        }
        (_, [client]) => {
            cmd.args(["--connect", client.id()]);
        }
        _ => {}
    }
    let status = cmd.status().context("running rowbit demo")?;
    anyhow::ensure!(status.success(), "rowbit demo stopped with {status}");
    Ok(())
}

fn exe() -> anyhow::Result<PathBuf> {
    std::env::current_exe().context("finding the rowbit binary")
}

fn make_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// What the database said when Rowbit connected.
struct Found {
    tables: Vec<String>,
}

/// Picks the database URL, checks it, and returns it with what was found.
async fn database(a: &SetupArgs, ask: &Ask, dir: &Path) -> anyhow::Result<(String, Found)> {
    let mut url = match first_url(a, dir)? {
        Some((url, source)) => {
            say(&format!("Database: {} (from {source})", mask(&url)));
            if ask.yes("Use it?", true)? {
                url
            } else {
                ask.secret("Database URL (mysql://user:password@host:port/database):")?
            }
        }
        None => {
            say(
                "Rowbit needs your database's URL. It isn't echoed and stays in a file only you can read.",
            );
            ask.secret("Database URL (mysql://user:password@host:port/database):")?
        }
    };
    loop {
        url = normalize(&url)?;
        crate::mysql_database(&url)?;
        let checked = match check(&url).await {
            Ok(checked) => checked,
            Err(e) if ask.asking => {
                say(&format!("Couldn't connect: {e:#}"));
                url = ask.secret("Database URL (mysql://user:password@host:port/database):")?;
                continue;
            }
            Err(e) => return Err(e),
        };
        let Some(suggested) = report(&url, &checked) else {
            return Ok((url, checked.found));
        };
        if !ask.asking {
            return Ok((url, checked.found));
        }
        if ask.yes(
            "Switch to that user now? Run the statements above first.",
            false,
        )? {
            url = suggested;
            continue;
        }
        say(
            "Carrying on with this user. Rowbit can do whatever it can, and so can an agent without scopes.",
        );
        return Ok((url, checked.found));
    }
}

/// The first URL on offer, and where it came from.
fn first_url(a: &SetupArgs, dir: &Path) -> anyhow::Result<Option<(String, String)>> {
    if let Some(url) = &a.database_url {
        return Ok(Some((url.clone(), "--database-url".into())));
    }
    if let Some(url) = place::read_env_file(&dir.join(place::ENV_FILE))? {
        return Ok(Some((url, "this setup".into())));
    }
    for var in ["ROWBIT_DATABASE_URL", "DATABASE_URL"] {
        if let Ok(url) = std::env::var(var)
            && !url.is_empty()
        {
            return Ok(Some((url, var.into())));
        }
    }
    if let Ok(src) = std::fs::read_to_string(".env")
        && let Some(url) = place::env_value(&src, "DATABASE_URL")
    {
        return Ok(Some((url, "DATABASE_URL in .env".into())));
    }
    Ok(None)
}

/// `mysql://`, with `mariadb://` taken as the same; refuses the rest.
fn normalize(url: &str) -> anyhow::Result<String> {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("mariadb://") {
        return Ok(format!("mysql://{rest}"));
    }
    if url.starts_with("mysql://") {
        return Ok(url.to_owned());
    }
    let scheme = url.split_once("://").map(|(s, _)| s).unwrap_or("");
    anyhow::ensure!(
        !scheme.starts_with("postgres"),
        "Rowbit works with MariaDB and MySQL for now; PostgreSQL is planned"
    );
    anyhow::bail!("give a mysql:// URL, as in mysql://user:password@host:3306/app")
}

/// The URL with its password replaced by `****`.
fn mask(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let Some((userinfo, host)) = rest.rsplit_once('@') else {
        return url.to_owned();
    };
    match userinfo.split_once(':') {
        Some((user, _)) => format!("{scheme}://{user}:****@{host}"),
        None => url.to_owned(),
    }
}

/// The URL with other credentials.
fn with_user(url: &str, user: &str, password: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("mysql", url));
    let host = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
    format!("{scheme}://{user}:{password}@{host}")
}

struct Checked {
    found: Found,
    account: Account,
    database: String,
    missing: Vec<String>,
    too_much: Vec<String>,
}

/// Connects, says what answered, and compares the user's grants with
/// what Rowbit needs. Changes nothing.
async fn check(url: &str) -> anyhow::Result<Checked> {
    let database = crate::mysql_database(url)?;
    let opts = Opts::from_url(url).context("the database URL isn't valid")?;
    let mut conn = Conn::new(opts)
        .await
        .with_context(|| format!("connecting to {}", mask(url)))?;
    let version: String = conn
        .query_first("SELECT VERSION()")
        .await?
        .unwrap_or_default();
    let current: String = conn
        .query_first("SELECT CURRENT_USER()")
        .await?
        .unwrap_or_default();
    let account = Account::parse_current(&current).context("reading the current user")?;
    let tables: Vec<String> = conn
        .exec(
            "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME",
            (&database,),
        )
        .await?;
    let have = grants::current(&mut conn, &account).await?;
    let _ = conn.disconnect().await;
    let engine = if version.contains("MariaDB") {
        "MariaDB"
    } else {
        "MySQL"
    };
    let version = version.split('-').next().unwrap_or(&version);
    say(&format!(
        "Connected to {engine} {version} as {account}: database {database}, {} table{}.",
        tables.len(),
        if tables.len() == 1 { "" } else { "s" }
    ));
    let (missing, too_much) = compare(&have.grants, &database);
    Ok(Checked {
        found: Found { tables },
        account,
        database,
        missing,
        too_much,
    })
}

/// What's missing from Rowbit's grants on `database`, and what's more than
/// it should hold.
fn compare(have: &BTreeSet<Grant>, database: &str) -> (Vec<String>, Vec<String>) {
    let escaped = grants::escape_database(database);
    let holds = |privilege: &str, object: &Object| {
        have.iter()
            .any(|g| g.column.is_none() && g.privilege == privilege && &g.object == object)
    };
    let missing = NEEDED
        .iter()
        .filter(|p| {
            !holds(p, &Object::Global)
                && !holds(p, &Object::Database(escaped.clone()))
                && !holds(p, &Object::Database(database.to_owned()))
        })
        .map(|p| p.to_string())
        .chain((!holds("PROCESS", &Object::Global)).then(|| "PROCESS".to_owned()))
        .collect();
    let mut too_much = BTreeSet::new();
    for g in have {
        let on_mysql =
            matches!(&g.object, Object::Database(d) | Object::Table(d, _) if d == "mysql");
        if g.privilege == "GRANT OPTION" {
            too_much.insert("GRANT OPTION".to_owned());
        } else if on_mysql {
            too_much.insert("privileges on mysql.*".to_owned());
        } else if g.object == Object::Global && g.privilege != "PROCESS" {
            too_much.insert(format!("{} on *.*", g.privilege));
        }
    }
    (missing, too_much.into_iter().collect())
}

/// Prints what the grant check found. Returns the URL of a user of
/// Rowbit's own to switch to, when it suggests one.
fn report(url: &str, c: &Checked) -> Option<String> {
    let to = c.account.sql();
    let db = grants::quote_ident(&grants::escape_database(&c.database));
    let grant_lines = |to: &str, privileges: &[String]| {
        let (global, per_db): (Vec<&String>, Vec<&String>) =
            privileges.iter().partition(|p| *p == "PROCESS");
        let mut out = Vec::new();
        if !per_db.is_empty() {
            let list: Vec<&str> = per_db.iter().map(|p| p.as_str()).collect();
            out.push(format!("GRANT {} ON {db}.* TO {to};", list.join(", ")));
        }
        if !global.is_empty() {
            out.push(format!("GRANT PROCESS ON *.* TO {to};"));
        }
        out
    };
    if !c.missing.is_empty() {
        say(&format!(
            "{} lacks what Rowbit needs (see docs/privileges.md). An administrator can add it with:",
            c.account
        ));
        for line in grant_lines(&to, &c.missing) {
            say(&format!("  {line}"));
        }
    }
    if !c.too_much.is_empty() {
        let shown: Vec<&str> = c.too_much.iter().take(5).map(String::as_str).collect();
        let more = match c.too_much.len().saturating_sub(5) {
            0 => String::new(),
            n => format!(" and {n} more"),
        };
        say(&format!(
            "{} also holds more than Rowbit needs: {}{more}.",
            c.account,
            shown.join(", ")
        ));
    }
    if c.account.user == "rowbit" {
        if c.missing.is_empty() && c.too_much.is_empty() {
            say("Rowbit's grants are as they should be.");
        }
        return None;
    }
    let rowbit = Account {
        user: "rowbit".into(),
        host: c.account.host.clone(),
    };
    let password = rowbit_web::new_password();
    println!();
    say(&format!(
        "Rowbit should connect as a user of its own, used for nothing else, not as {}. An administrator can make one with:",
        c.account
    ));
    say(&format!(
        "  CREATE USER {} IDENTIFIED BY {};",
        rowbit.sql(),
        grants::quote_string(&password)
    ));
    let all: Vec<String> = NEEDED
        .iter()
        .map(|p| p.to_string())
        .chain(["PROCESS".to_owned()])
        .collect();
    for line in grant_lines(&rowbit.sql(), &all) {
        say(&format!("  {line}"));
    }
    Some(with_user(url, "rowbit", &password))
}

/// Which clients to connect: the flags, or a pick with the installed ones
/// checked.
fn pick_clients(a: &SetupArgs, ask: &Ask) -> anyhow::Result<Vec<Client>> {
    if a.no_client {
        return Ok(Vec::new());
    }
    if !a.clients.is_empty() || !ask.asking {
        return Ok(a.clients.clone());
    }
    let env = Env::current()?;
    let installed: Vec<bool> = Client::ALL.iter().map(|c| c.installed(&env)).collect();
    say("Agent clients to connect (installed ones checked):");
    for (i, c) in Client::ALL.iter().enumerate() {
        let mark = if installed[i] { "x" } else { " " };
        say(&format!("  [{mark}] {}. {}", i + 1, c.label()));
    }
    let checked: Vec<String> = installed
        .iter()
        .enumerate()
        .filter(|(_, on)| **on)
        .map(|(i, _)| (i + 1).to_string())
        .collect();
    let default = if checked.is_empty() {
        "n".to_owned()
    } else {
        checked.join(",")
    };
    let answer = ask.line("Numbers, comma-separated, or n for none:", &default)?;
    Ok(choose(&answer))
}

fn choose(answer: &str) -> Vec<Client> {
    if answer.eq_ignore_ascii_case("n") || answer.eq_ignore_ascii_case("none") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for part in answer.split([',', ' ']).filter(|p| !p.is_empty()) {
        let client = part
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| Client::ALL.get(i).copied())
            .or_else(|| Client::ALL.into_iter().find(|c| c.id() == part));
        if let Some(c) = client
            && !out.contains(&c)
        {
            out.push(c);
        }
    }
    out
}

/// Claude Code's scope: this directory in a git repository, else
/// everywhere, unless asked.
fn claude_scope(a: &SetupArgs, ask: &Ask, cwd: &Path) -> anyhow::Result<Scope> {
    if let Some(scope) = a.scope {
        return Ok(scope);
    }
    let default = if cwd.join(".git").exists() { "1" } else { "2" };
    if ask.asking {
        say("Claude Code: use Rowbit");
        say(&format!("  1. in {} only", cwd.display()));
        say("  2. everywhere");
    }
    Ok(match ask.line("Which?", default)?.as_str() {
        "1" => Scope::Local,
        _ => Scope::User,
    })
}

/// `rowbit.yml` with one agent per client, each holding every write.
fn starter_policy(database: &str, tables: &[String], agents: &[String]) -> String {
    let mut out = String::from(
        "# Which agents' writes go through, wait for a person, or are refused.\n\
         # Rules and scopes: https://ermag.github.io/rowbit/docs/spec/policy.html\n\
         version: 0\n\
         agents:\n",
    );
    for agent in agents {
        out.push_str(&format!(
            "  {agent}:\n    # Every write waits for a person until a rule allows it.\n    default: hold\n"
        ));
    }
    if !tables.is_empty() {
        out.push_str(&format!(
            "\n# Tables in {database}{}:\n",
            if tables.len() > 50 {
                " (the first 50)"
            } else {
                ""
            }
        ));
        for t in tables.iter().take(50) {
            out.push_str(&format!("#   {database}.{t}\n"));
        }
    }
    out
}

/// The review page's address: 8080, or the next free port up to 8099.
fn free_port() -> anyhow::Result<std::net::SocketAddr> {
    (8080..=8099)
        .map(|port| std::net::SocketAddr::from(([127, 0, 0, 1], port)))
        .find(|a| std::net::TcpListener::bind(a).is_ok())
        .context("ports 8080 to 8099 are all taken; set listen in rowbit-server.yml")
}

/// Writes a launchd agent or a systemd user unit, and returns the command
/// that starts it. Runs nothing.
fn install_service(name: &str, dir: &Path) -> anyhow::Result<String> {
    let exe = exe()?;
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME isn't set")?);
    if cfg!(target_os = "macos") {
        let label = format!("dev.rowbit.{name}");
        let file = home.join(format!("Library/LaunchAgents/{label}.plist"));
        let x = |p: &Path| xml_escape(&p.display().to_string());
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>serve</string><string>--dir</string><string>{dir}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            exe = x(&exe),
            dir = x(dir),
            log = x(&dir.join("serve.log")),
        );
        write_new(&file, &plist)?;
        say(&format!("Wrote {}.", file.display()));
        Ok(format!(
            "launchctl bootstrap gui/$(id -u) {}",
            shell_quote(&file)
        ))
    } else {
        let unit = format!("rowbit-{name}");
        let file = home.join(format!(".config/systemd/user/{unit}.service"));
        let q = |p: &Path| format!("\"{}\"", p.display().to_string().replace('"', "\\\""));
        let body = format!(
            "[Unit]\nDescription=Rowbit ({name})\n\n[Service]\nExecStart={} serve --dir {}\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
            q(&exe),
            q(dir)
        );
        write_new(&file, &body)?;
        say(&format!("Wrote {}.", file.display()));
        Ok(format!(
            "systemctl --user daemon-reload && systemctl --user enable --now {unit}"
        ))
    }
}

/// `dir` with the home directory shown as `~`.
fn tilde(dir: &Path) -> String {
    match std::env::var_os("HOME").and_then(|h| dir.strip_prefix(h).ok().map(Path::to_owned)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    }
}

fn write_new(file: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(file, contents).with_context(|| format!("writing {}", file.display()))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', r"'\''"))
}

fn say(line: &str) {
    println!("{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(object: Object, privilege: &str) -> Grant {
        Grant {
            object,
            privilege: privilege.into(),
            column: None,
        }
    }

    #[test]
    fn urls_are_masked_and_rewritten() {
        assert_eq!(
            mask("mysql://app:s3cr@t@db:3306/shop"),
            "mysql://app:****@db:3306/shop"
        );
        assert_eq!(mask("mysql://app@db/shop"), "mysql://app@db/shop");
        assert_eq!(
            with_user("mysql://app:x@db:3306/shop", "rowbit", "pw"),
            "mysql://rowbit:pw@db:3306/shop"
        );
        assert_eq!(normalize("mariadb://a@h/d").unwrap(), "mysql://a@h/d");
        assert!(
            normalize("postgres://a@h/d")
                .unwrap_err()
                .to_string()
                .contains("PostgreSQL is planned")
        );
    }

    #[test]
    fn grants_are_compared_with_what_rowbit_needs() {
        let mut have: BTreeSet<Grant> = NEEDED
            .iter()
            .map(|p| g(Object::Database("shop".into()), p))
            .collect();
        have.insert(g(Object::Global, "PROCESS"));
        assert_eq!(compare(&have, "shop"), (vec![], vec![]));

        let have: BTreeSet<Grant> = [
            g(Object::Database("shop".into()), "SELECT"),
            g(Object::Global, "SUPER"),
            g(Object::Database("shop".into()), "GRANT OPTION"),
        ]
        .into();
        let (missing, too_much) = compare(&have, "shop");
        assert_eq!(
            missing,
            [
                "INSERT",
                "UPDATE",
                "DELETE",
                "CREATE TEMPORARY TABLES",
                "TRIGGER",
                "PROCESS"
            ]
        );
        assert_eq!(too_much, ["GRANT OPTION", "SUPER on *.*"]);

        // Global privileges cover every database.
        let have: BTreeSet<Grant> = NEEDED
            .iter()
            .chain(&["PROCESS"])
            .map(|p| g(Object::Global, p))
            .collect();
        let (missing, too_much) = compare(&have, "my_db");
        assert!(missing.is_empty());
        assert_eq!(too_much.len(), 6);
    }

    #[test]
    fn the_starter_policy_parses_and_holds_everything() {
        let tables: Vec<String> = (0..60).map(|i| format!("t{i}")).collect();
        let src = starter_policy("shop", &tables, &["claude-code".into(), "cursor".into()]);
        let policy = rowbit_policy::PolicyFile::from_yaml(&src).unwrap();
        assert_eq!(policy.agents.len(), 2);
        assert!(
            policy
                .agents
                .values()
                .all(|a| a.default == rowbit_policy::Decision::Hold && a.rules.is_empty())
        );
        assert!(src.contains("(the first 50)") && !src.contains("t50"));
    }

    #[test]
    fn clients_are_picked_by_number_or_id() {
        assert_eq!(choose("n"), vec![]);
        assert_eq!(
            choose("1, cursor,1"),
            vec![Client::ClaudeCode, Client::Cursor]
        );
        assert_eq!(choose("99"), vec![]);
    }
}
