//! The tables an agent may use, for the MCP `describe_schema` tool.

use std::collections::BTreeSet;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, OptsBuilder};

use crate::MySqlAdapter;
use crate::grants::Catalog;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    pub database: String,
    pub name: String,
    pub columns: Vec<ColumnInfo>,
    /// A view that runs with its definer's rights (not `SQL SECURITY
    /// INVOKER`).
    pub definer_view: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    /// As the server writes it, e.g. `decimal(10,2)`.
    pub column_type: String,
    pub nullable: bool,
    pub primary_key: bool,
}

impl MySqlAdapter {
    /// Every table and view in `databases` that Rowbit's user can see,
    /// with its columns, by database and name.
    pub async fn tables(
        &self,
        databases: &BTreeSet<String>,
    ) -> Result<Vec<TableInfo>, mysql_async::Error> {
        let mut conn =
            Conn::new(OptsBuilder::from_opts(self.opts.clone()).db_name(None::<String>)).await?;
        let mut out: Vec<TableInfo> = Vec::new();
        for db in databases {
            let rows: Vec<(String, String, String, String, String)> = conn
                .exec(
                    "SELECT TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY \
                     FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? \
                     ORDER BY TABLE_NAME, ORDINAL_POSITION",
                    (db,),
                )
                .await?;
            for (table, name, column_type, nullable, key) in rows {
                if out
                    .last()
                    .is_none_or(|t| t.database != *db || t.name != table)
                {
                    out.push(TableInfo {
                        database: db.clone(),
                        name: table,
                        columns: Vec::new(),
                        definer_view: false,
                    });
                }
                if let Some(t) = out.last_mut() {
                    t.columns.push(ColumnInfo {
                        name,
                        column_type,
                        nullable: nullable == "YES",
                        primary_key: key == "PRI",
                    });
                }
            }
        }
        for db in databases {
            let views: Vec<String> = conn
                .exec(
                    "SELECT TABLE_NAME FROM information_schema.VIEWS \
                     WHERE TABLE_SCHEMA = ? AND SECURITY_TYPE <> 'INVOKER'",
                    (db,),
                )
                .await?;
            for t in out
                .iter_mut()
                .filter(|t| t.database == *db && views.contains(&t.name))
            {
                t.definer_view = true;
            }
        }
        let _ = conn.disconnect().await;
        Ok(out)
    }
}

/// The tables and their column names, for compiling scopes.
pub fn catalog(tables: &[TableInfo]) -> Catalog {
    let mut catalog = Catalog::default();
    for t in tables {
        catalog.tables.insert(
            (t.database.clone(), t.name.clone()),
            t.columns.iter().map(|c| c.name.clone()).collect(),
        );
        if t.definer_view {
            catalog
                .definer_views
                .insert((t.database.clone(), t.name.clone()));
        }
    }
    catalog
}
