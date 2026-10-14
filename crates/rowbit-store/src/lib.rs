//! Change state and the audit log, in one SQLite file.
//!
//! Every state change goes through [`Store::transition`], which runs
//! [`rowbit_core::change::transition`] and appends the audit event in the
//! same SQLite transaction, so state and log can never disagree. Events are
//! hash-chained (see "Audit log" in `docs/spec/change-lifecycle.md`), and
//! [`Store::verify`] detects any edit to history, to what was reviewed, or
//! to a change's own record.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rowbit_core::change::{self, Actor, Event, State, TransitionError};
use rowbit_core::preview::Preview;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub type ChangeId = i64;

/// What a person reviews and a policy decides on: the rows a write would
/// change, or for DDL the statement Rowbit will run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Review {
    Rows(Preview),
    Ddl { sql: String },
}

impl Review {
    /// The stored form, and the bytes the review hash covers.
    pub fn to_canonical_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a review always serializes")
    }
}

/// The detail of a `Proposed` event, so the log itself records what was proposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    database: String,
    sql: String,
}

/// Hash that the first event chains from.
const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("change {0} not found")]
    NotFound(ChangeId),
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error("change {0} already has a review")]
    ReviewAlreadySet(ChangeId),
    #[error("change {0} has nothing to review yet")]
    NoReview(ChangeId),
    #[error("what was reviewed is not what is stored")]
    ReviewMismatch,
    #[error("change {0} can only get a review while proposed")]
    NotProposed(ChangeId),
    #[error("stored data is corrupt: {0}")]
    Corrupt(String),
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// What [`Store::verify`] found wrong.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Tampered {
    #[error("event {seq} does not chain from the event before it")]
    BrokenLink { seq: i64 },
    #[error("event {seq} was modified")]
    EventModified { seq: i64 },
    #[error("event {seq} belongs to no change")]
    OrphanEvent { seq: i64 },
    #[error("the review of change {0} does not match its hash or its events")]
    ReviewModified(ChangeId),
    #[error("the state of change {0} does not match its last event")]
    StateModified(ChangeId),
    #[error("the agent, database or statement of change {0} does not match its proposal")]
    ProposalModified(ChangeId),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub id: ChangeId,
    pub agent: String,
    pub database: String,
    /// The statement exactly as the agent sent it.
    pub sql: String,
    pub state: State,
    pub review: Option<Review>,
    /// Hex SHA-256 of the review's canonical JSON.
    pub review_hash: Option<String>,
    pub created_at_ms: i64,
}

/// One audit log entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditEvent {
    pub seq: i64,
    pub change_id: ChangeId,
    /// `None` for the event that records the proposal.
    pub from: Option<String>,
    pub to: String,
    /// An [`Event::name`], or `Proposed`.
    pub event: String,
    /// The reason for blocking, rejecting or failing. For `Proposed`, JSON
    /// with the database and the statement exactly as the agent sent it.
    pub detail: Option<String>,
    pub actor: String,
    /// The review this event decided on, for `PolicyAllowed`, `PolicyHeld` and `Approve`.
    pub review_hash: Option<String>,
    pub at_ms: i64,
    pub prev_hash: String,
    #[serde(skip)]
    pub hash: String,
}

impl AuditEvent {
    /// SHA-256 over the event's canonical JSON, which includes `prev_hash`.
    fn compute_hash(&self) -> String {
        hex(&Sha256::digest(
            serde_json::to_vec(self).expect("an event always serializes"),
        ))
    }
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Store, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store, StoreError> {
        // The CLI and the server share the file; wait for each other's writes.
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS changes (
                 id INTEGER PRIMARY KEY,
                 agent TEXT NOT NULL,
                 database_name TEXT NOT NULL,
                 sql TEXT NOT NULL,
                 state TEXT NOT NULL,
                 review BLOB,
                 review_hash TEXT,
                 created_at_ms INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS events (
                 seq INTEGER PRIMARY KEY,
                 change_id INTEGER NOT NULL REFERENCES changes (id),
                 from_state TEXT,
                 to_state TEXT NOT NULL,
                 event TEXT NOT NULL,
                 detail TEXT,
                 actor TEXT NOT NULL,
                 review_hash TEXT,
                 at_ms INTEGER NOT NULL,
                 prev_hash TEXT NOT NULL,
                 hash TEXT NOT NULL UNIQUE
             );
             CREATE INDEX IF NOT EXISTS events_by_change ON events (change_id, seq);",
        )?;
        Ok(Store { conn })
    }

