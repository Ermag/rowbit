//! What a preview found: the rows a write would change, with their before
//! and after images. See "Preview" in `docs/spec/change-lifecycle.md`.
//!
//! This is what a reviewer approves and what apply writes, so values are
//! kept exactly as the server sent them over its text protocol.

/// One column value: `None` is SQL `NULL`, otherwise the server's text
/// rendering of the value (raw bytes for binary columns).
pub type Cell = Option<Vec<u8>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    Insert,
    Update,
    Delete,
}

/// The table a write targets, as the server describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub database: String,
    pub name: String,
    /// Every column, in table order.
    pub columns: Vec<String>,
    /// Indexes into `columns`, in key order. Never empty.
    pub primary_key: Vec<usize>,
}

impl Table {
    /// Canonical `database.table`, as `rowbit.yml` names tables.
    pub fn qualified_name(&self) -> String {
        format!("{}.{}", self.database, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub kind: WriteKind,
    pub table: Table,
    /// Indexes into `table.columns` of the columns the write sets. Every
    /// column for a delete.
    pub written: Vec<usize>,
    pub rows: Vec<RowChange>,
}

/// One row the write would change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowChange {
    /// Every column, in table order. `None` for an insert.
    pub before: Option<Vec<Cell>>,
    /// The written columns, in the order of [`Preview::written`]. `None`
    /// for a delete.
    pub after: Option<Vec<Cell>>,
}

impl Preview {
    /// The row's primary key, or `None` for an inserted row whose key the
    /// server assigns (`AUTO_INCREMENT`).
    pub fn key(&self, row: &RowChange) -> Option<Vec<Cell>> {
        if let Some(before) = &row.before {
            return Some(
                self.table
                    .primary_key
                    .iter()
                    .map(|&i| before[i].clone())
                    .collect(),
            );
        }
        let after = row.after.as_ref()?;
        self.table
            .primary_key
            .iter()
            .map(|k| {
                self.written
                    .iter()
                    .position(|w| w == k)
                    .map(|p| after[p].clone())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(s: &str) -> Cell {
        Some(s.as_bytes().to_vec())
    }

    fn table() -> Table {
        Table {
            database: "billing".into(),
            name: "refunds".into(),
            columns: vec!["id".into(), "status".into(), "amount".into()],
            primary_key: vec![0],
        }
    }

    #[test]
    fn key_comes_from_the_before_image() {
        let p = Preview {
            kind: WriteKind::Update,
            table: table(),
            written: vec![1],
            rows: vec![],
        };
        let row = RowChange {
            before: Some(vec![cell("7"), cell("approved"), cell("12.50")]),
            after: Some(vec![cell("refunded")]),
        };
        assert_eq!(p.key(&row), Some(vec![cell("7")]));
        assert_eq!(p.table.qualified_name(), "billing.refunds");
    }

    #[test]
    fn inserted_key_is_known_only_when_written() {
        let mut p = Preview {
            kind: WriteKind::Insert,
            table: table(),
            written: vec![2, 0],
            rows: vec![],
        };
        let row = RowChange {
            before: None,
            after: Some(vec![cell("5.00"), cell("9")]),
        };
        assert_eq!(p.key(&row), Some(vec![cell("9")]));
        p.written = vec![2];
        let row = RowChange {
            before: None,
            after: Some(vec![cell("5.00")]),
        };
        assert_eq!(p.key(&row), None);
    }
}
