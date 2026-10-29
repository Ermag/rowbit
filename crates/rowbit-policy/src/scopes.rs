//! `agents.<id>.scopes`: which tables an agent may touch at all. See
//! "Scopes" in `docs/spec/policy.md`. The database enforces them through
//! grants on a user per agent; this module only parses and checks them.

use serde::Deserialize;

use crate::PolicyError;
use crate::eval::glob;

/// MySQL caps user names at 32 characters, and the agent's user is
/// `rowbit_<id>`.
pub const MAX_SCOPED_AGENT_ID: usize = 32 - AGENT_USER_PREFIX.len();
pub const AGENT_USER_PREFIX: &str = "rowbit_";

const SYSTEM_SCHEMAS: [&str; 4] = ["information_schema", "performance_schema", "mysql", "sys"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scopes {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub insert: Vec<String>,
    #[serde(default)]
    pub update: Vec<String>,
    #[serde(default)]
    pub delete: Vec<String>,
    #[serde(default)]
    pub ddl: Vec<String>,
    #[serde(default)]
    pub deny_read: DenyRead,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DenyRead {
    /// `database.table` patterns the agent may never read.
    #[serde(default)]
    pub tables: Vec<String>,
    /// `database.table.column` names the agent may never read.
    #[serde(default)]
    pub columns: Vec<String>,
}

/// One of the operations a scope grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Operation {
    Read,
    Insert,
    Update,
    Delete,
    Ddl,
}

impl Operation {
    pub const ALL: [Operation; 5] = [
        Operation::Read,
        Operation::Insert,
        Operation::Update,
        Operation::Delete,
        Operation::Ddl,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Operation::Read => "read",
            Operation::Insert => "insert",
            Operation::Update => "update",
            Operation::Delete => "delete",
            Operation::Ddl => "ddl",
        }
    }
}

impl Scopes {
    /// The patterns that grant `op`.
    pub fn patterns(&self, op: Operation) -> &[String] {
        match op {
            Operation::Read => &self.read,
            Operation::Insert => &self.insert,
            Operation::Update => &self.update,
            Operation::Delete => &self.delete,
            Operation::Ddl => &self.ddl,
        }
    }

    /// Whether the scopes let the agent do `op` on `database.table`. For
    /// `Read`, a denied table is never readable; denied columns are
    /// [`Scopes::column_denied`]'s business.
    ///
    /// Names compare without case here and below. On a server whose table
    /// names ignore case (`lower_case_table_names` 1 or 2, the defaults on
    /// Windows and macOS), `ORDERS` is `orders`, and a case-sensitive
    /// comparison would find no deny for it. On other servers this can
    /// only refuse more, and the database decides the rest.
    pub fn allows(&self, op: Operation, table: &str) -> bool {
        let table = table.to_lowercase();
        let listed = self
            .patterns(op)
            .iter()
            .any(|p| glob(&p.to_lowercase(), &table));
        listed && !(op == Operation::Read && self.table_denied(&table))
    }

    pub fn table_denied(&self, table: &str) -> bool {
        let table = table.to_lowercase();
        self.deny_read
            .tables
            .iter()
            .any(|p| glob(&p.to_lowercase(), &table))
    }

    /// The denied columns of `database.table`, as `deny_read` writes them.
    pub fn denied_columns(&self, table: &str) -> Vec<&str> {
        let table = table.to_lowercase();
        self.deny_read
            .columns
            .iter()
            .filter_map(|c| {
                let (t, column) = c.rsplit_once('.')?;
                (t.to_lowercase() == table).then_some(column)
            })
            .collect()
    }

    /// Whether `column` of `database.table` may not be read.
    pub fn column_denied(&self, table: &str, column: &str) -> bool {
        self.denied_columns(table)
            .iter()
            .any(|c| c.eq_ignore_ascii_case(column))
    }

    /// Whether any table or column in `database` is denied for reading, so
    /// a database-wide grant can't be used there.
    pub fn denies_in(&self, database: &str) -> bool {
        let in_db = |name: &str| name.split('.').next() == Some(database);
        self.deny_read.tables.iter().any(|p| in_db(p))
            || self.deny_read.columns.iter().any(|c| in_db(c))
    }

