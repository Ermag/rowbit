use rowbit_core::preview::{AppliedKey, Column, RowChange, Table, WriteKind};

use super::*;

fn column(name: &str) -> Column {
    Column {
        name: name.into(),
        column_type: "varchar(20)".into(),
        generated: false,
        auto_increment: false,
    }
}

fn preview(status: &str) -> Review {
    Review::Rows(Preview {
        kind: WriteKind::Update,
        table: Table {
            database: "billing".into(),
            name: "refunds".into(),
            columns: vec![column("id"), column("status")],
            primary_key: vec![0],
        },
        written: vec![1],
        rows: vec![RowChange {
            before: Some(vec![Some(b"7".to_vec()), Some(b"approved".to_vec())]),
            after: Some(vec![Some(b"7".to_vec()), Some(status.as_bytes().to_vec())]),
        }],
        as_of: "1791900000.000000".into(),
    })
}

fn held() -> Event {
    Event::PolicyHeld {
        reason: "large batch".into(),
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
    let hash = store.set_review(id, &preview("refunded")).unwrap();
    (id, hash)
}

#[test]
fn held_change_is_approved_applied_and_undone() {
    let mut store = Store::open_in_memory().unwrap();
    let (id, hash) = previewed(&mut store);

    store.transition(id, &held(), &policy(), None).unwrap();
    store
        .transition(id, &Event::Approve, &human(), Some(&hash))
        .unwrap();
    assert!(matches!(
        store.transition(id, &Event::ApplySucceeded, &Actor::System, None),
        Err(StoreError::ApplyNeedsRecord)
    ));
    let applied = Applied {
        keys: vec![AppliedKey {
            key: Some(vec![Some(b"7".to_vec())]),
        }],
    };
    store.record_applied(id, &applied).unwrap();
    store
        .transition(id, &Event::UndoSucceeded, &Actor::System, None)
        .unwrap();

    let change = store.get(id).unwrap();
    assert_eq!(change.state, State::Undone);
    assert_eq!(change.applied, Some(applied.clone()));
    assert_eq!(change.review, Some(preview("refunded")));
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
        Some(r#"{"database":"billing","sql":"UPDATE refunds SET status = 'refunded'"}"#)
    );
    assert_eq!(log[0].actor, "agent:refund-bot");
    assert_eq!(log[1].review_hash.as_deref(), Some(hash.as_str()));
    assert_eq!(log[2].review_hash.as_deref(), Some(hash.as_str()));
    assert_eq!(log[2].actor, "human:philip");
    assert_eq!(log[3].review_hash, None);
    assert_eq!(store.verify().unwrap(), Ok(()));
}

#[test]
fn agent_can_never_approve_and_nothing_is_logged() {
    let mut store = Store::open_in_memory().unwrap();
    let (id, hash) = previewed(&mut store);
    store.transition(id, &held(), &policy(), None).unwrap();

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
    store.transition(id, &held(), &policy(), None).unwrap();

    for seen in [None, Some("0".repeat(64))] {
        let err = store
            .transition(id, &Event::Approve, &human(), seen.as_deref())
            .unwrap_err();
        assert!(matches!(err, StoreError::ReviewMismatch), "{err}");
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
        Err(StoreError::NoReview(_))
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
        store.set_review(id, &preview("something else")),
        Err(StoreError::ReviewAlreadySet(_))
    ));

    let other = store
        .propose("refund-bot", "billing", "DELETE FROM refunds")
        .unwrap();
    store
        .transition(other, &Event::Withdraw, &agent(), None)
        .unwrap();
    assert!(matches!(
        store.set_review(other, &preview("x")),
        Err(StoreError::NotProposed(_))
    ));
}

#[test]
fn notes_leave_the_state_alone() {
    let mut store = Store::open_in_memory().unwrap();
    let (id, _) = previewed(&mut store);
    store
        .note(
            id,
            &Actor::System,
            "ErrorDetail",
            "Incorrect decimal value: 'x'",
        )
        .unwrap();
    let log = store.events(id).unwrap();
    let last = log.last().unwrap();
    assert_eq!(
        (last.event.as_str(), last.from.as_deref(), last.to.as_str()),
        ("ErrorDetail", Some("proposed"), "proposed")
    );
    assert_eq!(store.get(id).unwrap().state, State::Proposed);
    assert_eq!(store.verify().unwrap(), Ok(()));
}

#[test]
fn list_by_state() {
    let mut store = Store::open_in_memory().unwrap();
    let (a, _) = previewed(&mut store);
    let (b, _) = previewed(&mut store);
    store.transition(a, &held(), &policy(), None).unwrap();
    store.transition(b, &held(), &policy(), None).unwrap();
    assert_eq!(store.list(State::Held).unwrap(), [a, b]);
    assert!(store.list(State::Applied).unwrap().is_empty());
}

