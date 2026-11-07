//! `rowbit status`: every setup on this machine, what database each
//! guards, whether its review page is up, and where its agents are
//! connected. Reads files only; never connects to a database. See "What
//! Rowbit is connected to" in docs/spec/setup.md.

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
    let up = std::net::TcpStream::connect_timeout(
        &address.listen,
        std::time::Duration::from_millis(300),
    )
    .is_ok();
    let held = held(dir);
    lines.push(format!(
        "review page: {}, {}{}",
        crate::term::link(&format!("{}/", address.base().trim_end_matches('/'))),
        if up { "running" } else { "not running" },
        match held {
            Some(0) => ", nothing held".to_owned(),
            Some(n) => format!(", {n} held"),
            None => String::new(),
        }
    ));
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
                    lines.push(format!(
                        "  {} ({})",
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
    lines
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

fn store(dir: &Path) -> Option<rowbit_store::Store> {
    let path: PathBuf = dir.join("rowbit.db");
    path.exists()
        .then(|| rowbit_store::Store::open(&path).ok())
        .flatten()
}

fn held(dir: &Path) -> Option<usize> {
    let store = store(dir)?;
    store
        .list(rowbit_core::change::State::Held)
        .ok()
        .map(|h| h.len())
}

fn agents(dir: &Path) -> anyhow::Result<Vec<overview::Agent>> {
    let src = std::fs::read_to_string(dir.join("rowbit.yml"))?;
    let policy = rowbit_policy::PolicyFile::from_yaml(&src)?;
    let store = match store(dir) {
        Some(s) => s,
        None => rowbit_store::Store::open_in_memory()?,
    };
    Ok(overview::agents(&policy, &store)?)
}
