//! `rowbit connect` against a home directory of its own, with a fake
//! `claude` on PATH. See "rowbit connect" in docs/spec/setup.md.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Case {
    dir: PathBuf,
    home: PathBuf,
    bin: PathBuf,
}

impl Case {
    fn new(name: &str) -> Case {
        let root =
            std::env::temp_dir().join(format!("rowbit-connect-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (dir, home, bin) = (root.join("work"), root.join("home"), root.join("bin"));
        for d in [&dir, &home, &bin] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/quickstart/rowbit.yml"),
            dir.join("rowbit.yml"),
        )
        .unwrap();
        Case { dir, home, bin }
    }

    fn http(&self) {
        std::fs::write(
            self.dir.join("rowbit-server.yml"),
            "version: 0\nlisten: 127.0.0.1:1\nmcp:\n  http: true\n",
        )
        .unwrap();
    }

    /// A `claude` that records its arguments, and says whether `rowbit`
    /// exists by whether it has been added.
    fn fake_claude(&self) -> PathBuf {
        let log = self.bin.join("claude.log");
        let script = format!(
            "#!/bin/sh\necho \"$@\" >> '{log}'\ncase \"$2\" in get) grep -q ' add ' '{log}' ;; *) exit 0 ;; esac\n",
            log = log.display()
        );
        let path = self.bin.join("claude");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        log
    }

    fn run(&self, args: &[&str]) -> (bool, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_rowbit"))
            .args(args)
            .current_dir(&self.dir)
            .env("HOME", &self.home)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("ROWBIT_DATABASE_URL")
            .env_remove("ROWBIT_SERVER_CONFIG")
            .env_remove("ROWBIT_POLICY")
            .env_remove("ROWBIT_STORE")
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }
}

fn token_in(text: &str) -> String {
    let at = text.find("rbt_").expect("a token");
    text[at..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

#[test]
fn cursor_gets_the_address_and_a_token_of_its_own() {
    let c = Case::new("cursor");
    c.http();
    let (ok, out) = c.run(&["connect", "cursor"]);
    assert!(!ok && out.contains("doesn't look installed"), "{out}");

    std::fs::create_dir_all(c.home.join(".cursor")).unwrap();
    let (ok, out) = c.run(&["connect", "cursor"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("as coding-agent, over HTTP at http://127.0.0.1:1/mcp"),
        "{out}"
    );
    assert!(out.contains("Nothing answers"), "{out}");
    assert!(!out.contains("rbt_"), "the token isn't printed: {out}");
    let file = std::fs::read_to_string(c.home.join(".cursor/mcp.json")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&file).unwrap();
    let header = doc["mcpServers"]["rowbit"]["headers"]["Authorization"]
        .as_str()
        .unwrap();
    assert!(header.starts_with("Bearer rbt_"), "{file}");
    assert!(out.contains(&header[header.len() - 4..]), "{out}");

    // The entry exists, and the agent already has a token.
    let (ok, out) = c.run(&["connect", "cursor"]);
    assert!(!ok && out.contains("--force"), "{out}");
    let (ok, out) = c.run(&["connect", "cursor", "--force"]);
    assert!(!ok && out.contains("--new-token"), "{out}");
    assert_eq!(
        std::fs::read_to_string(c.home.join(".cursor/mcp.json")).unwrap(),
        file
    );
    let (ok, out) = c.run(&["connect", "cursor", "--force", "--new-token"]);
    assert!(ok, "{out}");
    assert_ne!(
        std::fs::read_to_string(c.home.join(".cursor/mcp.json")).unwrap(),
        file
    );
    assert_eq!(
        std::fs::read_to_string(c.home.join(".cursor/mcp.json.rowbit-backup")).unwrap(),
        file
    );
}

#[test]
fn a_file_that_doesnt_parse_costs_no_token() {
    let c = Case::new("unparsed");
    c.http();
    std::fs::create_dir_all(c.home.join(".codeium/windsurf")).unwrap();
    let file = c.home.join(".codeium/windsurf/mcp_config.json");
    std::fs::write(&file, "{ // mine\n}").unwrap();
    let (ok, out) = c.run(&["connect", "windsurf"]);
    assert!(!ok && out.contains("isn't strict JSON"), "{out}");
    assert!(!c.dir.join("rowbit.db").exists(), "no token was made");
    // --print still works, and shows the whole token.
    let (ok, out) = c.run(&["connect", "windsurf", "--print"]);
    assert!(
        ok && out.contains("\"serverUrl\"") && out.contains("Bearer rbt_"),
        "{out}"
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "{ // mine\n}");
}

#[test]
fn claude_code_is_set_up_with_its_own_command() {
    let c = Case::new("claude");
    c.http();
    let log = c.fake_claude();
    let (ok, out) = c.run(&["connect", "claude-code", "--scope", "project"]);
    assert!(!ok && out.contains("committed"), "{out}");
    let (ok, out) = c.run(&["connect", "claude-code"]);
    assert!(ok, "{out}");
    assert!(out.contains("Start Claude Code in this directory"), "{out}");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.starts_with("mcp get rowbit\n"), "{calls}");
    assert!(
        calls.contains(
            "mcp add --scope local --transport http rowbit http://127.0.0.1:1/mcp --header Authorization: Bearer rbt_"
        ),
        "{calls}"
    );
    let (ok, out) = c.run(&["connect", "claude-code", "--new-token"]);
    assert!(
        !ok && out.contains("already has a server called rowbit"),
        "{out}"
    );
}

#[test]
fn over_stdio_the_client_starts_rowbit_with_the_database_url() {
    let c = Case::new("stdio");
    std::fs::create_dir_all(c.home.join(".gemini")).unwrap();
    let (ok, out) = c.run(&["connect", "gemini"]);
    assert!(!ok && out.contains("ROWBIT_DATABASE_URL"), "{out}");
    let (ok, out) = c.run(&["connect", "gemini", "--url", "mysql://r:p@127.0.0.1:1/shop"]);
    assert!(
        ok && out.contains("over stdio") && out.contains("around Rowbit"),
        "{out}"
    );
    let doc: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(c.home.join(".gemini/settings.json")).unwrap(),
    )
    .unwrap();
    let entry = &doc["mcpServers"]["rowbit"];
    assert_eq!(
        entry["env"]["ROWBIT_DATABASE_URL"],
        "mysql://r:p@127.0.0.1:1/shop"
    );
    let args: Vec<&str> = entry["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(&args[..3], ["mcp", "--agent", "coding-agent"]);
    assert!(
        Path::new(args[4]).is_absolute() && args[4].ends_with("rowbit.yml"),
        "{args:?}"
    );
    assert!(Path::new(entry["command"].as_str().unwrap()).is_absolute());
}

#[test]
fn lists_clients_and_prints_an_address() {
    let c = Case::new("list");
    std::fs::create_dir_all(c.home.join(".codex")).unwrap();
    let (ok, out) = c.run(&["connect"]);
    assert!(ok, "{out}");
    let codex = out.lines().find(|l| l.contains("codex")).unwrap();
    assert!(codex.ends_with(" found"), "{out}");
    let cursor = out.lines().find(|l| l.contains("cursor")).unwrap();
    assert!(cursor.ends_with("not found"), "{out}");

    let (ok, out) = c.run(&["connect", "--print"]);
    assert!(!ok && out.contains("mcp.http"), "{out}");
    c.http();
    let (ok, out) = c.run(&["connect", "--print"]);
    assert!(
        ok && out.contains("URL:    http://127.0.0.1:1/mcp"),
        "{out}"
    );
    let token = token_in(&out);
    assert_eq!(token.len(), 4 + 43, "{out}");
}