    /// Records a new change in `proposed`, proposed by `agent`.
    pub fn propose(
        &mut self,
        agent: &str,
        database: &str,
        sql: &str,
    ) -> Result<ChangeId, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let at = now_ms();
        tx.execute(
            "INSERT INTO changes (agent, database_name, sql, state, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![agent, database, sql, State::Proposed.as_str(), at],
        )?;
        let id = tx.last_insert_rowid();
        let proposal = serde_json::to_string(&Proposal {
            database: database.to_owned(),
            sql: sql.to_owned(),
        })
        .expect("a proposal always serializes");
        append(
            &tx,
            id,
            None,
            State::Proposed,
            "Proposed",
            Some(&proposal),
            &Actor::Agent(agent.to_owned()).to_string(),
            None,
            at,
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Stores what a proposed change will be reviewed on. It is set once and
    /// never replaced. Returns its hash.
    pub fn set_review(&mut self, id: ChangeId, review: &Review) -> Result<String, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, existing): (String, Option<String>) = tx
            .query_row(
                "SELECT state, review_hash FROM changes WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or(StoreError::NotFound(id))?;
        if existing.is_some() {
            return Err(StoreError::ReviewAlreadySet(id));
        }
        if parse_state(&state)? != State::Proposed {
            return Err(StoreError::NotProposed(id));
        }
        let json = review.to_canonical_json();
        let hash = hex(&Sha256::digest(&json));
        tx.execute(
            "UPDATE changes SET review = ?1, review_hash = ?2 WHERE id = ?3",
            params![json, hash, id],
        )?;
        tx.commit()?;
        Ok(hash)
    }

    /// Moves a change by `event`, sent by `actor`, and logs it.
    ///
    /// `reviewed` is the review hash the person saw. `Approve` requires it,
    /// and it must match the stored review. `PolicyAllowed` and
    /// `PolicyHeld` require a stored review.
    pub fn transition(
        &mut self,
        id: ChangeId,
        event: &Event,
        actor: &Actor,
        reviewed: Option<&str>,
    ) -> Result<State, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (agent, state, review_hash): (String, String, Option<String>) = tx
            .query_row(
                "SELECT agent, state, review_hash FROM changes WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or(StoreError::NotFound(id))?;
        let from = parse_state(&state)?;
        let to = change::transition(from, event, actor, &agent)?;

        let decides_on_review = matches!(
            event,
            Event::PolicyAllowed | Event::PolicyHeld { .. } | Event::Approve
        );
        if decides_on_review && review_hash.is_none() {
            return Err(StoreError::NoReview(id));
        }
        if matches!(event, Event::Approve) && reviewed != review_hash.as_deref() {
            return Err(StoreError::ReviewMismatch);
        }

        let at = now_ms();
        tx.execute(
            "UPDATE changes SET state = ?1 WHERE id = ?2",
            params![to.as_str(), id],
        )?;
        append(
            &tx,
            id,
            Some(from),
            to,
            event.name(),
            event.reason(),
            &actor.to_string(),
            if decides_on_review {
                review_hash.as_deref()
            } else {
                None
            },
            at,
        )?;
        tx.commit()?;
        Ok(to)
    }

