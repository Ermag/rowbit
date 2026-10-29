//! Per-agent database users: compiles `scopes` from `rowbit.yml` into
//! MariaDB and MySQL grants, reads what a user holds now, and works out
//! the statements between the two. See "Scopes" in `docs/spec/policy.md`
//! and "Database users for agents" in `docs/spec/server.md`.
//!
//! Both servers report a user's privileges in the same four
//! `information_schema` tables, readable by the user itself and by an
//! administrator, so the same code serves `rowbit grants` and Rowbit's own
//! check at startup.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use mysql_async::Conn;
use mysql_async::prelude::Queryable;
use rowbit_policy::{Operation, Scopes, glob};

/// What a privilege is on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Object {
    /// `*.*`.
    Global,
    /// `db.*`. The name is as the server stores it for database-level
    /// grants, where `_` and `%` are wildcards unless escaped with `\`.
    Database(String),
    /// `db.table`, names exactly as they are.
    Table(String, String),
}

/// One privilege on one object, or on one column of a table.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Grant {
    pub object: Object,
    /// `SELECT`, `INSERT`, ..., or `GRANT OPTION`.
    pub privilege: String,
    pub column: Option<String>,
}

impl Grant {
    fn on(object: Object, privilege: &str) -> Grant {
        Grant {
            object,
            privilege: privilege.to_owned(),
            column: None,
        }
    }
}

impl fmt::Display for Object {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Object::Global => f.write_str("*.*"),
            Object::Database(db) => write!(f, "{}.*", unescape_database(db)),
            Object::Table(db, t) => write!(f, "{db}.{t}"),
        }
    }
}

impl fmt::Display for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.column {
            Some(c) => write!(f, "{} ({c}) ON {}", self.privilege, self.object),
            None => write!(f, "{} ON {}", self.privilege, self.object),
        }
    }
}

/// The tables and columns of the databases scopes name.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    /// `(database, table)` to its columns in order.
    pub tables: BTreeMap<(String, String), Vec<String>>,
    /// Views that run with their definer's rights (`SQL SECURITY DEFINER`,
    /// the default). Reading one reads whatever its definer can, whatever
    /// the agent's own grants say.
    pub definer_views: BTreeSet<(String, String)>,
}

impl Catalog {
    /// Reads every table and view, with their columns, in `databases`.
    /// The connection must be able to see them all, as an
    /// administrator's can.
    pub async fn read(
        conn: &mut Conn,
        databases: &BTreeSet<String>,
    ) -> Result<Catalog, mysql_async::Error> {
        let mut catalog = Catalog::default();
        for db in databases {
            let rows: Vec<(String, String)> = conn
                .exec(
                    "SELECT TABLE_NAME, COLUMN_NAME FROM information_schema.COLUMNS \
                     WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME, ORDINAL_POSITION",
                    (db,),
                )
                .await?;
            for (table, column) in rows {
                catalog
                    .tables
                    .entry((db.clone(), table))
                    .or_default()
                    .push(column);
            }
            let views: Vec<String> = conn
                .exec(
                    "SELECT TABLE_NAME FROM information_schema.VIEWS \
                     WHERE TABLE_SCHEMA = ? AND SECURITY_TYPE <> 'INVOKER'",
                    (db,),
                )
                .await?;
            catalog
                .definer_views
                .extend(views.into_iter().map(|v| (db.clone(), v)));
        }
        Ok(catalog)
    }
}

/// The databases a set of scopes names, which is what [`Catalog::read`]
/// needs.
pub fn databases(scopes: &Scopes) -> BTreeSet<String> {
    Operation::ALL
        .iter()
        .flat_map(|op| scopes.patterns(*op))
        .chain(&scopes.deny_read.tables)
        .chain(&scopes.deny_read.columns)
        .filter_map(|p| p.split('.').next())
        .map(str::to_owned)
        .collect()
}

