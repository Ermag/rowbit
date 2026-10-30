//! `rowbit connect`: adds Rowbit to an agent client's MCP configuration.
//! See "rowbit connect" in `docs/spec/setup.md`.
//!
//! Claude Code is set up with its own `claude mcp add`, since its settings
//! file is the app's own. Every other client's file is edited here: read as
//! strict JSON (TOML for Codex), refused if it doesn't parse, every other
//! key kept in its order, the old file kept as `<file>.rowbit-backup`, and
//! the new one written through a rename. Tokens never go on a command line
//! except Claude Code's.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Client {
    ClaudeCode,
    Cursor,
    Vscode,
    ClaudeDesktop,
    Codex,
    Gemini,
    Windsurf,
}

impl Client {
    pub const ALL: [Client; 7] = [
        Client::ClaudeCode,
        Client::Cursor,
        Client::Vscode,
        Client::ClaudeDesktop,
        Client::Codex,
        Client::Gemini,
        Client::Windsurf,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Client::ClaudeCode => "claude-code",
            Client::Cursor => "cursor",
            Client::Vscode => "vscode",
            Client::ClaudeDesktop => "claude-desktop",
            Client::Codex => "codex",
            Client::Gemini => "gemini",
            Client::Windsurf => "windsurf",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Client::ClaudeCode => "Claude Code",
            Client::Cursor => "Cursor",
            Client::Vscode => "VS Code",
            Client::ClaudeDesktop => "Claude Desktop",
            Client::Codex => "Codex",
            Client::Gemini => "Gemini CLI",
            Client::Windsurf => "Windsurf",
        }
    }

    /// Whether the client can only start servers itself (stdio).
    pub fn stdio_only(self) -> bool {
        self == Client::ClaudeDesktop
    }

    /// The command the client installs, if it has one.
    fn command(self) -> Option<&'static str> {
        match self {
            Client::ClaudeCode => Some("claude"),
            Client::Cursor => Some("cursor"),
            Client::Vscode => Some("code"),
            Client::ClaudeDesktop => None,
            Client::Codex => Some("codex"),
            Client::Gemini => Some("gemini"),
            Client::Windsurf => Some("windsurf"),
        }
    }

    /// The file Rowbit edits, for every client but Claude Code.
    pub fn config_file(self, env: &Env) -> Option<PathBuf> {
        let home = &env.home;
        Some(match self {
            Client::ClaudeCode => return None,
            Client::Cursor => home.join(".cursor/mcp.json"),
            Client::Vscode => env.app_config_dir().join("Code/User/mcp.json"),
            Client::ClaudeDesktop if env.macos => {
                home.join("Library/Application Support/Claude/claude_desktop_config.json")
            }
            Client::ClaudeDesktop => return None,
            Client::Codex => home.join(".codex/config.toml"),
            Client::Gemini => home.join(".gemini/settings.json"),
            Client::Windsurf => home.join(".codeium/windsurf/mcp_config.json"),
        })
    }

    /// Whether the client looks installed: its command on `PATH`, or the
    /// directory its configuration lives in.
    pub fn installed(self, env: &Env) -> bool {
        self.command().is_some_and(|c| env.on_path(c))
            || self
                .config_file(env)
                .and_then(|f| f.parent().map(Path::is_dir))
                .unwrap_or(false)
    }

    /// The key servers live under in the client's JSON file.
    fn servers_key(self) -> &'static str {
        match self {
            Client::Vscode => "servers",
            _ => "mcpServers",
        }
    }

    /// The server's entry in the client's JSON file.
    fn json_entry(self, server: &Server) -> Json {
        let mut entry = IndexMap::new();
        let s = |v: &str| Json::Str(v.to_owned());
        match server {
            Server::Http { url, token } => {
                let key = match self {
                    Client::Gemini => "httpUrl",
                    Client::Windsurf => "serverUrl",
                    _ => "url",
                };
                if self == Client::Vscode {
                    entry.insert("type".into(), s("http"));
                }
                entry.insert(key.into(), s(url));
                let mut headers = IndexMap::new();
                headers.insert("Authorization".into(), s(&format!("Bearer {token}")));
                entry.insert("headers".into(), Json::Obj(headers));
            }
            Server::Stdio { command, args, env } => {
                if self == Client::Vscode {
                    entry.insert("type".into(), s("stdio"));
                }
                entry.insert("command".into(), s(command));
                entry.insert(
                    "args".into(),
                    Json::Arr(args.iter().map(|a| s(a)).collect()),
                );
                entry.insert(
                    "env".into(),
                    Json::Obj(env.iter().map(|(k, v)| (k.clone(), s(v))).collect()),
                );
            }
        }
        Json::Obj(entry)
    }
}

