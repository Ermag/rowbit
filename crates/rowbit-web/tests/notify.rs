//! Held-change notifications end to end: queued with the hold, sent by the
//! notifier to receivers run by the test, retried, and given up on. Runs on
//! the MariaDB test database from `compose.yaml`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use mysql_async::prelude::Queryable;
use rowbit_engine::{Engine, NotifySetting, NotifyTarget};
use rowbit_mysql::MySqlAdapter;
use rowbit_policy::PolicyFile;
use rowbit_store::Store;
use rowbit_web::config::ServerConfig;
use rowbit_web::notify::{Notifier, signature};
use serde_json::Value;

#[derive(Default)]
struct Receiver {
    /// (path, headers, body) of every request.
    got: Mutex<Vec<(String, HeaderMap, String)>>,
    /// Statuses /hook answers with, in order; 200 once empty.
    hook_answers: Mutex<VecDeque<u16>>,
}

async fn receiver() -> (Arc<Receiver>, String) {
    let r = Arc::new(Receiver::default());
    let record = |path: &'static str| {
        move |State(r): State<Arc<Receiver>>, headers: HeaderMap, body: String| async move {
            r.got.lock().unwrap().push((path.to_owned(), headers, body));
            let status = match path {
                "/hook" => r.hook_answers.lock().unwrap().pop_front().unwrap_or(200),
                "/bad" => 400,
                _ => 200,
            };
            StatusCode::from_u16(status).unwrap()
        }
    };
    let app = axum::Router::new()
        .route("/hook", post(record("/hook")))
        .route("/slack", post(record("/slack")))
        .route("/bad", post(record("/bad")))
        .with_state(r.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (r, base)
}

#[tokio::test]
async fn a_held_change_is_announced_and_retried() {
    let Ok(url) = std::env::var("ROWBIT_TEST_MARIADB_URL") else {
        assert!(std::env::var("ROWBIT_REQUIRE_DB").as_deref() != Ok("1"));
        return;
    };
    let mut conn = mysql_async::Conn::from_url(&url).await.unwrap();
    conn.query_drop(
        "DROP TABLE IF EXISTS notify_refunds; \
         CREATE TABLE notify_refunds (id INT PRIMARY KEY, card VARCHAR(20) NOT NULL) ENGINE=InnoDB; \
         INSERT INTO notify_refunds VALUES (1, 's3cr3t-4242'), (2, 's3cr3t-4242')",
    )
    .await
    .unwrap();
    conn.disconnect().await.unwrap();

    let (rx, base) = receiver().await;
    let hook_secret = format!("{:?}", std::time::Instant::now());
    let config = ServerConfig::from_yaml(
        &format!(
            "version: 0\nnotify:\n  - {{ name: hook, kind: webhook, url: {base}/hook, secret: \"${{HOOK}}\" }}\n  - {{ name: slack, kind: slack, url: \"${{SLACK}}\", agents: [refund-bot] }}\n  - {{ name: bad, kind: webhook, url: {base}/bad, secret: \"${{HOOK}}\" }}\n  - {{ name: other, kind: slack, url: \"${{SLACK}}\", agents: [someone-else] }}\n"
        ),
        |name| match name {
            "HOOK" => Some(hook_secret.clone()),
            "SLACK" => Some(format!("{base}/slack")),
            _ => None,
        },
    )
    .unwrap();
    let engine = Arc::new(Engine::new(
        MySqlAdapter::new(&url).unwrap(),
        PolicyFile::from_yaml("version: 0\nagents:\n  refund-bot:\n    default: hold\n").unwrap(),
        Store::open_in_memory().unwrap(),
    ));
    let mut targets: Vec<NotifyTarget> = config
        .notify
        .iter()
        .map(|n| NotifyTarget {
            name: n.name.clone(),
            agents: n.agents.clone(),
        })
        .collect();
    // A target the queue knows but the sender doesn't, as after an edit.
    targets.push(NotifyTarget {
        name: "gone".into(),
        agents: None,
    });
    engine
        .set_notify(&NotifySetting {
            review_url: Some("https://rowbit.example.com".into()),
            targets,
        })
        .unwrap();

    let held = engine
        .propose(
            "refund-bot",
            "rowbit_test",
            "UPDATE notify_refunds SET card = 'x' WHERE card = 's3cr3t-4242'",
        )
        .await
        .unwrap();
    assert_eq!(held.state.as_str(), "held");
    let queued = engine.deliveries(10).unwrap();
    let mut names: Vec<&str> = queued.iter().map(|d| d.target.as_str()).collect();
    names.sort();
    assert_eq!(
        names,
        ["bad", "gone", "hook", "slack"],
        "not 'other', which is for another agent"
    );

    rx.hook_answers.lock().unwrap().extend([503, 503]);
    let notifier = Notifier::new(engine.clone(), config.notify.clone()).with_retry_after(&[0, 0]);
    assert_eq!(notifier.tick().await.unwrap(), 4);
    let state = |t: &str| {
        let d = engine
            .deliveries(10)
            .unwrap()
            .into_iter()
            .find(|d| d.target == t)
            .unwrap();
        (d.state, d.attempts, d.last_error)
    };
    assert_eq!(state("slack").0, "delivered");
    assert_eq!(state("bad"), ("failed".into(), 1, Some("HTTP 400".into())));
    assert_eq!(state("gone").0, "failed");
    assert_eq!(
        state("hook"),
        ("pending".into(), 1, Some("HTTP 503".into()))
    );
    // Second try: 503 again. Third: delivered.
    notifier.tick().await.unwrap();
    assert_eq!(
        state("hook"),
        ("pending".into(), 2, Some("HTTP 503".into()))
    );
    notifier.tick().await.unwrap();
    assert_eq!(state("hook").0, "delivered");

    let got = rx.got.lock().unwrap();
    let hooks: Vec<_> = got.iter().filter(|(p, ..)| p == "/hook").collect();
    assert_eq!(hooks.len(), 3);
    let (_, headers, body) = hooks[2];
    // The same delivery id every try, and a signature that checks out.
    let delivery = headers["rowbit-delivery"].to_str().unwrap();
    assert!(
        hooks
            .iter()
            .all(|(_, h, _)| h["rowbit-delivery"] == delivery)
    );
    let sig = headers["rowbit-signature"].to_str().unwrap();
    let t: u64 = sig
        .strip_prefix("t=")
        .unwrap()
        .split(',')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(sig, signature(&hook_secret, t, body));
    let payload: Value = serde_json::from_str(body).unwrap();
    assert_eq!(payload["event"], "change.held");
    assert_eq!(payload["delivery"], delivery);
    assert_eq!(payload["change"]["rows"], 2);
    assert_eq!(payload["change"]["table"], "rowbit_test.notify_refunds");
    assert_eq!(
        payload["change"]["url"],
        format!("https://rowbit.example.com/#/changes/{}", held.id)
    );
    // Never row values or the statement.
    for (_, _, body) in got.iter() {
        assert!(
            !body.contains("s3cr3t") && !body.contains("UPDATE"),
            "{body}"
        );
    }
    let (_, _, slack) = got.iter().find(|(p, ..)| p == "/slack").unwrap();
    let slack: Value = serde_json::from_str(slack).unwrap();
    assert!(
        slack["text"]
            .as_str()
            .unwrap()
            .contains("refund-bot wants to update 2 rows"),
        "{slack}"
    );
    assert_eq!(slack["blocks"][2]["elements"][0]["text"]["text"], "Review");

    // The change itself never moved.
    assert_eq!(engine.change(held.id).unwrap().state.as_str(), "held");
}

