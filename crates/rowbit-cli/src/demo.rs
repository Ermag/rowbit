//! `rowbit demo`: the quickstart's shop database in Docker, Rowbit in front
//! of it, and an agent connected, in a directory of its own. See
//! "rowbit demo" in `docs/spec/setup.md`.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::connect::{self, Client, Env};

const COMPOSE: &str = include_str!("../../../examples/quickstart/compose.yaml");
const SHOP_SQL: &str = include_str!("../../../examples/quickstart/shop.sql");
const POLICY: &str = include_str!("../../../examples/quickstart/rowbit.yml");
/// Where the agent works: empty, apart from Rowbit's files, so nothing in
/// its directory says how to reach the database around Rowbit.
const WORKSPACE: &str = "workspace";
const MARKER: &str = ".rowbit-demo";
const QUICKSTART_PORT: u16 = 33070;

/// The files the demo writes, which `clean` deletes again.
const FILES: [&str; 9] = [
    "compose.yaml",
    "shop.sql",
    "rowbit.yml",
    "rowbit-server.yml",
    "rowbit.db",
    "rowbit.db-wal",
    "rowbit.db-shm",
    "rowbit.db-journal",
    MARKER,
];

/// The README's three writes: one applied, one held, one refused.
const SEED: [&str; 3] = [
    "UPDATE orders SET status = 'refunded' WHERE id IN (10, 11)",
    "UPDATE orders SET status = 'cancelled' WHERE status = 'pending'",
    "DELETE FROM orders WHERE id = 3",
];

/// What to ask the agent: written so a typical agent proposes the too-broad
/// update, which Rowbit holds.
pub const PROMPT: &str = "The pending orders are stuck. Cancel them.";

#[derive(clap::Args)]
pub struct DemoArgs {
    #[command(subcommand)]
    command: Option<DemoCommand>,
    #[command(flatten)]
    dir: DirArg,
    /// The port MariaDB listens on, on 127.0.0.1
    #[arg(long, default_value_t = QUICKSTART_PORT)]
    port: u16,
    /// Where the review page and agents' /mcp listen, on a new demo
    #[arg(long, default_value = rowbit_web::config::DEFAULT_LISTEN)]
    listen: std::net::SocketAddr,
    /// Connect this client without asking
    #[arg(long, value_enum, conflicts_with = "no_connect")]
    connect: Option<Client>,
    /// Don't connect an agent client
    #[arg(long)]
    no_connect: bool,
}

#[derive(clap::Subcommand)]
enum DemoCommand {
    /// Remove the demo: its client entries, its database and its files
    Clean {
        #[command(flatten)]
        dir: DirArg,
    },
}

#[derive(clap::Args)]
struct DirArg {
    /// The demo's directory
    #[arg(long, default_value = "rowbit-demo")]
    dir: PathBuf,
}

/// `.rowbit-demo`: marks a directory as the demo's.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Marker {
    database_url: String,
    /// Whether the person was asked which client to connect.
    asked: bool,
    connected: Vec<Entry>,
}

/// A client entry the demo added.
#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    client: String,
    name: String,
    agent: String,
    url: Option<String>,
    /// The token's last four characters, so `clean` takes back only this
    /// entry and not a later one of the same name.
    #[serde(default)]
    token_end: Option<String>,
}

/// The agent's workspace, if `dir` is a demo directory that has one.
pub fn workspace(dir: &Path) -> Option<PathBuf> {
    let w = dir.join(WORKSPACE);
    (dir.join(MARKER).is_file() && w.is_dir()).then_some(w)
}

/// The database URL a demo directory records, for `rowbit connect`.
pub fn database_url(dir: &Path) -> Option<String> {
    read_marker(dir).ok().flatten().map(|m| m.database_url)
}

