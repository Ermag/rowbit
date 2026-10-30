//! `rowbit-server.yml`: how one Rowbit runs. See `docs/spec/server.md`.
//!
//! Secrets are never written in the file: a field that holds one must be a
//! `${VARIABLE}` reference, read from the environment when the file loads,
//! so the file can be committed and reviewed.

use std::collections::BTreeSet;
use std::fmt;
use std::net::SocketAddr;

use serde::Deserialize;
use thiserror::Error;

pub const SCHEMA_VERSION: u32 = 0;
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8080";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("rowbit-server.yml is not valid YAML for schema v0: {0}")]
    Parse(#[from] serde_norway::Error),
    #[error("rowbit-server.yml: {0}")]
    Invalid(String),
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError::Invalid(msg.into()))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u32,
    public_url: Option<String>,
    listen: Option<SocketAddr>,
    reviewers: Option<ReviewersFile>,
    #[serde(default)]
    notify: Vec<NotifyFile>,
    mcp: Option<McpFile>,
    agent_users: Option<AgentUsersFile>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentUsersFile {
    host: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpFile {
    #[serde(default)]
    http: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyFile {
    name: String,
    kind: NotifyKind,
    url: String,
    secret: Option<String>,
    agents: Option<Vec<String>>,
}

/// How a notification is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyKind {
    /// A Slack incoming webhook. Its URL is the secret.
    Slack,
    /// Signed JSON to any URL.
    Webhook,
}

/// Where to announce held changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notify {
    pub name: String,
    pub kind: NotifyKind,
    pub url: Secret,
    /// The signing key, for a webhook.
    pub secret: Option<Secret>,
    pub agents: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewersFile {
    oidc: OidcFile,
    allow: Allow,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct OidcFile {
    issuer: String,
    client_id: String,
    client_secret: String,
    #[serde(default)]
    scopes: Vec<String>,
}

/// Who may sign in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allow {
    #[serde(default)]
    pub emails: BTreeSet<String>,
    #[serde(default)]
    pub domains: BTreeSet<String>,
    #[serde(default)]
    pub groups: BTreeSet<String>,
}

impl Allow {
    /// Whether a verified `email` in `groups` may sign in.
    pub fn admits(&self, email: &str, groups: &[String]) -> bool {
        let email = email.to_ascii_lowercase();
        let domain = email.rsplit_once('@').map(|(_, d)| d);
        self.emails.iter().any(|e| e.eq_ignore_ascii_case(&email))
            || domain.is_some_and(|d| self.domains.iter().any(|a| a.eq_ignore_ascii_case(d)))
            || groups.iter().any(|g| self.groups.contains(g))
    }
}

/// A value read from the environment. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// Sign-in through an OpenID Connect provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Oidc {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Secret,
    /// Asked for beyond `openid` and `email`, such as `groups`.
    pub scopes: Vec<String>,
    pub allow: Allow,
}

/// A loaded, checked `rowbit-server.yml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerConfig {
    /// Without a trailing `/`.
    pub public_url: Option<String>,
    pub listen: Option<SocketAddr>,
    pub oidc: Option<Oidc>,
    pub notify: Vec<Notify>,
    /// Serve agents over MCP at `/mcp`.
    pub mcp_http: bool,
    /// The host part for scoped agents' database users, if not Rowbit's own.
    pub agent_users_host: Option<String>,
}

