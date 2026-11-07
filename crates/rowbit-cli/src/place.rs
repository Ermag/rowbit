//! Where commands find their files: `--dir` (or `ROWBIT_DIR`), else the
//! current directory if it holds a `rowbit.yml`, else `~/.rowbit/default`
//! if that holds a setup. See "Where commands find their files" in
//! docs/spec/setup.md.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::Context;

/// The file in a setup's directory that holds `ROWBIT_DATABASE_URL`.
pub const ENV_FILE: &str = "env";

/// Commands that have a `--dir` of their own, or make the directory.
const OWN_DIR: [&str; 2] = ["demo", "setup"];

static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Whether `--url` was on the command line, as opposed to clap taking it
/// from `ROWBIT_DATABASE_URL`.
static URL_FLAG: OnceLock<bool> = OnceLock::new();

/// Whether `dir` holds what `rowbit setup` writes.
pub fn is_setup(dir: &Path) -> bool {
    dir.join("rowbit.yml").is_file() && dir.join(ENV_FILE).is_file()
}

/// Whether `dir` holds any of Rowbit's own files, so a setup that stopped
/// half way can be picked up again.
pub fn has_rowbit_files(dir: &Path) -> bool {
    ["rowbit.yml", ENV_FILE, "rowbit-server.yml", "rowbit.db"]
        .iter()
        .any(|f| dir.join(f).exists())
}

/// `~/.rowbit`, where `rowbit setup` keeps each setup.
pub fn home() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME isn't set")?;
    Ok(PathBuf::from(home).join(".rowbit"))
}

/// Takes `--dir` out of the arguments and settles the directory, before
/// clap sees them. Called once, first thing.
pub fn init(args: Vec<OsString>) -> anyhow::Result<Vec<OsString>> {
    let _ = URL_FLAG.set(args.iter().any(|a| {
        let a = a.to_string_lossy();
        a == "--url" || a.starts_with("--url=")
    }));
    let (command, dir_first) = command(&args);
    if let Some(c) = command.as_deref().filter(|c| OWN_DIR.contains(c)) {
        anyhow::ensure!(
            !dir_first,
            "rowbit {c} doesn't take --dir before it; see rowbit {c} --help"
        );
    }
    if command
        .as_deref()
        .is_some_and(|c| OWN_DIR.contains(&c) || c == "help")
    {
        let _ = DIR.set(None);
        return Ok(args);
    }
    let (args, named) = take_dir(args)?;
    let dir = match named.or_else(|| std::env::var_os("ROWBIT_DIR").map(PathBuf::from)) {
        Some(dir) => {
            anyhow::ensure!(dir.is_dir(), "{} isn't a directory", dir.display());
            Some(dir)
        }
        // A folder with Rowbit's own files in it is Rowbit's.
        None if ["rowbit.yml", "rowbit-server.yml", "rowbit.db"]
            .iter()
            .any(|f| Path::new(f).exists()) =>
        {
            None
        }
        // `status` lists every setup unless told which.
        None if command.as_deref() == Some("status") => None,
        // Told where its files are: no setup found elsewhere may lend its
        // database URL to them.
        None if names_files(&args) => None,
        None => {
            let found = home().map(|h| setups(&h)).unwrap_or_default();
            match pick(&found) {
                Picked::One(s) => {
                    if s.name != "default" {
                        eprintln!("rowbit: using {}", s.describe());
                    }
                    Some(s.dir.clone())
                }
                Picked::Several if needs_files(&args) => anyhow::bail!(
                    "Rowbit has several setups and none is the default; pick one with --dir:\n{}",
                    found
                        .iter()
                        .map(|s| format!("  {}", s.describe()))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
                Picked::None if needs_files(&args) => anyhow::bail!(
                    "no Rowbit setup here: no rowbit.yml in this folder and none in ~/.rowbit. \
                     Set one up with `rowbit setup` (or try the sample with `rowbit setup --sample`), \
                     or point at one with --dir <folder>"
                ),
                _ => None,
            }
        }
    };
    let _ = DIR.set(dir);
    Ok(args)
}

/// A setup `rowbit setup` or `rowbit demo` made, found without `--dir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Its name in `~/.rowbit`, or `demo`.
    pub name: String,
    pub dir: PathBuf,
}

impl Found {
    pub fn describe(&self) -> String {
        let shown = tilde(&self.dir);
        if self.name == "demo" && crate::demo::is_demo(&self.dir) {
            format!("the demo in {shown}")
        } else {
            format!("{} in {shown}", self.name)
        }
    }
}

