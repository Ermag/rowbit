use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;

use crate::PolicyError;
use crate::scopes::{MAX_SCOPED_AGENT_ID, Scopes};

pub const SCHEMA_VERSION: u32 = 0;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    pub version: u32,
    /// Keyed by agent id. An agent missing from this map is blocked.
    #[serde(default)]
    pub agents: BTreeMap<String, AgentPolicy>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPolicy {
    #[serde(default)]
    pub description: Option<String>,
    /// What happens when no rule matches. `hold` or `block`, never `allow`.
    #[serde(default = "default_decision")]
    pub default: Decision,
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// What the agent may touch at all. `None` means Rowbit's own
    /// privileges, as in v0.1.
    #[serde(default)]
    pub scopes: Option<Scopes>,
}

fn default_decision() -> Decision {
    Decision::Hold
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub name: String,
    /// All conditions must hold. An empty `when` matches every change.
    #[serde(default)]
    pub when: Match,
    pub then: Decision,
    /// Shown to reviewers, and to the agent when the change is held or blocked.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Match {
    pub statement: Option<OneOrMany<StatementKind>>,
    /// `database.table` patterns; `*` matches any run of characters.
    pub table: Option<OneOrMany<String>>,
    /// Matches when the change writes any of these columns.
    pub columns: Option<OneOrMany<String>>,
    /// Number of affected rows, inclusive bounds.
    pub rows: Option<Range>,
    pub sum: Option<SumMatch>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    pub fn as_slice(&self) -> &[T] {
        match self {
            OneOrMany::One(t) => std::slice::from_ref(t),
            OneOrMany::Many(v) => v,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    pub min: Option<u64>,
    pub max: Option<u64>,
}

/// Sum of `column` over the affected rows: after images for inserts and
/// updates, before images for deletes. Inclusive bounds.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SumMatch {
    pub column: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatementKind {
    Insert,
    Update,
    Delete,
    Ddl,
}

/// Ordered from least to most restrictive, so `max` picks the stricter one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Hold,
    Block,
}

fn invalid(msg: String) -> PolicyError {
    PolicyError::Invalid(msg)
}

fn valid_agent_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

impl PolicyFile {
    pub(crate) fn validate(&self) -> Result<(), PolicyError> {
        if self.version != SCHEMA_VERSION {
            return Err(invalid(format!(
                "unsupported version {}; this Rowbit reads version {SCHEMA_VERSION}",
                self.version
            )));
        }
        for (agent, policy) in &self.agents {
            if !valid_agent_id(agent) {
                return Err(invalid(format!(
                    "agent id {agent:?} must be 1-64 characters of a-z, 0-9, '-' or '_', starting with a letter or digit"
                )));
            }
            if policy.default == Decision::Allow {
                return Err(invalid(format!(
                    "agents.{agent}.default cannot be allow; use hold or block"
                )));
            }
            if let Some(scopes) = &policy.scopes {
                if agent.len() > MAX_SCOPED_AGENT_ID {
                    return Err(invalid(format!(
                        "agent id {agent:?} has scopes, so it can be at most {MAX_SCOPED_AGENT_ID} characters (its database user's name is rowbit_{agent})"
                    )));
                }
                scopes.validate(&format!("agents.{agent}"))?;
            }
            let mut names = HashSet::new();
            for rule in &policy.rules {
                let at = format!("agents.{agent}.rules.{}", rule.name);
                if rule.name.trim().is_empty() {
                    return Err(invalid(format!("agents.{agent}: every rule needs a name")));
                }
                if !names.insert(rule.name.as_str()) {
                    return Err(invalid(format!("{at}: duplicate rule name")));
                }
                rule.when.validate(&at)?;
                if rule.then == Decision::Allow
                    && rule
                        .when
                        .statement
                        .as_ref()
                        .is_some_and(|s| s.as_slice().contains(&StatementKind::Ddl))
                {
                    return Err(invalid(format!(
                        "{at}: DDL is always held at least, so it cannot be allowed"
                    )));
                }
            }
        }
        Ok(())
    }
}

impl PolicyFile {
    /// Things that are valid but probably not what the author meant.
    pub fn warnings(&self) -> Vec<String> {
        self.agents
            .iter()
            .filter_map(|(id, a)| a.scopes.as_ref().map(|s| s.warnings(id)))
            .flatten()
            .collect()
    }

    /// Whether any agent has scopes.
    pub fn has_scopes(&self) -> bool {
        self.agents.values().any(|a| a.scopes.is_some())
    }
}

impl Match {
    fn validate(&self, at: &str) -> Result<(), PolicyError> {
        for pattern in self.table.iter().flat_map(OneOrMany::as_slice) {
            if pattern.split('.').count() != 2 || pattern.split('.').any(str::is_empty) {
                return Err(invalid(format!(
                    "{at}: table {pattern:?} must be written database.table"
                )));
            }
        }
        if let Some(r) = self.rows {
            if r.min.is_none() && r.max.is_none() {
                return Err(invalid(format!("{at}: rows needs min, max or both")));
            }
            if let (Some(lo), Some(hi)) = (r.min, r.max)
                && lo > hi
            {
                return Err(invalid(format!("{at}: rows.min is larger than rows.max")));
            }
        }
        if let Some(s) = &self.sum {
            if s.column.is_empty() {
                return Err(invalid(format!("{at}: sum.column is empty")));
            }
            let finite = |v: Option<f64>| v.is_none_or(f64::is_finite);
            if !finite(s.min) || !finite(s.max) {
                return Err(invalid(format!("{at}: sum bounds must be finite numbers")));
            }
            match (s.min, s.max) {
                (None, None) => return Err(invalid(format!("{at}: sum needs min, max or both"))),
                (Some(lo), Some(hi)) if lo > hi => {
                    return Err(invalid(format!("{at}: sum.min is larger than sum.max")));
                }
                _ => {}
            }
        }
        Ok(())
    }
}
