//! Runs a proposed change through the lifecycle in
//! `docs/spec/change-lifecycle.md`: record it, classify it, preview it,
//! evaluate `rowbit.yml`, and record the decision. Every surface (MCP, CLI,
//! web) calls this; none moves a change any other way.
//!
//! Anything that goes wrong before a decision blocks the change. What an
//! agent gets back is a [`ChangeStatus`], which never carries row values.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Mutex;

use rowbit_core::change::{Actor, Event, State};
use rowbit_core::preview::{Applied, Preview, WriteKind};
use rowbit_mysql::agent::{AgentDb, ExplainError};
use rowbit_mysql::grants::{self, Diff};
use rowbit_mysql::schema::{self, TableInfo};
use rowbit_mysql::statement::{
    Classified, Kind, TableName, classify, classify_read, is_system_schema, references,
};
use rowbit_mysql::{
    ApplyError, MySqlAdapter, PreviewError, PreviewLimits, QueryLimits, QueryResult, server_message,
};
use rowbit_policy::{
    ChangeFacts, Decision, Operation, PolicyFile, Scopes, StatementKind, Verdict, agent_user,
    evaluate,
};
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
    /// Scoped agents whose database user passed [`Engine::check_scopes`].
    /// An agent missing here is refused.
    scopes_checked: Mutex<BTreeSet<String>>,
}

/// A table an agent may use, for the `describe_schema` tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaTable {
    pub table: TableInfo,
    /// What the agent may do with it.
    pub operations: Vec<Operation>,
}

