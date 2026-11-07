//! What a setup is connected to, for `rowbit status` and the review page's
//! Setup view: the database without its password, each agent's rules in
//! brief, and where each agent is connected. See "What Rowbit is
//! connected to" in docs/spec/setup.md.

use rowbit_policy::{Decision, Mode, PolicyFile};
use rowbit_store::{Store, StoreError};
use serde::Serialize;

/// The database a URL names. Never the password.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Database {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    /// "no TLS", "TLS, certificate and host name checked", ...
    pub tls: String,
}

impl Database {
    pub fn from_url(url: &str) -> Option<Database> {
        let opts = rowbit_mysql::url::opts(url).ok()?;
        Some(Database {
            host: opts.ip_or_hostname().to_owned(),
            port: opts.tcp_port(),
            database: opts.db_name().unwrap_or_default().to_owned(),
            user: opts.user().unwrap_or_default().to_owned(),
            tls: rowbit_mysql::url::describe(&opts).to_owned(),
        })
    }

    /// "shop on db.internal:3306 as rowbit, TLS ..."
    pub fn line(&self) -> String {
        format!(
            "{} on {}:{} as {}, {}",
            self.database, self.host, self.port, self.user, self.tls
        )
    }
}

/// One agent in `rowbit.yml`, and where it is connected.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Agent {
    pub id: String,
    pub description: Option<String>,
    /// "enforce" or "shadow".
    pub mode: &'static str,
    /// What happens to a write no rule matches: "allow", "hold", "block".
    pub default: &'static str,
    pub rules: usize,
    pub scoped: bool,
    pub connections: Vec<Connection>,
}

impl Agent {
    /// "holds every write", "3 rules, else holds", ...
    pub fn rules_line(&self) -> String {
        let default = match self.default {
            "allow" => "allows",
            "block" => "refuses",
            _ => "holds",
        };
        let mut s = match self.rules {
            0 => format!("{default} every write"),
            1 => format!("1 rule, else {default}"),
            n => format!("{n} rules, else {default}"),
        };
        if self.mode == "shadow" {
            s.push_str(" (shadow mode: logged, not enforced)");
        }
        if self.scoped {
            s.push_str(", scoped");
        }
        s
    }
}

/// A token an agent signs in with over HTTP: one per client entry
/// `rowbit connect` made, and the one `rowbit agents token` made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Connection {
    /// "Claude Code in ~/projects/app"
    pub label: String,
    pub created_ms: i64,
    pub used_ms: Option<i64>,
}

/// The label of the token `rowbit agents token` makes.
pub const OWN_TOKEN: &str = "single token (rowbit agents token, or rowbit connect before 0.2.3)";

pub fn agents(policy: &PolicyFile, store: &Store) -> Result<Vec<Agent>, StoreError> {
    let tokens = store.connection_tokens()?;
    let mut out = Vec::new();
    for (id, a) in &policy.agents {
        let mut connections = Vec::new();
        if let Some((created_ms, used_ms)) = store.agent_token_times(id)? {
            connections.push(Connection {
                label: OWN_TOKEN.into(),
                created_ms,
                used_ms,
            });
        }
        connections.extend(
            tokens
                .iter()
                .filter(|t| &t.agent == id)
                .map(|t| Connection {
                    label: t.label.clone(),
                    created_ms: t.created_ms,
                    used_ms: t.used_ms,
                }),
        );
        out.push(Agent {
            id: id.clone(),
            description: a.description.clone(),
            mode: match a.mode {
                Mode::Enforce => "enforce",
                Mode::Shadow => "shadow",
            },
            default: match a.default {
                Decision::Allow => "allow",
                Decision::Hold => "hold",
                Decision::Block => "block",
            },
            rules: a.rules.len(),
            scoped: a.scopes.is_some(),
            connections,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_database_comes_without_its_password() {
        let d = Database::from_url(
            "mysql://rowbit:s3cret@db.internal:3307/shop?ssl-mode=VERIFY_IDENTITY",
        )
        .unwrap();
        assert_eq!(
            d.line(),
            "shop on db.internal:3307 as rowbit, TLS, certificate and host name checked"
        );
        assert!(!format!("{d:?}").contains("s3cret"));
    }

    #[test]
    fn agents_list_their_rules_and_connections() {
        let policy = PolicyFile::from_yaml(
            "version: 0\nagents:\n  bot:\n    default: hold\n  ops:\n    mode: shadow\n    default: block\n    rules:\n      - name: no deletes\n        when: { statement: delete }\n        then: block\n",
        )
        .unwrap();
        let mut store = Store::open_in_memory().unwrap();
        store
            .issue_connection_token("bot", "Claude Code in ~/app")
            .unwrap();
        let agents = agents(&policy, &store).unwrap();
        assert_eq!(agents[0].rules_line(), "holds every write");
        assert_eq!(agents[0].connections[0].label, "Claude Code in ~/app");
        assert_eq!(
            agents[1].rules_line(),
            "1 rule, else refuses (shadow mode: logged, not enforced)"
        );
        assert!(agents[1].connections.is_empty());
    }
}
