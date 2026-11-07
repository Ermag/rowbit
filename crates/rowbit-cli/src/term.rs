//! Links people can click in a terminal. Plain URLs depend on the
//! terminal spotting them, and some only open them on ⌘-click or not at
//! all; OSC 8 marks the text as a link outright. See "Links in the
//! terminal" in docs/spec/setup.md.

use std::io::IsTerminal;

/// `url` as a link where the terminal is known to show OSC 8 links, and
/// as it is otherwise (a pipe, a log, an unknown terminal).
pub fn link(url: &str) -> String {
    link_if(url, std::io::stdout().is_terminal())
}

/// [`link`], for standard error.
pub fn link_err(url: &str) -> String {
    link_if(url, std::io::stderr().is_terminal())
}

fn link_if(url: &str, terminal: bool) -> String {
    if terminal && supports(|k| std::env::var(k).ok()) {
        osc8(url, url)
    } else {
        url.to_owned()
    }
}

fn osc8(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Whether the environment names a terminal that shows OSC 8 links.
/// `ROWBIT_HYPERLINKS=0` or `1` decides; otherwise a terminal on the list,
/// and never inside tmux or screen, which drop the sequence unless set up
/// to pass it on.
fn supports(var: impl Fn(&str) -> Option<String>) -> bool {
    match var("ROWBIT_HYPERLINKS").as_deref() {
        Some("0") => return false,
        Some("1") => return true,
        _ => {}
    }
    if var("TMUX").is_some() || var("STY").is_some() {
        return false;
    }
    if var("TERM").as_deref() == Some("dumb") {
        return false;
    }
    let program = var("TERM_PROGRAM").unwrap_or_default();
    matches!(
        program.as_str(),
        "ghostty" | "iTerm.app" | "WezTerm" | "vscode" | "Hyper" | "rio" | "WarpTerminal"
    ) || var("TERM").is_some_and(|t| t == "xterm-kitty" || t == "xterm-ghostty")
        || var("KITTY_WINDOW_ID").is_some()
        || var("WT_SESSION").is_some()
        || var("VTE_VERSION")
            .and_then(|v| v.parse::<u32>().ok())
            .is_some_and(|v| v >= 5000)
}

/// Opens `url` in the default browser. Returns whether that started.
pub fn open_browser(url: &str) -> bool {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// Opens `url` in the browser once something answers there, from a
/// thread of its own, giving up after ten seconds.
pub fn open_when_up(url: String) {
    std::thread::spawn(move || wait_and_open(&url, 50));
}

/// Opens `url` in the browser once something answers there, trying
/// `tries` times, 200ms apart. Returns whether it opened.
pub fn wait_and_open(url: &str, tries: u32) -> bool {
    let Some(addr) = url
        .strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .and_then(|host| std::net::ToSocketAddrs::to_socket_addrs(host).ok())
        .and_then(|mut a| a.next())
    else {
        return false;
    };
    for _ in 0..tries {
        if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(200))
            .is_ok()
        {
            return open_browser(url);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn known_terminals_get_links() {
        assert!(supports(env(&[("TERM_PROGRAM", "ghostty")])));
        assert!(supports(env(&[("TERM_PROGRAM", "iTerm.app")])));
        assert!(supports(env(&[("TERM", "xterm-kitty")])));
        assert!(supports(env(&[("VTE_VERSION", "7600")])));
        assert!(!supports(env(&[("TERM_PROGRAM", "Apple_Terminal")])));
        assert!(!supports(env(&[])));
        assert!(!supports(env(&[
            ("TERM_PROGRAM", "ghostty"),
            ("TMUX", "/tmp/x")
        ])));
        assert!(!supports(env(&[
            ("TERM_PROGRAM", "ghostty"),
            ("ROWBIT_HYPERLINKS", "0")
        ])));
        assert!(supports(env(&[("ROWBIT_HYPERLINKS", "1")])));
    }

    #[test]
    fn a_link_shows_its_address() {
        assert_eq!(
            osc8("http://127.0.0.1:8080/", "http://127.0.0.1:8080/"),
            "\x1b]8;;http://127.0.0.1:8080/\x1b\\http://127.0.0.1:8080/\x1b]8;;\x1b\\"
        );
    }
}