#[test]
fn verify_detects_tampering() {
    let tamper = |sql: &str| {
        let mut store = Store::open_in_memory().unwrap();
        let (id, hash) = previewed(&mut store);
        store.transition(id, &held(), &policy(), None).unwrap();
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
    store.transition(id, &held(), &policy(), None).unwrap();
    store
        .transition(id, &Event::Approve, &human(), Some(&original))
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE changes SET review = ?1, review_hash = ?2",
            params![json, hash],
        )
        .unwrap();
    assert_eq!(store.verify().unwrap(), Err(Tampered::ReviewModified(id)));
    assert!(tamper("UPDATE changes SET review_hash = 'x'").is_err());
}

#[test]
fn survives_reopening() {
    let dir = std::env::temp_dir().join(format!("rowbit-store-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rowbit.db");
    for file in ["rowbit.db", "rowbit.db-wal", "rowbit.db-shm"] {
        let _ = std::fs::remove_file(dir.join(file));
    }
    let id = {
        let mut store = Store::open(&path).unwrap();
        let id = previewed(&mut store).0;
        // Found in review: the journal files were world-readable. Checked
        // while the store is open, since SQLite removes them on close.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for file in ["rowbit.db", "rowbit.db-wal", "rowbit.db-shm"] {
                let mode = std::fs::metadata(dir.join(file))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{file} holds copies of rows");
            }
        }
        id
    };
    let mut store = Store::open(&path).unwrap();
    store.transition(id, &held(), &policy(), None).unwrap();
    assert_eq!(store.events(id).unwrap().len(), 2);
    assert_eq!(store.verify().unwrap(), Ok(()));
    drop(store);
    std::fs::remove_dir_all(&dir).unwrap();
}

// Found in review: DDL has no preview, so it could never be held.
#[test]
fn ddl_is_held_and_approved_on_the_statement_rowbit_will_run() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store
        .propose("refund-bot", "billing", "alter table refunds add note text")
        .unwrap();
    let review = Review::Ddl {
        sql: "ALTER TABLE refunds ADD COLUMN note TEXT".into(),
    };
    let hash = store.set_review(id, &review).unwrap();
    store.transition(id, &held(), &policy(), None).unwrap();
    store
        .transition(id, &Event::Approve, &human(), Some(&hash))
        .unwrap();
    assert_eq!(store.get(id).unwrap().review, Some(review));
    assert_eq!(store.verify().unwrap(), Ok(()));
}

// Found in review: the change row's agent, database and statement were not verified.
#[test]
fn verify_detects_an_edited_proposal_or_a_deleted_change() {
    for sql in [
        "UPDATE changes SET sql = 'DELETE FROM refunds'",
        "UPDATE changes SET agent = 'other-bot'",
        "UPDATE changes SET database_name = 'crm'",
    ] {
        let mut store = Store::open_in_memory().unwrap();
        let (id, _) = previewed(&mut store);
        store.conn.execute_batch(sql).unwrap();
        assert_eq!(
            store.verify().unwrap(),
            Err(Tampered::ProposalModified(id)),
            "{sql}"
        );
    }

    let mut store = Store::open_in_memory().unwrap();
    previewed(&mut store);
    store
        .conn
        .execute_batch("PRAGMA foreign_keys = OFF; DELETE FROM changes;")
        .unwrap();
    assert_eq!(
        store.verify().unwrap(),
        Err(Tampered::OrphanEvent { seq: 1 })
    );
}

