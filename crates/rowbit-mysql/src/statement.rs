//! Classifies the one SQL statement an agent proposes, and derives the read
//! that previews it. See "Propose" and "Preview" in
//! `docs/spec/change-lifecycle.md`.
//!
//! Everything here fails closed: a statement is accepted only if every part
//! of it is a shape this module knows. Syntax trees are destructured without
//! `..` where it matters, so a new field in a `sqlparser` upgrade is a
//! compile error here rather than a clause that is silently ignored.

use std::ops::ControlFlow;

use sqlparser::ast::{
    Assignment, AssignmentTarget, BinaryOperator, Delete, Expr, FromTable, Ident, Insert,
    ObjectName, ObjectNamePart, ObjectType, OrderByExpr, Query, Select, SetExpr, Statement,
    TableAlias, TableFactor, TableObject, TableWithJoins, Update, Value, ValueWithSpan, Visit,
    Visitor,
};
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;
use thiserror::Error;

/// Why a statement cannot be proposed. Shown to the agent and the reviewer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0}")]
pub struct Refusal(pub String);

fn refuse<T>(reason: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal(reason.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Insert,
    Update,
    Delete,
    Ddl,
}

/// A table as the statement names it. The database is `None` when the
/// statement leaves it to the connection's default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableName {
    pub database: Option<String>,
    pub table: String,
}

/// A statement Rowbit accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    pub kind: Kind,
    pub table: TableName,
    /// Columns the statement writes, in statement order. Empty for a delete,
    /// which writes every column, and for DDL.
    pub columns: Vec<String>,
    /// The statement as Rowbit understood it. This is what reviewers see,
    /// and what runs for DDL.
    pub canonical: String,
    /// How to preview it. `None` for DDL, which is not previewed.
    pub preview: Option<PreviewRead>,
}

/// The pieces of a write that a preview reuses, each printed from the
/// checked syntax tree. The adapter assembles them into SQL that reads the
/// affected rows and replays the write on a private copy of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewRead {
    /// An `UPDATE` or `DELETE`.
    Existing {
        /// The target as the statement writes it, e.g. `billing.refunds AS r`.
        from: String,
        /// What the statement calls the table: its alias, or its name.
        qualifier: String,
        /// ` WHERE ... ORDER BY ... LIMIT n`, or empty.
        filter: String,
        /// For an update, the `SET` list with bare column names, e.g.
        /// `` `status` = 'refunded' ``. Empty for a delete.
        assignments: String,
    },
    /// An `INSERT`: `VALUES (...), ...` or a `SELECT`, giving values for
    /// [`Classified::columns`] in order.
    New { source: String },
}

/// Parses and classifies one statement in MySQL syntax.
pub fn classify(sql: &str) -> Result<Classified, Refusal> {
    let statement = parse_one(sql)?;
    let canonical = statement.to_string();
    match &statement {
        Statement::Update(u) => update(u, canonical),
        Statement::Delete(d) => delete(d, canonical),
        Statement::Insert(i) => insert(i, canonical),
        other => ddl(other, canonical),
    }
}

/// Checks a read for the query tool: exactly one `SELECT` (or other query
/// expression) that passes the same checks as a write. Returns the SQL
/// Rowbit will run, printed from the parsed statement.
pub fn classify_read(sql: &str) -> Result<String, Refusal> {
    match parse_one(sql)? {
        Statement::Query(q) => Ok(q.to_string()),
        _ => refuse("the query tool only reads; propose writes with propose_write"),
    }
}

/// Parses exactly one statement and runs the guard over all of it.
fn parse_one(sql: &str) -> Result<Statement, Refusal> {
    let dialect = MySqlDialect {};
    refuse_executable_comments(sql)?;
    if sql.contains('\\') {
        // The parser and MySQL disagree on some escapes (`\\%`, `\\_`), and
        // printing does not escape a backslash again. Refused anywhere.
        return refuse("backslashes are not allowed; write quotes inside strings as ''");
    }
    let mut statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| Refusal(format!("could not parse: {e}")))?;
    if statements.len() != 1 {
        return refuse(format!(
            "send exactly one statement, not {}",
            statements.len()
        ));
    }
    let statement = statements.remove(0);
    if let ControlFlow::Break(refusal) = statement.visit(&mut Guard) {
        return Err(refusal);
    }
    Ok(statement)
}

/// MySQL runs `/*! ... */` comments, and conditional ones (`/*!80000 ... */`)
/// only on some versions, so what the statement means depends on the
/// server. Refused outright, even inside a string literal: rare, and safe.
fn refuse_executable_comments(sql: &str) -> Result<(), Refusal> {
    if sql.contains("/*!") {
        return refuse("executable comments (/*! ... */) are not allowed");
    }
    Ok(())
}

