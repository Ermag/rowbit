//! The change state machine. See `docs/spec/change-lifecycle.md`.
//!
//! [`transition`] is the only way a change moves between states. It fails
//! closed: any pair of state and event not listed in the spec is an error,
//! and so is any event sent by an actor who may not send it.

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    /// Recorded, not yet previewed and checked against policy.
    Proposed,
    /// Waiting for a person.
    Held,
    /// Cleared to apply, by a policy rule or a person.
    Approved,
    /// Written to the database.
    Applied,
    /// Will never be applied. Terminal.
    Rejected,
    /// Approved, but applying it was refused or failed. Nothing was written. Terminal.
    Failed,
    /// Applied, then reversed from its before images. Terminal.
    Undone,
}

impl State {
    pub fn is_terminal(self) -> bool {
        matches!(self, State::Rejected | State::Failed | State::Undone)
    }
}

/// Who is asking for a transition. Identities come from the transport's
/// authentication, never from tool arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    Agent(String),
    Human(String),
    /// The policy engine, naming the rule that decided (or the default).
    Policy(String),
    /// Rowbit itself: the executor, timers.
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Policy evaluated the preview and allowed the change.
    PolicyAllowed,
    /// Policy evaluated the preview and wants a person to decide.
    PolicyHeld,
    /// Policy blocked the change, or the preview itself failed.
    PolicyBlocked {
        reason: String,
    },
    Approve,
    Reject {
        reason: String,
    },
    /// The proposing agent takes its change back.
    Withdraw,
    /// A held change waited longer than allowed. Never approves.
    Expire,
    /// The executor wrote exactly the previewed rows.
    ApplySucceeded,
    /// The executor refused (rows drifted since the preview) or the write failed.
    ApplyFailed {
        reason: String,
    },
    /// The executor restored the before images.
    UndoSucceeded,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Proposed => "proposed",
            State::Held => "held",
            State::Approved => "approved",
            State::Applied => "applied",
            State::Rejected => "rejected",
            State::Failed => "failed",
            State::Undone => "undone",
        }
    }

    pub fn parse(s: &str) -> Option<State> {
        [
            State::Proposed,
            State::Held,
            State::Approved,
            State::Applied,
            State::Rejected,
            State::Failed,
            State::Undone,
        ]
        .into_iter()
        .find(|state| state.as_str() == s)
    }
}

impl std::fmt::Display for Actor {
    /// `agent:<id>`, `human:<id>`, `policy:<rule>` or `system`, as the spec writes them.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Actor::Agent(id) => write!(f, "agent:{id}"),
            Actor::Human(id) => write!(f, "human:{id}"),
            Actor::Policy(rule) => write!(f, "policy:{rule}"),
            Actor::System => f.write_str("system"),
        }
    }
}

impl Event {
    /// The event's name, as the audit log records it.
    pub fn name(&self) -> &'static str {
        match self {
            Event::PolicyAllowed => "PolicyAllowed",
            Event::PolicyHeld => "PolicyHeld",
            Event::PolicyBlocked { .. } => "PolicyBlocked",
            Event::Approve => "Approve",
            Event::Reject { .. } => "Reject",
            Event::Withdraw => "Withdraw",
            Event::Expire => "Expire",
            Event::ApplySucceeded => "ApplySucceeded",
            Event::ApplyFailed { .. } => "ApplyFailed",
            Event::UndoSucceeded => "UndoSucceeded",
        }
    }

    /// The reason carried by blocking, rejecting and failing events.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Event::PolicyBlocked { reason }
            | Event::Reject { reason }
            | Event::ApplyFailed { reason } => Some(reason),
            _ => None,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TransitionError {
    #[error("{event:?} is not allowed from {from:?}")]
    InvalidTransition { from: State, event: Event },
    #[error("{actor:?} may not send {event:?}")]
    Forbidden { actor: Actor, event: Event },
}