    pub fn get(&self, id: ChangeId) -> Result<Change, StoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT agent, database_name, sql, state, review, review_hash, created_at_ms
                 FROM changes WHERE id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<Vec<u8>>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, i64>(6)?,
                    ))
                },
            )
            .optional()?
            .ok_or(StoreError::NotFound(id))?;
        let (agent, database, sql, state, review, review_hash, created_at_ms) = row;
        let review = review
            .map(|json| {
                serde_json::from_slice(&json)
                    .map_err(|e| StoreError::Corrupt(format!("review of change {id}: {e}")))
            })
            .transpose()?;
        Ok(Change {
            id,
            agent,
            database,
            sql,
            state: parse_state(&state)?,
            review,
            review_hash,
            created_at_ms,
        })
    }

    /// Changes in `state`, oldest first.
    pub fn list(&self, state: State) -> Result<Vec<ChangeId>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM changes WHERE state = ?1 ORDER BY id")?;
        let ids = stmt
            .query_map([state.as_str()], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// The audit log of one change, oldest first.
    pub fn events(&self, id: ChangeId) -> Result<Vec<AuditEvent>, StoreError> {
        self.query_events("WHERE change_id = ?1", [id])
    }

    /// Checks the whole log: every event's hash and link, that every event
    /// belongs to a change, every stored review against its hash and the
    /// hashes its events name, every change's agent, database and statement
    /// against its `Proposed` event, and its state against its last event.
    /// Returns the first problem found.
    pub fn verify(&self) -> Result<Result<(), Tampered>, StoreError> {
        let events = self.query_events("", [])?;
        let mut prev = GENESIS.to_owned();
        for e in &events {
            if e.prev_hash != prev {
                return Ok(Err(Tampered::BrokenLink { seq: e.seq }));
            }
            if e.compute_hash() != e.hash {
                return Ok(Err(Tampered::EventModified { seq: e.seq }));
            }
            prev = e.hash.clone();
        }

        let mut stmt = self.conn.prepare(
            "SELECT id, agent, database_name, sql, state, review, review_hash
             FROM changes ORDER BY id",
        )?;
        let changes = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, ChangeId>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<Vec<u8>>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(e) = events
            .iter()
            .find(|e| !changes.iter().any(|c| c.0 == e.change_id))
        {
            return Ok(Err(Tampered::OrphanEvent { seq: e.seq }));
        }
        for (id, agent, database, sql, state, review, hash) in changes {
            let mine: Vec<&AuditEvent> = events.iter().filter(|e| e.change_id == id).collect();

            let proposal = mine
                .first()
                .filter(|e| e.event == "Proposed")
                .and_then(|e| {
                    Some((
                        e,
                        serde_json::from_str::<Proposal>(e.detail.as_deref()?).ok()?,
                    ))
                });
            let matches_proposal = proposal.is_some_and(|(e, p)| {
                e.actor == Actor::Agent(agent.clone()).to_string()
                    && p.database == database
                    && p.sql == sql
            });
            if !matches_proposal {
                return Ok(Err(Tampered::ProposalModified(id)));
            }

            let actual = review.map(|r| hex(&Sha256::digest(&r)));
            let decided_on_other = mine
                .iter()
                .any(|e| e.review_hash.is_some() && e.review_hash != hash);
            if actual != hash || decided_on_other {
                return Ok(Err(Tampered::ReviewModified(id)));
            }

            if mine.last().map(|e| e.to.as_str()) != Some(state.as_str()) {
                return Ok(Err(Tampered::StateModified(id)));
            }
        }
        Ok(Ok(()))
    }

    fn query_events<P: rusqlite::Params>(
        &self,
        filter: &str,
        params: P,
    ) -> Result<Vec<AuditEvent>, StoreError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT seq, change_id, from_state, to_state, event, detail, actor, review_hash,
                    at_ms, prev_hash, hash
             FROM events {filter} ORDER BY seq"
        ))?;
        let events = stmt
            .query_map(params, |r| {
                Ok(AuditEvent {
                    seq: r.get(0)?,
                    change_id: r.get(1)?,
                    from: r.get(2)?,
                    to: r.get(3)?,
                    event: r.get(4)?,
                    detail: r.get(5)?,
                    actor: r.get(6)?,
                    review_hash: r.get(7)?,
                    at_ms: r.get(8)?,
                    prev_hash: r.get(9)?,
                    hash: r.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(events)
    }
}

/// Appends one event to the chain. Must run inside the caller's
/// `IMMEDIATE` transaction, so no other writer can append in between.
#[allow(clippy::too_many_arguments)]
fn append(
    tx: &rusqlite::Transaction<'_>,
    change_id: ChangeId,
    from: Option<State>,
    to: State,
    event: &str,
    detail: Option<&str>,
    actor: &str,
    review_hash: Option<&str>,
    at_ms: i64,
) -> Result<(), StoreError> {
    let (last_seq, prev_hash): (i64, String) = tx
        .query_row(
            "SELECT seq, hash FROM events ORDER BY seq DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or((0, GENESIS.to_owned()));
    let mut e = AuditEvent {
        seq: last_seq + 1,
        change_id,
        from: from.map(|s| s.as_str().to_owned()),
        to: to.as_str().to_owned(),
        event: event.to_owned(),
        detail: detail.map(str::to_owned),
        actor: actor.to_owned(),
        review_hash: review_hash.map(str::to_owned),
        at_ms,
        prev_hash,
        hash: String::new(),
    };
    e.hash = e.compute_hash();
    tx.execute(
        "INSERT INTO events (seq, change_id, from_state, to_state, event, detail, actor,
                             review_hash, at_ms, prev_hash, hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            e.seq,
            e.change_id,
            e.from,
            e.to,
            e.event,
            e.detail,
            e.actor,
            e.review_hash,
            e.at_ms,
            e.prev_hash,
            e.hash
        ],
    )?;
    Ok(())
}

fn parse_state(s: &str) -> Result<State, StoreError> {
    State::parse(s).ok_or_else(|| StoreError::Corrupt(format!("unknown state {s:?}")))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests;