    pub(crate) fn validate(&self, at: &str) -> Result<(), PolicyError> {
        let invalid = |msg: String| Err(PolicyError::Invalid(format!("{at}.scopes: {msg}")));
        let lists = Operation::ALL
            .iter()
            .map(|op| (op.as_str(), self.patterns(*op)))
            .chain([("deny_read.tables", self.deny_read.tables.as_slice())]);
        for (key, patterns) in lists {
            for pattern in patterns {
                let parts: Vec<&str> = pattern.split('.').collect();
                if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
                    return invalid(format!("{key}: {pattern:?} must be written database.table"));
                }
                check_database(parts[0]).or_else(|m| invalid(format!("{key}: {pattern:?} {m}")))?;
            }
        }
        for column in &self.deny_read.columns {
            let parts: Vec<&str> = column.split('.').collect();
            if parts.len() != 3 || parts.iter().any(|p| p.is_empty() || p.contains('*')) {
                return invalid(format!(
                    "deny_read.columns: {column:?} must be written database.table.column, without *"
                ));
            }
            check_database(parts[0])
                .or_else(|m| invalid(format!("deny_read.columns: {column:?} {m}")))?;
        }
        Ok(())
    }

    /// Things that are valid but probably not what the author meant.
    pub(crate) fn warnings(&self, agent: &str) -> Vec<String> {
        let mut out = Vec::new();
        // An insert of plain values reads nothing, so only these need reads.
        for op in [Operation::Update, Operation::Delete] {
            for pattern in self.patterns(op) {
                // A pattern is covered if a read pattern matches it as a name,
                // which is exact for plain names and for identical patterns.
                if !self.allows(Operation::Read, pattern) {
                    out.push(format!(
                        "agents.{agent}.scopes: {pattern} is in {} but not readable, so a WHERE on it will be refused",
                        op.as_str()
                    ));
                }
            }
        }
        out
    }
}

fn check_database(name: &str) -> Result<(), String> {
    if name.contains('*') {
        return Err("can't have * in its database part".into());
    }
    if SYSTEM_SCHEMAS.contains(&name.to_ascii_lowercase().as_str()) {
        return Err("names a system schema, which agents may never use".into());
    }
    Ok(())
}

/// The database user Rowbit keeps for a scoped agent.
pub fn agent_user(agent: &str) -> String {
    format!("{AGENT_USER_PREFIX}{agent}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(yaml: &str) -> Scopes {
        serde_norway::from_str(yaml).unwrap()
    }

    #[test]
    fn denied_tables_and_columns() {
        let s = scopes(
            "read: [billing.*]\nupdate: [billing.refunds]\ndeny_read: { tables: [billing.cards], columns: [billing.customers.email] }",
        );
        assert!(s.allows(Operation::Read, "billing.refunds"));
        assert!(!s.allows(Operation::Read, "billing.cards"));
        assert!(!s.allows(Operation::Read, "crm.people"));
        assert!(s.allows(Operation::Update, "billing.refunds"));
        assert!(!s.allows(Operation::Delete, "billing.refunds"));
        assert!(s.column_denied("billing.customers", "email"));
        assert!(
            s.column_denied("billing.customers", "EMAIL"),
            "column names ignore case"
        );
        assert!(!s.column_denied("billing.customers", "name"));
        // Found in second review: on servers whose table names ignore case,
        // BILLING.CUSTOMERS is billing.customers.
        assert!(s.column_denied("BILLING.Customers", "email"));
        assert_eq!(s.denied_columns("billing.CUSTOMERS"), ["email"]);
        assert!(!s.allows(Operation::Read, "billing.CARDS"));
        assert!(s.allows(Operation::Update, "billing.REFUNDS"));
        assert!(!s.column_denied("billing.customer", "s.email"));
        assert!(s.denies_in("billing"));
        assert!(!s.denies_in("crm"));
    }

    #[test]
    fn empty_scopes_allow_nothing() {
        let s = scopes("{}");
        for op in Operation::ALL {
            assert!(!s.allows(op, "app.orders"));
        }
    }

    #[test]
    fn bad_patterns_are_errors() {
        for yaml in [
            "read: ['*.*']",
            "read: ['*.orders']",
            "read: [orders]",
            "update: [mysql.user]",
            "read: [INFORMATION_SCHEMA.tables]",
            "deny_read: { columns: [app.orders] }",
            "deny_read: { columns: ['app.orders.*'] }",
            "deny_read: { tables: [sys.x] }",
        ] {
            assert!(scopes(yaml).validate("agents.a").is_err(), "{yaml}");
        }
        assert!(
            scopes("read: ['app.*', 'app.ord*']")
                .validate("agents.a")
                .is_ok()
        );
    }

    #[test]
    fn writable_but_unreadable_is_a_warning() {
        let s = scopes("read: [app.orders]\nupdate: [app.orders, app.items]\ninsert: [app.log]");
        let w = s.warnings("bot");
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("app.items"));
    }
}
