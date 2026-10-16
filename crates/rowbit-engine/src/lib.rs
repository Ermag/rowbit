//! Runs a proposed change through the lifecycle in
//! `docs/spec/change-lifecycle.md`: record it, classify it, preview it,
//! evaluate `rowbit.yml`, and record the decision. Every surface (MCP, CLI,
//! web) calls this; none moves a change any other way.
//!
//! Anything that goes wrong before a decision blocks the change. What an
//! agent gets back is a [`ChangeStatus`], which never carries row values.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;

use rowbit_core::change::{Actor, Event, State};
use rowbit_core::preview::{Applied, Preview, WriteKind};
use rowbit_mysql::statement::{Classified, Kind, classify, classify_read, is_system_schema};
use rowbit_mysql::{
    ApplyError, MySqlAdapter, PreviewError, PreviewLimits, QueryLimits, QueryResult, server_message,
};
use rowbit_policy::{ChangeFacts, Decision, PolicyFile, StatementKind, Verdict, evaluate};
use rowbit_store::{AuditEvent, Change, ChangeId, Review, Store, StoreError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("change {0} not found")]
    NotFound(ChangeId),
    #[error("change {id} is {state}, so it cannot be {action}")]
    WrongState {
        id: ChangeId,
        state: &'static str,
        action: &'static str,
    },
    /// Undo would overwrite a later change, or cannot run. Nothing was
    /// written and the change stays applied.
    #[error("undo refused: {0}")]
    UndoRefused(String),
    #[error("change {0} is already being applied or undone")]
    Busy(ChangeId),
    /// A read was refused or failed. The reason is safe to show the agent,
    /// who could have read the same values anyway.
    #[error("{0}")]
    Query(String),
    /// The database may or may not hold the change. A person has to check;
    /// see [`Engine::interrupted`].
    #[error("change {0} was interrupted; its outcome is unknown and a person must check it")]
    Interrupted(ChangeId),
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
    /// Changes being applied or undone right now.
    busy: Mutex<HashSet<ChangeId>>,
}

struct Claim<'a> {
    engine: &'a Engine,
    id: ChangeId,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let mut busy = self.engine.busy.lock().unwrap_or_else(|p| p.into_inner());
        busy.remove(&self.id);
    }
}