/// What scopes compile to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Compiled {
    pub grants: BTreeSet<Grant>,
    /// Things a person should know: a named table that doesn't exist.
    pub notes: Vec<String>,
    /// Deny entries that match nothing: a typo, or a name in the wrong
    /// case. A deny list that denies nothing must not pass, so these stop
    /// `rowbit grants` and the startup check.
    pub errors: Vec<String>,
}

fn privileges(op: Operation) -> &'static [&'static str] {
    match op {
        Operation::Read => &["SELECT"],
        Operation::Insert => &["INSERT"],
        Operation::Update => &["UPDATE"],
        Operation::Delete => &["DELETE"],
        Operation::Ddl => &["CREATE", "ALTER", "DROP", "INDEX"],
    }
}

/// Compiles scopes against the tables that exist now. See the table in
/// "What Rowbit compiles" in `docs/spec/policy.md`.
///
/// Reads and writes are granted table by table, never on a whole
/// database, so that a view running with its definer's rights is granted
/// only when a scope names it exactly: through one, the agent would reach
/// whatever the view's definer can, denied tables and columns included.
pub fn compile(scopes: &Scopes, catalog: &Catalog) -> Compiled {
    let mut out = Compiled::default();
    for entry in &scopes.deny_read.columns {
        let mut parts = entry.splitn(3, '.');
        let (db, table, column) = (parts.next(), parts.next(), parts.next());
        let found = catalog.tables.iter().any(|((d, t), cols)| {
            Some(d.as_str()) == db
                && Some(t.as_str()) == table
                && column.is_some_and(|x| cols.iter().any(|c| c.eq_ignore_ascii_case(x)))
        });
        if !found {
            out.errors.push(format!(
                "deny_read.columns: {entry} matches no column; check the spelling, or Rowbit would deny nothing"
            ));
        }
    }
    for entry in scopes.deny_read.tables.iter().filter(|t| !t.contains('*')) {
        if !catalog
            .tables
            .keys()
            .any(|(d, t)| format!("{d}.{t}") == *entry)
        {
            out.errors.push(format!(
                "deny_read.tables: {entry} matches no table; check the spelling, or Rowbit would deny nothing"
            ));
        }
    }
    for op in Operation::ALL {
        for pattern in scopes.patterns(op) {
            let Some((db, table_pattern)) = pattern.split_once('.') else {
                continue;
            };
            // Only schema changes are granted database-wide, so `ddl` can
            // create new tables.
            if table_pattern == "*" && op == Operation::Ddl {
                for p in privileges(op) {
                    out.grants
                        .insert(Grant::on(Object::Database(escape_database(db)), p));
                }
                continue;
            }
            let matching: Vec<(&String, &Vec<String>)> = catalog
                .tables
                .iter()
                .filter(|((d, t), _)| d == db && glob(pattern, &format!("{d}.{t}")))
                .filter(|(key, _)| {
                    let definer = catalog.definer_views.contains(*key);
                    if definer && table_pattern.contains('*') {
                        out.notes.push(format!(
                            "{}.{} is a view with its definer's rights, so {pattern} doesn't cover it; name it exactly to grant it",
                            key.0, key.1
                        ));
                    }
                    !definer || !table_pattern.contains('*')
                })
                .map(|((_, t), cols)| (t, cols))
                .collect();
            if matching.is_empty() && !table_pattern.contains('*') {
                if op == Operation::Ddl {
                    out.grants.insert(Grant::on(
                        Object::Table(db.to_owned(), table_pattern.to_owned()),
                        "CREATE",
                    ));
                    out.notes.push(format!(
                        "{pattern} doesn't exist yet: only CREATE is granted on it until the next `rowbit grants apply`"
                    ));
                } else {
                    out.notes.push(format!(
                        "{pattern} doesn't exist: nothing is granted on it for {}",
                        op.as_str()
                    ));
                }
            }
            for (table, columns) in matching {
                let name = format!("{db}.{table}");
                let object = Object::Table(db.to_owned(), table.clone());
                if catalog
                    .definer_views
                    .contains(&(db.to_owned(), table.clone()))
                {
                    out.notes.push(format!(
                        "{name} is a view with its definer's rights: the agent reads through it whatever its definer can, deny_read included"
                    ));
                }
                if op != Operation::Read {
                    for p in privileges(op) {
                        out.grants.insert(Grant::on(object.clone(), p));
                    }
                } else if scopes.table_denied(&name) {
                    continue;
                } else if columns.iter().any(|c| scopes.column_denied(&name, c)) {
                    for column in columns.iter().filter(|c| !scopes.column_denied(&name, c)) {
                        out.grants.insert(Grant {
                            object: object.clone(),
                            privilege: "SELECT".into(),
                            column: Some(column.clone()),
                        });
                    }
                } else {
                    out.grants.insert(Grant::on(object, "SELECT"));
                }
            }
        }
    }
    out
}

