//! Runs a proposed change through the lifecycle in
//! `docs/spec/change-lifecycle.md`: record it, classify it, preview it,
//! evaluate `rowbit.yml`, and record the decision. Every surface (MCP, CLI,
//! web) calls this; none moves a change any other way.
//!
//! Anything that goes wrong before a decision blocks the change. What an
//! agent gets back is a [`ChangeStatus`], which never carries row values.

use std::collections::BTreeMap;
use std::sync::Mutex;

use rowbit_core::change::{Actor, Event, State};
use rowbit_core::preview::{Preview, WriteKind};
use rowbit_mysql::statement::{Classified, Kind, classify};
use rowbit_mysql::{MySqlAdapter, PreviewLimits};
use rowbit_policy::{ChangeFacts, Decision, PolicyFile, StatementKind, Verdict, evaluate};
use rowbit_store::{ChangeId, Review, Store, StoreError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("change {0} not found")]
    NotFound(ChangeId),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// What an agent may learn about a change: never row values, which can come
/// from tables the agent should not read (see "Preview" in the spec).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeStatus {
    pub id: ChangeId,
    pub state: State,
    /// `database.table`, once known.
    pub table: Option<String>,
    /// How many rows the change writes, once previewed.
    pub rows: Option<usize>,
    /// Why it was held, blocked, rejected or failed.
    pub reason: Option<String>,
}

pub struct Engine {
    adapter: MySqlAdapter,
    policy: PolicyFile,
    store: Mutex<Store>,
    limits: PreviewLimits,
}

impl Engine {
    pub fn new(adapter: MySqlAdapter, policy: PolicyFile, store: Store) -> Engine {
        Engine {
            adapter,
            policy,
            store: Mutex::new(store),
            limits: PreviewLimits::default(),
        }
    }

    pub fn with_limits(mut self, limits: PreviewLimits) -> Engine {
        self.limits = limits;
        self
    }

    /// Records one statement from `agent` against `database` and takes it
    /// as far as policy allows: `approved`, `held` or `rejected`.
    ///
    /// `agent` must come from the transport's authentication, never from
    /// the agent's own arguments.
    pub async fn propose(
        &self,
        agent: &str,
        database: &str,
        sql: &str,
    ) -> Result<ChangeStatus, EngineError> {
        let id = self.store().propose(agent, database, sql)?;

        if !self.policy.agents.contains_key(agent) {
            // Not even previewed: an unknown agent gets no reads run for it.
            return self.block(id, agent, "agent is not listed in rowbit.yml");
        }
        let classified = match classify(sql) {
            Ok(c) => c,
            Err(refusal) => return self.block(id, agent, &refusal.to_string()),
        };

        let (review, facts) = if classified.kind == Kind::Ddl {
            let facts = ddl_facts(&classified, database);
            let review = Review::Ddl {
                sql: classified.canonical.clone(),
            };
            (review, facts)
        } else {
            match self
                .adapter
                .preview(database, &classified, self.limits)
                .await
            {
                Ok(preview) => {
                    let facts = facts(&preview);
                    (Review::Rows(preview), facts)
                }
                Err(e) => return self.block(id, agent, &format!("preview failed: {e}")),
            }
        };

        let Verdict {
            decision,
            rule,
            reason,
        } = evaluate(&self.policy, agent, &facts);
        let actor = Actor::Policy(rule.clone().unwrap_or_else(|| "default".to_owned()));
        let reason = reason.unwrap_or_else(|| match &rule {
            Some(rule) => format!("rule {rule}"),
            None => "no rule matched".to_owned(),
        });
        let event = match decision {
            Decision::Allow => Event::PolicyAllowed,
            Decision::Hold => Event::PolicyHeld { reason },
            Decision::Block => Event::PolicyBlocked { reason },
        };

        {
            // Kept for blocked changes too, so the log shows what was refused.
            let mut store = self.store();
            store.set_review(id, &review)?;
            store.transition(id, &event, &actor, None)?;
        }
        self.status(agent, id)
    }

