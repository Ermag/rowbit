use rowbit_core::preview::{RowChange, Table, WriteKind};

use super::*;

fn preview(status: &str) -> Preview {
    Preview {
        kind: WriteKind::Update,
        table: Table {
            database: "billing".into(),
            name: "refunds".into(),
            columns: vec!["id".into(), "status".into()],
            primary_key: vec![0],
        },
        written: vec![1],
        rows: vec![RowChange {
            before: Some(vec![Some(b"7".to_vec()), Some(b"approved".to_vec())]),
            after: Some(vec![Some(status.as_bytes().to_vec())]),
        }],
    }
}

fn agent() -> Actor {
    Actor::Agent("refund-bot".into())
}
fn human() -> Actor {
    Actor::Human("philip".into())
}
fn policy() -> Actor {
    Actor::Policy("refunds-need-a-person".into())
}

/// A change proposed and previewed, ready for policy. Returns its preview hash.
fn previewed(store: &mut Store) -> (ChangeId, String) {
    let id = store
        .propose(
            "refund-bot",
            "billing",
            "UPDATE refunds SET status = 'refunded'",
        )
        .unwrap();
    let hash = store.set_preview(id, &preview("refunded")).unwrap();
    (id, hash)
}

#[test]
fn held_change_is_approved_applied_and_undone() {
    let mut store = Store::open_in_memory().unwrap();
    let (id, hash) = previewed(&mut store);

    store
        .transition(id, &Event::PolicyHeld, &policy(), None)
        .unwrap();
    store
        .transition(id, &Event::Approve, &human(), Some(&hash))
        .unwrap();
    store
        .transition(id, &Event::ApplySucceeded, &Actor::System, None)
        .unwrap();
    store
        .transition(id, &Event::UndoSucceeded, &Actor::System, None)
        .unwrap();

    let change = store.get(id).unwrap();
    assert_eq!(change.state, State::Undone);
    assert_eq!(change.preview, Some(preview("refunded")));
    assert_eq!(change.agent, "refund-bot");

    let log = store.events(id).unwrap();
    let names: Vec<&str> = log.iter().map(|e| e.event.as_str()).collect();
    assert_eq!(
        names,
        [
            "Proposed",
            "PolicyHeld",
            "Approve",
            "ApplySucceeded",
            "UndoSucceeded"
        ]
    );
    assert_eq!(
        log[0].detail.as_deref(),
        Some("UPDATE refunds SET status = 'refunded'")
    );
    assert_eq!(log[0].actor, "agent:refund-bot");
    assert_eq!(log[1].preview_hash.as_deref(), Some(hash.as_str()));
    assert_eq!(log[2].preview_hash.as_deref(), Some(hash.as_str()));
    assert_eq!(log[2].actor, "human:philip");
    assert_eq!(log[3].preview_hash, None);
    assert_eq!(store.verify().unwrap(), Ok(()));
}

#[test]
fn agent_can_never_approve_and_nothing_is_logged() {
    let mut store = Store::open_in_memory().unwrap();
    let (id, hash) = previewed(&mut store);
    store
        .transition(id, &Event::PolicyHeld, &policy(), None)
        .unwrap();

    let err = store
        .transition(id, &Event::Approve, &agent(), Some(&hash))
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Transition(TransitionError::Forbidden { .. })
    ));
    assert_eq!(store.get(id).unwrap().state, State::Held);
    assert_eq!(store.events(id).unwrap().len(), 2);
}

#[test]
fn approval_must_name_the_stored_preview() {
    let mut store = Store::open_in_memory().unwrap();
    let (id, _) = previewed(&mut store);
    store
        .transition(id, &Event::PolicyHeld, &policy(), None)
        .unwrap();

    for seen in [None, Some("0".repeat(64))] {
        let err = store
            .transition(id, &Event::Approve, &human(), seen.as_deref())
            .unwrap_err();
        assert!(matches!(err, StoreError::PreviewMismatch), "{err}");
    }
    assert_eq!(store.get(id).unwrap().state, State::Held);
}

