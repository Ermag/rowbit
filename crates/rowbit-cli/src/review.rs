//! How changes look to a person in the terminal: one line per change in a
//! list, and the whole review (statement, rows before and after, history)
//! for one change.

use std::fmt::Write;

use rowbit_core::preview::{Cell, Preview, WriteKind};
use rowbit_store::{AuditEvent, Change, Review};

/// Rows shown per change before the rest is summarised.
pub const SHOWN_ROWS: usize = 50;

/// Text the agent or the database controls, made safe for a terminal:
/// control characters (escape sequences that recolour, hide or move text,
/// or write to the clipboard), newlines (so a value can't fake a line of
/// its own) and invisible or direction-changing Unicode are shown as
/// `\u{..}` instead of acting on the screen.
pub fn clean(text: &str) -> String {
    rowbit_core::text::reveal(text)
}

/// The statement as Rowbit understood it (comments dropped), or the
/// agent's text if it never parsed. Cleaned either way.
fn statement(sql: &str) -> String {
    match rowbit_mysql::statement::classify(sql) {
        Ok(c) => clean(&c.canonical),
        Err(_) => clean(sql),
    }
}

pub fn line(change: &Change) -> String {
    let (table, rows) = match &change.review {
        Some(Review::Rows(p)) => (p.table.qualified_name(), count(p.rows.len())),
        Some(Review::Ddl { .. }) => ("schema change".to_owned(), String::new()),
        None => ("-".to_owned(), String::new()),
    };
    format!(
        "{:>5}  {:<9} {:<16} {:<32} {}",
        change.id,
        change.state.as_str(),
        clean(&change.agent),
        clean(&table),
        rows
    )
}

/// Everything about one change. `all` lists every row, not just the first
/// [`SHOWN_ROWS`].
pub fn full(change: &Change, history: &[AuditEvent], all: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Change {}  {}", change.id, change.state.as_str());
    let _ = writeln!(out, "Agent     {}", clean(&change.agent));
    let _ = writeln!(out, "Database  {}", clean(&change.database));
    let _ = writeln!(out, "Statement {}", statement(&change.sql));
    match &change.review {
        Some(Review::Rows(p)) => {
            let _ = writeln!(out, "Table     {}", clean(&p.table.qualified_name()));
            let _ = writeln!(out);
            out.push_str(&rows(p, change.id, all));
        }
        Some(Review::Ddl { sql }) => {
            let _ = writeln!(out, "\nRuns      {}", clean(sql));
            let _ = writeln!(out, "Schema changes cannot be undone.");
        }
        None => {}
    }
    if let Some(hash) = &change.review_hash {
        let _ = writeln!(out, "\nReview    {hash}");
    }
    let _ = writeln!(out, "\nHistory");
    for e in history {
        let detail = match e.event.as_str() {
            // The statement is shown above, canonical; the agent's own text
            // (comments included) stays in the log for whoever needs it.
            "Proposed" | "ApplySucceeded" => None,
            _ => e.detail.as_deref().filter(|d| !d.is_empty()),
        }
        .map(|d| format!("  {}", clean(d)))
        .unwrap_or_default();
        let _ = writeln!(
            out,
            "  {:<15} {:<28} {}{detail}",
            clean(&e.event),
            clean(&e.actor),
            clean(&e.to)
        );
    }
    out
}

/// Each row's key and the columns that change, before and after.
fn rows(p: &Preview, id: i64, all: bool) -> String {
    let mut out = String::new();
    let verb = match p.kind {
        WriteKind::Insert => "inserts",
        WriteKind::Update => "updates",
        WriteKind::Delete => "deletes",
    };
    let _ = writeln!(out, "This change {verb} {}:", count(p.rows.len()));
    let shown = if all { p.rows.len() } else { SHOWN_ROWS };
    for row in p.rows.iter().take(shown) {
        let key = match p.key(row) {
            Some(key) => key.iter().map(show).collect::<Vec<_>>().join(", "),
            None => "new".to_owned(),
        };
        let _ = writeln!(out, "  row {key}");
        for (i, column) in p.table.columns.iter().enumerate() {
            let before = row.before.as_ref().map(|b| &b[i]);
            let after = row.after.as_ref().map(|a| &a[i]);
            let line = match (before, after) {
                (Some(b), Some(a)) if b == a => continue,
                (Some(b), Some(a)) => format!("{} -> {}", show(b), show(a)),
                (Some(b), None) => show(b),
                (None, Some(a)) => show(a),
                (None, None) => continue,
            };
            let _ = writeln!(out, "    {:<20} {line}", clean(&column.name));
        }
    }
    if p.rows.len() > shown {
        let _ = writeln!(
            out,
            "  ... and {} more. Approving covers all {}; `rowbit show {id} --all` lists them.",
            p.rows.len() - shown,
            p.rows.len()
        );
    }
    out
}

/// "1 row", "2 rows".
pub fn count(rows: usize) -> String {
    match rows {
        1 => "1 row".to_owned(),
        n => format!("{n} rows"),
    }
}

fn show(cell: &Cell) -> String {
    match cell {
        None => "NULL".to_owned(),
        Some(bytes) => match std::str::from_utf8(bytes) {
            // Numbers plain, text quoted, so "12" and 12 can't be confused.
            Ok(text) if !text.is_empty() && text.parse::<f64>().is_ok() => text.to_owned(),
            Ok(text) => format!("{text:?}"),
            Err(_) => {
                let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                format!("0x{hex}")
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Found in review: an agent's string literal carried ESC[8m (conceal)
    // to the reviewer's terminal, hiding what followed.
    #[test]
    fn agent_text_cannot_drive_the_terminal() {
        let nasty = "a\u{1b}[8mhidden\u{7}\nfake line\u{202e}rtl\u{200b}\u{1b}]52;c;Zm9v\u{7}";
        let cleaned = clean(nasty);
        assert!(!cleaned.chars().any(|c| c.is_control()), "{cleaned:?}");
        assert!(!cleaned.contains('\u{202e}') && !cleaned.contains('\u{200b}'));
        assert!(cleaned.contains("\\u{1b}[8m"), "{cleaned}");
        assert_eq!(clean("plain 'text', 12.50"), "plain 'text', 12.50");
    }

    #[test]
    fn statements_show_without_comments() {
        assert_eq!(
            statement("UPDATE t SET a = 1 /* reviewed, safe */ WHERE id = 2 -- trust me"),
            "UPDATE t SET a = 1 WHERE id = 2"
        );
        assert_eq!(statement("not sql \u{1b}[2J"), "not sql \\u{1b}[2J");
    }
}