impl Engine {
    pub fn new(adapter: MySqlAdapter, policy: PolicyFile, store: Store) -> Engine {
        Engine {
            adapter,
            policy,
            store: Mutex::new(store),
            limits: PreviewLimits::default(),
            busy: Mutex::new(HashSet::new()),
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
        if is_system_schema(database) {
            return self.block(
                id,
                agent,
                &format!("{database} is a system schema, which agents may not write"),
                None,
            );
        }

        if !self.policy.agents.contains_key(agent) {
            // Not even previewed: an unknown agent gets no reads run for it.
            return self.block(id, agent, "agent is not listed in rowbit.yml", None);
        }
        let classified = match classify(sql) {
            Ok(c) => c,
            Err(refusal) => return self.block(id, agent, &refusal.to_string(), None),
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
                Ok(preview) if preview.rows.is_empty() => {
                    return self.block(id, agent, "the statement matches no rows", None);
                }
                Ok(preview) => {
                    let facts = facts(&preview);
                    (Review::Rows(preview), facts)
                }
                Err(e) => {
                    // People see the server's message in the log; the agent
                    // gets only Rowbit's words.
                    let detail = e.to_string();
                    return self.block(id, agent, &e.agent_reason(), Some(&detail));
                }
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
        if decision == Decision::Allow {
            self.apply(id).await?;
        }
        self.status(agent, id)
    }

    /// Runs a read for `agent`. Only agents listed in `rowbit.yml` may read,
    /// and only with a statement that passes the same checks as a write.
    /// Reads are not logged as changes.
    ///
    /// The agent reads with Rowbit's database privileges; scope them to what
    /// agents may see (per-agent grants come in v0.2).
    pub async fn query(
        &self,
        agent: &str,
        database: &str,
        sql: &str,
    ) -> Result<QueryResult, EngineError> {
        if !self.policy.agents.contains_key(agent) {
            return Err(EngineError::Query(
                "agent is not listed in rowbit.yml".to_owned(),
            ));
        }
        if is_system_schema(database) {
            return Err(EngineError::Query(format!(
                "{database} is a system schema, which agents may not read"
            )));
        }
        let sql = classify_read(sql).map_err(|r| EngineError::Query(r.to_string()))?;
        self.adapter
            .query(database, &sql, QueryLimits::default())
            .await
            .map_err(|e| {
                EngineError::Query(match e {
                    PreviewError::Refused(reason) => reason,
                    // The server's message: about the agent's own read, of
                    // tables it may read anyway.
                    PreviewError::Database(mysql_async_error) => {
                        format!("database error: {}", server_message(&mysql_async_error))
                    }
                })
            })
    }

    /// A person approves a held change, naming the review hash they saw,
    /// and Rowbit applies it.
    pub async fn approve(
        &self,
        person: &str,
        id: ChangeId,
        reviewed: &str,
    ) -> Result<Change, EngineError> {
        self.store().transition(
            id,
            &Event::Approve,
            &Actor::Human(person.to_owned()),
            Some(reviewed),
        )?;
        self.apply(id).await?;
        self.change(id)
    }

    /// A person rejects a held change.
    pub fn reject(&self, person: &str, id: ChangeId, reason: &str) -> Result<Change, EngineError> {
        self.store().transition(
            id,
            &Event::Reject {
                reason: reason.to_owned(),
            },
            &Actor::Human(person.to_owned()),
            None,
        )?;
        self.change(id)
    }

    /// A person undoes an applied change. If any row changed since apply,
    /// nothing is written, the refusal is logged, and the change stays
    /// applied.
    pub async fn undo(&self, person: &str, id: ChangeId) -> Result<Change, EngineError> {
        let _working = self.claim(id)?;
        let change = self.change(id)?;
        if change.state != State::Applied {
            return Err(EngineError::WrongState {
                id,
                state: change.state.as_str(),
                action: "undone",
            });
        }
        let person = Actor::Human(person.to_owned());
        let (preview, applied) = match (&change.review, &change.applied) {
            (Some(Review::Rows(preview)), Some(applied)) => (preview, applied),
            (Some(Review::Ddl { .. }), _) => {
                return self.refuse_undo(id, &person, "schema changes cannot be undone");
            }
            _ => return self.refuse_undo(id, &person, "nothing was recorded to undo"),
        };
        self.store().note(id, &person, "UndoStarted", "")?;
        match self.adapter.undo(&change.database, preview, applied).await {
            Ok(()) => {
                self.store()
                    .transition(id, &Event::UndoSucceeded, &Actor::System, None)?;
                self.change(id)
            }
            Err(ApplyError::Uncertain(detail)) => {
                // Leave UndoStarted as the last word, for a person to check.
                self.store()
                    .note(id, &Actor::System, "ErrorDetail", &detail)?;
                Err(EngineError::Interrupted(id))
            }
            Err(e) => self.refuse_undo(id, &person, &e.to_string()),
        }
    }

    fn refuse_undo(
        &self,
        id: ChangeId,
        person: &Actor,
        reason: &str,
    ) -> Result<Change, EngineError> {
        self.store().note(id, person, "UndoRefused", reason)?;
        Err(EngineError::UndoRefused(reason.to_owned()))
    }

    /// Everything about a change, row values included. For people only:
    /// never return this to an agent.
    pub fn change(&self, id: ChangeId) -> Result<Change, EngineError> {
        match self.store().get(id) {
            Ok(change) => Ok(change),
            Err(StoreError::NotFound(_)) => Err(EngineError::NotFound(id)),
            Err(e) => Err(e.into()),
        }
    }

    /// Changes waiting for a person, oldest first.
    pub fn held(&self) -> Result<Vec<ChangeId>, EngineError> {
        Ok(self.store().list(State::Held)?)
    }

    /// Changes whose apply or undo started but was never recorded as done
    /// or failed: Rowbit stopped, or the outcome was unknown. A person has
    /// to compare the rows with the preview. Report these on startup.
    pub fn interrupted(&self) -> Result<Vec<ChangeId>, EngineError> {
        let store = self.store();
        let mut ids = Vec::new();
        for state in [State::Approved, State::Applied] {
            for id in store.list(state)? {
                let last = store.events(id)?.pop();
                if last.is_some_and(|e| e.event == "ApplyStarted" || e.event == "UndoStarted") {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Writes an approved change, and records `ApplySucceeded` with what
    /// was written, or `ApplyFailed` with why nothing was. An `ApplyStarted`
    /// note goes first, so a crash between the database commit and the
    /// record shows up in [`Engine::interrupted`].
    async fn apply(&self, id: ChangeId) -> Result<(), EngineError> {
        let _working = self.claim(id)?;
        let change = self.change(id)?;
        if change.state != State::Approved {
            return Err(EngineError::WrongState {
                id,
                state: change.state.as_str(),
                action: "applied",
            });
        }
        self.store().note(id, &Actor::System, "ApplyStarted", "")?;
        let result = match &change.review {
            Some(Review::Rows(preview)) => match classify(&change.sql) {
                Ok(statement) => {
                    self.adapter
                        .apply(&change.database, &statement, preview)
                        .await
                }
                Err(refusal) => Err(ApplyError::Conflict {
                    reason: refusal.to_string(),
                    row: None,
                }),
            },
            Some(Review::Ddl { sql }) => self
                .adapter
                .run_ddl(&change.database, sql)
                .await
                .map(|()| Applied { keys: Vec::new() }),
            None => Err(ApplyError::Conflict {
                reason: "nothing was reviewed".to_owned(),
                row: None,
            }),
        };
        let mut store = self.store();
        match result {
            Ok(applied) => {
                store.record_applied(id, &applied)?;
            }
            Err(ApplyError::Uncertain(detail)) => {
                store.note(id, &Actor::System, "ErrorDetail", &detail)?;
                return Err(EngineError::Interrupted(id));
            }
            Err(e) => {
                let reason = e.agent_reason();
                let detail = e.to_string();
                if detail != reason {
                    store.note(id, &Actor::System, "ErrorDetail", &detail)?;
                }
                store.transition(id, &Event::ApplyFailed { reason }, &Actor::System, None)?;
            }
        }
        Ok(())
    }

    /// Marks `id` as being applied or undone, so two of those never run at
    /// once for the same change. Released when the guard drops.
    fn claim(&self, id: ChangeId) -> Result<Claim<'_>, EngineError> {
        let mut busy = self.busy.lock().unwrap_or_else(|p| p.into_inner());
        if !busy.insert(id) {
            return Err(EngineError::Busy(id));
        }
        Ok(Claim { engine: self, id })
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
            .iter()
            .rev()
            .find(|e| e.from.as_deref() != Some(e.to.as_str()) || e.event == "Proposed")
            .filter(|e| {
                matches!(
                    e.event.as_str(),
                    "PolicyHeld" | "PolicyBlocked" | "Reject" | "ApplyFailed"
                )
            })
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

    /// The full audit log of a change, error details included. For people
    /// only: never return this to an agent.
    pub fn history(&self, id: ChangeId) -> Result<Vec<AuditEvent>, EngineError> {
        Ok(self.store().events(id)?)
    }

    /// Blocks before policy could decide. `reason` is shown to the agent and
    /// must be Rowbit's own words; `detail`, if any, is only logged.
    fn block(
        &self,
        id: ChangeId,
        agent: &str,
        reason: &str,
        detail: Option<&str>,
    ) -> Result<ChangeStatus, EngineError> {
        let mut store = self.store();
        if let Some(detail) = detail {
            store.note(id, &Actor::System, "ErrorDetail", detail)?;
        }
        store.transition(
            id,
            &Event::PolicyBlocked {
                reason: reason.to_owned(),
            },
            &Actor::Policy("floor".to_owned()),
            None,
        )?;
        drop(store);
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
        sums_before: BTreeMap::new(),
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
        sums: sums(p, p.kind == WriteKind::Delete),
        sums_before: if p.kind == WriteKind::Update {
            sums(p, true)
        } else {
            BTreeMap::new()
        },
        has_primary_key: !p.table.primary_key.is_empty(),
    }
}

/// Sums every column whose values are all numbers (or `NULL`), over the
/// before or the after images. A column with any other value gets no sum,
/// so a rule that needs one blocks.
fn sums(p: &Preview, before: bool) -> BTreeMap<String, f64> {
    let mut sums = BTreeMap::new();
    for (i, column) in p.table.columns.iter().enumerate() {
        let mut total = 0.0;
        let numeric = p.rows.iter().all(|row| {
            let image = if before { &row.before } else { &row.after };
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

    /// An engine whose database is never reached.
    fn offline() -> Engine {
        Engine::new(
            MySqlAdapter::new("mysql://nobody@127.0.0.1:1/none").unwrap(),
            PolicyFile::from_yaml("version: 0\n").unwrap(),
            Store::open_in_memory().unwrap(),
        )
    }

    // Found in review: a crash between the database commit and the log
    // record left them disagreeing with nothing to say so.
    #[test]
    fn an_apply_that_never_finished_is_reported() {
        let engine = offline();
        let id = {
            let mut store = engine.store();
            let id = store.propose("bot", "db", "DELETE FROM t").unwrap();
            store
                .set_review(id, &Review::Ddl { sql: "x".into() })
                .unwrap();
            store
                .transition(
                    id,
                    &Event::PolicyHeld { reason: "r".into() },
                    &Actor::Policy("p".into()),
                    None,
                )
                .unwrap();
            let hash = store.get(id).unwrap().review_hash.unwrap();
            store
                .transition(
                    id,
                    &Event::Approve,
                    &Actor::Human("ann".into()),
                    Some(&hash),
                )
                .unwrap();
            store.note(id, &Actor::System, "ApplyStarted", "").unwrap();
            id
        };
        assert_eq!(engine.interrupted().unwrap(), [id]);
        assert_eq!(engine.change(id).unwrap().state, State::Approved);
    }

    #[test]
    fn one_apply_or_undo_at_a_time() {
        let engine = offline();
        let first = engine.claim(7).unwrap();
        assert!(matches!(engine.claim(7), Err(EngineError::Busy(7))));
        assert!(engine.claim(8).is_ok());
        drop(first);
        assert!(engine.claim(7).is_ok());
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