/// What a database user holds now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Current {
    pub grants: BTreeSet<Grant>,
    /// `SHOW GRANTS` lines Rowbit doesn't manage, such as roles or proxy
    /// grants. They can give the user more than its scopes, so the user
    /// isn't trusted until a person removes them.
    pub unmanaged: Vec<String>,
}

/// A user, `'name'@'host'`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Account {
    pub user: String,
    pub host: String,
}

impl Account {
    /// The account as SQL: `'name'@'host'`.
    pub fn sql(&self) -> String {
        format!("{}@{}", quote_string(&self.user), quote_string(&self.host))
    }

    /// Parses `CURRENT_USER()`'s `name@host`.
    pub fn parse_current(s: &str) -> Option<Account> {
        let (user, host) = s.rsplit_once('@')?;
        Some(Account {
            user: user.to_owned(),
            host: host.to_owned(),
        })
    }
}

impl fmt::Display for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.user, self.host)
    }
}

/// Whether the account exists. Needs an administrator's connection.
pub async fn exists(conn: &mut Conn, account: &Account) -> Result<bool, mysql_async::Error> {
    let n: Option<i64> = conn
        .exec_first(
            "SELECT COUNT(*) FROM mysql.user WHERE User = ? AND Host = ?",
            (&account.user, &account.host),
        )
        .await?;
    Ok(n.unwrap_or(0) > 0)
}

/// What `account` holds, from `information_schema` and `SHOW GRANTS`.
/// Works on an administrator's connection, or on the account's own.
pub async fn current(conn: &mut Conn, account: &Account) -> Result<Current, mysql_async::Error> {
    let grantee = account.sql();
    let mut out = Current::default();
    let mut add = |object: Object, privilege: String, column: Option<String>, grantable: String| {
        if privilege == "USAGE" {
            return;
        }
        if grantable == "YES" {
            out.grants.insert(Grant::on(object.clone(), "GRANT OPTION"));
        }
        out.grants.insert(Grant {
            object,
            privilege,
            column,
        });
    };
    let rows: Vec<(String, String)> = conn
        .exec(
            "SELECT PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.USER_PRIVILEGES WHERE GRANTEE = ?",
            (&grantee,),
        )
        .await?;
    for (p, g) in rows {
        add(Object::Global, p, None, g);
    }
    let rows: Vec<(String, String, String)> = conn
        .exec(
            "SELECT TABLE_SCHEMA, PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.SCHEMA_PRIVILEGES WHERE GRANTEE = ?",
            (&grantee,),
        )
        .await?;
    for (db, p, g) in rows {
        add(Object::Database(db), p, None, g);
    }
    let rows: Vec<(String, String, String, String)> = conn
        .exec(
            "SELECT TABLE_SCHEMA, TABLE_NAME, PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.TABLE_PRIVILEGES WHERE GRANTEE = ?",
            (&grantee,),
        )
        .await?;
    for (db, t, p, g) in rows {
        add(Object::Table(db, t), p, None, g);
    }
    let rows: Vec<(String, String, String, String, String)> = conn
        .exec(
            "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.COLUMN_PRIVILEGES WHERE GRANTEE = ?",
            (&grantee,),
        )
        .await?;
    for (db, t, c, p, g) in rows {
        add(Object::Table(db, t), p, Some(c), g);
    }
    let lines: Vec<String> = conn.query(format!("SHOW GRANTS FOR {grantee}")).await?;
    out.unmanaged = lines
        .into_iter()
        .filter(|l| !is_privilege_line(l))
        .collect();
    Ok(out)
}