#[test]
fn policy_cannot_decide_without_a_preview() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store
        .propose("refund-bot", "billing", "DELETE FROM refunds")
        .unwrap();
    assert!(matches!(
        store.transition(id, &Event::PolicyAllowed, &policy(), None),
        Err(StoreError::NoPreview(_))
    ));
    // A failed preview blocks without one.
    store
        .transition(
            id,
            &Event::PolicyBlocked {
                reason: "table has triggers".into(),
            },
            &policy(),
            None,
        )
        .unwrap();
    let log = store.events(id).unwrap();
    assert_eq!(log[1].detail.as_deref(), Some("table has triggers"));
}

#[test]
fn preview_is_set_once_and_only_while_proposed() {
    let mut store = Store::open_in_memory().unwrap();
    let (id, _) = previewed(&mut store);
    assert!(matches!(
        store.set_preview(id, &preview("something else")),
        Err(StoreError::PreviewAlreadySet(_))
    ));

    let other = store
        .propose("refund-bot", "billing", "DELETE FROM refunds")
        .unwrap();
    store
        .transition(other, &Event::Withdraw, &agent(), None)
        .unwrap();
    assert!(matches!(
        store.set_preview(other, &preview("x")),
        Err(StoreError::NotProposed(_))
    ));
}

#[test]
fn list_by_state() {
    let mut store = Store::open_in_memory().unwrap();
    let (a, _) = previewed(&mut store);
    let (b, _) = previewed(&mut store);
    store
        .transition(a, &Event::PolicyHeld, &policy(), None)
        .unwrap();
    store
        .transition(b, &Event::PolicyHeld, &policy(), None)
        .unwrap();
    assert_eq!(store.list(State::Held).unwrap(), [a, b]);
    assert!(store.list(State::Applied).unwrap().is_empty());
}

#[test]
fn verify_detects_tampering() {
    let tamper = |sql: &str| {
        let mut store = Store::open_in_memory().unwrap();
        let (id, hash) = previewed(&mut store);
        store
            .transition(id, &Event::PolicyHeld, &policy(), None)
            .unwrap();
        store
            .transition(id, &Event::Approve, &human(), Some(&hash))
            .unwrap();
        assert_eq!(store.verify().unwrap(), Ok(()));
        store.conn.execute_batch(sql).unwrap();
        store.verify().unwrap()
    };

    assert_eq!(
        tamper("UPDATE events SET actor = 'agent:refund-bot' WHERE event = 'Approve'"),
        Err(Tampered::EventModified { seq: 3 })
    );
    assert_eq!(
        tamper("DELETE FROM events WHERE seq = 2"),
        Err(Tampered::BrokenLink { seq: 3 })
    );
    assert_eq!(
        tamper("UPDATE changes SET state = 'held'"),
        Err(Tampered::StateModified(1))
    );
    // Swapping in another preview with its own matching hash is still caught,
    // because the logged decisions name the original.
    let other = preview("paid twice");
    let json = other.to_canonical_json();
    let hash = hex(&Sha256::digest(&json));
    let mut store = Store::open_in_memory().unwrap();
    let (id, original) = previewed(&mut store);
    store
        .transition(id, &Event::PolicyHeld, &policy(), None)
        .unwrap();
    store
        .transition(id, &Event::Approve, &human(), Some(&original))
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE changes SET preview = ?1, preview_hash = ?2",
            params![json, hash],
        )
        .unwrap();
    assert_eq!(store.verify().unwrap(), Err(Tampered::PreviewModified(id)));
    assert!(tamper("UPDATE changes SET preview_hash = 'x'").is_err());
}

#[test]
fn survives_reopening() {
    let dir = std::env::temp_dir().join(format!("rowbit-store-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rowbit.db");
    let _ = std::fs::remove_file(&path);
    let id = {
        let mut store = Store::open(&path).unwrap();
        previewed(&mut store).0
    };
    let mut store = Store::open(&path).unwrap();
    store
        .transition(id, &Event::PolicyHeld, &policy(), None)
        .unwrap();
    assert_eq!(store.events(id).unwrap().len(), 2);
    assert_eq!(store.verify().unwrap(), Ok(()));
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}