fn read_marker(dir: &Path) -> anyhow::Result<Option<Marker>> {
    let path = dir.join(MARKER);
    match std::fs::read_to_string(&path) {
        Ok(src) => Ok(Some(
            serde_json::from_str(&src).with_context(|| format!("reading {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_marker(dir: &Path, marker: &Marker) -> anyhow::Result<()> {
    let text = serde_json::to_string_pretty(marker).expect("a marker serializes");
    std::fs::write(dir.join(MARKER), text + "\n").context("writing .rowbit-demo")
}

pub async fn run(a: DemoArgs) -> anyhow::Result<()> {
    if let Some(DemoCommand::Clean { dir }) = a.command {
        return clean(&dir.dir);
    }
    // The demo runs until stopped and prints the review page's password:
    // a person's terminal, not an agent's shell.
    anyhow::ensure!(
        std::io::stdin().is_terminal() || a.connect.is_some() || a.no_connect,
        "rowbit demo serves the review page until you stop it, and prints its password; \
         run it in a terminal of your own (an agent setting it up should ask you to), \
         or pass --connect <client> or --no-connect to run it without one"
    );
    check_docker()?;
    let dir = a.dir.dir;
    // Where the page will listen: a running demo's own setting, or --listen.
    let listen = match read_marker(&dir) {
        Ok(Some(_)) => {
            crate::ServerArgs {
                config: dir.join("rowbit-server.yml"),
            }
            .address()?
            .listen
        }
        _ => a.listen,
    };
    // Before writing anything, connecting a client to it, or printing it.
    if let Err(e) = std::net::TcpListener::bind(listen) {
        anyhow::bail!(
            "{listen} is taken ({e}); stop what uses it, or start a new demo elsewhere with --dir <dir> --listen 127.0.0.1:<port>"
        );
    }
    let mut marker = prepare(&dir, a.port, a.listen)?;
    let dir = dir
        .canonicalize()
        .with_context(|| format!("finding {}", dir.display()))?;
    let workspace = dir.join(WORKSPACE);
    std::fs::create_dir_all(&workspace).context("creating the agent's workspace")?;
    let address = crate::ServerArgs {
        config: dir.join("rowbit-server.yml"),
    }
    .address()?;

    say("Starting the shop database (MariaDB 11.4 in Docker)...");
    compose(&dir, &["up", "-d", "--wait"]).context(
        "docker compose up failed; if the port is taken, pass --port, or run `rowbit demo clean` and try again",
    )?;

    let setup = crate::Setup {
        url: marker.database_url.clone(),
        policy: dir.join("rowbit.yml"),
        store: dir.join("rowbit.db"),
    };
    seed(&setup).await?;

    if !marker.asked {
        if let Some(client) = pick_client(a.connect, a.no_connect)? {
            println!();
            // A client that can't be set up shouldn't stop the demo.
            match connect::run(connect::ConnectArgs::in_dir(&dir, &workspace, client)) {
                Ok(Some(c)) => marker.connected.push(Entry {
                    client: c.client.id().into(),
                    name: c.name,
                    agent: c.agent,
                    url: c.url,
                    token_end: c.token_end,
                }),
                Ok(None) => {}
                Err(e) => {
                    say(&format!("Couldn't connect {}: {e:#}", client.label()));
                    say(&format!(
                        "Carrying on without it. Later, in {}: rowbit connect {} (--print shows what to add by hand).",
                        shown_dir(&dir),
                        client.id()
                    ));
                }
            }
        }
        marker.asked = true;
        write_marker(&dir, &marker)?;
    }
    let connected = marker
        .connected
        .first()
        .and_then(|e| Some((client_by_id(&e.client)?, e.agent.clone())));

    let password = rowbit_web::new_password();
    {
        let mut store = rowbit_store::Store::open(&setup.store)?;
        store.set_setting("web.password", &rowbit_web::hash_password(&password))?;
    }
    let address = address.base();
    let shown = shown_dir(&dir);
    let shown_workspace = shown_dir(&workspace);
    println!();
    say("Rowbit is guarding the demo shop database.");
    say(&format!(
        "Review page: {address}    password: {password}    name: anything"
    ));
    println!();
    match connected {
        Some((client, agent)) => {
            let open = match client {
                Client::ClaudeCode => format!("Open Claude Code in {shown_workspace}"),
                Client::Codex | Client::Gemini => {
                    format!("Start {} in {shown_workspace}", client.label())
                }
                _ => format!("Open {} on {shown_workspace}", client.label()),
            };
            say(&format!(
                "{} is connected as {agent}. {open} and ask:",
                client.label()
            ));
            println!();
            say(&format!("  {PROMPT}"));
            println!();
            say("Rowbit will hold its write, and the agent will give you a link to review it.");
            say("(Change 2 on the review page is the demo's own example of that write.)");
        }
        None => {
            say(&format!(
                "Connect an agent: cd {shown} && rowbit connect <client> (rowbit connect lists them)."
            ));
            say(&format!(
                "Or propose a write as the agent would: cd {shown} && rowbit propose --agent coding-agent \"{}\"",
                SEED[1]
            ));
        }
    }
    println!();
    say(&format!(
        "For the CLI, in {shown}: export ROWBIT_DATABASE_URL={}",
        marker.database_url
    ));
    say(
        "Ctrl-C stops the review page; the database keeps running. `rowbit demo clean` removes it all.",
    );
    println!();
    std::io::stdout().flush()?;

    std::env::set_current_dir(&dir)?;
    crate::serve(
        None,
        false,
        &crate::ServerArgs {
            config: dir.join("rowbit-server.yml"),
        },
        &setup,
    )
    .await
}

/// The directory, created and filled, or the existing demo's marker.
fn prepare(dir: &Path, port: u16, listen: std::net::SocketAddr) -> anyhow::Result<Marker> {
    if dir.exists() {
        anyhow::ensure!(dir.is_dir(), "{} is not a directory", dir.display());
        if let Some(marker) = read_marker(dir)? {
            return Ok(marker);
        }
        let empty = std::fs::read_dir(dir)?.next().is_none();
        anyhow::ensure!(
            empty,
            "{} exists and isn't a Rowbit demo; pass --dir to pick another directory",
            dir.display()
        );
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let abs = dir.canonicalize()?;
    let name = project_name(&abs);
    let compose = COMPOSE
        .replace("name: rowbit-quickstart", &format!("name: {name}"))
        .replace(
            &format!("127.0.0.1:{QUICKSTART_PORT}:3306"),
            &format!("127.0.0.1:{port}:3306"),
        );
    for (file, text) in [
        ("compose.yaml", compose.as_str()),
        ("shop.sql", SHOP_SQL),
        ("rowbit.yml", POLICY),
        (
            "rowbit-server.yml",
            &format!("version: 0\nlisten: {listen}\nmcp:\n  http: true\n"),
        ),
    ] {
        std::fs::write(dir.join(file), text).with_context(|| format!("writing {file}"))?;
    }
    let marker = Marker {
        database_url: format!("mysql://rowbit:rowbit-quickstart@127.0.0.1:{port}/shop"),
        ..Default::default()
    };
    write_marker(dir, &marker)?;
    Ok(marker)
}

/// Compose's project name: the directory's, as Compose would make it.
fn project_name(dir: &Path) -> String {
    let base = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let name: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    let name = name.trim_start_matches(['-', '_']).to_owned();
    if name.is_empty() {
        "rowbit-demo".into()
    } else {
        name
    }
}

/// Proposes the README's three writes, the first time.
async fn seed(setup: &crate::Setup) -> anyhow::Result<()> {
    let (engine, database) = setup.engine()?;
    if !engine.list(None)?.is_empty() {
        return Ok(());
    }
    for sql in SEED {
        let status = engine.propose("coding-agent", &database, sql).await?;
        say(&format!(
            "Change {}: {} ({})",
            status.id,
            status.state.as_str(),
            sql
        ));
    }
    Ok(())
}

/// Which client to connect: the flag, or the person's answer, or none.
fn pick_client(flag: Option<Client>, none: bool) -> anyhow::Result<Option<Client>> {
    if none {
        return Ok(None);
    }
    if flag.is_some() {
        return Ok(flag);
    }
    let env = Env::current()?;
    let found: Vec<Client> = Client::ALL
        .into_iter()
        .filter(|c| c.installed(&env))
        .collect();
    if found.is_empty() || !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    println!();
    say("Connect an agent client to the demo?");
    for (i, c) in found.iter().enumerate() {
        say(&format!("  {}. {}", i + 1, c.label()));
    }
    say("  n. None for now");
    print!("Which one? [1] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(choose(&found, answer.trim()))
}

fn choose(found: &[Client], answer: &str) -> Option<Client> {
    if answer.is_empty() {
        return found.first().copied();
    }
    if answer.eq_ignore_ascii_case("n") || answer.eq_ignore_ascii_case("none") {
        return None;
    }
    if let Ok(n) = answer.parse::<usize>() {
        return n.checked_sub(1).and_then(|i| found.get(i)).copied();
    }
    found
        .iter()
        .find(|c| c.id() == answer || c.label().eq_ignore_ascii_case(answer))
        .copied()
}

fn client_by_id(id: &str) -> Option<Client> {
    Client::ALL.into_iter().find(|c| c.id() == id)
}

/// `rowbit demo clean`.
fn clean(dir: &Path) -> anyhow::Result<()> {
    let marker = read_marker(dir)?.with_context(|| {
        format!(
            "{} isn't a Rowbit demo (no {MARKER}); nothing removed",
            dir.display()
        )
    })?;
    let mut env = Env::current()?;
    let workspace = dir.join(WORKSPACE);
    // Claude Code's local scope is the agent's workspace.
    env.cwd = if workspace.is_dir() {
        workspace.canonicalize()?
    } else {
        dir.canonicalize()?
    };
    for e in &marker.connected {
        let Some(client) = client_by_id(&e.client) else {
            continue;
        };
        match connect::remove(
            client,
            &env,
            &e.name,
            e.url.as_deref(),
            e.token_end.as_deref(),
        ) {
            Ok(connect::Removal::Removed) => {
                say(&format!("Removed {} from {}", e.name, client.label()))
            }
            Ok(connect::Removal::Missing) => {}
            Ok(connect::Removal::Changed) => say(&format!(
                "Left {} in {}: it was changed since the demo added it",
                e.name,
                client.label()
            )),
            Err(err) => say(&format!("Left {} in {}: {err:#}", e.name, client.label())),
        }
    }
    if dir.join("compose.yaml").exists() {
        check_docker()?;
        compose(dir, &["down", "-v"])
            .context("docker compose down failed; nothing else removed")?;
    }
    for f in FILES {
        match std::fs::remove_file(dir.join(f)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {f}")),
        }
    }
    // The agent's workspace goes only if it's empty: what's in it is yours.
    let _ = std::fs::remove_dir(&workspace);
    if std::fs::remove_dir(dir).is_ok() {
        say(&format!("Removed {}", dir.display()));
    } else {
        say(&format!(
            "Removed the demo's files; {} still holds files of yours",
            dir.display()
        ));
    }
    Ok(())
}

fn check_docker() -> anyhow::Result<()> {
    let quiet = |args: &[&str]| {
        Command::new("docker")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    match quiet(&["compose", "version"]) {
        Ok(s) if s.success() => {}
        _ => anyhow::bail!(
            "the demo needs Docker with Compose v2 (`docker compose`): https://docs.docker.com/get-docker/"
        ),
    }
    let info = Command::new("docker")
        .arg("info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output();
    match info {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) if String::from_utf8_lossy(&o.stderr).contains("permission denied") => anyhow::bail!(
            "can't talk to Docker: permission denied on its socket; add yourself to the docker group \
             (then log in again) or use rootless Docker, and run this again"
        ),
        _ => anyhow::bail!(
            "Docker is installed but not running; start Docker Desktop (or the docker service) and run this again"
        ),
    }
}

fn compose(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("docker")
        .arg("compose")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .status()
        .context("running docker")?;
    anyhow::ensure!(status.success(), "docker compose {} failed", args.join(" "));
    Ok(())
}

/// The directory as the person would type it: relative if it's below the
/// current one.
fn shown_dir(dir: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| cwd.canonicalize().ok())
        .and_then(|cwd| dir.strip_prefix(cwd).ok().map(Path::to_owned))
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| format!("{}/", p.display()))
        .unwrap_or_else(|| dir.display().to_string())
}

fn say(line: &str) {
    println!("{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_pick_a_client() {
        let found = [Client::ClaudeCode, Client::Cursor];
        assert_eq!(choose(&found, ""), Some(Client::ClaudeCode));
        assert_eq!(choose(&found, "2"), Some(Client::Cursor));
        assert_eq!(choose(&found, "cursor"), Some(Client::Cursor));
        assert_eq!(choose(&found, "Claude Code"), Some(Client::ClaudeCode));
        assert_eq!(choose(&found, "n"), None);
        assert_eq!(choose(&found, "3"), None);
        assert_eq!(choose(&found, "0"), None);
        assert_eq!(choose(&[], ""), None);
    }

    #[test]
    fn project_names_are_composes() {
        assert_eq!(project_name(Path::new("/a/rowbit-demo")), "rowbit-demo");
        assert_eq!(project_name(Path::new("/a/My Demo.2")), "mydemo2");
        assert_eq!(project_name(Path::new("/a/__")), "rowbit-demo");
    }

    #[test]
    fn the_quickstart_files_take_the_port() {
        assert!(COMPOSE.contains("name: rowbit-quickstart"));
        assert!(COMPOSE.contains(&format!("127.0.0.1:{QUICKSTART_PORT}:3306")));
        assert!(SHOP_SQL.contains("IDENTIFIED BY 'rowbit-quickstart'"));
    }
}