/// What an agent is told when its database user didn't pass the startup
/// check. People get the details on the terminal.
const SCOPE_NOT_READY: &str =
    "Rowbit can't serve this agent until an operator runs `rowbit grants apply`";

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
            scopes_checked: Mutex::new(BTreeSet::new()),
        }
    }

    /// Compares every scoped agent's database user with what `rowbit.yml`
    /// compiles to. An agent whose user holds more, or can't be reached, is
    /// refused until this runs again; one that holds less is served, and the
    /// database denies what is missing. Returns what people should know.
    /// Run it at startup: until it has run, scoped agents are refused.
    pub async fn check_scopes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        let mut ready = BTreeSet::new();
        for (agent, policy) in &self.policy.agents {
            let Some(scopes) = &policy.scopes else {
                continue;
            };
            match self.check_agent_user(agent, scopes).await {
                Ok(missing) => {
                    ready.insert(agent.clone());
                    if missing > 0 {
                        notes.push(format!(
                            "agent {agent}: its database user lacks {missing} grant(s) rowbit.yml gives it; \
                             the database will refuse those until `rowbit grants apply` runs"
                        ));
                    }
                }
                Err(why) => notes.push(format!("agent {agent} is refused: {why}")),
            }
        }
        *self
            .scopes_checked
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = ready;
        notes
    }

    /// How many compiled grants the agent's user lacks, or why it can't be
    /// trusted.
    async fn check_agent_user(&self, agent: &str, scopes: &Scopes) -> Result<usize, String> {
        let db = self.agent_db(agent)?.ok_or("it has no scopes")?;
        let (_, current) = db.grants().await.map_err(|e| {
            format!(
                "Rowbit can't connect as {} ({}); run `rowbit grants apply`",
                agent_user(agent),
                rowbit_mysql::db_error_code(&e)
            )
        })?;
        if let Some(line) = current.unmanaged.first() {
            return Err(format!(
                "its database user has a grant Rowbit doesn't manage ({line}); remove it by hand"
            ));
        }
        let tables = self
            .adapter
            .tables(&grants::databases(scopes))
            .await
            .map_err(|e| {
                format!(
                    "reading the schema failed ({})",
                    rowbit_mysql::db_error_code(&e)
                )
            })?;
        let compiled = grants::compile(scopes, &schema::catalog(&tables));
        let diff = Diff::between(&compiled.grants, &current.grants);
        if let Some(extra) = diff.extra.iter().next() {
            return Err(format!(
                "its database user holds more than rowbit.yml gives it ({extra}, and {} more); \
                 run `rowbit grants apply`",
                diff.extra.len() - 1
            ));
        }
        Ok(diff.missing.len())
    }

    fn scopes(&self, agent: &str) -> Option<&Scopes> {
        self.policy.agents.get(agent)?.scopes.as_ref()
    }

    /// The agent's own database user, if it is scoped.
    fn agent_db(&self, agent: &str) -> Result<Option<AgentDb>, String> {
        if self.scopes(agent).is_none() {
            return Ok(None);
        }
        let password = self
            .store()
            .setting(&format!("agent.{agent}.password"))
            .map_err(|e| e.to_string())?
            .ok_or("no database password is stored for it; run `rowbit grants apply`")?;
        Ok(Some(self.adapter.agent(&agent_user(agent), &password)))
    }

    /// For a scoped agent, its database user, once it passed the startup
    /// check. `Err` holds the reason the agent is told.
    fn ready_agent_db(&self, agent: &str) -> Result<Option<AgentDb>, String> {
        if self.scopes(agent).is_none() {
            return Ok(None);
        }
        let checked = self
            .scopes_checked
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(agent);
        if !checked {
            return Err(SCOPE_NOT_READY.to_owned());
        }
        self.agent_db(agent).map_err(|_| SCOPE_NOT_READY.to_owned())
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
        if self.scopes(agent).is_some() {
            if let Err(reason) = self.check_write_scope(agent, database, &classified).await {
                return self.block_by("scope", id, agent, &reason, None);
            }
        }

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
    /// An agent without scopes reads with Rowbit's database privileges; a
    /// scoped one reads on its own database user, after the scope check.
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
        let agent_db = self.ready_agent_db(agent).map_err(EngineError::Query)?;
        let result = match &agent_db {
            Some(db) => {
                let scopes = self.scopes(agent).expect("an agent with a user is scoped");
                parser_scope_check(agent, scopes, database, None, &sql)
                    .map_err(EngineError::Query)?;
                db.query(database, &sql, QueryLimits::default()).await
            }
            None => {
                self.adapter
                    .query(database, &sql, QueryLimits::default())
                    .await
            }
        };
        result.map_err(|e| {
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

    /// Changes in `state`, or in any state, oldest first.
    pub fn list(&self, state: Option<State>) -> Result<Vec<ChangeId>, EngineError> {
        let store = self.store();
        let states = match state {
            Some(state) => vec![state],
            None => State::ALL.to_vec(),
        };
        let mut ids = Vec::new();
        for state in states {
            ids.extend(store.list(state)?);
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Checks the whole audit log; see [`Store::verify`].
    pub fn verify(&self) -> Result<Result<(), rowbit_store::Tampered>, EngineError> {
        Ok(self.store().verify()?)
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
        // The agent's user, if it is scoped: a missing password fails the
        // apply rather than falling back to Rowbit's own privileges.
        let agent_db = self
            .agent_db(&change.agent)
            .map_err(|reason| ApplyError::Conflict {
                reason: format!("outside the agent's scope: {reason}"),
                row: None,
            });
        self.store().note(id, &Actor::System, "ApplyStarted", "")?;
        let result = match (&change.review, agent_db) {
            (_, Err(e)) => Err(e),
            (Some(Review::Rows(preview)), Ok(agent_db)) => match classify(&change.sql) {
                Ok(statement) => {
                    let scope = match &agent_db {
                        Some(db) => db
                            .explain(&change.database, &statement.canonical)
                            .await
                            .map_err(|e| ApplyError::Conflict {
                                reason: e.to_string(),
                                row: None,
                            }),
                        None => Ok(()),
                    };
                    match scope {
                        Ok(()) => {
                            self.adapter
                                .apply(&change.database, &statement, preview)
                                .await
                        }
                        Err(e) => Err(e),
                    }
                }
                Err(refusal) => Err(ApplyError::Conflict {
                    reason: refusal.to_string(),
                    row: None,
                }),
            },
            (Some(Review::Ddl { sql }), Ok(agent_db)) => match &agent_db {
                Some(db) => db.run_ddl(&change.database, sql).await,
                None => self.adapter.run_ddl(&change.database, sql).await,
            }
            .map(|()| Applied { keys: Vec::new() }),
            (None, Ok(_)) => Err(ApplyError::Conflict {
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
        self.block_by("floor", id, agent, reason, detail)
    }

    /// Blocks before policy could decide, as `policy:<by>`.
    fn block_by(
        &self,
        by: &str,
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
            &Actor::Policy(by.to_owned()),
            None,
        )?;
        drop(store);
        self.status(agent, id)
    }

    /// The scope check for a write: the parser check, then for a row write
    /// an `EXPLAIN` on the agent's own user. `Err` is the reason the agent
    /// is told.
    async fn check_write_scope(
        &self,
        agent: &str,
        database: &str,
        c: &Classified,
    ) -> Result<(), String> {
        let db = self.ready_agent_db(agent)?.ok_or(SCOPE_NOT_READY)?;
        let scopes = self.scopes(agent).ok_or(SCOPE_NOT_READY)?;
        let target = qualified(&c.table, database);
        parser_scope_check(
            agent,
            scopes,
            database,
            Some((c.kind, &target)),
            &c.canonical,
        )?;
        if c.kind == Kind::Ddl {
            // EXPLAIN doesn't take DDL; it runs on the agent's user once approved.
            return Ok(());
        }
        db.explain(database, &c.canonical)
            .await
            .map_err(|e: ExplainError| e.to_string())
    }

    /// The tables `agent` may use: for a scoped agent, those in its scopes,
    /// without denied tables and columns; otherwise those in `database`.
    pub async fn describe(
        &self,
        agent: &str,
        database: &str,
    ) -> Result<Vec<SchemaTable>, EngineError> {
        let Some(policy) = self.policy.agents.get(agent) else {
            return Err(EngineError::Query(
                "agent is not listed in rowbit.yml".to_owned(),
            ));
        };
        let unreadable = |e: rowbit_mysql::Error| {
            EngineError::Query(format!(
                "reading the schema failed ({})",
                rowbit_mysql::db_error_code(&e)
            ))
        };
        let Some(scopes) = &policy.scopes else {
            if is_system_schema(database) {
                return Err(EngineError::Query(format!(
                    "{database} is a system schema, which agents may not read"
                )));
            }
            let tables = self
                .adapter
                .tables(&BTreeSet::from([database.to_owned()]))
                .await
                .map_err(unreadable)?;
            return Ok(tables
                .into_iter()
                .map(|table| SchemaTable {
                    table,
                    operations: Operation::ALL.to_vec(),
                })
                .collect());
        };
        self.ready_agent_db(agent).map_err(EngineError::Query)?;
        let tables = self
            .adapter
            .tables(&grants::databases(scopes))
            .await
            .map_err(unreadable)?;
        Ok(tables
            .into_iter()
            .filter_map(|mut table| {
                let name = format!("{}.{}", table.database, table.name);
                let operations: Vec<Operation> = Operation::ALL
                    .into_iter()
                    .filter(|op| scopes.allows(*op, &name))
                    .collect();
                if operations.is_empty() {
                    return None;
                }
                table
                    .columns
                    .retain(|c| !scopes.column_denied(&name, &c.name));
                Some(SchemaTable { table, operations })
            })
            .collect())
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        // A panic while holding the lock leaves nothing half-written: every
        // store call is one SQLite transaction.
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn qualified(t: &TableName, database: &str) -> String {
    format!("{}.{}", t.database.as_deref().unwrap_or(database), t.table)
}

fn operation(kind: Kind) -> Operation {
    match kind {
        Kind::Insert => Operation::Insert,
        Kind::Update => Operation::Update,
        Kind::Delete => Operation::Delete,
        Kind::Ddl => Operation::Ddl,
    }
}

/// Compares what `sql` names with the agent's scopes, so the agent gets a
/// clear reason. The database checks the real thing afterwards. `write` is
/// the statement's kind and target, `None` for a read.
fn parser_scope_check(
    agent: &str,
    scopes: &Scopes,
    database: &str,
    write: Option<(Kind, &str)>,
    sql: &str,
) -> Result<(), String> {
    let refs = references(sql).map_err(|r| r.to_string())?;
    let tables: Vec<String> = refs.tables.iter().map(|t| qualified(t, database)).collect();
    if let Some((kind, target)) = write {
        let op = operation(kind);
        if !scopes.allows(op, target) {
            return Err(format!(
                "{target} is outside {agent}'s {} scope",
                op.as_str()
            ));
        }
        if kind == Kind::Ddl {
            // Every table a schema change names, a new name included.
            if let Some(t) = tables.iter().find(|t| !scopes.allows(Operation::Ddl, t)) {
                return Err(format!("{t} is outside {agent}'s ddl scope"));
            }
            return Ok(());
        }
    }
    let target = write.map(|(_, t)| t);
    for t in &tables {
        // Reading the written table is left to the database, which checks
        // it column by column.
        if Some(t.as_str()) != target && !scopes.allows(Operation::Read, t) {
            return Err(format!("{t} is outside {agent}'s read scope"));
        }
    }
    for t in &tables {
        let denied: Vec<&String> = scopes
            .deny_read
            .columns
            .iter()
            .filter(|c| {
                c.strip_prefix(t.as_str())
                    .is_some_and(|r| r.starts_with('.'))
            })
            .collect();
        if denied.is_empty() {
            continue;
        }
        if refs.wildcard {
            return Err(format!(
                "* includes columns of {t} that {agent} may not read; name the columns"
            ));
        }
        for c in denied {
            let column = &c[t.len() + 1..];
            if refs.columns.contains(&column.to_lowercase()) {
                return Err(format!("{agent} may not read column {column} of {t}"));
            }
        }
    }
    Ok(())
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