/// `path` with the home directory written as `~`.
pub fn tilde(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if !home.as_os_str().is_empty() => match path.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        _ => path.display().to_string(),
    }
}

/// Every setup in `rowbit_home` (`~/.rowbit`), by name, and the demo:
/// the one `~/.rowbit/demo` links to, or else `~/rowbit-demo`, where
/// `rowbit setup --sample` run from the home directory puts it.
pub fn setups(rowbit_home: &Path) -> Vec<Found> {
    let mut found = Vec::new();
    let mut demo = None;
    if let Ok(entries) = std::fs::read_dir(rowbit_home) {
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for dir in entries {
            let Some(name) = dir.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
                continue;
            };
            if is_setup(&dir) {
                found.push(Found { name, dir });
            } else if name == DEMO_LINK && crate::demo::is_demo(&dir) {
                demo = Some(dir.canonicalize().unwrap_or(dir));
            }
        }
    }
    let demo = demo.or_else(|| {
        let d = rowbit_home.parent()?.join("rowbit-demo");
        crate::demo::is_demo(&d).then_some(d)
    });
    if let Some(dir) = demo {
        found.push(Found {
            name: "demo".into(),
            dir,
        });
    }
    found
}

/// The link in `~/.rowbit` to the demo's directory, which `rowbit demo`
/// makes and `rowbit demo clean` removes.
pub const DEMO_LINK: &str = "demo";

/// Whether the command is told where its files are: `--policy`,
/// `--store`, `--config` or their variables.
fn names_files(args: &[OsString]) -> bool {
    args.iter().any(|a| {
        let a = a.to_string_lossy();
        ["--policy", "--store", "--config"]
            .iter()
            .any(|f| a == *f || a.starts_with(&format!("{f}=")))
    }) || ["ROWBIT_POLICY", "ROWBIT_STORE", "ROWBIT_SERVER_CONFIG"]
        .iter()
        .any(|v| std::env::var_os(v).is_some())
}

#[derive(Debug, PartialEq, Eq)]
enum Picked<'a> {
    One(&'a Found),
    Several,
    None,
}

/// The default setup if there is one, else the only one.
fn pick(found: &[Found]) -> Picked<'_> {
    if let Some(d) = found.iter().find(|s| s.name == "default") {
        return Picked::One(d);
    }
    match found {
        [one] => Picked::One(one),
        [] => Picked::None,
        _ => Picked::Several,
    }
}

/// Whether the command reads a setup's files, so finding none is an error
/// worth stopping for. `ping`, `policy`, `server` and `connect` with no
/// client name (it lists them) work without one, and so does a command
/// told where its files are (`--policy`, `ROWBIT_STORE`, ...).
fn needs_files(args: &[OsString]) -> bool {
    if names_files(args) {
        return false;
    }
    let (command, _) = command(args);
    match command.as_deref() {
        None | Some("ping" | "policy" | "server" | "help") => false,
        Some("connect") => {
            // What follows the command's name: a client, or --print.
            let after = args
                .iter()
                .skip(1)
                .skip_while(|a| a.to_string_lossy() != "connect")
                .skip(1);
            let mut after = after.peekable();
            let mut needs = false;
            while let Some(a) = after.next() {
                let a = a.to_string_lossy();
                if a == "--print" {
                    needs = true;
                } else if a == "--agent" || a == "--name" || a == "--scope" || a == "--url" {
                    after.next();
                } else if !a.starts_with('-') {
                    needs = true;
                }
            }
            needs
        }
        Some(_) => !args.iter().any(|a| a == "--help" || a == "-h"),
    }
}

/// The subcommand, and whether a `--dir` came before it.
fn command(args: &[OsString]) -> (Option<String>, bool) {
    let mut dir_first = false;
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        let a = a.to_string_lossy();
        if a == "--dir" {
            dir_first = true;
            it.next();
        } else if a.starts_with("--dir=") {
            dir_first = true;
        } else if !a.starts_with('-') {
            return (Some(a.into_owned()), dir_first);
        }
    }
    (None, dir_first)
}

fn take_dir(args: Vec<OsString>) -> anyhow::Result<(Vec<OsString>, Option<PathBuf>)> {
    let mut out = Vec::with_capacity(args.len());
    let mut dir = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let s = a.to_string_lossy();
        if s == "--" {
            out.push(a);
            out.extend(it);
            break;
        }
        if s == "--dir" {
            dir = Some(PathBuf::from(it.next().context("--dir needs a directory")?));
        } else if let Some(d) = s.strip_prefix("--dir=") {
            dir = Some(PathBuf::from(d));
        } else {
            out.push(a);
        }
    }
    Ok((out, dir))
}

