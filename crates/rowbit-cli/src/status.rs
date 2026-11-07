//! `rowbit status`: every setup on this machine, what database each
//! guards, whether its review page is up, and where its agents are
//! connected. Reads files, opening state files read-only, and never
//! connects to a database. See "What Rowbit is connected to" in
//! docs/spec/setup.md.

use std::path::{Path, PathBuf};

use rowbit_engine::overview::{self, Database};

use crate::place::{self, Found};

#[derive(clap::Args)]
pub struct StatusArgs {}

pub fn run(_: StatusArgs) -> anyhow::Result<()> {
    let found = setups()?;
    if found.is_empty() {
        println!("No Rowbit setup on this machine yet. Set one up with: rowbit setup");
        return Ok(());
    }
    let width = found.iter().map(|f| f.name.len()).max().unwrap_or(0);
    for (i, f) in found.iter().enumerate() {
        if i > 0 {
            println!();
        }
        for (j, line) in describe(f).iter().enumerate() {
            let name = if j == 0 { f.name.as_str() } else { "" };
            println!("{name:<width$}  {line}");
        }
    }
    Ok(())
}

/// With `--dir` (or in a folder of Rowbit's), that setup only; otherwise
/// every one `place` knows about.
fn setups() -> anyhow::Result<Vec<Found>> {
    let here = match place::dir() {
        Some(d) => Some(d.to_owned()),
        None if Path::new("rowbit.yml").exists() => Some(std::env::current_dir()?),
        None => None,
    };
    if let Some(dir) = here {
        let dir = dir.canonicalize().unwrap_or(dir);
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("here")
            .to_owned();
        return Ok(vec![Found { name, dir }]);
    }
    Ok(place::home().map(|h| place::setups(&h)).unwrap_or_default())
}

/// The lines `rowbit status` prints for one setup.
fn describe(f: &Found) -> Vec<String> {
    let dir = &f.dir;
    let mut lines = vec![place::tilde(dir)];
    lines.push(match database_url(dir) {
        Ok(Some(url)) => match Database::from_url(&url) {
            Some(db) => format!("database: {}", db.line()),
            None => "database: the URL in the env file doesn't parse".into(),
        },
        Ok(None) => "database: no URL (no env file)".into(),
        Err(e) => format!("database: {e:#}"),
    });
    let address = server_address(dir);
    lines.push(format!(
        "review page: {}, {}{}",
        crate::term::link(&format!("{}/", address.base().trim_end_matches('/'))),
        if rowbit_answers(address.listen) {
            // Some Rowbit: two setups on one address can't tell which.
            "a Rowbit answers there"
        } else {
            "not running"
        },
        match held(dir) {
            Ok(Some(0)) => ", nothing held".to_owned(),
            Ok(Some(n)) => format!(", {n} held"),
            Ok(None) => String::new(),
            Err(e) => format!(", couldn't read rowbit.db: {e:#}"),
        }
    ));
    let mut connected = false;
    match agents(dir) {
        Ok(agents) => {
            for a in agents {
                lines.push(format!("agent {}: {}", a.id, a.rules_line()));
                if a.connections.is_empty() {
                    lines.push(
                        "  not connected over HTTP; `rowbit connect <client>` connects it".into(),
                    );
                }
                for c in a.connections {
                    connected = true;
                    lines.push(format!(
                        "  {:<10} {} ({})",
                        c.id,
                        c.label,
                        c.used_ms
                            .map(|u| format!("last used {}", crate::review::when(u)))
                            .unwrap_or_else(|| "never used".into())
                    ));
                }
            }
        }
        Err(e) => lines.push(format!("agents: {e:#}")),
    }
    if connected {
        lines.push(format!(
            "sign one out: rowbit agents revoke <agent> --connection <id> (or --single) --dir {}",
            crate::connect::shell_quote(&dir.display().to_string())
        ));
    }
    lines
}

/// Whether a Rowbit review page answers at `addr`: its sign-in page, not
/// just anything listening.
fn rowbit_answers(addr: std::net::SocketAddr) -> bool {
    use std::io::{Read, Write};
    let timeout = std::time::Duration::from_millis(300);
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    if write!(s, "GET /login HTTP/1.0\r\nHost: {addr}\r\n\r\n").is_err() {
        return false;
    }
    let mut body = Vec::new();
    let _ = s.take(16 * 1024).read_to_end(&mut body);
    String::from_utf8_lossy(&body).contains("Sign in to Rowbit")
}

fn database_url(dir: &Path) -> anyhow::Result<Option<String>> {
    Ok(
        place::read_env_file(&dir.join(place::ENV_FILE))?
            .or_else(|| crate::demo::database_url(dir)),
    )
}

fn server_address(dir: &Path) -> rowbit_web::config::Address {
    std::fs::read_to_string(dir.join("rowbit-server.yml"))
        .ok()
        .and_then(|src| rowbit_web::config::ServerConfig::address(&src).ok())
        .unwrap_or_default()
}

/// The setup's state file, read-only; `None` if it has none yet.
fn store(dir: &Path) -> anyhow::Result<Option<rowbit_store::Store>> {
    let path: PathBuf = dir.join("rowbit.db");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(rowbit_store::Store::open_read_only(&path)?))
}

fn held(dir: &Path) -> anyhow::Result<Option<usize>> {
    let Some(store) = store(dir)? else {
        return Ok(None);
    };
    Ok(Some(store.list(rowbit_core::change::State::Held)?.len()))
}

fn agents(dir: &Path) -> anyhow::Result<Vec<overview::Agent>> {
    let src = std::fs::read_to_string(dir.join("rowbit.yml"))?;
    let policy = rowbit_policy::PolicyFile::from_yaml(&src)?;
    // A setup that has never run has no tokens; one whose state can't be
    // read says so, rather than claiming nobody is connected.
    let store = match store(dir).map_err(|e| anyhow::anyhow!("couldn't read rowbit.db: {e:#}"))? {
        Some(s) => s,
        None => rowbit_store::Store::open_in_memory()?,
    };
    Ok(overview::agents(&policy, &store)?)
}