/// `GRANT <privileges> ON <object> TO ...`, the lines `information_schema`
/// covers. Role grants have no `ON`; proxy grants and partial revokes are
/// other shapes.
fn is_privilege_line(line: &str) -> bool {
    line.starts_with("GRANT ") && line.contains(" ON ") && !line.starts_with("GRANT PROXY ON ")
}

/// Creates the account with a password. Needs an administrator.
pub async fn create_user(
    conn: &mut Conn,
    account: &Account,
    password: &str,
) -> Result<(), mysql_async::Error> {
    conn.query_drop(format!(
        "CREATE USER {} IDENTIFIED BY {}",
        account.sql(),
        quote_string(password)
    ))
    .await
}

/// Gives the account a new password. Needs an administrator.
pub async fn set_password(
    conn: &mut Conn,
    account: &Account,
    password: &str,
) -> Result<(), mysql_async::Error> {
    conn.query_drop(format!(
        "ALTER USER {} IDENTIFIED BY {}",
        account.sql(),
        quote_string(password)
    ))
    .await
}

/// Drops the account if it exists. Needs an administrator.
pub async fn drop_user(conn: &mut Conn, account: &Account) -> Result<(), mysql_async::Error> {
    conn.query_drop(format!("DROP USER IF EXISTS {}", account.sql()))
        .await
}

/// The difference between what a user holds and what it should.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    pub missing: BTreeSet<Grant>,
    pub extra: BTreeSet<Grant>,
}