/// Moves a change from `from` by `event`, sent by `actor`.
///
/// `proposer` is the agent that proposed the change; only it may withdraw.
pub fn transition(
    from: State,
    event: &Event,
    actor: &Actor,
    proposer: &str,
) -> Result<State, TransitionError> {
    use Event as E;
    use State as S;

    let to = match (from, event) {
        (S::Proposed, E::PolicyAllowed) => S::Approved,
        (S::Proposed, E::PolicyHeld) => S::Held,
        (S::Proposed, E::PolicyBlocked { .. }) => S::Rejected,
        (S::Held, E::Approve) => S::Approved,
        (S::Held | S::Approved, E::Reject { .. }) => S::Rejected,
        (S::Proposed | S::Held, E::Withdraw) => S::Rejected,
        (S::Held, E::Expire) => S::Rejected,
        (S::Approved, E::ApplySucceeded) => S::Applied,
        (S::Approved, E::ApplyFailed { .. }) => S::Failed,
        (S::Applied, E::UndoSucceeded) => S::Undone,
        _ => {
            return Err(TransitionError::InvalidTransition {
                from,
                event: event.clone(),
            });
        }
    };

    let allowed = match event {
        E::PolicyAllowed | E::PolicyHeld | E::PolicyBlocked { .. } => {
            matches!(actor, Actor::Policy(_))
        }
        E::Approve | E::Reject { .. } => matches!(actor, Actor::Human(_)),
        E::Withdraw => matches!(actor, Actor::Agent(id) if id == proposer),
        E::Expire | E::ApplySucceeded | E::ApplyFailed { .. } | E::UndoSucceeded => {
            matches!(actor, Actor::System)
        }
    };
    if !allowed {
        return Err(TransitionError::Forbidden {
            actor: actor.clone(),
            event: event.clone(),
        });
    }

    Ok(to)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_STATES: [State; 7] = [
        State::Proposed,
        State::Held,
        State::Approved,
        State::Applied,
        State::Rejected,
        State::Failed,
        State::Undone,
    ];

    fn agent() -> Actor {
        Actor::Agent("refund-bot".into())
    }
    fn human() -> Actor {
        Actor::Human("philip".into())
    }
    fn policy() -> Actor {
        Actor::Policy("small-refunds".into())
    }

    fn step(from: State, event: Event, actor: Actor) -> Result<State, TransitionError> {
        transition(from, &event, &actor, "refund-bot")
    }

    #[test]
    fn auto_approved_change_applies_and_undoes() {
        let s = step(State::Proposed, Event::PolicyAllowed, policy()).unwrap();
        let s = step(s, Event::ApplySucceeded, Actor::System).unwrap();
        assert_eq!(s, State::Applied);
        let s = step(s, Event::UndoSucceeded, Actor::System).unwrap();
        assert_eq!(s, State::Undone);
    }

    #[test]
    fn held_change_needs_a_human() {
        let s = step(State::Proposed, Event::PolicyHeld, policy()).unwrap();
        assert_eq!(s, State::Held);
        assert_eq!(step(s, Event::Approve, human()), Ok(State::Approved));
    }

    #[test]
    fn agent_can_never_approve() {
        for from in ALL_STATES {
            assert!(step(from, Event::Approve, agent()).is_err(), "{from:?}");
        }
        assert!(matches!(
            step(State::Held, Event::Approve, agent()),
            Err(TransitionError::Forbidden { .. })
        ));
    }

    #[test]
    fn policy_and_system_cannot_approve_a_held_change() {
        assert!(step(State::Held, Event::Approve, policy()).is_err());
        assert!(step(State::Held, Event::Approve, Actor::System).is_err());
    }

    #[test]
    fn agents_cannot_fake_policy_or_executor_events() {
        assert!(step(State::Proposed, Event::PolicyAllowed, agent()).is_err());
        assert!(step(State::Approved, Event::ApplySucceeded, agent()).is_err());
        assert!(step(State::Applied, Event::UndoSucceeded, agent()).is_err());
    }

    #[test]
    fn only_the_proposer_can_withdraw() {
        assert_eq!(
            step(State::Held, Event::Withdraw, agent()),
            Ok(State::Rejected)
        );
        let other = Actor::Agent("other-bot".into());
        assert!(step(State::Held, Event::Withdraw, other).is_err());
        assert!(step(State::Approved, Event::Withdraw, agent()).is_err());
    }

    #[test]
    fn expiry_rejects_and_never_approves() {
        assert_eq!(
            step(State::Held, Event::Expire, Actor::System),
            Ok(State::Rejected)
        );
    }

    #[test]
    fn failed_apply_is_terminal() {
        let s = step(
            State::Approved,
            Event::ApplyFailed {
                reason: "row 42 changed since preview".into(),
            },
            Actor::System,
        )
        .unwrap();
        assert_eq!(s, State::Failed);
        assert!(step(s, Event::ApplySucceeded, Actor::System).is_err());
    }

    #[test]
    fn terminal_states_accept_nothing() {
        let events = [
            Event::PolicyAllowed,
            Event::PolicyHeld,
            Event::PolicyBlocked { reason: "x".into() },
            Event::Approve,
            Event::Reject { reason: "x".into() },
            Event::Withdraw,
            Event::Expire,
            Event::ApplySucceeded,
            Event::ApplyFailed { reason: "x".into() },
            Event::UndoSucceeded,
        ];
        let actors = [agent(), human(), policy(), Actor::System];
        for from in ALL_STATES.into_iter().filter(|s| s.is_terminal()) {
            for e in &events {
                for a in &actors {
                    assert!(transition(from, e, a, "refund-bot").is_err());
                }
            }
        }
    }

    #[test]
    fn states_round_trip_through_their_names() {
        for s in ALL_STATES {
            assert_eq!(State::parse(s.as_str()), Some(s));
        }
        assert_eq!(State::parse("Approved"), None);
    }

    #[test]
    fn actors_print_as_the_spec_writes_them() {
        assert_eq!(agent().to_string(), "agent:refund-bot");
        assert_eq!(policy().to_string(), "policy:small-refunds");
        assert_eq!(Actor::System.to_string(), "system");
    }

    #[test]
    fn approval_cannot_skip_the_preview() {
        assert!(step(State::Proposed, Event::Approve, human()).is_err());
    }
}
