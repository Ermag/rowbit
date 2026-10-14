//! What a preview found: the rows a write would change, with their before
//! and after images. See "Preview" in `docs/spec/change-lifecycle.md`.
//!
//! This is what a reviewer approves and what apply writes, so values are
//! kept exactly as the server sent them over its text protocol, and after
//! images are what the table would store, not what an expression evaluated
//! to.
//!
//! [`Preview::to_canonical_json`] is the stored form and the input to the
//! preview hash. Cells are hex strings in it, so binary values survive.

use serde::{Deserialize, Serialize};

/// One column value: `None` is SQL `NULL`, otherwise the server's text
/// rendering of the value (raw bytes for binary columns).
pub type Cell = Option<Vec<u8>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WriteKind {
    Insert,
    Update,
    Delete,
}

/// The table a write targets, as the server describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    pub database: String,
    pub name: String,
    /// Every column, in table order.
    pub columns: Vec<Column>,
    /// Indexes into `columns`, in key order. Never empty.
    pub primary_key: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub name: String,
    /// The server's full type, e.g. `decimal(10,2)` or `enum('a','b')`.
    pub column_type: String,
    /// Computed by the server (`VIRTUAL` or `STORED`). Never written by
    /// apply or undo; its images are shown and compared only.
    pub generated: bool,
    pub auto_increment: bool,
}

impl Table {
    /// Canonical `database.table`, as `rowbit.yml` names tables.
    pub fn qualified_name(&self) -> String {
        format!("{}.{}", self.database, self.name)
    }

    pub fn column_names(&self) -> impl Iterator<Item = &str> {
        self.columns.iter().map(|c| c.name.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preview {
    pub kind: WriteKind,
    pub table: Table,
    /// Indexes into `table.columns` of the columns the statement names.
    /// Every column for a delete. Other columns can still change (for
    /// example `ON UPDATE CURRENT_TIMESTAMP` or generated columns); the
    /// images show them.
    pub written: Vec<usize>,
    pub rows: Vec<RowChange>,
    /// The server time the preview ran at, as `UNIX_TIMESTAMP(NOW(6))`.
    /// `NOW()` and friends are pinned to it, during the preview and when
    /// apply reads the filter again.
    pub as_of: String,
}

/// One row the write would change. Both images cover every column, in
/// table order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RowChange {
    /// `None` for an insert.
    #[serde(with = "hex_cells")]
    pub before: Option<Vec<Cell>>,
    /// `None` for a delete. For an insert whose key the server assigns,
    /// the key's cell is `None`.
    #[serde(with = "hex_cells")]
    pub after: Option<Vec<Cell>>,
}

impl Preview {
    /// The stored form of a preview, and the bytes its hash covers. Field
    /// order is fixed by the struct definitions, so equal previews always
    /// give equal bytes.
    pub fn to_canonical_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a preview always serializes")
    }

    pub fn from_canonical_json(bytes: &[u8]) -> Result<Preview, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// The row's primary key, or `None` for an inserted row whose key the
    /// server assigns (`AUTO_INCREMENT`).
    pub fn key(&self, row: &RowChange) -> Option<Vec<Cell>> {
        if let Some(before) = &row.before {
            return Some(self.pick_key(before));
        }
        let assigned = self
            .table
            .primary_key
            .iter()
            .any(|k| !self.written.contains(k));
        if assigned {
            return None;
        }
        row.after.as_ref().map(|after| self.pick_key(after))
    }

    fn pick_key(&self, image: &[Cell]) -> Vec<Cell> {
        self.table
            .primary_key
            .iter()
            .map(|&i| image[i].clone())
            .collect()
    }
}

/// What apply wrote: the primary key of each previewed row, in preview
/// order, including keys the server assigned to inserted rows. Undo finds
/// the rows by these keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Applied {
    pub keys: Vec<AppliedKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedKey {
    #[serde(with = "hex_cells")]
    pub key: Option<Vec<Cell>>,
}

impl Applied {
    /// The stored form, and the bytes the `ApplySucceeded` event's hash covers.
    pub fn to_canonical_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("an apply record always serializes")
    }
}

/// Serializes cells as hex strings (`null` for SQL `NULL`).
mod hex_cells {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::Cell;

    pub fn serialize<S: Serializer>(image: &Option<Vec<Cell>>, s: S) -> Result<S::Ok, S::Error> {
        let hex: Option<Vec<Option<String>>> = image.as_ref().map(|cells| {
            cells
                .iter()
                .map(|c| {
                    c.as_ref()
                        .map(|b| b.iter().map(|x| format!("{x:02x}")).collect())
                })
                .collect()
        });
        hex.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<Cell>>, D::Error> {
        let hex: Option<Vec<Option<String>>> = Deserialize::deserialize(d)?;
        hex.map(|cells| {
            cells
                .into_iter()
                .map(|c| {
                    c.map(|h| decode(&h).ok_or_else(|| D::Error::custom("bad hex cell")))
                        .transpose()
                })
                .collect()
        })
        .transpose()
    }

    fn decode(h: &str) -> Option<Vec<u8>> {
        if h.len() % 2 != 0 {
            return None;
        }
        (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(s: &str) -> Cell {
        Some(s.as_bytes().to_vec())
    }

    fn column(name: &str) -> Column {
        Column {
            name: name.into(),
            column_type: "varchar(20)".into(),
            generated: false,
            auto_increment: false,
        }
    }

    fn table() -> Table {
        Table {
            database: "billing".into(),
            name: "refunds".into(),
            columns: vec![column("id"), column("status"), column("amount")],
            primary_key: vec![0],
        }
    }

    fn preview(kind: WriteKind, written: Vec<usize>) -> Preview {
        Preview {
            kind,
            table: table(),
            written,
            rows: vec![],
            as_of: "1791900000.000000".into(),
        }
    }

    #[test]
    fn key_comes_from_the_before_image() {
        let p = preview(WriteKind::Update, vec![1]);
        let row = RowChange {
            before: Some(vec![cell("7"), cell("approved"), cell("12.50")]),
            after: Some(vec![cell("7"), cell("refunded"), cell("12.50")]),
        };
        assert_eq!(p.key(&row), Some(vec![cell("7")]));
        assert_eq!(p.table.qualified_name(), "billing.refunds");
    }

    #[test]
    fn canonical_json_round_trips_binary_and_null() {
        let mut p = preview(WriteKind::Update, vec![1]);
        p.rows.push(RowChange {
            before: Some(vec![cell("7"), Some(vec![0, 255, b'"']), None]),
            after: Some(vec![cell("7"), None, None]),
        });
        let json = p.to_canonical_json();
        let text = String::from_utf8(json.clone()).unwrap();
        assert!(
            text.contains(r#""rows":[{"before":["37","00ff22",null],"after":["37",null,null]}]"#),
            "{text}"
        );
        assert!(
            text.starts_with(r#"{"kind":"update","table":{"database":"billing""#),
            "{text}"
        );
        assert_eq!(Preview::from_canonical_json(&json).unwrap(), p);
        assert!(Preview::from_canonical_json(br#"{"kind":"update"}"#).is_err());
    }

    #[test]
    fn inserted_key_is_known_only_when_written() {
        let row = RowChange {
            before: None,
            after: Some(vec![cell("9"), None, cell("5.00")]),
        };
        assert_eq!(
            preview(WriteKind::Insert, vec![2, 0]).key(&row),
            Some(vec![cell("9")])
        );
        assert_eq!(preview(WriteKind::Insert, vec![2]).key(&row), None);
    }
}
