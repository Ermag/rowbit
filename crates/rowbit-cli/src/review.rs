//! How changes look to a person in the terminal: one line per change in a
//! list, and the whole review (statement, rows before and after, history)
//! for one change.

use std::fmt::Write;

use rowbit_core::preview::{Cell, Preview, WriteKind};
use rowbit_store::{AuditEvent, Change, Review};

/// Rows shown per change before the rest is summarised.
const SHOWN_ROWS: usize = 50;

pub fn line(change: &Change) -> String {
    let (table, rows) = match &change.review {
        Some(Review::Rows(p)) => (p.table.qualified_name(), format!("{} rows", p.rows.len())),
        Some(Review::Ddl { .. }) => ("schema change".to_owned(), String::new()),
        None => ("-".to_owned(), String::new()),
    };
    format!(
        "{:>5}  {:<9} {:<16} {:<32} {}",
        change.id,
        change.state.as_str(),
        change.agent,
        table,
        rows
    )
}

pub fn full(change: &Change, history: &[AuditEvent]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Change {}  {}", change.id, change.state.as_str());
    let _ = writeln!(out, "Agent     {}", change.agent);
    let _ = writeln!(out, "Database  {}", change.database);
    let _ = writeln!(out, "Statement {}", change.sql);
    match &change.review {
        Some(Review::Rows(p)) => {
            let _ = writeln!(out, "Table     {}", p.table.qualified_name());
            let _ = writeln!(out);
            out.push_str(&rows(p));
        }
        Some(Review::Ddl { sql }) => {
            let _ = writeln!(out, "\nRuns      {sql}");
            let _ = writeln!(out, "Schema changes cannot be undone.");
        }
        None => {}
    }
    if let Some(hash) = &change.review_hash {
        let _ = writeln!(out, "\nReview    {hash}");
    }
    let _ = writeln!(out, "\nHistory");
    for e in history {
        let detail = e
            .detail
            .as_deref()
            .filter(|d| !d.is_empty() && e.event != "ApplySucceeded")
            .map(|d| format!("  {d}"))
            .unwrap_or_default();
        let _ = writeln!(out, "  {:<15} {:<28} {}{detail}", e.event, e.actor, e.to);
    }
    out
}

/// Each row's key and the columns that change, before and after.
fn rows(p: &Preview) -> String {
    let mut out = String::new();
    let verb = match p.kind {
        WriteKind::Insert => "inserts",
        WriteKind::Update => "updates",
        WriteKind::Delete => "deletes",
    };
    let _ = writeln!(out, "This change {verb} {} rows:", p.rows.len());
    for row in p.rows.iter().take(SHOWN_ROWS) {
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
            let _ = writeln!(out, "    {:<20} {line}", column.name);
        }
    }
    if p.rows.len() > SHOWN_ROWS {
        let _ = writeln!(out, "  ... and {} more", p.rows.len() - SHOWN_ROWS);
    }
    out
}

fn show(cell: &Cell) -> String {
    match cell {
        None => "NULL".to_owned(),
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => format!("{text:?}"),
            Err(_) => {
                let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                format!("0x{hex}")
            }
        },
    }
}
