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
use rowbit_policy::{ChangeFacts, Decision, Mode, PolicyFile, StatementKind, Verdict, evaluate};
use rowbit_store::{AuditEvent, Change, ChangeId, NewDelivery, Review, Store, StoreError};
pub use rowbit_store::{Delivery, DeliveryOutcome};
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
            floor,
        } = evaluate(&self.policy, agent, &facts);
        // Shadow mode records a rule's hold or block instead of doing it,
        // but never overrides a floor, and never runs a schema change
        // nobody approved: those can't be undone.
        let shadow = decision != Decision::Allow
            && !floor
            && facts.statement != StatementKind::Ddl
            && self.policy.agents.get(agent).map(|a| a.mode) == Some(Mode::Shadow);
        let actor = Actor::Policy(rule.clone().unwrap_or_else(|| "default".to_owned()));
        let reason = reason.unwrap_or_else(|| match &rule {
            Some(rule) => format!("rule {rule}"),
            None => "no rule matched".to_owned(),
        });
        let event = match decision {
            _ if shadow => Event::PolicyShadowed {
                reason: format!(
                    "would {}: {reason}",
                    if decision == Decision::Hold {
                        "hold"
                    } else {
                        "block"
                    }
                ),
            },
            Decision::Allow => Event::PolicyAllowed,
            Decision::Hold => Event::PolicyHeld { reason },
            Decision::Block => Event::PolicyBlocked { reason },
        };

        {
            // Kept for blocked changes too, so the log shows what was refused.
            let mut store = self.store();
            store.set_review(id, &review)?;
            let notify = match &event {
                Event::PolicyHeld { reason } => {
                    let held = Held {
                        id,
                        agent,
                        database,
                        table: &facts.table,
                        statement: statement_name(facts.statement),
                        rows: (facts.statement != StatementKind::Ddl).then_some(facts.rows),
                        rule: rule.as_deref(),
                        reason,
                    };
                    notifications(&store, &held)
                }
                _ => Vec::new(),
            };
            store.transition_notifying(id, &event, &actor, None, &notify)?;
        }
        if decision == Decision::Allow || shadow {
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

    /// Where held-change notifications go, for every process that shares
    /// the state file. `rowbit serve` sets it from `rowbit-server.yml`.
    pub fn set_notify(&self, setting: &NotifySetting) -> Result<(), EngineError> {
        let json = serde_json::to_string(setting).expect("a setting always serializes");
        Ok(self.store().set_setting(NOTIFY_SETTING, &json)?)
    }

    /// Notifications due now, leased to the caller; see
    /// [`Store::claim_deliveries`].
    pub fn claim_deliveries(
        &self,
        limit: usize,
        lease_ms: i64,
    ) -> Result<Vec<Delivery>, EngineError> {
        Ok(self.store().claim_deliveries(limit, lease_ms)?)
    }

    pub fn record_delivery(&self, id: i64, outcome: DeliveryOutcome) -> Result<(), EngineError> {
        Ok(self.store().record_delivery(id, outcome)?)
    }

    /// The newest notifications, newest first.
    pub fn deliveries(&self, limit: usize) -> Result<Vec<Delivery>, EngineError> {
        Ok(self.store().deliveries(limit)?)
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

/// What a policy would decide now about a change already recorded, from
/// its stored review. For `rowbit policy test`. `None` when nothing was
/// reviewed (the change was refused before its preview).
pub fn replay(policy: &PolicyFile, change: &Change) -> Option<Verdict> {
    let facts = match change.review.as_ref()? {
        Review::Rows(p) => facts(p),
        Review::Ddl { .. } => ddl_facts(&classify(&change.sql).ok()?, &change.database),
    };
    Some(evaluate(policy, &change.agent, &facts))
}

/// What policy decided about a recorded change, from its log: `allow`,
/// `hold` or `block`, or `None` if it never reached policy. A shadowed
/// change counts as what it would have been.
pub fn recorded_decision(history: &[AuditEvent]) -> Option<Decision> {
    history.iter().find_map(|e| match e.event.as_str() {
        "PolicyAllowed" => Some(Decision::Allow),
        "PolicyHeld" => Some(Decision::Hold),
        "PolicyBlocked" if e.actor != "policy:floor" && e.actor != "policy:scope" => {
            Some(Decision::Block)
        }
        "PolicyShadowed" => Some(
            if e.detail
                .as_deref()
                .is_some_and(|d| d.starts_with("would block"))
            {
                Decision::Block
            } else {
                Decision::Hold
            },
        ),
        _ => None,
    })
}

/// Where `rowbit serve` said to send held-change notifications, kept in the
/// state file so every process that proposes changes queues them.
pub const NOTIFY_SETTING: &str = "notify.targets";

/// What `rowbit serve` keeps under [`NOTIFY_SETTING`]. No secrets: those
/// stay in its memory.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotifySetting {
    /// The UI's address, for links to a change; `None` without `public_url`.
    pub review_url: Option<String>,
    pub targets: Vec<NotifyTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NotifyTarget {
    pub name: String,
    /// Only changes from these agents; every agent if `None`.
    pub agents: Option<Vec<String>>,
}

struct Held<'a> {
    id: ChangeId,
    agent: &'a str,
    database: &'a str,
    table: &'a str,
    statement: &'static str,
    rows: Option<u64>,
    rule: Option<&'a str>,
    reason: &'a str,
}

fn statement_name(k: StatementKind) -> &'static str {
    match k {
        StatementKind::Insert => "insert",
        StatementKind::Update => "update",
        StatementKind::Delete => "delete",
        StatementKind::Ddl => "ddl",
    }
}

/// The notifications to queue for a held change: one per matching target.
/// What they carry is in "Notifications" in `docs/spec/server.md`: never
/// row values or the statement's text.
fn notifications(store: &Store, h: &Held<'_>) -> Vec<NewDelivery> {
    let Some(setting) = store
        .setting(NOTIFY_SETTING)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<NotifySetting>(&s).ok())
    else {
        return Vec::new();
    };
    setting
        .targets
        .iter()
        .filter(|t| {
            t.agents
                .as_ref()
                .is_none_or(|a| a.iter().any(|a| a == h.agent))
        })
        .map(|t| {
            let delivery = uuid();
            let payload = serde_json::json!({
                "event": "change.held",
                "delivery": delivery,
                "time": rfc3339_now(),
                "change": {
                    "id": h.id,
                    "agent": h.agent,
                    "database": h.database,
                    "table": h.table,
                    "statement": h.statement,
                    "rows": h.rows,
                    "rule": h.rule,
                    "reason": h.reason,
                    "url": setting.review_url.as_ref().map(|u| format!("{u}/#/changes/{}", h.id)),
                },
            });
            NewDelivery {
                target: t.name.clone(),
                delivery,
                payload: payload.to_string(),
            }
        })
        .collect()
}

/// A random version 4 UUID.
fn uuid() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("the system has a random source");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// Now, in UTC, as `2026-11-20T14:03:11Z`.
fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    rfc3339(secs)
}

fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
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
    fn times_and_ids_are_well_formed() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_795_701_791), "2026-11-26T14:03:11Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        let u = uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert_ne!(u, uuid());
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