fn update(u: &Update, canonical: String) -> Result<Classified, Refusal> {
    let Update {
        update_token: _,
        optimizer_hints: _,
        table,
        assignments,
        from,
        selection,
        returning,
        output,
        or,
        order_by,
        limit,
    } = u;
    if from.is_some() {
        return refuse("UPDATE ... FROM is not supported");
    }
    if returning.is_some() || output.is_some() || or.is_some() {
        return refuse("this UPDATE syntax is not supported");
    }
    let (name, alias) = single_table(table)?;
    let qualifier = alias.as_ref().unwrap_or(&name.table);

    let mut columns: Vec<String> = Vec::new();
    let mut set_list = Vec::new();
    for Assignment { target, value } in assignments {
        let column = match target {
            AssignmentTarget::ColumnName(target) => assigned_column(target, &name, qualifier)?,
            AssignmentTarget::Tuple(_) => return refuse("tuple assignments are not supported"),
        };
        if columns.iter().any(|c| c.eq_ignore_ascii_case(&column)) {
            return refuse(format!("column {column} is assigned twice"));
        }
        set_list.push(format!("{} = {value}", quote(&column)));
        columns.push(column);
    }
    let qualifier = qualifier.clone();
    let filter = filters(selection.as_ref(), order_by, limit.as_ref())?;

    Ok(Classified {
        kind: Kind::Update,
        table: name,
        columns,
        canonical,
        preview: Some(PreviewRead::Existing {
            from: table.to_string(),
            qualifier,
            filter,
            assignments: set_list.join(", "),
        }),
    })
}

fn delete(d: &Delete, canonical: String) -> Result<Classified, Refusal> {
    let Delete {
        delete_token: _,
        optimizer_hints: _,
        tables,
        from,
        using,
        selection,
        returning,
        output,
        order_by,
        limit,
    } = d;
    if !tables.is_empty() || using.is_some() {
        return refuse("multi-table DELETE is not supported");
    }
    if returning.is_some() || output.is_some() {
        return refuse("this DELETE syntax is not supported");
    }
    let from = match from {
        FromTable::WithFromKeyword(from) => from,
        FromTable::WithoutKeyword(_) => return refuse("DELETE needs FROM"),
    };
    let [table] = from.as_slice() else {
        return refuse("multi-table DELETE is not supported");
    };
    let (name, alias) = single_table(table)?;
    let qualifier = alias.as_ref().unwrap_or(&name.table);

    let qualifier = qualifier.clone();
    let filter = filters(selection.as_ref(), order_by, limit.as_ref())?;

    Ok(Classified {
        kind: Kind::Delete,
        table: name,
        columns: Vec::new(),
        canonical,
        preview: Some(PreviewRead::Existing {
            from: table.to_string(),
            qualifier,
            filter,
            assignments: String::new(),
        }),
    })
}