/// The directory commands use, if not the current one.
pub fn dir() -> Option<&'static Path> {
    DIR.get().and_then(|d| d.as_deref())
}

/// `path` in the directory, if it is still clap's default file name.
pub fn file(path: &mut PathBuf, default: &str) {
    if let Some(dir) = dir()
        && path == Path::new(default)
    {
        *path = dir.join(default);
    }
}

/// `ROWBIT_DATABASE_URL` from the directory's `env` file, for when neither
/// the flag nor the environment set it.
pub fn database_url() -> anyhow::Result<Option<String>> {
    // Run inside a setup's or the demo's folder, that folder.
    let dir = match dir() {
        Some(dir) => dir,
        None if Path::new("rowbit.yml").exists() => Path::new("."),
        None => return Ok(None),
    };
    Ok(read_env_file(&dir.join(ENV_FILE))?.or_else(|| crate::demo::database_url(dir)))
}

/// The database URL a command uses. `--url` wins. Otherwise, in a setup's
/// directory, its env file wins over `ROWBIT_DATABASE_URL`, which may be
/// left in the shell from the demo, and Rowbit says so when they differ.
pub fn pick_url(given: Option<String>) -> anyhow::Result<Option<String>> {
    if URL_FLAG.get().copied().unwrap_or(false) {
        return Ok(given);
    }
    let file = database_url()?;
    Ok(match (given, file) {
        (Some(shell), Some(file)) if shell != file => {
            if let Some(dir) = dir() {
                eprintln!(
                    "rowbit: using the database URL in {}, not ROWBIT_DATABASE_URL from your shell",
                    dir.join(ENV_FILE).display()
                );
            }
            Some(file)
        }
        (given, file) => given.or(file),
    })
}

/// The URL in an `env` file, refusing one that others can read.
pub fn read_env_file(file: &Path) -> anyhow::Result<Option<String>> {
    let src = match std::fs::read_to_string(file) {
        Ok(src) => src,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    };
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(file)?.permissions().mode();
        anyhow::ensure!(
            mode & 0o077 == 0,
            "{} holds the database's password and others can read it; run chmod 600 on it",
            file.display()
        );
    }
    Ok(env_value(&src, "ROWBIT_DATABASE_URL"))
}

/// `NAME=value` from a dotenv-style file: `export` and quotes allowed.
pub fn env_value(src: &str, name: &str) -> Option<String> {
    src.lines().find_map(|line| {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, value) = line.split_once('=')?;
        if key.trim() != name {
            return None;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        (!value.is_empty()).then(|| value.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_comes_out_of_the_arguments() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        let (rest, dir) = take_dir(args(&[
            "rowbit", "changes", "--dir", "/x", "--state", "held",
        ]))
        .unwrap();
        assert_eq!(rest, args(&["rowbit", "changes", "--state", "held"]));
        assert_eq!(dir, Some(PathBuf::from("/x")));
        let (rest, dir) = take_dir(args(&["rowbit", "serve", "--dir=/y"])).unwrap();
        assert_eq!(rest, args(&["rowbit", "serve"]));
        assert_eq!(dir, Some(PathBuf::from("/y")));
        let (rest, dir) = take_dir(args(&["rowbit", "propose", "--", "--dir"])).unwrap();
        assert_eq!(rest, args(&["rowbit", "propose", "--", "--dir"]));
        assert_eq!(dir, None);
        assert!(take_dir(args(&["rowbit", "serve", "--dir"])).is_err());
        assert_eq!(
            command(&args(&["rowbit", "--dir", "x", "setup"])),
            (Some("setup".into()), true)
        );
        assert_eq!(
            command(&args(&["rowbit", "changes", "--dir", "x"])),
            (Some("changes".into()), false)
        );
    }

    #[test]
    fn env_files_read_like_dotenv() {
        let src = "# app\nexport DATABASE_URL=\"mysql://a:b@h/app\"\nOTHER=1\n";
        assert_eq!(
            env_value(src, "DATABASE_URL").as_deref(),
            Some("mysql://a:b@h/app")
        );
        assert_eq!(
            env_value("DATABASE_URL='x'", "DATABASE_URL").as_deref(),
            Some("x")
        );
        assert_eq!(env_value("DATABASE_URL=", "DATABASE_URL"), None);
        assert_eq!(env_value("MY_DATABASE_URL=x", "DATABASE_URL"), None);
    }
}