impl Diff {
    pub fn between(want: &BTreeSet<Grant>, have: &BTreeSet<Grant>) -> Diff {
        Diff {
            missing: want.difference(have).cloned().collect(),
            extra: have.difference(want).cloned().collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty()
    }

    /// The statements that close the difference: revokes first, so the
    /// user never holds more than either side while they run.
    pub fn statements(&self, account: &Account) -> Vec<String> {
        let to = account.sql();
        let mut out: Vec<String> = group(&self.extra)
            .into_iter()
            .map(|g| format!("REVOKE {g} FROM {to}"))
            .collect();
        out.extend(
            group(&self.missing)
                .into_iter()
                .map(|g| format!("GRANT {g} TO {to}")),
        );
        out
    }
}

/// Grants as `PRIVS ON object` clauses, one per object, with column
/// privileges as `SELECT (`a`, `b`)`. `GRANT OPTION` goes alone, since it
/// can't be revoked in a list with other privileges on MySQL.
fn group(grants: &BTreeSet<Grant>) -> Vec<String> {
    let mut by_object: BTreeMap<&Object, BTreeMap<&str, Vec<&str>>> = BTreeMap::new();
    let mut option: Vec<&Object> = Vec::new();
    for g in grants {
        if g.privilege == "GRANT OPTION" {
            option.push(&g.object);
            continue;
        }
        let cols = by_object
            .entry(&g.object)
            .or_default()
            .entry(&g.privilege)
            .or_default();
        if let Some(c) = &g.column {
            cols.push(c);
        }
    }
    let mut out = Vec::new();
    for (object, privs) in by_object {
        let list: Vec<String> = privs
            .iter()
            .map(|(p, cols)| {
                if cols.is_empty() {
                    (*p).to_owned()
                } else {
                    let cols: Vec<String> = cols.iter().map(|c| quote_ident(c)).collect();
                    format!("{p} ({})", cols.join(", "))
                }
            })
            .collect();
        out.push(format!("{} ON {}", list.join(", "), object_sql(object)));
    }
    for object in option {
        out.push(format!("GRANT OPTION ON {}", object_sql(object)));
    }
    out
}

fn object_sql(object: &Object) -> String {
    match object {
        Object::Global => "*.*".into(),
        Object::Database(db) => format!("{}.*", quote_ident(db)),
        Object::Table(db, t) => format!("{}.{}", quote_ident(db), quote_ident(t)),
    }
}

/// A database name as a database-level grant must spell it, so `_` and
/// `%` match only themselves.
pub fn escape_database(db: &str) -> String {
    let mut out = String::with_capacity(db.len());
    for c in db.chars() {
        if matches!(c, '_' | '%' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn unescape_database(db: &str) -> String {
    let mut out = String::with_capacity(db.len());
    let mut chars = db.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// A string literal. Rowbit refuses accounts with a backslash, so doubling
/// quotes is enough whatever the server's `sql_mode`.
pub fn quote_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(yaml: &str) -> Scopes {
        serde_norway::from_str(yaml).unwrap()
    }

    fn catalog() -> Catalog {
        let mut c = Catalog::default();
        for (db, t, cols) in [
            ("billing", "refunds", vec!["id", "status", "amount"]),
            ("billing", "cards", vec!["id", "number"]),
            ("billing", "customers", vec!["id", "name", "email"]),
            ("crm", "people", vec!["id", "name"]),
        ] {
            c.tables.insert(
                (db.into(), t.into()),
                cols.into_iter().map(str::to_owned).collect(),
            );
        }
        c
    }

    fn shown(c: &Compiled) -> Vec<String> {
        c.grants.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn table_by_table_even_when_nothing_is_denied() {
        let c = compile(
            &scopes("read: [crm.*]\ninsert: [crm.*]\nddl: [crm.*]"),
            &catalog(),
        );
        assert_eq!(
            shown(&c),
            [
                "ALTER ON crm.*",
                "CREATE ON crm.*",
                "DROP ON crm.*",
                "INDEX ON crm.*",
                "INSERT ON crm.people",
                "SELECT ON crm.people"
            ]
        );
    }

    // Found in review: a view with its definer's rights, covered by a
    // pattern, read denied tables and columns for the agent.
    #[test]
    fn definer_views_need_their_own_name() {
        let mut cat = catalog();
        for (v, cols) in [
            ("card_report", vec!["id", "number"]),
            ("open_refunds", vec!["id"]),
        ] {
            cat.tables.insert(
                ("billing".into(), v.into()),
                cols.into_iter().map(str::to_owned).collect(),
            );
        }
        cat.definer_views
            .insert(("billing".into(), "card_report".into()));
        let c = compile(
            &scopes(
                "read: [billing.*]\nupdate: [billing.*]\ndeny_read: { tables: [billing.cards] }",
            ),
            &cat,
        );
        let s = shown(&c);
        assert!(!s.iter().any(|g| g.contains("card_report")), "{s:?}");
        assert!(
            s.contains(&"SELECT ON billing.open_refunds".to_owned()),
            "an invoker view is fine: {s:?}"
        );
        assert!(
            c.notes.iter().any(|n| n.contains("card_report")),
            "{:?}",
            c.notes
        );

        let named = compile(&scopes("read: [billing.card_report]"), &cat);
        assert_eq!(shown(&named), ["SELECT ON billing.card_report"]);
        assert!(named.notes.iter().any(|n| n.contains("definer's rights")));
    }

    // Found in review: a deny entry in the wrong case, or misspelled,
    // denied nothing, and the table was granted whole.
    #[test]
    fn deny_entries_ignore_case_and_must_match_something() {
        let c = compile(
            &scopes("read: [billing.customers]\ndeny_read: { columns: [billing.customers.EMAIL] }"),
            &catalog(),
        );
        assert_eq!(
            shown(&c),
            [
                "SELECT (id) ON billing.customers",
                "SELECT (name) ON billing.customers"
            ]
        );
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        for deny in [
            "columns: [billing.customers.emial]",
            "tables: [billing.crads]",
        ] {
            let c = compile(
                &scopes(&format!("read: [billing.*]\ndeny_read: {{ {deny} }}")),
                &catalog(),
            );
            assert_eq!(c.errors.len(), 1, "{deny}: {:?}", c.errors);
        }
    }

    #[test]
    fn denies_split_a_database_into_tables_and_columns() {
        let c = compile(
            &scopes(
                "read: [billing.*]\nupdate: [billing.refunds]\ndeny_read: { tables: [billing.cards], columns: [billing.customers.email] }",
            ),
            &catalog(),
        );
        assert_eq!(
            shown(&c),
            [
                "SELECT (id) ON billing.customers",
                "SELECT (name) ON billing.customers",
                "SELECT ON billing.refunds",
                "UPDATE ON billing.refunds",
            ]
        );
    }

    #[test]
    fn patterns_expand_to_existing_tables() {
        let c = compile(
            &scopes("read: [billing.c*, billing.gone]\nddl: [crm.new]"),
            &catalog(),
        );
        assert_eq!(
            shown(&c),
            [
                "SELECT ON billing.cards",
                "SELECT ON billing.customers",
                "CREATE ON crm.new"
            ]
        );
        assert_eq!(c.notes.len(), 2, "{:?}", c.notes);
    }

    #[test]
    fn underscores_in_database_names_are_literal() {
        let c = compile(&scopes("ddl: [my_db.*]"), &Catalog::default());
        let g = c.grants.iter().find(|g| g.privilege == "ALTER").unwrap();
        assert_eq!(g.object, Object::Database("my\\_db".into()));
        assert_eq!(g.to_string(), "ALTER ON my_db.*");
        let account = Account {
            user: "rowbit_bot".into(),
            host: "%".into(),
        };
        let d = Diff::between(&c.grants, &BTreeSet::new());
        assert_eq!(
            d.statements(&account),
            ["GRANT ALTER, CREATE, DROP, INDEX ON `my\\_db`.* TO 'rowbit_bot'@'%'"]
        );
    }

    #[test]
    fn statements_revoke_first_and_group_columns() {
        let want = compile(
            &scopes("read: [billing.customers]\ndeny_read: { columns: [billing.customers.email] }"),
            &catalog(),
        )
        .grants;
        let mut have = BTreeSet::new();
        have.insert(Grant::on(Object::Database("billing".into()), "SELECT"));
        have.insert(Grant::on(
            Object::Database("billing".into()),
            "GRANT OPTION",
        ));
        let account = Account {
            user: "rowbit_bot".into(),
            host: "10.0.0.%".into(),
        };
        assert_eq!(
            Diff::between(&want, &have).statements(&account),
            [
                "REVOKE SELECT ON `billing`.* FROM 'rowbit_bot'@'10.0.0.%'",
                "REVOKE GRANT OPTION ON `billing`.* FROM 'rowbit_bot'@'10.0.0.%'",
                "GRANT SELECT (`id`, `name`) ON `billing`.`customers` TO 'rowbit_bot'@'10.0.0.%'",
            ]
        );
    }

    #[test]
    fn role_and_proxy_lines_are_unmanaged() {
        assert!(is_privilege_line("GRANT SELECT ON `a`.* TO `u`@`%`"));
        assert!(is_privilege_line(
            "GRANT USAGE ON *.* TO `u`@`%` IDENTIFIED BY PASSWORD '*AB'"
        ));
        assert!(!is_privilege_line("GRANT `admin`@`%` TO `u`@`%`"));
        assert!(!is_privilege_line("GRANT PROXY ON ''@'' TO 'u'@'%'"));
        assert!(!is_privilege_line(
            "REVOKE SELECT ON `mysql`.* FROM `u`@`%`"
        ));
    }

    #[test]
    fn current_user_parses_at_the_last_at() {
        let a = Account::parse_current("rowbit@10.0.0.%").unwrap();
        assert_eq!((a.user.as_str(), a.host.as_str()), ("rowbit", "10.0.0.%"));
    }
}