#[test]
fn a_second_connection_waits_instead_of_failing() {
    let dir = std::env::temp_dir().join(format!("rowbit-store-busy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rowbit.db");
    let _ = std::fs::remove_file(&path);
    let mut a = Store::open(&path).unwrap();
    let mut b = Store::open(&path).unwrap();
    let held = a
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let writer = std::thread::spawn(move || b.propose("refund-bot", "billing", "DELETE FROM t"));
    std::thread::sleep(Duration::from_millis(200));
    held.commit().unwrap();
    assert!(writer.join().unwrap().is_ok());
    drop(a);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_record_is_chained_and_refusals_are_logged() {
    let mut store = Store::open_in_memory().unwrap();
    let (id, hash) = previewed(&mut store);
    store.transition(id, &held(), &policy(), None).unwrap();
    store
        .transition(id, &Event::Approve, &human(), Some(&hash))
        .unwrap();
    let applied = Applied {
        keys: vec![AppliedKey {
            key: Some(vec![Some(b"7".to_vec())]),
        }],
    };
    store.record_applied(id, &applied).unwrap();
    store
        .note(id, &human(), "UndoRefused", "row 7 changed since apply")
        .unwrap();
    let log = store.events(id).unwrap();
    let last = log.last().unwrap();
    assert_eq!(
        (last.event.as_str(), last.from.as_deref(), last.to.as_str()),
        ("UndoRefused", Some("applied"), "applied")
    );
    assert_eq!(store.get(id).unwrap().state, State::Applied);
    assert_eq!(store.verify().unwrap(), Ok(()));

    let other = Applied {
        keys: vec![AppliedKey {
            key: Some(vec![Some(b"8".to_vec())]),
        }],
    };
    store
        .conn
        .execute(
            "UPDATE changes SET applied = ?1",
            params![other.to_canonical_json()],
        )
        .unwrap();
    assert_eq!(
        store.verify().unwrap(),
        Err(Tampered::ApplyRecordModified(id))
    );
}

#[test]
fn settings_are_kept_and_replaced() {
    let mut store = Store::open_in_memory().unwrap();
    assert_eq!(store.setting("web.password").unwrap(), None);
    store.set_setting("web.password", "a").unwrap();
    store.set_setting("web.password", "b").unwrap();
    assert_eq!(store.setting("web.password").unwrap().as_deref(), Some("b"));
}

#[test]
fn notifications_are_queued_with_the_hold_and_sent_once() {
    let mut store = Store::open_in_memory().unwrap();
    let (id, _) = previewed(&mut store);
    let notify = [
        NewDelivery {
            target: "slack".into(),
            delivery: "d1".into(),
            payload: "{}".into(),
        },
        NewDelivery {
            target: "pager".into(),
            delivery: "d2".into(),
            payload: "{}".into(),
        },
    ];
    store
        .transition_notifying(id, &held(), &policy(), None, &notify)
        .unwrap();
    assert_eq!(store.deliveries(10).unwrap().len(), 2);

    // Leased: a second sender gets nothing until the lease runs out.
    let due = store.claim_deliveries(10, 60_000).unwrap();
    assert_eq!(due.len(), 2);
    assert!(store.claim_deliveries(10, 60_000).unwrap().is_empty());

    store
        .record_delivery(due[0].id, DeliveryOutcome::Delivered)
        .unwrap();
    store
        .record_delivery(
            due[1].id,
            DeliveryOutcome::Retry {
                error: "HTTP 503".into(),
                retry_in_ms: 60_000,
            },
        )
        .unwrap();
    // The retry isn't due yet, and the delivered one never comes back.
    assert!(store.claim_deliveries(10, 60_000).unwrap().is_empty());
    let all = store.deliveries(10).unwrap();
    let by_target = |t: &str| all.iter().find(|d| d.target == t).unwrap().clone();
    assert_eq!(by_target("slack").state, "delivered");
    let pager = by_target("pager");
    assert_eq!((pager.state.as_str(), pager.attempts), ("pending", 1));
    assert_eq!(pager.last_error.as_deref(), Some("HTTP 503"));
    store
        .record_delivery(
            pager.id,
            DeliveryOutcome::Failed {
                error: "gave up".into(),
            },
        )
        .unwrap();
    assert_eq!(store.deliveries(10).unwrap()[0].state, "failed");
    // Deliveries are outside the hash chain: the log still verifies.
    assert_eq!(store.verify().unwrap(), Ok(()));
}

#[test]
fn a_refused_transition_queues_nothing() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store
        .propose("refund-bot", "billing", "DELETE FROM t")
        .unwrap();
    let notify = [NewDelivery {
        target: "slack".into(),
        delivery: "d1".into(),
        payload: "{}".into(),
    }];
    // No review stored yet, so PolicyHeld is refused, and so is the queueing.
    assert!(
        store
            .transition_notifying(id, &held(), &policy(), None, &notify)
            .is_err()
    );
    assert!(store.deliveries(10).unwrap().is_empty());
}

#[test]
fn agent_tokens_are_kept_as_hashes_and_replaced() {
    let mut store = Store::open_in_memory().unwrap();
    let first = store.issue_agent_token("refund-bot").unwrap();
    assert!(
        first.starts_with("rbt_") && first.len() == 4 + 43,
        "{first}"
    );
    assert_eq!(
        store.agent_for_token(&first).unwrap().as_deref(),
        Some("refund-bot")
    );
    let (created, used) = store.agent_token_times("refund-bot").unwrap().unwrap();
    assert!(created > 0 && used.is_some());
    // The token itself is stored nowhere.
    let n: i64 = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM settings WHERE key LIKE ?1 OR value LIKE ?1",
            [format!("%{}%", &first[4..])],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);

    // A new token replaces the old one.
    let second = store.issue_agent_token("refund-bot").unwrap();
    assert_ne!(first, second);
    assert_eq!(store.agent_for_token(&first).unwrap(), None);
    assert_eq!(
        store.agent_for_token(&second).unwrap().as_deref(),
        Some("refund-bot")
    );
    assert_eq!(store.agent_for_token("rbt_made-up").unwrap(), None);

    assert!(store.revoke_agent_token("refund-bot").unwrap());
    assert_eq!(store.agent_for_token(&second).unwrap(), None);
    assert_eq!(store.agent_token_times("refund-bot").unwrap(), None);
    assert!(!store.revoke_agent_token("refund-bot").unwrap());
}
