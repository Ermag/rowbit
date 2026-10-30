//! `rowbit demo`'s refusals, with a fake `docker` on PATH. The real run,
//! with Docker, is in CI (see .github/workflows/ci.yml).

use std::path::{Path, PathBuf};
use std::process::Command;

fn root(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rowbit-demo-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("bin")).unwrap();
    d
}

/// A `docker` that exits with `code` for every command.
fn fake_docker(root: &Path, code: u8) {
    let path = root.join("bin/docker");
    std::fs::write(&path, format!("#!/bin/sh\nexit {code}\n")).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

fn rowbit(root: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rowbit"))
        .args(args)
        .current_dir(root)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[test]
fn says_what_docker_is_missing() {
    let r = root("docker");
    let (ok, out) = rowbit(&r, &["demo", "--no-connect"]);
    assert!(!ok && out.contains("needs Docker with Compose v2"), "{out}");
    fake_docker(&r, 1);
    let (ok, out) = rowbit(&r, &["demo", "--no-connect"]);
    assert!(!ok && out.contains("needs Docker with Compose v2"), "{out}");
    assert!(!r.join("rowbit-demo").exists(), "nothing written");
}

#[test]
fn never_writes_into_a_directory_of_yours() {
    let r = root("yours");
    fake_docker(&r, 0);
    std::fs::create_dir_all(r.join("mine")).unwrap();
    std::fs::write(r.join("mine/rowbit.yml"), "mine").unwrap();
    let (ok, out) = rowbit(&r, &["demo", "--dir", "mine", "--no-connect"]);
    assert!(!ok && out.contains("isn't a Rowbit demo"), "{out}");
    let (ok, out) = rowbit(&r, &["demo", "clean", "--dir", "mine"]);
    assert!(!ok && out.contains("nothing removed"), "{out}");
    assert_eq!(
        std::fs::read_to_string(r.join("mine/rowbit.yml")).unwrap(),
        "mine"
    );
}

#[test]
fn writes_the_quickstart_files_with_its_port() {
    let r = root("files");
    // Compose "succeeds" without starting anything, so the demo stops at
    // the database it can't reach, after writing its files.
    fake_docker(&r, 0);
    let (ok, out) = rowbit(&r, &["demo", "--port", "1", "--no-connect"]);
    assert!(!ok, "{out}");
    let dir = r.join("rowbit-demo");
    let compose = std::fs::read_to_string(dir.join("compose.yaml")).unwrap();
    assert!(
        compose.contains("127.0.0.1:1:3306") && compose.contains("name: rowbit-demo"),
        "{compose}"
    );
    assert!(
        std::fs::read_to_string(dir.join("rowbit-server.yml"))
            .unwrap()
            .contains("http: true")
    );
    let marker = std::fs::read_to_string(dir.join(".rowbit-demo")).unwrap();
    assert!(marker.contains("@127.0.0.1:1/shop"), "{marker}");

    std::fs::write(dir.join("notes.txt"), "mine").unwrap();
    let (ok, out) = rowbit(&r, &["demo", "clean"]);
    assert!(ok, "{out}");
    let left: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, ["notes.txt"]);
}