fn insert(i: &Insert, canonical: String) -> Result<Classified, Refusal> {
    let Insert {
        insert_token: _,
        optimizer_hints: _,
        or,
        ignore,
        into: _,
        table,
        table_alias,
        columns,
        overwrite,
        source,
        assignments,
        partitioned,
        after_columns,
        has_table_keyword,
        on,
        returning,
        output,
        replace_into,
        priority,
        insert_alias,
        settings,
        format_clause,
        multi_table_insert_type,
        multi_table_into_clauses,
        multi_table_when_clauses,
        multi_table_else_clause,
    } = i;
    if *replace_into {
        return refuse("REPLACE is not supported");
    }
    if *ignore {
        return refuse("INSERT IGNORE is not supported");
    }
    if on.is_some() {
        return refuse("ON DUPLICATE KEY UPDATE is not supported");
    }
    if !assignments.is_empty() {
        return refuse("INSERT ... SET is not supported; use INSERT INTO t (columns) VALUES ...");
    }
    if or.is_some()
        || table_alias.is_some()
        || *overwrite
        || partitioned.is_some()
        || !after_columns.is_empty()
        || *has_table_keyword
        || returning.is_some()
        || output.is_some()
        || priority.is_some()
        || insert_alias.is_some()
        || settings.is_some()
        || format_clause.is_some()
        || multi_table_insert_type.is_some()
        || !multi_table_into_clauses.is_empty()
        || !multi_table_when_clauses.is_empty()
        || multi_table_else_clause.is_some()
    {
        return refuse("this INSERT syntax is not supported");
    }
    let TableObject::TableName(table) = table else {
        return refuse("INSERT INTO a table function is not supported");
    };
    let name = table_name(table)?;
    if columns.is_empty() {
        return refuse("name the columns: INSERT INTO t (a, b) VALUES ...");
    }
    let columns = columns
        .iter()
        .map(|c| match c.0.as_slice() {
            [ObjectNamePart::Identifier(id)] => Ok(id.value.clone()),
            _ => refuse(format!("unsupported column name {c}")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let Some(source) = source else {
        return refuse("INSERT needs VALUES or SELECT");
    };

    match source.body.as_ref() {
        SetExpr::Values(values) if is_plain(source) => {
            for row in &values.rows {
                if row.len() != columns.len() {
                    return refuse(format!(
                        "a VALUES row has {} values for {} columns",
                        row.len(),
                        columns.len()
                    ));
                }
                for value in row.iter() {
                    if matches!(value, Expr::Identifier(id) if id.quote_style.is_none() && id.value.eq_ignore_ascii_case("default"))
                    {
                        return refuse("DEFAULT in VALUES is not supported; leave the column out");
                    }
                }
            }
        }
        SetExpr::Values(_) => return refuse("this INSERT syntax is not supported"),
        _ => {}
    }
    let source = source.to_string();

    Ok(Classified {
        kind: Kind::Insert,
        table: name,
        columns,
        canonical,
        preview: Some(PreviewRead::New { source }),
    })
}

/// A `VALUES` list with nothing around it (no `WITH`, `ORDER BY`, ...).
fn is_plain(q: &Query) -> bool {
    q.with.is_none()
        && q.order_by.is_none()
        && q.limit_clause.is_none()
        && q.fetch.is_none()
        && q.locks.is_empty()
        && q.for_clause.is_none()
        && q.settings.is_none()
        && q.format_clause.is_none()
        && q.pipe_operators.is_empty()
}

fn ddl(statement: &Statement, canonical: String) -> Result<Classified, Refusal> {
    let table = match statement {
        Statement::CreateTable(c) => &c.name,
        Statement::AlterTable(a) => &a.name,
        Statement::CreateIndex(c) => &c.table_name,
        Statement::Drop {
            object_type: ObjectType::Table,
            names,
            ..
        } => match names.as_slice() {
            [name] => name,
            _ => return refuse("drop one table at a time"),
        },
        Statement::Truncate(t) => match t.table_names.as_slice() {
            [target] => &target.name,
            _ => return refuse("truncate one table at a time"),
        },
        Statement::RenameTable(renames) => match renames.as_slice() {
            [rename] => {
                // The new name too: renaming into a system schema is as bad.
                refuse_system_table(&table_name(&rename.new_name)?)?;
                &rename.old_name
            }
            _ => return refuse("rename one table at a time"),
        },
        Statement::Query(_) => {
            return refuse("this is a read; use the query tool");
        }
        _ => {
            return refuse(
                "only INSERT, UPDATE, DELETE, and CREATE, ALTER, DROP, TRUNCATE or RENAME TABLE \
                 and CREATE INDEX are supported",
            );
        }
    };
    // The guard sees most table names, but not those of DROP and RENAME.
    let table = table_name(table)?;
    refuse_system_table(&table)?;
    Ok(Classified {
        kind: Kind::Ddl,
        table,
        columns: Vec::new(),
        canonical,
        preview: None,
    })
}

fn refuse_system_table(table: &TableName) -> Result<(), Refusal> {
    match &table.database {
        Some(db) if is_system_schema(db) => refuse(format!(
            "{db} is a system schema, which agents may not read or write"
        )),
        _ => Ok(()),
    }
}

/// The one table an `UPDATE` or `DELETE` targets, and its alias.
fn single_table(t: &TableWithJoins) -> Result<(TableName, Option<String>), Refusal> {
    if !t.joins.is_empty() {
        return refuse("multi-table UPDATE and DELETE are not supported");
    }
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = &t.relation
    else {
        return refuse("the target must be a table");
    };
    if args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return refuse("table options (PARTITION, index hints, ...) are not supported");
    }
    let name = table_name(name)?;
    let alias = match alias {
        None => None,
        Some(TableAlias {
            explicit: _,
            name: alias,
            columns,
            at,
        }) => {
            if !columns.is_empty() || at.is_some() {
                return refuse("unsupported table alias");
            }
            refuse_modifier_as_name(alias)?;
            Some(alias.value.clone())
        }
    };
    Ok((name, alias))
}

/// The parser reads `UPDATE IGNORE t` as table `IGNORE` with alias `t`.
/// Refuse rather than preview the wrong table.
fn refuse_modifier_as_name(id: &Ident) -> Result<(), Refusal> {
    const MODIFIERS: [&str; 5] = [
        "IGNORE",
        "LOW_PRIORITY",
        "QUICK",
        "DELAYED",
        "HIGH_PRIORITY",
    ];
    if id.quote_style.is_none() && MODIFIERS.iter().any(|m| id.value.eq_ignore_ascii_case(m)) {
        return refuse(format!("{} modifiers are not supported", id.value));
    }
    Ok(())
}

fn table_name(name: &ObjectName) -> Result<TableName, Refusal> {
    let parts = name
        .0
        .iter()
        .map(|p| match p {
            ObjectNamePart::Identifier(id) => {
                refuse_modifier_as_name(id)?;
                Ok(id.value.clone())
            }
            ObjectNamePart::Function(_) => refuse(format!("unsupported table name {name}")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    match <[String; 2]>::try_from(parts) {
        Ok([database, table]) => Ok(TableName {
            database: Some(database),
            table,
        }),
        Err(parts) => match <[String; 1]>::try_from(parts) {
            Ok([table]) => Ok(TableName {
                database: None,
                table,
            }),
            Err(_) => refuse(format!("unsupported table name {name}")),
        },
    }
}

/// The column a `SET` target names. `t.col` must qualify by the table or its alias.
fn assigned_column(
    target: &ObjectName,
    table: &TableName,
    qualifier: &str,
) -> Result<String, Refusal> {
    let ids = target
        .0
        .iter()
        .map(|p| match p {
            ObjectNamePart::Identifier(id) => Ok(id.value.as_str()),
            ObjectNamePart::Function(_) => refuse(format!("unsupported column {target}")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    match ids.as_slice() {
        [column] => Ok((*column).to_owned()),
        [q, column] if q.eq_ignore_ascii_case(qualifier) => Ok((*column).to_owned()),
        [db, t, column]
            if table
                .database
                .as_deref()
                .is_some_and(|d| d.eq_ignore_ascii_case(db))
                && t.eq_ignore_ascii_case(&table.table) =>
        {
            Ok((*column).to_owned())
        }
        _ => refuse(format!(
            "SET target {target} is not a column of the target table"
        )),
    }
}

/// ` WHERE ... ORDER BY ... LIMIT n` for a preview read. The filter decides
/// which rows a person approves, and is read again at apply, so it may not
/// depend on chance.
fn filters(
    selection: Option<&Expr>,
    order_by: &[OrderByExpr],
    limit: Option<&Expr>,
) -> Result<String, Refusal> {
    let mut sql = String::new();
    if let Some(w) = selection {
        refuse_chance(w)?;
        sql.push_str(&format!(" WHERE {w}"));
    }
    if !order_by.is_empty() {
        for o in order_by {
            refuse_chance(&o.expr)?;
        }
        let items: Vec<String> = order_by.iter().map(ToString::to_string).collect();
        sql.push_str(&format!(" ORDER BY {}", items.join(", ")));
    }
    if let Some(limit) = limit {
        if order_by.is_empty() {
            // Without an order the server may pick different rows when apply
            // reads the filter again.
            return refuse("LIMIT needs an ORDER BY, so it picks the same rows every time");
        }
        match limit {
            Expr::Value(ValueWithSpan {
                value: Value::Number(n, false),
                ..
            }) if n.bytes().all(|b| b.is_ascii_digit()) => {
                sql.push_str(&format!(" LIMIT {n}"));
            }
            _ => return refuse("LIMIT must be a plain number"),
        }
    }
    Ok(sql)
}

/// Functions whose result differs on every call, so a filter using them
/// would pick different rows at apply than in the preview.
fn refuse_chance(e: &Expr) -> Result<(), Refusal> {
    let hit = sqlparser::ast::visit_expressions(e, |e| match e {
        Expr::Function(f)
            if ["RAND", "UUID"]
                .iter()
                .any(|n| f.name.to_string().eq_ignore_ascii_case(n)) =>
        {
            ControlFlow::Break(f.name.to_string())
        }
        _ => ControlFlow::Continue(()),
    });
    match hit {
        ControlFlow::Break(name) => refuse(format!(
            "{name}() may not decide which rows are written; it gives a different answer at apply"
        )),
        ControlFlow::Continue(()) => Ok(()),
    }
}

/// The server's own schemas, which hold other sessions' statements, users
/// and grants. Never read or written for an agent, also not as a default
/// database.
pub fn is_system_schema(name: &str) -> bool {
    ["information_schema", "performance_schema", "mysql", "sys"]
        .iter()
        .any(|s| s.eq_ignore_ascii_case(name))
}

fn uses_table_statement(body: &SetExpr) -> bool {
    match body {
        SetExpr::Table(_) => true,
        SetExpr::SetOperation { left, right, .. } => {
            uses_table_statement(left) || uses_table_statement(right)
        }
        SetExpr::Query(q) => uses_table_statement(&q.body),
        _ => false,
    }
}

/// Backquotes an identifier for MySQL.
fn quote(id: &str) -> String {
    format!("`{}`", id.replace('`', "``"))
}

/// Built-in functions a statement may call. Anything else is refused,
/// including stored functions, which can write, and built-ins that sleep,
/// lock, read files or touch sequences.
#[rustfmt::skip]
const ALLOWED_FUNCTIONS: &[&str] = &[
    // numbers
    "ABS", "CEIL", "CEILING", "FLOOR", "ROUND", "TRUNCATE", "MOD", "GREATEST", "LEAST", "SIGN",
    // nulls and conditions
    "COALESCE", "IFNULL", "NULLIF", "IF",
    // strings
    "CONCAT", "CONCAT_WS", "LOWER", "UPPER", "LCASE", "UCASE", "TRIM", "LTRIM", "RTRIM",
    "SUBSTRING", "SUBSTR", "LEFT", "RIGHT", "LENGTH", "CHAR_LENGTH", "CHARACTER_LENGTH",
    "REPLACE", "LPAD", "RPAD", "REVERSE", "LOCATE", "INSTR", "MD5", "SHA1", "SHA2", "HEX",
    "UNHEX",
    // dates and times
    "NOW", "CURRENT_TIMESTAMP", "CURRENT_DATE", "CURRENT_TIME", "CURDATE", "CURTIME",
    "UTC_TIMESTAMP", "UTC_DATE", "UTC_TIME", "DATE", "DATE_ADD", "DATE_SUB", "ADDDATE",
    "SUBDATE", "DATEDIFF", "TIMESTAMPDIFF", "TIMESTAMPADD", "DATE_FORMAT", "STR_TO_DATE",
    "YEAR", "MONTH", "DAY", "HOUR", "MINUTE", "SECOND", "UNIX_TIMESTAMP", "FROM_UNIXTIME",
    "LAST_DAY",
    // identifiers and chance
    "UUID", "RAND",
    // JSON
    "JSON_SET", "JSON_INSERT", "JSON_REPLACE", "JSON_REMOVE", "JSON_EXTRACT", "JSON_UNQUOTE",
    "JSON_OBJECT", "JSON_ARRAY", "JSON_CONTAINS",
    // aggregates, for subqueries
    "COUNT", "SUM", "AVG", "MIN", "MAX",
];

/// Walks the whole statement and refuses anything that could have an effect
/// beyond the statement itself, or that a preview cannot reproduce.
struct Guard;

impl Visitor for Guard {
    type Break = Refusal;

    fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<Refusal> {
        if uses_table_statement(&q.body) {
            // `TABLE t` names its table outside the parts the relation check sees.
            return ControlFlow::Break(Refusal(
                "TABLE statements are not supported; use SELECT".into(),
            ));
        }
        if !q.locks.is_empty() {
            return ControlFlow::Break(Refusal("locking reads are not allowed".into()));
        }
        if q.for_clause.is_some()
            || q.settings.is_some()
            || q.format_clause.is_some()
            || !q.pipe_operators.is_empty()
        {
            return ControlFlow::Break(Refusal("unsupported query syntax".into()));
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_select(&mut self, s: &Select) -> ControlFlow<Refusal> {
        if s.into.is_some() {
            return ControlFlow::Break(Refusal("SELECT ... INTO is not allowed".into()));
        }
        ControlFlow::Continue(())
    }

    /// Server-wide schemas are off limits: with the PROCESS grant Rowbit
    /// needs, `information_schema.PROCESSLIST` and `INNODB_TRX` show every
    /// session's running SQL, for every user and database.
    fn pre_visit_relation(&mut self, name: &ObjectName) -> ControlFlow<Refusal> {
        let database = match name.0.as_slice() {
            [ObjectNamePart::Identifier(db), _] => Some(db.value.as_str()),
            _ => None,
        };
        match database {
            Some(db) if is_system_schema(db) => ControlFlow::Break(Refusal(format!(
                "{db} is a system schema, which agents may not read or write"
            ))),
            _ => ControlFlow::Continue(()),
        }
    }

    fn pre_visit_table_factor(&mut self, t: &TableFactor) -> ControlFlow<Refusal> {
        match t {
            TableFactor::Table { args: None, .. }
            | TableFactor::Derived { .. }
            | TableFactor::NestedJoin { .. } => ControlFlow::Continue(()),
            _ => ControlFlow::Break(Refusal(format!("unsupported table source {t}"))),
        }
    }

    fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<Refusal> {
        match e {
            Expr::Function(f) => {
                let allowed = match f.name.0.as_slice() {
                    [ObjectNamePart::Identifier(id)] => ALLOWED_FUNCTIONS
                        .iter()
                        .any(|a| a.eq_ignore_ascii_case(&id.value)),
                    _ => false,
                };
                if !allowed {
                    return ControlFlow::Break(Refusal(format!(
                        "function {} is not allowed",
                        f.name
                    )));
                }
            }
            Expr::BinaryOp {
                op: BinaryOperator::Assignment,
                ..
            } => {
                return ControlFlow::Break(Refusal("variable assignment is not allowed".into()));
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_ident(&mut self, id: &Ident) -> ControlFlow<Refusal> {
        match id.quote_style {
            None if id.value.starts_with('@') => {
                ControlFlow::Break(Refusal(format!("variables ({}) are not allowed", id.value)))
            }
            None | Some('`') => ControlFlow::Continue(()),
            Some(_) => ControlFlow::Break(Refusal(format!("unsupported identifier {id}"))),
        }
    }

    /// Only literals that print back exactly as MySQL reads them. Strings
    /// may not contain a backslash: the parser unescapes `\\` but printing
    /// does not escape it again, which would let a literal end early in the
    /// SQL Rowbit runs.
    fn pre_visit_value(&mut self, v: &ValueWithSpan) -> ControlFlow<Refusal> {
        let refusal = match &v.value {
            Value::Number(..) | Value::Boolean(_) | Value::Null => None,
            Value::SingleQuotedString(s)
            | Value::DoubleQuotedString(s)
            | Value::NationalStringLiteral(s) => s
                .contains('\\')
                .then(|| "string literals may not contain a backslash".to_owned()),
            Value::HexStringLiteral(h) => (!h.bytes().all(|b| b.is_ascii_hexdigit()))
                .then(|| "malformed hex literal".to_owned()),
            Value::Placeholder(_) => Some("placeholders are not allowed".to_owned()),
            other => Some(format!("unsupported literal {other}")),
        };
        match refusal {
            Some(r) => ControlFlow::Break(Refusal(r)),
            None => ControlFlow::Continue(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(sql: &str) -> Classified {
        classify(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    fn refused(sql: &str, needle: &str) {
        match classify(sql) {
            Ok(c) => panic!("{sql} was accepted: {c:?}"),
            Err(Refusal(reason)) => assert!(
                reason.to_lowercase().contains(&needle.to_lowercase()),
                "{sql} refused with {reason:?}, expected {needle:?}"
            ),
        }
    }

    fn preview(c: &Classified) -> &PreviewRead {
        c.preview.as_ref().expect("has a preview")
    }

    fn existing(from: &str, qualifier: &str, filter: &str, assignments: &str) -> PreviewRead {
        PreviewRead::Existing {
            from: from.into(),
            qualifier: qualifier.into(),
            filter: filter.into(),
            assignments: assignments.into(),
        }
    }

    #[test]
    fn update_keeps_target_filter_and_assignments() {
        let c = ok(
            "UPDATE billing.refunds SET status = 'refunded', updated_at = NOW() \
             WHERE status = 'approved' AND amount < 100 ORDER BY id LIMIT 50",
        );
        assert_eq!(c.kind, Kind::Update);
        assert_eq!(
            c.table,
            TableName {
                database: Some("billing".into()),
                table: "refunds".into()
            }
        );
        assert_eq!(c.columns, ["status", "updated_at"]);
        assert_eq!(
            preview(&c),
            &existing(
                "billing.refunds",
                "refunds",
                " WHERE status = 'approved' AND amount < 100 ORDER BY id LIMIT 50",
                "`status` = 'refunded', `updated_at` = NOW()"
            )
        );
    }

    #[test]
    fn update_with_alias_qualifies_by_alias() {
        let c = ok("UPDATE refunds r SET r.status = 'x' WHERE r.id = 1");
        assert_eq!(c.columns, ["status"]);
        assert_eq!(
            preview(&c),
            &existing("refunds r", "r", " WHERE r.id = 1", "`status` = 'x'")
        );
    }

    #[test]
    fn delete_keeps_target_and_filter() {
        let c = ok("DELETE FROM `app`.`sessions` WHERE expires_at < NOW() ORDER BY id LIMIT 1000");
        assert_eq!(c.kind, Kind::Delete);
        assert_eq!(c.table.table, "sessions");
        assert!(c.columns.is_empty());
        assert_eq!(
            preview(&c),
            &existing(
                "`app`.`sessions`",
                "sessions",
                " WHERE expires_at < NOW() ORDER BY id LIMIT 1000",
                ""
            )
        );
    }

    #[test]
    fn insert_keeps_its_source() {
        let c = ok("INSERT INTO notes (id, body) VALUES (1, 'a'), (2, CONCAT('b', 'c'))");
        assert_eq!(c.kind, Kind::Insert);
        assert_eq!(c.columns, ["id", "body"]);
        assert_eq!(
            preview(&c),
            &PreviewRead::New {
                source: "VALUES (1, 'a'), (2, CONCAT('b', 'c'))".into()
            }
        );
        let c = ok("INSERT INTO archive (id, total) SELECT id, amount FROM orders WHERE id < 10");
        assert_eq!(
            preview(&c),
            &PreviewRead::New {
                source: "SELECT id, amount FROM orders WHERE id < 10".into()
            }
        );
    }

    /// Found in review: `'x\\'` printed back as `'x\'`, so the literal ran on
    /// into the rest of the SQL and the agent controlled the preview read.
    #[test]
    fn backslashes_in_literals_are_refused() {
        refused(
            "UPDATE t SET a = 'x\\\\', b = ') AS q, SLEEP(1) AS r FROM t -- ' WHERE id = 1",
            "backslash",
        );
        refused("UPDATE t SET a = 'it\\'s'", "backslash");
        refused("UPDATE t SET a = \"x\\\\\"", "backslash");
        refused("UPDATE t SET a = N'x\\\\'", "backslash");
        refused("DELETE FROM t WHERE a = 'x\\\\'", "backslash");
        refused("INSERT INTO t (a) VALUES ('\\\\')", "backslash");
        refused("UPDATE t SET a = b'0101'", "unsupported literal");
        // Quotes inside literals and identifiers print back doubled, which is safe.
        let c = ok("UPDATE `we``ird` SET a = 'it''s' WHERE b = \"say \"\"hi\"\"\"");
        assert_eq!(c.table.table, "we`ird");
        assert!(c.canonical.contains("'it''s'"), "{}", c.canonical);
        ok("UPDATE t SET a = X'0aFF', b = TRUE, c = NULL, d = -1.5e3");
    }

    #[test]
    fn chance_may_not_pick_rows() {
        refused("DELETE FROM t WHERE RAND() < 0.5", "RAND");
        refused("UPDATE t SET a = 1 ORDER BY rand() LIMIT 1", "rand");
        refused(
            "DELETE FROM t WHERE id IN (SELECT id FROM u WHERE UUID() > 'a')",
            "UUID",
        );
        // Chance in the new values is fine: it is evaluated once, in the preview.
        ok("UPDATE t SET token = UUID(), r = RAND() WHERE id = 1");
    }

    #[test]
    fn ddl_is_classified_without_preview() {
        for (sql, table) in [
            ("ALTER TABLE users ADD COLUMN age INT", "users"),
            ("CREATE TABLE t (id INT PRIMARY KEY)", "t"),
            ("DROP TABLE app.old", "old"),
            ("TRUNCATE TABLE logs", "logs"),
            ("CREATE INDEX i ON users (email)", "users"),
            ("RENAME TABLE a TO b", "a"),
        ] {
            let c = ok(sql);
            assert_eq!(c.kind, Kind::Ddl, "{sql}");
            assert_eq!(c.table.table, table, "{sql}");
            assert!(c.preview.is_none());
        }
    }

    // Found in review: with PROCESS, these show every session's SQL.
    #[test]
    fn system_schemas_are_off_limits() {
        for sql in [
            "SELECT INFO FROM information_schema.PROCESSLIST",
            "SELECT trx_query FROM INFORMATION_SCHEMA.INNODB_TRX",
            "SELECT * FROM performance_schema.events_statements_current",
            "SELECT user, authentication_string FROM mysql.user",
            "SELECT * FROM sys.processlist",
            "SELECT * FROM t WHERE a IN (SELECT ID FROM information_schema.PROCESSLIST)",
            "SELECT 1 UNION SELECT INFO FROM `information_schema`.`PROCESSLIST`",
            "TABLE information_schema.PROCESSLIST",
            "SELECT 1 UNION TABLE t",
        ] {
            assert!(classify_read(sql).is_err(), "{sql}");
        }
        refused(
            "UPDATE t SET a = (SELECT MAX(INFO) FROM information_schema.PROCESSLIST)",
            "system schema",
        );
        refused("DELETE FROM mysql.user", "system schema");
        // Found in review 4: DROP and RENAME names aren't visited as relations.
        for sql in [
            "DROP TABLE mysql.user",
            "RENAME TABLE shop.orders TO sys.orders",
            "RENAME TABLE mysql.user TO shop.u",
            "TRUNCATE TABLE mysql.general_log",
            "ALTER TABLE mysql.user ADD COLUMN x INT",
            "CREATE TABLE sys.x (id INT PRIMARY KEY)",
            "CREATE INDEX i ON mysql.user (host)",
        ] {
            refused(sql, "system schema");
        }
        assert!(classify_read("SELECT * FROM shop.orders").is_ok());
    }

    #[test]
    fn reads_are_checked_like_writes() {
        assert_eq!(
            classify_read("select id, status from refunds where amount > 10").unwrap(),
            "SELECT id, status FROM refunds WHERE amount > 10"
        );
        assert!(classify_read("UPDATE t SET a = 1").is_err());
        assert!(classify_read("SELECT SLEEP(10)").is_err());
        assert!(classify_read("SELECT * FROM t FOR UPDATE").is_err());
        assert!(classify_read("SELECT * FROM t INTO OUTFILE '/tmp/x'").is_err());
        assert!(classify_read("SELECT @@version").is_err());
        assert!(classify_read("SELECT 1; SELECT 2").is_err());
    }

    #[test]
    fn exactly_one_statement() {
        refused("UPDATE t SET a = 1; DELETE FROM t", "exactly one");
        refused("", "exactly one");
        refused("UPDATE t SET", "could not parse");
    }

    #[test]
    fn reads_and_other_statements_are_refused() {
        refused("SELECT * FROM t", "query tool");
        refused("CALL refund_all()", "only INSERT");
        refused("SET @x = 1", "variables");
        refused("GRANT ALL ON *.* TO 'a'", "");
        refused("DROP DATABASE app", "only INSERT");
        refused("DROP TABLE a, b", "one table");
    }

    #[test]
    fn unsupported_write_shapes_are_refused() {
        refused("REPLACE INTO t (id) VALUES (1)", "REPLACE");
        refused("INSERT IGNORE INTO t (id) VALUES (1)", "IGNORE");
        refused(
            "INSERT INTO t (id) VALUES (1) ON DUPLICATE KEY UPDATE id = 2",
            "DUPLICATE",
        );
        refused("INSERT INTO t VALUES (1)", "name the columns");
        refused("INSERT INTO t SET id = 1", "INSERT ... SET");
        refused("INSERT INTO t (id, a) VALUES (1)", "1 values for 2 columns");
        refused("INSERT INTO t (id) VALUES (DEFAULT)", "DEFAULT");
        refused("UPDATE a JOIN b ON a.id = b.id SET a.x = 1", "multi-table");
        refused("DELETE a FROM a JOIN b ON a.id = b.id", "multi-table");
        refused("DELETE FROM a USING a, b WHERE a.id = b.id", "multi-table");
        refused("UPDATE t PARTITION (p0) SET a = 1", "table options");
        refused("UPDATE t SET a = 1 ORDER BY id LIMIT 5 + 5", "LIMIT");
        refused("DELETE FROM t LIMIT 5", "ORDER BY");
        refused("UPDATE t SET (a, b) = (1, 2)", "");
    }

    #[test]
    fn modifiers_are_not_mistaken_for_tables() {
        refused("UPDATE IGNORE t SET a = 1", "IGNORE");
        refused("UPDATE LOW_PRIORITY t SET a = 1", "LOW_PRIORITY");
        refused("DELETE LOW_PRIORITY QUICK IGNORE FROM t", "");
    }

    #[test]
    fn set_may_read_earlier_assignments() {
        // The preview replays SET on the server, left to right, as the write would.
        ok("UPDATE t SET a = a + 1, b = a");
        refused("UPDATE t SET a = 1, a = 2", "twice");
    }

    #[test]
    fn set_target_must_belong_to_the_table() {
        refused("UPDATE t SET other.a = 1", "not a column");
        ok("UPDATE app.t SET app.t.a = 1");
    }

    #[test]
    fn side_effects_are_refused_anywhere() {
        refused("UPDATE t SET a = SLEEP(5)", "SLEEP");
        refused(
            "DELETE FROM t WHERE id IN (SELECT GET_LOCK('x', 1))",
            "GET_LOCK",
        );
        refused(
            "UPDATE t SET a = my_stored_function(1)",
            "my_stored_function",
        );
        refused("UPDATE t SET a = NEXTVAL(seq)", "NEXTVAL");
        refused("UPDATE t SET a = LOAD_FILE('/etc/passwd')", "LOAD_FILE");
        refused("UPDATE t SET a = @x", "variables");
        refused("UPDATE t SET a = @@sql_mode", "variables");
        refused("UPDATE t SET a = (@x := 1)", "");
        refused("UPDATE t SET a = ?", "placeholders");
        refused(
            "DELETE FROM t WHERE id IN (SELECT id FROM u FOR UPDATE)",
            "locking",
        );
        refused(
            "INSERT INTO t (id) SELECT id FROM u INTO OUTFILE '/tmp/x'",
            "",
        );
        refused("UPDATE t SET a = 1 /*!, b = 2 */", "executable comments");
    }

    #[test]
    fn subqueries_and_allowed_functions_pass() {
        let c = ok("UPDATE t SET a = (SELECT MAX(x) FROM u WHERE u.id = t.id) \
                    WHERE EXISTS (SELECT 1 FROM v WHERE v.t_id = t.id)");
        assert_eq!(c.columns, ["a"]);
        ok("UPDATE t SET a = COALESCE(b, 0), c = DATE_ADD(NOW(), INTERVAL 1 DAY) WHERE id = 1");
    }

    #[test]
    fn quoted_names_keep_their_quotes() {
        let c = ok("UPDATE `my table` SET `the col` = 1");
        assert_eq!(c.table.table, "my table");
        assert_eq!(c.columns, ["the col"]);
        assert_eq!(
            preview(&c),
            &existing("`my table`", "my table", "", "`the col` = 1")
        );
    }
}