    /// The status of one of `agent`'s own changes. Other agents' changes
    /// are reported as not found.
    pub fn status(&self, agent: &str, id: ChangeId) -> Result<ChangeStatus, EngineError> {
        let store = self.store();
        let change = match store.get(id) {
            Ok(change) if change.agent == agent => change,
            Ok(_) | Err(StoreError::NotFound(_)) => return Err(EngineError::NotFound(id)),
            Err(e) => return Err(e.into()),
        };
        let reason = store
            .events(id)?
            .last()
            .filter(|e| e.event != "Proposed")
            .and_then(|e| e.detail.clone());
        let (table, rows) = match &change.review {
            Some(Review::Rows(p)) => (Some(p.table.qualified_name()), Some(p.rows.len())),
            Some(Review::Ddl { .. }) | None => (None, None),
        };
        Ok(ChangeStatus {
            id,
            state: change.state,
            table,
            rows,
            reason,
        })
    }

    fn block(&self, id: ChangeId, agent: &str, reason: &str) -> Result<ChangeStatus, EngineError> {
        self.store().transition(
            id,
            &Event::PolicyBlocked {
                reason: reason.to_owned(),
            },
            &Actor::Policy("floor".to_owned()),
            None,
        )?;
        self.status(agent, id)
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        // A panic while holding the lock leaves nothing half-written: every
        // store call is one SQLite transaction.
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn ddl_facts(c: &Classified, database: &str) -> ChangeFacts {
    let db = c.table.database.as_deref().unwrap_or(database);
    ChangeFacts {
        statement: StatementKind::Ddl,
        table: format!("{db}.{}", c.table.table),
        columns: Vec::new(),
        rows: 0,
        sums: BTreeMap::new(),
        has_primary_key: true,
    }
}

/// What the policy matches on, from a preview.
fn facts(p: &Preview) -> ChangeFacts {
    let statement = match p.kind {
        WriteKind::Insert => StatementKind::Insert,
        WriteKind::Update => StatementKind::Update,
        WriteKind::Delete => StatementKind::Delete,
    };
    ChangeFacts {
        statement,
        table: p.table.qualified_name(),
        columns: p
            .written
            .iter()
            .map(|&i| p.table.columns[i].name.clone())
            .collect(),
        rows: p.rows.len() as u64,
        sums: sums(p),
        has_primary_key: !p.table.primary_key.is_empty(),
    }
}

/// Sums every column whose values are all numbers (or `NULL`), over the
/// after images, or the before images for a delete. A column with any other
/// value gets no sum, so a rule that needs one blocks.
fn sums(p: &Preview) -> BTreeMap<String, f64> {
    let mut sums = BTreeMap::new();
    for (i, column) in p.table.columns.iter().enumerate() {
        let mut total = 0.0;
        let numeric = p.rows.iter().all(|row| {
            let image = match p.kind {
                WriteKind::Delete => &row.before,
                WriteKind::Insert | WriteKind::Update => &row.after,
            };
            match image.as_ref().map(|cells| &cells[i]) {
                Some(None) => true,
                Some(Some(bytes)) => std::str::from_utf8(bytes)
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
                    .filter(|n| n.is_finite())
                    .map(|n| total += n)
                    .is_some(),
                None => false,
            }
        });
        if numeric {
            sums.insert(column.name.clone(), total);
        }
    }
    sums
}

#[cfg(test)]
mod tests {
    use rowbit_core::preview::{Column, RowChange, Table};

    use super::*;

    fn cell(s: &str) -> Option<Vec<u8>> {
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

    #[test]
    fn sums_skip_columns_that_are_not_numbers() {
        let p = Preview {
            kind: WriteKind::Update,
            table: Table {
                database: "billing".into(),
                name: "refunds".into(),
                columns: vec![column("id"), column("status"), column("amount")],
                primary_key: vec![0],
            },
            written: vec![1],
            rows: vec![
                RowChange {
                    before: None,
                    after: Some(vec![cell("1"), cell("refunded"), cell("12.50")]),
                },
                RowChange {
                    before: None,
                    after: Some(vec![cell("2"), cell("refunded"), None]),
                },
            ],
            as_of: "0".into(),
        };
        let f = facts(&p);
        assert_eq!(f.statement, StatementKind::Update);
        assert_eq!(f.columns, ["status"]);
        assert_eq!(f.rows, 2);
        assert_eq!(f.sums.get("amount"), Some(&12.5));
        assert_eq!(f.sums.get("id"), Some(&3.0));
        assert_eq!(f.sums.get("status"), None);
    }
}