impl ServerConfig {
    /// Parses and checks the file, reading `${VAR}` secrets with `env`.
    pub fn from_yaml(
        src: &str,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<ServerConfig, ConfigError> {
        let file: File = serde_norway::from_str(src)?;
        if file.version != SCHEMA_VERSION {
            return invalid(format!(
                "unsupported version {}; this Rowbit reads version {SCHEMA_VERSION}",
                file.version
            ));
        }
        let public_url = match &file.public_url {
            Some(url) => Some(
                check_url("public_url", url)?
                    .trim_end_matches('/')
                    .to_owned(),
            ),
            None => None,
        };
        let oidc = match file.reviewers {
            None => None,
            Some(r) => {
                check_url("reviewers.oidc.issuer", &r.oidc.issuer)?;
                if r.oidc.client_id.trim().is_empty() {
                    return invalid("reviewers.oidc.client_id is empty");
                }
                if r.allow == Allow::default() {
                    return invalid(
                        "reviewers.allow needs at least one of emails, domains or groups; an empty list would let nobody in",
                    );
                }
                if let Some(d) = r
                    .allow
                    .domains
                    .iter()
                    .find(|d| d.contains('@') || d.is_empty())
                {
                    return invalid(format!(
                        "reviewers.allow.domains: {d:?} should be a domain like example.com"
                    ));
                }
                Some(Oidc {
                    issuer: r.oidc.issuer,
                    client_id: r.oidc.client_id,
                    client_secret: from_env(
                        "reviewers.oidc.client_secret",
                        &r.oidc.client_secret,
                        &env,
                    )?,
                    scopes: r.oidc.scopes,
                    allow: r.allow,
                })
            }
        };
        let mut notify = Vec::new();
        let mut names = BTreeSet::new();
        for n in file.notify {
            let at = format!("notify.{}", n.name);
            if n.name.trim().is_empty() || !names.insert(n.name.clone()) {
                return invalid("every notify target needs a name of its own");
            }
            // A Slack webhook's URL is the secret; a webhook's may be plain.
            let url = match n.kind {
                NotifyKind::Slack => from_env(&format!("{at}.url"), &n.url, &env)?,
                NotifyKind::Webhook if n.url.starts_with("${") => {
                    from_env(&format!("{at}.url"), &n.url, &env)?
                }
                NotifyKind::Webhook => Secret(n.url.clone()),
            };
            check_url(&format!("{at}.url"), url.expose())?;
            let secret = match (n.kind, &n.secret) {
                (NotifyKind::Webhook, Some(s)) => Some(from_env(&format!("{at}.secret"), s, &env)?),
                (NotifyKind::Webhook, None) => {
                    return invalid(format!("{at}: a webhook needs a secret to sign with"));
                }
                (NotifyKind::Slack, Some(_)) => {
                    return invalid(format!(
                        "{at}: Slack webhooks aren't signed; leave out secret"
                    ));
                }
                (NotifyKind::Slack, None) => None,
            };
            notify.push(Notify {
                name: n.name,
                kind: n.kind,
                url,
                secret,
                agents: n.agents,
            });
        }
        let agent_users_host = file.agent_users.and_then(|a| a.host);
        if let Some(host) = &agent_users_host
            && (host.is_empty() || host.contains('\\') || host.contains('\''))
        {
            return invalid(
                "agent_users.host must be a host part such as 10.0.0.%, without quotes or backslashes",
            );
        }
        if oidc.is_some() && public_url.is_none() {
            return invalid(
                "public_url is needed with reviewers.oidc: the provider redirects back to it",
            );
        }
        Ok(ServerConfig {
            public_url,
            listen: file.listen,
            oidc,
            notify,
            mcp_http: file.mcp.is_some_and(|m| m.http),
            agent_users_host,
        })
    }

    /// Just `agent_users.host`, for `rowbit grants`, which shouldn't need
    /// the sign-in and notification secrets the rest of the file reads.
    pub fn agent_users_host(src: &str) -> Result<Option<String>, ConfigError> {
        let v: serde_norway::Value = serde_norway::from_str(src)?;
        Ok(v.get("agent_users")
            .and_then(|a| a.get("host"))
            .and_then(|h| h.as_str())
            .map(str::to_owned))
    }

    /// Where to listen: `flag` if given, else the file's, else the default.
    pub fn listen(&self, flag: Option<SocketAddr>) -> SocketAddr {
        flag.or(self.listen)
            .unwrap_or_else(|| DEFAULT_LISTEN.parse().expect("a valid address"))
    }

    /// Checks that serving on `addr` is safe with these settings: off
    /// loopback, people must sign in through a provider, behind HTTPS.
    pub fn check_listen(&self, addr: SocketAddr) -> Result<(), ConfigError> {
        if addr.ip().is_loopback() {
            return Ok(());
        }
        if self.oidc.is_none() {
            return invalid(format!(
                "{addr} is reachable beyond this machine, so reviewers must sign in through an identity provider; \
                 set reviewers.oidc, or listen on 127.0.0.1"
            ));
        }
        match &self.public_url {
            Some(url) if url.starts_with("https://") => Ok(()),
            _ => invalid(format!(
                "{addr} is reachable beyond this machine, so public_url must be https:// (Rowbit doesn't terminate TLS; put it behind a proxy that does)"
            )),
        }
    }

    /// Whether cookies should be marked `Secure`.
    pub fn https(&self) -> bool {
        self.public_url
            .as_deref()
            .is_some_and(|u| u.starts_with("https://"))
    }

    /// The `host[:port]` of `public_url`, which requests may be addressed to.
    pub fn public_host(&self) -> Option<String> {
        let url = self.public_url.as_deref()?;
        let rest = url.split_once("://")?.1;
        Some(rest.split('/').next().unwrap_or(rest).to_owned())
    }
}

/// `https://`, or `http://` to a loopback address.
fn check_url<'a>(key: &str, url: &'a str) -> Result<&'a str, ConfigError> {
    if let Some(rest) = url.strip_prefix("https://") {
        if rest.is_empty() || rest.starts_with('/') {
            return invalid(format!("{key}: {url:?} has no host"));
        }
        return Ok(url);
    }
    if let Some(rest) = url.strip_prefix("http://") {
        let authority = rest.split('/').next().unwrap_or("");
        let host = match authority.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or(""),
            None => authority.split(':').next().unwrap_or(""),
        };
        if matches!(host, "127.0.0.1" | "localhost" | "::1") {
            return Ok(url);
        }
    }
    invalid(format!(
        "{key}: {url:?} must be https://, or http:// to 127.0.0.1 or localhost"
    ))
}