/// Where Rowbit runs for the client: the address and token, or the
/// command the client starts.
#[derive(Debug, Clone)]
pub enum Server {
    Http {
        url: String,
        token: String,
    },
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
}

/// Claude Code's scopes. `project` is refused: it writes `.mcp.json`,
/// which is meant to be committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Scope {
    Local,
    User,
    Project,
}

impl Scope {
    fn as_str(self) -> &'static str {
        match self {
            Scope::Local => "local",
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

/// The parts of the environment `connect` reads, so tests can point it at
/// a home of their own.
pub struct Env {
    pub home: PathBuf,
    pub macos: bool,
    pub path: Option<std::ffi::OsString>,
    pub xdg_config_home: Option<PathBuf>,
}

impl Env {
    pub fn current() -> anyhow::Result<Env> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")?;
        Ok(Env {
            home,
            macos: cfg!(target_os = "macos"),
            path: std::env::var_os("PATH"),
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME")
                .filter(|h| !h.is_empty())
                .map(PathBuf::from),
        })
    }

    fn app_config_dir(&self) -> PathBuf {
        if self.macos {
            self.home.join("Library/Application Support")
        } else {
            self.xdg_config_home
                .clone()
                .unwrap_or_else(|| self.home.join(".config"))
        }
    }

    fn on_path(&self, command: &str) -> bool {
        self.path
            .as_ref()
            .is_some_and(|p| std::env::split_paths(p).any(|d| d.join(command).is_file()))
    }
}

/// A JSON value that keeps its objects' keys in file order, so an edited
/// file differs from the old one only by Rowbit's entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum Json {
    Null,
    Bool(bool),
    Num(serde_json::Number),
    Str(String),
    Arr(Vec<Json>),
    Obj(IndexMap<String, Json>),
}

/// What `apply` changed, for the summary.
#[derive(Debug)]
pub struct Applied {
    pub place: String,
}

/// Refuses an entry name a client would trip on.
pub fn check_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "--name may hold only letters, digits, - and _"
    );
    Ok(())
}

