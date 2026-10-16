//! `rowbit serve` and `rowbit server check` refuse unsafe settings before
//! touching the database.

use std::process::Command;

fn rowbit(args: &[&str], dir: &std::path::Path) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rowbit"))
        .args(args)
        .current_dir(dir)
        .env("ROWBIT_DATABASE_URL", "mysql://nobody@127.0.0.1:1/none")
        .env_remove("ROWBIT_SERVER_CONFIG")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("rowbit-serve-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn no_network_listening_without_an_identity_provider() {
    let d = dir("net");
    let (ok, out) = rowbit(&["serve", "--listen", "0.0.0.0:0"], &d);
    assert!(!ok, "{out}");
    assert!(out.contains("identity provider"), "{out}");
}

#[test]
fn server_check_reads_secrets_from_the_environment_only() {
    let d = dir("check");
    std::fs::write(
        d.join("rowbit-server.yml"),
        "version: 0\npublic_url: https://rowbit.example.com\nlisten: 0.0.0.0:8080\nreviewers:\n  oidc:\n    issuer: https://id.example.com\n    client_id: rowbit\n    client_secret: ${ROWBIT_TEST_UNSET_SECRET}\n  allow:\n    domains: [example.com]\n",
    )
    .unwrap();
    let (ok, out) = rowbit(&["server", "check"], &d);
    assert!(
        !ok && out.contains("ROWBIT_TEST_UNSET_SECRET is not set"),
        "{out}"
    );

    let (ok, out) = rowbit(&["server", "check", "--config", "missing.yml"], &d);
    assert!(!ok && out.contains("does not exist"), "{out}");
}

#[test]
fn agent_tokens_are_shown_once_and_revocable() {
    let d = dir("tokens");
    std::fs::write(
        d.join("rowbit.yml"),
        "version: 0\nagents:\n  refund-bot: {}\n",
    )
    .unwrap();
    let (ok, out) = rowbit(&["agents", "token", "stranger"], &d);
    assert!(!ok && out.contains("not an agent"), "{out}");
    let (ok, out) = rowbit(&["agents", "token", "refund-bot"], &d);
    assert!(ok, "{out}");
    let token = out
        .lines()
        .find(|l| l.starts_with("rbt_"))
        .expect("the token")
        .to_owned();
    assert_eq!(token.len(), 47);
    let (ok, out) = rowbit(&["agents"], &d);
    assert!(
        ok && out.contains("refund-bot  token made") && out.contains("never used"),
        "{out}"
    );
    assert!(!out.contains(&token));
    let (ok, out) = rowbit(&["agents", "revoke", "refund-bot"], &d);
    assert!(ok && out.contains("revoked"), "{out}");
    let (_, out) = rowbit(&["agents"], &d);
    assert!(out.contains("refund-bot  no token"), "{out}");
}