/// A `${VARIABLE}` reference, read from the environment. A literal value is
/// an error, so secrets never end up in the file.
fn from_env(
    key: &str,
    value: &str,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<Secret, ConfigError> {
    let name = value
        .strip_prefix("${")
        .and_then(|v| v.strip_suffix('}'))
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    let Some(name) = name else {
        return invalid(format!(
            "{key} must be a ${{VARIABLE}} reference to the environment, not the secret itself"
        ));
    };
    match env(name) {
        Some(v) if !v.is_empty() => Ok(Secret(v)),
        _ => invalid(format!("{key}: environment variable {name} is not set")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = "version: 0\npublic_url: https://rowbit.example.com/\nlisten: 0.0.0.0:8080\nreviewers:\n  oidc:\n    issuer: https://accounts.google.com\n    client_id: abc\n    client_secret: ${ROWBIT_OIDC_CLIENT_SECRET}\n  allow:\n    domains: [example.com]\n    emails: [Ann@Elsewhere.dev]\n";

    fn env(name: &str) -> Option<String> {
        (name == "ROWBIT_OIDC_CLIENT_SECRET").then(|| "s3cret".to_owned())
    }

    #[test]
    fn a_full_file_loads() {
        let c = ServerConfig::from_yaml(EXAMPLE, env).unwrap();
        assert_eq!(c.public_url.as_deref(), Some("https://rowbit.example.com"));
        assert_eq!(c.public_host().as_deref(), Some("rowbit.example.com"));
        let oidc = c.oidc.as_ref().unwrap();
        assert_eq!(oidc.client_secret.expose(), "s3cret");
        assert!(!format!("{c:?}").contains("s3cret"), "secrets never print");
        assert!(c.check_listen(c.listen(None)).is_ok());
        assert!(c.https());
    }

    #[test]
    fn who_may_sign_in() {
        let c = ServerConfig::from_yaml(EXAMPLE, env).unwrap();
        let allow = &c.oidc.unwrap().allow;
        assert!(allow.admits("bob@example.com", &[]));
        assert!(allow.admits("BOB@EXAMPLE.COM", &[]));
        assert!(allow.admits("ann@elsewhere.dev", &[]));
        assert!(!allow.admits("bob@example.com.evil.dev", &[]));
        assert!(!allow.admits("bob@sub.example.com", &[]));
        assert!(!allow.admits("eve@elsewhere.dev", &["db-reviewers".into()]));
    }

    #[test]
    fn mistakes_are_errors() {
        for (src, needle) in [
            (
                EXAMPLE.replace("${ROWBIT_OIDC_CLIENT_SECRET}", "s3cret"),
                "reference",
            ),
            (
                EXAMPLE.replace("ROWBIT_OIDC_CLIENT_SECRET", "UNSET_VAR"),
                "not set",
            ),
            (
                EXAMPLE.replace("https://accounts", "http://accounts"),
                "https",
            ),
            (
                EXAMPLE.replace(
                    "    domains: [example.com]\n    emails: [Ann@Elsewhere.dev]\n",
                    "    {}\n",
                ),
                "at least one",
            ),
            (
                EXAMPLE.replace("public_url: https://rowbit.example.com/\n", ""),
                "public_url is needed",
            ),
            (
                EXAMPLE.replace("version: 0", "version: 0\nmcp: { https: true }"),
                "unknown field",
            ),
            (
                EXAMPLE.replace("[example.com]", "[\"@example.com\"]"),
                "domain",
            ),
        ] {
            let err = ServerConfig::from_yaml(&src, env).unwrap_err().to_string();
            assert!(err.contains(needle), "{src}\n gave {err}");
        }
    }

    #[test]
    fn notify_targets_keep_their_secrets_in_the_environment() {
        let env = |name: &str| match name {
            "SLACK" => Some("https://hooks.slack.com/services/T/B/x".to_owned()),
            "HOOK" => Some("k".to_owned()),
            _ => None,
        };
        let ok = "version: 0\nnotify:\n  - { name: s, kind: slack, url: \"${SLACK}\", agents: [bot] }\n  - { name: w, kind: webhook, url: https://h.example.com/rowbit, secret: \"${HOOK}\" }\n";
        let c = ServerConfig::from_yaml(ok, env).unwrap();
        assert_eq!(c.notify.len(), 2);
        assert_eq!(c.notify[1].secret.as_ref().unwrap().expose(), "k");
        for (src, needle) in [
            (
                "version: 0\nnotify:\n  - { name: s, kind: slack, url: https://hooks.slack.com/x }\n",
                "reference",
            ),
            (
                "version: 0\nnotify:\n  - { name: w, kind: webhook, url: https://h.example.com }\n",
                "needs a secret",
            ),
            (
                "version: 0\nnotify:\n  - { name: w, kind: webhook, url: https://h.example.com, secret: plain }\n",
                "reference",
            ),
            (
                "version: 0\nnotify:\n  - { name: w, kind: webhook, url: http://h.example.com, secret: \"${HOOK}\" }\n",
                "https",
            ),
            (
                "version: 0\nnotify:\n  - { name: w, kind: email, url: https://h.example.com }\n",
                "unknown variant",
            ),
            (
                "version: 0\nnotify:\n  - { name: s, kind: slack, url: \"${SLACK}\" }\n  - { name: s, kind: slack, url: \"${SLACK}\" }\n",
                "name of its own",
            ),
        ] {
            let err = ServerConfig::from_yaml(src, env).unwrap_err().to_string();
            assert!(err.contains(needle), "{src}\n gave {err}");
        }
    }

    #[test]
    fn off_loopback_needs_a_provider_and_https() {
        let plain = ServerConfig::from_yaml("version: 0\n", env).unwrap();
        assert!(
            plain
                .check_listen("127.0.0.1:8080".parse().unwrap())
                .is_ok()
        );
        assert!(plain.check_listen("[::1]:8080".parse().unwrap()).is_ok());
        let err = plain
            .check_listen("0.0.0.0:8080".parse().unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("identity provider"), "{err}");

        let src = EXAMPLE.replace("https://rowbit.example.com/", "http://127.0.0.1:8080");
        let local = ServerConfig::from_yaml(&src, env).unwrap();
        let err = local
            .check_listen("10.0.0.5:8080".parse().unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("https"), "{err}");
    }
}