/// Whether the client already has an entry called `name`. Errors if its
/// file can't be read, before any token is made.
pub fn exists(client: Client, env: &Env, name: &str) -> anyhow::Result<bool> {
    if client == Client::ClaudeCode {
        let status = Command::new("claude")
            .args(["mcp", "get", name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("running claude")?;
        return Ok(status.success());
    }
    let file = client
        .config_file(env)
        .context("this client isn't supported on this system")?;
    if !file.exists() {
        return Ok(false);
    }
    let src =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    if client == Client::Codex {
        let doc = parse_toml(&file, &src)?;
        return Ok(doc
            .get("mcp_servers")
            .and_then(|t| t.as_table_like())
            .is_some_and(|t| t.contains_key(name)));
    }
    let doc = parse_json(&file, &src)?;
    Ok(match doc {
        Json::Obj(top) => {
            matches!(top.get(client.servers_key()), Some(Json::Obj(s)) if s.contains_key(name))
        }
        _ => false,
    })
}

/// Adds the entry. `force` replaces one by the same name.
pub fn apply(
    client: Client,
    env: &Env,
    name: &str,
    server: &Server,
    scope: Scope,
    force: bool,
) -> anyhow::Result<Applied> {
    if client == Client::ClaudeCode {
        if force {
            // Fails if there was nothing to remove, which is fine.
            let _ = Command::new("claude")
                .args(["mcp", "remove", "--scope", scope.as_str(), name])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let output = Command::new("claude")
            .args(claude_args(scope, name, server))
            .stdin(Stdio::null())
            .output()
            .context("running claude")?;
        anyhow::ensure!(
            output.status.success(),
            "claude mcp add failed: {}{}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return Ok(Applied {
            place: match scope {
                Scope::User => "Claude Code (user settings)".into(),
                _ => "Claude Code (this directory)".into(),
            },
        });
    }
    let file = client
        .config_file(env)
        .context("this client isn't supported on this system")?;
    let old = match std::fs::read_to_string(&file) {
        Ok(src) => Some(src),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    };
    let new = edit(client, &file, old.as_deref(), name, server, force)?;
    if old.is_some() {
        std::fs::copy(&file, backup_path(&file))
            .with_context(|| format!("backing up {}", file.display()))?;
    }
    write_private(&file, &new)?;
    Ok(Applied {
        place: file.display().to_string(),
    })
}

/// The file's new text, with the entry added.
fn edit(
    client: Client,
    file: &Path,
    old: Option<&str>,
    name: &str,
    server: &Server,
    force: bool,
) -> anyhow::Result<String> {
    let taken = || {
        anyhow::anyhow!(
            "{} already has a server called {name}; pass --force to replace it, or --name to add another",
            file.display()
        )
    };
    if client == Client::Codex {
        let mut doc = match old {
            Some(src) => parse_toml(file, src)?,
            None => toml_edit::DocumentMut::new(),
        };
        let servers = doc
            .entry("mcp_servers")
            .or_insert_with(|| {
                let mut t = toml_edit::Table::new();
                t.set_implicit(true);
                toml_edit::Item::Table(t)
            })
            .as_table_like_mut()
            .with_context(|| format!("{}: mcp_servers is not a table", file.display()))?;
        if servers.contains_key(name) && !force {
            return Err(taken());
        }
        servers.insert(name, toml_edit::Item::Table(codex_entry(server)));
        return Ok(doc.to_string());
    }
    let mut doc = match old {
        Some(src) if !src.trim().is_empty() => parse_json(file, src)?,
        _ => Json::Obj(IndexMap::new()),
    };
    let Json::Obj(top) = &mut doc else {
        anyhow::bail!("{} is not a JSON object", file.display());
    };
    let servers = top
        .entry(client.servers_key().to_owned())
        .or_insert_with(|| Json::Obj(IndexMap::new()));
    let Json::Obj(servers) = servers else {
        anyhow::bail!(
            "{}: {} is not an object",
            file.display(),
            client.servers_key()
        );
    };
    if servers.contains_key(name) && !force {
        return Err(taken());
    }
    servers.insert(name.to_owned(), client.json_entry(server));
    let mut text = serde_json::to_string_pretty(&doc).expect("JSON always serializes");
    text.push('\n');
    Ok(text)
}

fn codex_entry(server: &Server) -> toml_edit::Table {
    let mut t = toml_edit::Table::new();
    match server {
        Server::Http { url, token } => {
            t.insert("url", toml_edit::value(url.as_str()));
            let mut headers = toml_edit::InlineTable::new();
            headers.insert("Authorization", format!("Bearer {token}").into());
            t.insert("http_headers", toml_edit::value(headers));
        }
        Server::Stdio { command, args, env } => {
            t.insert("command", toml_edit::value(command.as_str()));
            let mut a = toml_edit::Array::new();
            for arg in args {
                a.push(arg.as_str());
            }
            t.insert("args", toml_edit::value(a));
            let mut e = toml_edit::InlineTable::new();
            for (k, v) in env {
                e.insert(k, v.as_str().into());
            }
            t.insert("env", toml_edit::value(e));
        }
    }
    t
}

/// `claude mcp add`'s arguments. `--header` and `--env` take several
/// values, so they come after the name and URL, and `--` ends them.
pub fn claude_args(scope: Scope, name: &str, server: &Server) -> Vec<String> {
    let mut a: Vec<String> = ["mcp", "add", "--scope", scope.as_str()]
        .map(str::to_owned)
        .into();
    match server {
        Server::Http { url, token } => {
            a.extend([
                "--transport".into(),
                "http".into(),
                name.into(),
                url.clone(),
                "--header".into(),
                format!("Authorization: Bearer {token}"),
            ]);
        }
        Server::Stdio { command, args, env } => {
            a.push(name.into());
            for (k, v) in env {
                a.extend(["--env".into(), format!("{k}={v}")]);
            }
            a.extend(["--".into(), command.clone()]);
            a.extend(args.iter().cloned());
        }
    }
    a
}

/// What to add by hand, for `--print`.
pub fn snippet(client: Client, env: &Env, name: &str, server: &Server, scope: Scope) -> String {
    if client == Client::ClaudeCode {
        let args: Vec<String> = claude_args(scope, name, server)
            .iter()
            .map(|a| shell_quote(a))
            .collect();
        return format!("claude {}\n", args.join(" "));
    }
    let file = client
        .config_file(env)
        .map(|f| f.display().to_string())
        .unwrap_or_else(|| "the client's MCP settings".into());
    let body = edit(client, Path::new(&file), None, name, server, true)
        .expect("a new file always takes the entry");
    format!("Add to {file}:\n\n{body}")
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn parse_json(file: &Path, src: &str) -> anyhow::Result<Json> {
    serde_json::from_str(src).map_err(|e| {
        anyhow::anyhow!(
            "{} isn't strict JSON ({e}); Rowbit won't rewrite it. Run with --print and add the entry by hand",
            file.display()
        )
    })
}

fn parse_toml(file: &Path, src: &str) -> anyhow::Result<toml_edit::DocumentMut> {
    src.parse().map_err(|e| {
        anyhow::anyhow!(
            "{} isn't valid TOML ({e}); Rowbit won't rewrite it. Run with --print and add the entry by hand",
            file.display()
        )
    })
}

fn backup_path(file: &Path) -> PathBuf {
    let mut name = file.file_name().unwrap_or_default().to_os_string();
    name.push(".rowbit-backup");
    file.with_file_name(name)
}

/// Writes through a temporary file and a rename, keeping an existing file's
/// permissions; a new file is its owner's only, since it holds a token or a
/// database URL.
fn write_private(file: &Path, contents: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = file.parent().context("a file has a directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mode = std::fs::metadata(file)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o600);
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file.file_name().unwrap_or_default());
    tmp_name.push(format!(".rowbit-{}", std::process::id()));
    let tmp = dir.join(tmp_name);
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
        // The mode above is masked by the umask; set it exactly.
        f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, file)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("writing {}", file.display()))
}

/// `rowbit connect`'s arguments.
#[derive(clap::Args)]
pub struct ConnectArgs {
    /// The client to set up; without one, lists them
    client: Option<Client>,
    /// The agent from rowbit.yml; needed if it lists more than one
    #[arg(long)]
    agent: Option<String>,
    /// The entry's name in the client
    #[arg(long, default_value = "rowbit")]
    name: String,
    /// Claude Code only: local (this directory) or user
    #[arg(long, value_enum, default_value = "local")]
    scope: Scope,
    /// Have the client start `rowbit mcp` itself, even with mcp.http on
    #[arg(long)]
    stdio: bool,
    /// Replace the agent's token, signing out the client that has the old one
    #[arg(long)]
    new_token: bool,
    /// Replace an entry of the same name in the client
    #[arg(long)]
    force: bool,
    /// Change nothing in the client: print what to add, with the token
    #[arg(long)]
    print: bool,
    /// For stdio: mysql://user:password@host:port/database
    #[arg(long, env = "ROWBIT_DATABASE_URL", hide_env_values = true)]
    url: Option<String>,
    /// The policy file
    #[arg(long, env = "ROWBIT_POLICY", default_value = "rowbit.yml")]
    policy: PathBuf,
    #[command(flatten)]
    store: super::StoreArgs,
    #[command(flatten)]
    server: super::ServerArgs,
}

pub fn run(a: ConnectArgs) -> anyhow::Result<()> {
    let env = Env::current()?;
    check_name(&a.name)?;
    let Some(client) = a.client else {
        if a.print {
            return print_address(&a);
        }
        println!("Clients Rowbit can set up (rowbit connect <client>):\n");
        for c in Client::ALL {
            let found = if c.installed(&env) {
                "found"
            } else {
                "not found"
            };
            println!("  {:<15} {:<15} {found}", c.id(), c.label());
        }
        println!("\nFor any other MCP client: rowbit connect --print");
        return Ok(());
    };
    anyhow::ensure!(
        a.scope != Scope::Project,
        "--scope project writes .mcp.json, which is meant to be committed, and would commit the token or database URL; use local or user"
    );
    anyhow::ensure!(
        client == Client::ClaudeCode || a.scope == Scope::Local,
        "--scope is for claude-code only"
    );
    if client.config_file(&env).is_none() && client != Client::ClaudeCode {
        anyhow::bail!("{} isn't available on this system", client.label());
    }
    let agent = pick_agent(&a)?;
    let address = a.server.address()?;
    let http = address.mcp_http && !a.stdio && !client.stdio_only();
    if !a.print {
        anyhow::ensure!(
            client.installed(&env),
            "{} doesn't look installed here{}; pass --print to see what to add by hand",
            client.label(),
            match client.command() {
                Some(cmd) if client == Client::ClaudeCode => format!(" (no {cmd} on PATH)"),
                _ => String::new(),
            }
        );
        anyhow::ensure!(
            a.force || !exists(client, &env, &a.name)?,
            "{} already has a server called {}; pass --force to replace it, or --name to add another",
            client.label(),
            a.name
        );
    }
    let server = if http {
        Server::Http {
            url: format!("{}/mcp", address.base()),
            token: issue_token(&a, &agent)?,
        }
    } else {
        stdio_server(&a, &agent)?
    };
    if a.print {
        print!("{}", snippet(client, &env, &a.name, &server, a.scope));
        return Ok(());
    }
    let applied = match apply(client, &env, &a.name, &server, a.scope, a.force) {
        Ok(applied) => applied,
        Err(e) => {
            if http {
                // Nothing holds the new token; don't leave it valid.
                let _ = a
                    .store
                    .open()
                    .and_then(|mut s| Ok(s.revoke_agent_token(&agent)?));
                eprintln!("rowbit: {agent}'s new token is revoked; it has none now");
            }
            return Err(e);
        }
    };
    match &server {
        Server::Http { url, token } => {
            println!(
                "Added {} to {} as {agent}, over HTTP at {url}, with a token ending {}.",
                a.name,
                applied.place,
                &token[token.len().saturating_sub(4)..]
            );
            if address.public_url.is_none()
                && std::net::TcpStream::connect_timeout(
                    &address.listen,
                    std::time::Duration::from_millis(500),
                )
                .is_err()
            {
                println!(
                    "Nothing answers at {} yet: start `rowbit serve`.",
                    address.base()
                );
            }
        }
        Server::Stdio { .. } => {
            println!(
                "Added {} to {} as {agent}, over stdio.\n\
                 The client's settings now hold the database URL and the path to {}. Anyone who can read them can reach the database around Rowbit; see \"Before you use it for real\" in the quickstart.",
                a.name,
                applied.place,
                a.store.store.display()
            );
        }
    }
    match client {
        Client::ClaudeCode if a.scope == Scope::Local => {
            println!("Start Claude Code in this directory to use it.")
        }
        Client::ClaudeCode | Client::Gemini | Client::Codex => {}
        _ => println!("Restart {} to load it.", client.label()),
    }
    Ok(())
}

fn pick_agent(a: &ConnectArgs) -> anyhow::Result<String> {
    let src = std::fs::read_to_string(&a.policy)
        .with_context(|| format!("reading {}", a.policy.display()))?;
    let policy = rowbit_policy::PolicyFile::from_yaml(&src)?;
    match &a.agent {
        Some(agent) => {
            anyhow::ensure!(
                policy.agents.contains_key(agent),
                "{agent} is not an agent in {}",
                a.policy.display()
            );
            Ok(agent.clone())
        }
        None => {
            let ids: Vec<&String> = policy.agents.keys().collect();
            match ids.as_slice() {
                [one] => Ok((*one).clone()),
                [] => anyhow::bail!("{} lists no agents", a.policy.display()),
                many => anyhow::bail!(
                    "{} lists several agents; pick one with --agent: {}",
                    a.policy.display(),
                    many.iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
    }
}

/// One token per agent: a new one signs out whoever has the old one, so
/// that takes --new-token.
fn issue_token(a: &ConnectArgs, agent: &str) -> anyhow::Result<String> {
    let mut store = rowbit_store::Store::open(&a.store.store)
        .with_context(|| format!("opening {}", a.store.store.display()))?;
    anyhow::ensure!(
        a.new_token || store.agent_token_times(agent)?.is_none(),
        "{agent} already has a token, which another client may be using; pass --new-token to replace it (that client is signed out), or give this client its own agent in rowbit.yml"
    );
    Ok(store.issue_agent_token(agent)?)
}

fn stdio_server(a: &ConnectArgs, agent: &str) -> anyhow::Result<Server> {
    let url = a.url.clone().context(
        "over stdio the client needs the database URL: set ROWBIT_DATABASE_URL, or turn on mcp.http in rowbit-server.yml",
    )?;
    let absolute = |p: &Path| -> anyhow::Result<String> {
        let p = if p.is_absolute() {
            p.to_owned()
        } else {
            std::env::current_dir()?.join(p)
        };
        Ok(p.canonicalize().unwrap_or(p).display().to_string())
    };
    anyhow::ensure!(a.policy.exists(), "{} does not exist", a.policy.display());
    Ok(Server::Stdio {
        command: absolute(&std::env::current_exe()?)?,
        args: vec![
            "mcp".into(),
            "--agent".into(),
            agent.into(),
            "--policy".into(),
            absolute(&a.policy)?,
            "--store".into(),
            absolute(&a.store.store)?,
        ],
        env: vec![("ROWBIT_DATABASE_URL".into(), url)],
    })
}

/// `rowbit connect --print` without a client: the address and a token.
fn print_address(a: &ConnectArgs) -> anyhow::Result<()> {
    let address = a.server.address()?;
    anyhow::ensure!(
        address.mcp_http && !a.stdio,
        "turn on mcp.http in rowbit-server.yml for an address to give a client, or name a client for its stdio setup"
    );
    let agent = pick_agent(a)?;
    let token = issue_token(a, &agent)?;
    println!("URL:    {}/mcp", address.base());
    println!("Header: Authorization: Bearer {token}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: &Path, macos: bool) -> Env {
        Env {
            home: home.to_owned(),
            macos,
            path: None,
            xdg_config_home: None,
        }
    }

    fn http() -> Server {
        Server::Http {
            url: "http://127.0.0.1:8080/mcp".into(),
            token: "rbt_test".into(),
        }
    }

    fn stdio() -> Server {
        Server::Stdio {
            command: "/usr/local/bin/rowbit".into(),
            args: vec!["mcp".into(), "--agent".into(), "coding-agent".into()],
            env: vec![("ROWBIT_DATABASE_URL".into(), "mysql://r:p@h/shop".into())],
        }
    }

    fn home(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rowbit-connect-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn each_client_gets_its_own_shape() {
        let e = env(Path::new("/h"), true);
        let entry = |c: Client, s: &Server| serde_json::to_value(c.json_entry(s)).unwrap();
        assert_eq!(
            entry(Client::Cursor, &http()),
            serde_json::json!({"url": "http://127.0.0.1:8080/mcp", "headers": {"Authorization": "Bearer rbt_test"}})
        );
        assert_eq!(
            entry(Client::Vscode, &http())["type"],
            serde_json::json!("http")
        );
        assert_eq!(
            entry(Client::Gemini, &http())["httpUrl"],
            serde_json::json!("http://127.0.0.1:8080/mcp")
        );
        assert_eq!(
            entry(Client::Windsurf, &http())["serverUrl"],
            serde_json::json!("http://127.0.0.1:8080/mcp")
        );
        assert_eq!(
            entry(Client::ClaudeDesktop, &stdio()),
            serde_json::json!({
                "command": "/usr/local/bin/rowbit",
                "args": ["mcp", "--agent", "coding-agent"],
                "env": {"ROWBIT_DATABASE_URL": "mysql://r:p@h/shop"},
            })
        );
        assert_eq!(
            Client::Vscode.config_file(&e).unwrap(),
            Path::new("/h/Library/Application Support/Code/User/mcp.json")
        );
        assert_eq!(
            Client::Vscode
                .config_file(&env(Path::new("/h"), false))
                .unwrap(),
            Path::new("/h/.config/Code/User/mcp.json")
        );
        assert_eq!(
            Client::ClaudeDesktop.config_file(&env(Path::new("/h"), false)),
            None
        );
    }

    #[test]
    fn claude_code_gets_its_own_command() {
        assert_eq!(
            claude_args(Scope::Local, "rowbit", &http()),
            [
                "mcp",
                "add",
                "--scope",
                "local",
                "--transport",
                "http",
                "rowbit",
                "http://127.0.0.1:8080/mcp",
                "--header",
                "Authorization: Bearer rbt_test"
            ]
        );
        assert_eq!(
            claude_args(Scope::User, "rowbit", &stdio()),
            [
                "mcp",
                "add",
                "--scope",
                "user",
                "rowbit",
                "--env",
                "ROWBIT_DATABASE_URL=mysql://r:p@h/shop",
                "--",
                "/usr/local/bin/rowbit",
                "mcp",
                "--agent",
                "coding-agent"
            ]
        );
    }

    #[test]
    fn edits_keep_everything_else_in_order() {
        let h = home("keep");
        let e = env(&h, false);
        let file = Client::Cursor.config_file(&e).unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let old = r#"{"zeta": 1, "mcpServers": {"other": {"command": "x", "n": 1.5}}, "alpha": [true, null]}"#;
        std::fs::write(&file, old).unwrap();
        std::fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o640))
            .unwrap();

        apply(Client::Cursor, &e, "rowbit", &http(), Scope::Local, false).unwrap();
        let new = std::fs::read_to_string(&file).unwrap();
        let keys: Vec<_> = ["zeta", "mcpServers", "other", "rowbit", "alpha"]
            .iter()
            .map(|k| new.find(&format!("\"{k}\"")).unwrap())
            .collect();
        assert!(keys.is_sorted(), "{new}");
        assert!(new.contains("1.5") && new.contains("null"), "{new}");
        assert_eq!(std::fs::read_to_string(backup_path(&file)).unwrap(), old);
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&file).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o640);
        assert!(exists(Client::Cursor, &e, "rowbit").unwrap());

        // A second time is refused without --force, and changes nothing.
        let err = apply(Client::Cursor, &e, "rowbit", &stdio(), Scope::Local, false).unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), new);
        apply(Client::Cursor, &e, "rowbit", &stdio(), Scope::Local, true).unwrap();
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("ROWBIT_DATABASE_URL")
        );
    }

    #[test]
    fn new_files_are_private() {
        let h = home("new");
        let e = env(&h, false);
        apply(Client::Windsurf, &e, "rowbit", &http(), Scope::Local, false).unwrap();
        let file = Client::Windsurf.config_file(&e).unwrap();
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&file).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);
        assert!(!backup_path(&file).exists());
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            doc["mcpServers"]["rowbit"]["headers"]["Authorization"],
            "Bearer rbt_test"
        );
    }

    #[test]
    fn files_that_dont_parse_are_left_alone() {
        let h = home("bad");
        let e = env(&h, false);
        let file = Client::Gemini.config_file(&e).unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let old = "{\n  // comments aren't JSON\n  \"theme\": \"dark\",\n}\n";
        std::fs::write(&file, old).unwrap();
        let err = exists(Client::Gemini, &e, "rowbit").unwrap_err();
        assert!(err.to_string().contains("--print"), "{err}");
        assert!(apply(Client::Gemini, &e, "rowbit", &http(), Scope::Local, false).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), old);
        assert!(!backup_path(&file).exists());

        std::fs::write(&file, "[1, 2]").unwrap();
        assert!(apply(Client::Gemini, &e, "rowbit", &http(), Scope::Local, false).is_err());
    }

    #[test]
    fn codex_keeps_its_comments() {
        let h = home("codex");
        let e = env(&h, false);
        let file = Client::Codex.config_file(&e).unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let old = "# my settings\nmodel = \"o3\"  # the default\n\n[mcp_servers.docs]\ncommand = \"docs-mcp\"\n";
        std::fs::write(&file, old).unwrap();
        apply(Client::Codex, &e, "rowbit", &http(), Scope::Local, false).unwrap();
        let new = std::fs::read_to_string(&file).unwrap();
        assert!(new.starts_with(old), "{new}");
        let doc: toml_edit::DocumentMut = new.parse().unwrap();
        assert_eq!(
            doc["mcp_servers"]["rowbit"]["url"].as_str(),
            Some("http://127.0.0.1:8080/mcp")
        );
        assert_eq!(
            doc["mcp_servers"]["rowbit"]["http_headers"]["Authorization"].as_str(),
            Some("Bearer rbt_test")
        );
        assert!(exists(Client::Codex, &e, "rowbit").unwrap());
        assert!(apply(Client::Codex, &e, "rowbit", &http(), Scope::Local, false).is_err());

        let fresh = home("codex-new");
        let e = env(&fresh, false);
        apply(Client::Codex, &e, "rowbit", &stdio(), Scope::Local, false).unwrap();
        let doc: toml_edit::DocumentMut =
            std::fs::read_to_string(Client::Codex.config_file(&e).unwrap())
                .unwrap()
                .parse()
                .unwrap();
        assert_eq!(
            doc["mcp_servers"]["rowbit"]["args"][1].as_str(),
            Some("--agent")
        );
    }

    #[test]
    fn print_shows_what_to_add() {
        let e = env(Path::new("/h"), false);
        let out = snippet(Client::Cursor, &e, "rowbit", &http(), Scope::Local);
        assert!(out.starts_with("Add to /h/.cursor/mcp.json:"), "{out}");
        assert!(out.contains("Bearer rbt_test"), "{out}");
        let out = snippet(Client::ClaudeCode, &e, "rowbit", &http(), Scope::Local);
        assert!(
            out.ends_with(
                "rowbit http://127.0.0.1:8080/mcp --header 'Authorization: Bearer rbt_test'\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn names_are_plain() {
        assert!(check_name("rowbit-demo_2").is_ok());
        assert!(check_name("").is_err());
        assert!(check_name("a b").is_err());
        assert!(check_name("x\"y").is_err());
    }
}