#[tokio::test]
async fn a_notification_that_keeps_failing_is_given_up_on() {
    let (rx, base) = receiver().await;
    rx.hook_answers.lock().unwrap().extend([500, 500, 500]);
    let config = ServerConfig::from_yaml(
        &format!("version: 0\nnotify:\n  - {{ name: hook, kind: webhook, url: {base}/hook, secret: \"${{K}}\" }}\n"),
        |_| Some("k".into()),
    )
    .unwrap();
    // A held change without a database: queue one by hand through the store.
    let mut store = Store::open_in_memory().unwrap();
    let id = store.propose("bot", "db", "DELETE FROM t").unwrap();
    store
        .set_review(id, &rowbit_store::Review::Ddl { sql: "x".into() })
        .unwrap();
    store
        .transition_notifying(
            id,
            &rowbit_core::change::Event::PolicyHeld { reason: "r".into() },
            &rowbit_core::change::Actor::Policy("p".into()),
            None,
            &[rowbit_store::NewDelivery {
                target: "hook".into(),
                delivery: "d".into(),
                payload: "{}".into(),
            }],
        )
        .unwrap();
    let engine = Arc::new(Engine::new(
        MySqlAdapter::new("mysql://nobody@127.0.0.1:1/none").unwrap(),
        PolicyFile::from_yaml("version: 0\n").unwrap(),
        store,
    ));
    let notifier = Notifier::new(engine.clone(), config.notify).with_retry_after(&[0, 0]);
    for _ in 0..3 {
        notifier.tick().await.unwrap();
    }
    let d = &engine.deliveries(1).unwrap()[0];
    assert_eq!(d.state, "failed");
    assert_eq!(d.attempts, 3);
    assert!(
        d.last_error
            .as_deref()
            .unwrap()
            .contains("gave up after 3 tries"),
        "{d:?}"
    );
    assert_eq!(notifier.tick().await.unwrap(), 0, "nothing more is sent");
}
