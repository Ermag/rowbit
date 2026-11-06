//! `rowbit setup`: Rowbit in front of a database of your own, or the
//! sample one. See "rowbit setup" in docs/spec/setup.md.

use std::collections::BTreeSet;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use mysql_async::Conn;
use mysql_async::prelude::Queryable;
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
        terminal || a.yes,
        "rowbit setup asks questions; without a terminal, pass --yes, with --sample or the URL in ROWBIT_DATABASE_URL"
    );
    anyhow::ensure!(
        !(a.sample && a.clients.len() > 1),
        "the sample connects one client; pick one with --client"
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
    let resuming = place::has_rowbit_files(&dir);
    if resuming {
        say(&format!(
            "Found the setup in {}; keeping its files.",
            dir.display()
        ));
    } else if dir.exists() {
        anyhow::ensure!(
            std::fs::read_dir(&dir)?.next().is_none(),
            "{} isn't empty and holds none of Rowbit's files; pick another --name",
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

    // Read what's there before writing anything, so a broken rowbit.yml
    // stops setup with nothing changed.
    let policy = dir.join("rowbit.yml");
    let existing = match std::fs::read_to_string(&policy) {
        Ok(src) => {
            rowbit_policy::PolicyFile::from_yaml(&src)
                .with_context(|| format!("reading {}", policy.display()))?;
            Some(src)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", policy.display())),
    };
    crate::connect::write_private(
        &dir.join(place::ENV_FILE),
        &format!("ROWBIT_DATABASE_URL={url}\n"),
    )?;
    let mut unknown = Vec::new();
    match existing {
        Some(mut src) => {
            for agent in &agents {
                let known = rowbit_policy::PolicyFile::from_yaml(&src)?;
                if known.agents.contains_key(agent) || clients.is_empty() {
                    continue;
                }
                match add_agent(&src, agent) {
                    Some(new) => {
                        src = new;
                        say(&format!(
                            "Added agent {agent} to {}, holding every write.",
                            tilde(&policy)
                        ));
                    }
                    None => {
                        say(&format!(
                            "Couldn't add agent {agent} to {}; add it under agents: and run setup again:\n\n  {agent}:\n    default: hold\n",
                            tilde(&policy)
                        ));
                        unknown.push(agent.clone());
                    }
                }
            }
            std::fs::write(&policy, src)?;
        }
        None => {
            std::fs::write(&policy, starter_policy(&database, &found.tables, &agents))?;
            say(&format!(
                "Wrote {}: every write waits for a person until you add rules.",
                tilde(&policy)
            ));
        }
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
    for client in clients
        .iter()
        .filter(|c| !unknown.contains(&c.id().to_owned()))
    {
        let mut env = Env::current()?;
        env.cwd = cwd.clone();
        if connect::exists(*client, &env, "rowbit").unwrap_or(false) {
            say(&format!(
                "{} already has Rowbit; leaving it as it is.",
                client.label()
            ));
            continue;
        }
        let scope = if *client == Client::ClaudeCode {
            claude_scope(&a, &ask, &cwd)?
        } else {
            Scope::Local
        };
        let args = connect::ConnectArgs::for_setup(&dir, &cwd, *client, client.id(), scope, &url);
        match connect::run(args) {
            Ok(Some(c)) => {
                if !connect::exists(c.client, &env, &c.name).unwrap_or(false) {
                    say(&format!(
                        "{} doesn't show {} yet; once Rowbit runs, `rowbit connect {} --check --dir {}` says why.",
                        c.client.label(),
                        c.name,
                        c.client.id(),
                        quoted(&dir)
                    ));
                }
                connected.push((c.client, scope));
            }
            Ok(None) => {}
            Err(e) => say(&format!(
                "Couldn't connect {}: {e:#}\nLater, in {}: rowbit connect {} --agent {} --dir {}",
                client.label(),
                quoted(&cwd),
                client.id(),
                client.id(),
                quoted(&dir)
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
    let serve = format!("rowbit serve --dir {}", quoted(&dir));
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
            "Connect an agent: run rowbit setup again, or rowbit connect <client> --agent coding-agent --dir {} (rowbit connect lists them).",
            quoted(&dir)
        )),
        _ => {}
    }
    say("Writes wait for you on the review page until you add rules to rowbit.yml.");
    say(&format!(
        "Agents can read every table {} can. Limit what each one sees with scopes: https://ermag.github.io/rowbit/docs/spec/policy.html#scopes",
        found.account
    ));
    if dir != place::home()?.join("default") {
        say(&format!(
            "Other commands find this setup with --dir {}.",
            quoted(&dir)
        ));
    }
    if !found.own_user {
        println!();
        say(&format!(
            "Rowbit is connecting as {}, not a user of its own. Once an administrator has made one with the statements above, run rowbit setup again and switch to it, or set ROWBIT_DATABASE_URL to its URL and run rowbit setup.",
            found.account
        ));
    }
    anyhow::ensure!(
        found.missing.is_empty(),
        "set up, but not ready: {} lacks {}, so Rowbit refuses every write until an administrator grants it",
        found.account,
        found.missing.join(", ")
    );
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
    /// The user Rowbit connects as.
    account: String,
    /// Whether it's a user of Rowbit's own.
    own_user: bool,
    /// Privileges Rowbit needs that it lacks.
    missing: Vec<String>,
}

/// Picks the database URL, checks it, and returns it with what was found.
/// On a terminal, every mistake asks again rather than ending setup.
async fn database(a: &SetupArgs, ask: &Ask, dir: &Path) -> anyhow::Result<(String, Found)> {
    let mut url = match first_url(a, dir)? {
        Some((url, source)) if is_demo(&url) => {
            say(&format!(
                "{source} holds the demo's database ({}), not yours.",
                mask(&url)
            ));
            anyhow::ensure!(
                ask.asking,
                "unset ROWBIT_DATABASE_URL, or give your database's URL in it"
            );
            enter_url(ask, None)?
        }
        Some((url, source)) => {
            say(&format!("Database: {} (from {source})", mask(&url)));
            if ask.yes("Use it?", true)? {
                url
            } else {
                enter_url(ask, Some(&url))?
            }
        }
        None if ask.asking => enter_url(ask, None)?,
        None => anyhow::bail!(
            "no database URL: put it in ROWBIT_DATABASE_URL (kept out of your shell history and process list), or pass --database-url"
        ),
    };
    // The URL to fall back to if a switch to Rowbit's own user fails.
    let mut previous: Option<String> = None;
    // Once someone went back from Rowbit's own user, it isn't offered again.
    let mut went_back = false;
    loop {
        let checked = match prepare(&url) {
            Ok(u) => {
                url = u;
                check(&url).await
            }
            Err(e) => Err(e),
        };
        let checked = match checked {
            Ok(checked) => checked,
            Err(e) if !ask.asking => return Err(e),
            Err(e) => {
                say(&explain(&e, &url));
                if let Some(old) = &previous {
                    if ask.yes(
                        "Try again, once an administrator has run the statements?",
                        true,
                    )? {
                        continue;
                    }
                    say("Going back to the previous user.");
                    url = old.clone();
                    previous = None;
                    went_back = true;
                    continue;
                }
                url = enter_url(ask, Some(&url))?;
                continue;
            }
        };
        if went_back {
            say(
                "Carrying on with this user. Rowbit can do whatever it can, and so can an agent without scopes.",
            );
            return Ok((url, checked.found));
        }
        match report(&url, &checked, ask.asking) {
            Next::Done => return Ok((url, checked.found)),
            Next::Recheck => {
                if ask.yes("Check again, once an administrator has run them?", true)? {
                    continue;
                }
                return Ok((url, checked.found));
            }
            Next::Switch(suggested) => {
                if ask.yes(
                    &format!("Switch to that user now? Yes once an administrator has run the statements above; no carries on as {}.", checked.account),
                    false,
                )? {
                    previous = Some(url);
                    url = suggested;
                    continue;
                }
                say(
                    "Carrying on with this user. Rowbit can do whatever it can, and so can an agent without scopes.",
                );
                return Ok((url, checked.found));
            }
        }
    }
}

/// The demo's fixed credentials, so its URL left in the shell isn't taken
/// for yours.
fn is_demo(url: &str) -> bool {
    url.contains("://rowbit:rowbit-quickstart@")
}

/// Asks for the URL a piece at a time, with only the password hidden.
/// Asks for the URL a piece at a time, with only the password hidden. After
/// a failed try, `previous` fills in the answers given, so only the wrong
/// one needs typing again.
fn enter_url(ask: &Ask, previous: Option<&str>) -> anyhow::Result<String> {
    let prev = previous.and_then(|u| rowbit_mysql::url::opts(u).ok());
    let prev_tls = previous.map(|u| {
        if u.contains("ssl-mode=VERIFY_IDENTITY") {
            "v"
        } else if u.contains("ssl-mode=REQUIRED") {
            "r"
        } else {
            "n"
        }
    });
    say("Your database (MariaDB or MySQL); only the password is hidden:");
    let host = ask.line(
        "  Host:",
        prev.as_ref().map_or("127.0.0.1", |o| o.ip_or_hostname()),
    )?;
    let port = ask.line(
        "  Port:",
        &prev.as_ref().map_or(3306, |o| o.tcp_port()).to_string(),
    )?;
    let database = ask.line(
        "  Database:",
        prev.as_ref().and_then(|o| o.db_name()).unwrap_or(""),
    )?;
    let user = ask.line(
        "  User:",
        prev.as_ref().and_then(|o| o.user()).unwrap_or(""),
    )?;
    let mut password = ask.secret(if prev.is_some() {
        "  Password (Enter keeps the last one):"
    } else {
        "  Password:"
    })?;
    if password.is_empty()
        && let Some(o) = &prev
    {
        password = o.pass().unwrap_or("").to_owned();
    }
    let default = prev_tls.unwrap_or(if rowbit_mysql::url::is_local(&host) {
        "n"
    } else {
        "v"
    });
    let mut tls = match ask
        .line(
            "  TLS (n: none; r: encrypted, certificate not checked, open to interception; v: encrypted and the certificate checked):",
            default,
        )?
        .to_ascii_lowercase()
        .as_str()
    {
        "r" => "?ssl-mode=REQUIRED".to_owned(),
        "v" => "?ssl-mode=VERIFY_IDENTITY".to_owned(),
        _ => String::new(),
    };
    if tls.ends_with("VERIFY_IDENTITY") {
        let ca = ask.line(
            "  Your provider's CA file, if it gives you one (Enter for the public CAs):",
            "",
        )?;
        if !ca.is_empty() {
            let ca = match ca.strip_prefix("~/") {
                Some(rest) => {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest)
                }
                None => PathBuf::from(&ca),
            };
            let ca = std::path::absolute(&ca).unwrap_or(ca);
            tls.push_str(&format!("&ssl-ca={}", encode(&ca.display().to_string())));
        }
    }
    let userinfo = if password.is_empty() {
        encode(&user)
    } else {
        format!("{}:{}", encode(&user), encode(&password))
    };
    Ok(format!(
        "mysql://{userinfo}@{host}:{port}/{}{tls}",
        encode(&database)
    ))
}

/// Why a connection failed, in a line, with what to do for the usual
/// causes.
fn explain(e: &anyhow::Error, url: &str) -> String {
    let cause = e.root_cause().to_string();
    let hint = if cause.contains("UnknownIssuer") {
        " The server's certificate isn't signed by a CA Rowbit trusts. If your provider gives you a CA file, give it when asked (or add ssl-ca=<file> to the URL); r encrypts without checking the certificate."
    } else if cause.contains("NotValidForName") {
        " The server's certificate is for another name: connect by the host name it was issued for."
    } else if cause.contains("secure transport") {
        " The server only takes TLS connections: choose r or v."
    } else if cause.contains("Access denied") {
        " Check the user and password."
    } else {
        ""
    };
    format!("Couldn't connect to {}: {cause}.{hint}", mask(url))
}

/// Percent-encodes everything but the URL's unreserved characters.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The first URL on offer, and where it came from.
fn first_url(a: &SetupArgs, dir: &Path) -> anyhow::Result<Option<(String, String)>> {
    if let Some(url) = &a.database_url {
        return Ok(Some((url.clone(), "--database-url".into())));
    }
    // Set on purpose, it wins over the setup's own URL, so a re-run can
    // switch users; left from the demo, it doesn't.
    let shell = std::env::var("ROWBIT_DATABASE_URL")
        .ok()
        .filter(|u| !u.is_empty());
    if let Some(url) = shell.clone().filter(|u| !is_demo(u)) {
        return Ok(Some((url, "ROWBIT_DATABASE_URL".into())));
    }
    if let Some(url) = place::read_env_file(&dir.join(place::ENV_FILE))? {
        return Ok(Some((url, "this setup".into())));
    }
    if let Ok(url) = std::env::var("DATABASE_URL")
        && !url.is_empty()
    {
        return Ok(Some((url, "DATABASE_URL".into())));
    }
    if let Ok(src) = std::fs::read_to_string(".env")
        && let Some(url) = place::env_value(&src, "DATABASE_URL")
    {
        return Ok(Some((url, "DATABASE_URL in .env".into())));
    }
    // The demo's URL, offered only to say it isn't yours.
    Ok(shell.map(|url| (url, "ROWBIT_DATABASE_URL".into())))
}

/// The URL as Rowbit will use it, or why it can't.
fn prepare(url: &str) -> anyhow::Result<String> {
    let url = params(&normalize(url)?)?;
    crate::mysql_database(&url)?;
    Ok(url)
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

/// The URL parameters Rowbit's driver takes.
const DRIVER_PARAMS: [&str; 19] = [
    "abs_conn_ttl",
    "abs_conn_ttl_jitter",
    "client_found_rows",
    "compression",
    "conn_ttl",
    "inactive_connection_ttl",
    "max_allowed_packet",
    "pool_max",
    "pool_min",
    "prefer_socket",
    "reset_connection",
    "secure_auth",
    "socket",
    "stmt_cache_size",
    "tcp_keepalive",
    "tcp_nodelay",
    "ttl_check_interval",
    "wait_timeout",
    "enable_cleartext_plugin",
];

/// Drops the parameters other drivers use that Rowbit doesn't, saying so.
/// TLS parameters stay, in whichever client's spelling: the adapter reads
/// them, and refuses what it can't do.
fn params(url: &str) -> anyhow::Result<String> {
    let Some((base, query)) = url.split_once('?') else {
        return Ok(url.to_owned());
    };
    let mut kept = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let key = pair.split_once('=').map(|(k, _)| k).unwrap_or(pair);
        let lower = key.to_ascii_lowercase();
        let tls = lower.contains("ssl")
            || lower.contains("tls")
            || lower.starts_with("verify_")
            || lower == "built_in_roots";
        if tls || DRIVER_PARAMS.contains(&key) {
            kept.push(pair);
        } else {
            say(&format!(
                "Leaving out the URL parameter {key}, which Rowbit doesn't use."
            ));
        }
    }
    let url = if kept.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    rowbit_mysql::url::opts(&url).context("the database URL isn't valid")?;
    Ok(url)
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
    compared: Compared,
    /// Roles are active, and `information_schema` doesn't show what they grant.
    roles: bool,
}

/// Connects, says what answered, and compares the user's grants with
/// what Rowbit needs. Changes nothing.
async fn check(url: &str) -> anyhow::Result<Checked> {
    let database = crate::mysql_database(url)?;
    let opts = rowbit_mysql::url::opts(url).context("the database URL isn't valid")?;
    let opts_used = opts.clone();
    if opts.ssl_opts().is_none() && !rowbit_mysql::url::is_local(opts.ip_or_hostname()) {
        say(&format!(
            "The URL doesn't ask for TLS, so the password and data cross the network to {} in plain text. Add ssl-mode=VERIFY_IDENTITY if the server supports TLS.",
            opts.ip_or_hostname()
        ));
    }
    let mut conn = Conn::new(opts)
        .await
        .with_context(|| format!("Couldn't connect to {}", mask(url)))?;
    let version: String = conn
        .query_first("SELECT VERSION()")
        .await?
        .unwrap_or_default();
    let current: String = conn
        .query_first("SELECT CURRENT_USER()")
        .await?
        .unwrap_or_default();
    let role: Option<String> = conn
        .query_first::<Option<String>, _>("SELECT CURRENT_ROLE()")
        .await
        .ok()
        .flatten()
        .flatten();
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
        "Connected to {engine} {version} as {account} ({}): database {database}, {} table{}.",
        rowbit_mysql::url::describe(&opts_used),
        tables.len(),
        if tables.len() == 1 { "" } else { "s" }
    ));
    let compared = compare(&have.grants, &database);
    Ok(Checked {
        found: Found {
            tables,
            account: account.to_string(),
            own_user: account.user == "rowbit",
            missing: compared.missing.clone(),
        },
        account,
        compared,
        database,
        roles: role.is_some_and(|r| !r.eq_ignore_ascii_case("NONE")),
    })
}

/// The user's grants against what Rowbit needs on `database`.
#[derive(Debug, Default, PartialEq)]
struct Compared {
    missing: Vec<String>,
    /// More than Rowbit should hold, anywhere.
    too_much: Vec<String>,
    /// Schema changes on the database: Rowbit runs approved ones as this
    /// user, so keeping them is the operator's call.
    ddl: Vec<String>,
    /// Other databases the user reaches.
    elsewhere: Vec<String>,
}

const DDL: [&str; 4] = ["CREATE", "ALTER", "DROP", "INDEX"];

fn compare(have: &BTreeSet<Grant>, database: &str) -> Compared {
    let escaped = grants::escape_database(database);
    let here = |d: &str| d == database || d == escaped;
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
    let (mut too_much, mut ddl, mut elsewhere) =
        (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    for g in have {
        let db = match &g.object {
            Object::Database(d) | Object::Table(d, _) => Some(d.as_str()),
            Object::Global => None,
        };
        if g.privilege == "GRANT OPTION" {
            too_much.insert("GRANT OPTION".to_owned());
        } else if db == Some("mysql") {
            too_much.insert("privileges on mysql.*".to_owned());
        } else if db.is_none() {
            if g.privilege != "PROCESS" {
                too_much.insert(format!("{} on *.*", g.privilege));
            }
        } else if let Some(d) = db.filter(|d| !here(d)) {
            elsewhere.insert(d.replace("\\_", "_").replace("\\%", "%"));
        } else if DDL.contains(&g.privilege.as_str()) {
            ddl.insert(g.privilege.clone());
        } else if !NEEDED.contains(&g.privilege.as_str()) {
            too_much.insert(format!("{} on {}", g.privilege, g.object));
        }
    }
    Compared {
        missing,
        too_much: too_much.into_iter().collect(),
        ddl: ddl.into_iter().collect(),
        elsewhere: elsewhere.into_iter().collect(),
    }
}

/// What setup does after the grant check.
enum Next {
    Done,
    /// Grants are missing from Rowbit's own user; check again once added.
    Recheck,
    /// Switch to a user of Rowbit's own, at this URL.
    Switch(String),
}

/// A list of up to five, and how many more.
fn some_of(items: &[String]) -> String {
    let shown: Vec<&str> = items.iter().take(5).map(String::as_str).collect();
    match items.len().saturating_sub(5) {
        0 => shown.join(", "),
        n => format!("{} and {n} more", shown.join(", ")),
    }
}

/// The statements that give `to` the privileges, per database or global.
fn grant_lines(database: &str, to: &str, privileges: &[String]) -> Vec<String> {
    let db = grants::quote_ident(&grants::escape_database(database));
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
}

/// Prints what the grant check found, and what to do about it. `asking`
/// says whether a person can act on a new user's password now; without a
/// terminal the statement gets a placeholder instead.
fn report(url: &str, c: &Checked, asking: bool) -> Next {
    let r = &c.compared;
    let own = c.account.user == "rowbit";
    // Grants for an application's user would widen it; it gets a user of
    // Rowbit's own suggested instead, below.
    if own && !r.missing.is_empty() {
        say(&format!(
            "{} lacks what Rowbit needs (see docs/privileges.md). An administrator can add it with:",
            c.account
        ));
        for line in grant_lines(&c.database, &c.account.sql(), &r.missing) {
            say(&format!("  {line}"));
        }
    }
    if !r.too_much.is_empty() {
        say(&format!(
            "{} holds more than Rowbit needs: {}. Agents without scopes act with it too.",
            c.account,
            some_of(&r.too_much)
        ));
    }
    if !r.ddl.is_empty() {
        say(&format!(
            "{} can change the schema of {} ({}). Rowbit runs approved schema changes as this user; leave these out if migrations go through your own tool.",
            c.account,
            c.database,
            r.ddl.join(", ")
        ));
    }
    if !r.elsewhere.is_empty() {
        say(&format!(
            "{} reaches other databases too: {}. Rowbit, and agents without scopes, can read them.",
            c.account,
            some_of(&r.elsewhere)
        ));
    }
    if c.roles {
        say("Roles are active for this user; this check can't see what they grant.");
    }
    if c.account.user == "rowbit" {
        if r == &Compared::default() && !c.roles {
            say("Rowbit's grants are as they should be.");
        }
        return if r.missing.is_empty() || !asking {
            Next::Done
        } else {
            Next::Recheck
        };
    }
    let rowbit = Account {
        user: "rowbit".into(),
        host: c.account.host.clone(),
    };
    println!();
    say(&format!(
        "Rowbit should connect as a user of its own, used for nothing else, so it holds exactly what it needs and nothing your application relies on. Not {}. An administrator can make one with:",
        c.account
    ));
    let password = asking.then(rowbit_web::new_password);
    let shown = match &password {
        Some(p) => grants::quote_string(p),
        None => "'<a new password>'".to_owned(),
    };
    say(&format!(
        "  CREATE USER {} IDENTIFIED BY {shown};",
        rowbit.sql()
    ));
    let all: Vec<String> = NEEDED
        .iter()
        .map(|p| p.to_string())
        .chain(["PROCESS".to_owned()])
        .collect();
    for line in grant_lines(&c.database, &rowbit.sql(), &all) {
        say(&format!("  {line}"));
    }
    match password {
        Some(p) => Next::Switch(with_user(url, "rowbit", &p)),
        None => {
            say("Then run rowbit setup again with that user's URL in ROWBIT_DATABASE_URL.");
            Next::Done
        }
    }
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
    // Claude Desktop only starts servers itself, so its config would hold
    // the database URL: never checked by default.
    let installed: Vec<bool> = Client::ALL
        .iter()
        .map(|c| c.installed(&env) && !c.stdio_only())
        .collect();
    say("Agent clients to connect (installed ones checked):");
    for (i, c) in Client::ALL.iter().enumerate() {
        let mark = if installed[i] { "x" } else { " " };
        let note = if c.stdio_only() {
            " (puts the database URL in its own config)"
        } else {
            ""
        };
        say(&format!("  [{mark}] {}. {}{note}", i + 1, c.label()));
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
        for t in tables
            .iter()
            .filter(|t| !t.chars().any(char::is_control))
            .take(50)
        {
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
        write_service(&file, &plist)?;
        Ok(format!(
            "launchctl bootstrap gui/$(id -u) {}",
            shell_quote(&file)
        ))
    } else {
        let unit = format!("rowbit-{name}");
        let file = home.join(format!(".config/systemd/user/{unit}.service"));
        // In ExecStart, % starts a specifier and $ a variable.
        let q = |p: &Path| {
            let s = p
                .display()
                .to_string()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('%', "%%")
                .replace('$', "$$");
            format!("\"{s}\"")
        };
        let body = format!(
            "[Unit]\nDescription=Rowbit ({name})\n\n[Service]\nExecStart={} serve --dir {}\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
            q(&exe),
            q(dir)
        );
        write_service(&file, &body)?;
        Ok(format!(
            "systemctl --user daemon-reload && systemctl --user enable --now {unit}"
        ))
    }
}

/// `dir` to paste into a shell: `~/...` where that's safe, else quoted.
fn quoted(dir: &Path) -> String {
    let short = tilde(dir);
    let safe = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./~".contains(c))
    };
    if safe(&short) {
        short
    } else {
        connect::shell_quote(&dir.display().to_string())
    }
}

/// `rowbit.yml` with an agent that holds every write added under `agents:`,
/// indented as the agents already there are. None if the file isn't in a
/// shape Rowbit can edit safely.
fn add_agent(src: &str, agent: &str) -> Option<String> {
    let lines: Vec<&str> = src.lines().collect();
    let at = lines.iter().position(|l| l.trim_end() == "agents:")?;
    let indent = lines[at + 1..]
        .iter()
        .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| l.len() - l.trim_start().len())
        .filter(|n| *n > 0)
        .unwrap_or(2);
    let pad = " ".repeat(indent);
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    out.insert(
        at + 1,
        format!("{pad}{agent}:\n{pad}{pad}# Every write waits for a person until a rule allows it.\n{pad}{pad}default: hold"),
    );
    let new = out.join("\n") + "\n";
    let policy = rowbit_policy::PolicyFile::from_yaml(&new).ok()?;
    let agent_ok = policy
        .agents
        .get(agent)
        .is_some_and(|a| a.default == rowbit_policy::Decision::Hold && a.rules.is_empty());
    let before = rowbit_policy::PolicyFile::from_yaml(src).ok()?;
    (agent_ok && policy.agents.len() == before.agents.len() + 1).then_some(new)
}

/// `dir` with the home directory shown as `~`.
fn tilde(dir: &Path) -> String {
    match std::env::var_os("HOME").and_then(|h| dir.strip_prefix(h).ok().map(Path::to_owned)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    }
}

/// Writes a service file, saying so if it replaced one.
fn write_service(file: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let replaced = file.exists();
    std::fs::write(file, contents).with_context(|| format!("writing {}", file.display()))?;
    say(&format!(
        "{} {}.",
        if replaced { "Replaced" } else { "Wrote" },
        file.display()
    ));
    Ok(())
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

    fn db(name: &str) -> Object {
        Object::Database(name.into())
    }

    #[test]
    fn urls_are_masked_rewritten_and_cleaned() {
        assert_eq!(
            mask("mysql://app:s3cr@t@db:3306/shop"),
            "mysql://app:****@db:3306/shop"
        );
        assert_eq!(mask("mysql://app@db/shop"), "mysql://app@db/shop");
        let secret = rowbit_web::new_password();
        assert_eq!(
            with_user("mysql://app:x@db:3306/shop", "rowbit", &secret),
            format!("mysql://rowbit:{secret}@db:3306/shop")
        );
        assert_eq!(prepare("mariadb://a@h/d").unwrap(), "mysql://a@h/d");
        assert!(
            prepare("postgres://a@h/d")
                .unwrap_err()
                .to_string()
                .contains("PostgreSQL is planned")
        );
        assert!(
            prepare("mysql://a@h:3306")
                .unwrap_err()
                .to_string()
                .contains("must name a database")
        );
        assert_eq!(
            prepare("mysql://a@h/d?connection_limit=5&wait_timeout=60").unwrap(),
            "mysql://a@h/d?wait_timeout=60"
        );
        assert_eq!(
            prepare("mysql://a@h/d?charset=utf8").unwrap(),
            "mysql://a@h/d"
        );
        for tls in ["sslaccept=strict", "ssl-mode=REQUIRED", "require_ssl=true"] {
            let url = format!("mysql://a@h/d?{tls}");
            assert_eq!(prepare(&url).unwrap(), url);
        }
        assert!(prepare("mysql://a@h/d?ssl-cert=/c.pem").is_err());
        assert_eq!(encode("p@ss:w/rd é"), "p%40ss%3Aw%2Frd%20%C3%A9");
        assert!(is_demo(
            "mysql://rowbit:rowbit-quickstart@127.0.0.1:33070/shop"
        ));
    }

    #[test]
    fn grants_are_compared_with_what_rowbit_needs() {
        let mut have: BTreeSet<Grant> = NEEDED.iter().map(|p| g(db("shop"), p)).collect();
        have.insert(g(Object::Global, "PROCESS"));
        assert_eq!(compare(&have, "shop"), Compared::default());

        let have: BTreeSet<Grant> = [
            g(db("shop"), "SELECT"),
            g(Object::Global, "SUPER"),
            g(db("shop"), "GRANT OPTION"),
        ]
        .into();
        let c = compare(&have, "shop");
        assert_eq!(
            c.missing,
            [
                "INSERT",
                "UPDATE",
                "DELETE",
                "CREATE TEMPORARY TABLES",
                "TRIGGER",
                "PROCESS"
            ]
        );
        assert_eq!(c.too_much, ["GRANT OPTION", "SUPER on *.*"]);

        // ALL ON shop.*, as an application's user usually has it.
        let mut have: BTreeSet<Grant> = NEEDED
            .iter()
            .chain(&["DROP", "ALTER", "CREATE", "INDEX", "EVENT", "LOCK TABLES"])
            .map(|p| g(db("shop"), p))
            .collect();
        have.insert(g(Object::Global, "PROCESS"));
        have.insert(g(db("billing"), "SELECT"));
        let c = compare(&have, "shop");
        assert!(c.missing.is_empty());
        assert_eq!(c.ddl, ["ALTER", "CREATE", "DROP", "INDEX"]);
        assert_eq!(c.too_much, ["EVENT on shop.*", "LOCK TABLES on shop.*"]);
        assert_eq!(c.elsewhere, ["billing"]);

        // Global privileges cover every database.
        let have: BTreeSet<Grant> = NEEDED
            .iter()
            .chain(&["PROCESS"])
            .map(|p| g(Object::Global, p))
            .collect();
        let c = compare(&have, "my_db");
        assert!(c.missing.is_empty());
        assert_eq!(c.too_much.len(), 6);
    }

    #[test]
    fn grant_statements_quote_and_escape() {
        let to = "'rowbit'@'%'";
        let lines = grant_lines(
            "my_shop",
            to,
            &["SELECT".into(), "PROCESS".into(), "TRIGGER".into()],
        );
        assert_eq!(
            lines,
            [
                "GRANT SELECT, TRIGGER ON `my\\_shop`.* TO 'rowbit'@'%';",
                "GRANT PROCESS ON *.* TO 'rowbit'@'%';"
            ]
        );
    }

    #[test]
    fn report_switches_to_rowbits_own_user_only_on_a_terminal() {
        let checked = |user: &str, compared: Compared| Checked {
            found: Found {
                tables: vec![],
                account: format!("{user}@%"),
                own_user: user == "rowbit",
                missing: compared.missing.clone(),
            },
            account: Account {
                user: user.into(),
                host: "%".into(),
            },
            database: "shop".into(),
            compared,
            roles: false,
        };
        let url = "mysql://app:x@db:3306/shop";
        match report(url, &checked("app", Compared::default()), true) {
            Next::Switch(u) => {
                assert!(u.starts_with("mysql://rowbit:") && u.ends_with("@db:3306/shop"))
            }
            _ => panic!("expected a switch"),
        }
        assert!(matches!(
            report(url, &checked("app", Compared::default()), false),
            Next::Done
        ));
        let missing = Compared {
            missing: vec!["TRIGGER".into()],
            ..Compared::default()
        };
        assert!(matches!(
            report(url, &checked("rowbit", missing), true),
            Next::Recheck
        ));
        assert!(matches!(
            report(url, &checked("rowbit", Compared::default()), true),
            Next::Done
        ));
    }

    #[test]
    fn the_starter_policy_parses_and_holds_everything() {
        let mut tables: Vec<String> = (0..60).map(|i| format!("t{i}")).collect();
        tables.insert(0, "x\nagents: {}".into());
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
    fn agents_are_added_where_they_fit() {
        let src = starter_policy("shop", &[], &["claude-code".into()]);
        let added = add_agent(&src, "cursor").unwrap();
        let policy = rowbit_policy::PolicyFile::from_yaml(&added).unwrap();
        assert!(policy.agents.contains_key("cursor") && policy.agents.contains_key("claude-code"));

        let four = "version: 0\nagents:\n    bot:\n        default: block\n";
        let added = add_agent(four, "cursor").unwrap();
        assert!(added.contains("\n    cursor:\n"), "{added}");

        assert_eq!(add_agent("version: 0\nagents: {bot: {}}\n", "cursor"), None);
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
