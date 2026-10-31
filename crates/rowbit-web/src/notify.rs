//! Sends held-change notifications: signed webhooks and Slack messages. See
//! "Notifications" in `docs/spec/server.md`.
//!
//! Notifications are queued in the state file when a change is held, by
//! whichever process proposed it; `rowbit serve` sends them. A notification
//! never affects the change: if it can't be delivered, the change stays
//! held and waits in the UI.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use rowbit_engine::{Delivery, DeliveryOutcome, Engine};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::config::{Notify, NotifyKind};
use crate::visible;

/// Waits before each retry; six tries in all, about ten minutes.
const RETRY_AFTER_MS: [i64; 5] = [10_000, 30_000, 60_000, 180_000, 300_000];
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a claimed notification is ours before another sender may try.
const LEASE_MS: i64 = 60_000;
const POLL: Duration = Duration::from_secs(2);

pub struct Notifier {
    engine: Arc<Engine>,
    targets: HashMap<String, Notify>,
    http: reqwest::Client,
    retry_after_ms: Vec<i64>,
}

impl Notifier {
    pub fn new(engine: Arc<Engine>, targets: Vec<Notify>) -> Notifier {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(SEND_TIMEOUT)
            .user_agent(concat!("rowbit/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("an HTTP client with these settings builds");
        Notifier {
            engine,
            targets: targets.into_iter().map(|t| (t.name.clone(), t)).collect(),
            http,
            retry_after_ms: RETRY_AFTER_MS.to_vec(),
        }
    }

    /// Other waits between tries (one per retry), for tests.
    pub fn with_retry_after(mut self, ms: &[i64]) -> Notifier {
        self.retry_after_ms = ms.to_vec();
        self
    }

    /// Sends due notifications until the process stops.
    pub async fn run(self) {
        loop {
            if let Err(e) = self.tick().await {
                eprintln!("rowbit: notifications: {e}");
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Sends what is due now. Returns how many were tried.
    pub async fn tick(&self) -> Result<usize, rowbit_engine::EngineError> {
        let due = self.engine.claim_deliveries(20, LEASE_MS)?;
        for d in &due {
            let outcome = self.send(d).await;
            self.engine.record_delivery(d.id, outcome)?;
        }
        Ok(due.len())
    }

    async fn send(&self, d: &Delivery) -> DeliveryOutcome {
        let Some(target) = self.targets.get(&d.target) else {
            return DeliveryOutcome::Failed {
                error: "this target is no longer in rowbit-server.yml".into(),
            };
        };
        let request = match target.kind {
            NotifyKind::Webhook => {
                let secret = target
                    .secret
                    .as_ref()
                    .map(|s| s.expose())
                    .unwrap_or_default();
                let t = unix_now();
                self.http
                    .post(target.url.expose())
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .header("Rowbit-Signature", signature(secret, t, &d.payload))
                    .header("Rowbit-Delivery", &d.delivery)
                    .body(d.payload.clone())
            }
            NotifyKind::Slack => {
                let payload: Value = serde_json::from_str(&d.payload).unwrap_or(Value::Null);
                self.http
                    .post(target.url.expose())
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(slack_message(&payload).to_string())
            }
        };
        let tries = d.attempts as usize + 1;
        let retry = |error: String| match self.retry_after_ms.get(tries - 1) {
            Some(&retry_in_ms) => DeliveryOutcome::Retry { error, retry_in_ms },
            None => DeliveryOutcome::Failed {
                error: format!("{error}; gave up after {tries} tries"),
            },
        };
        match request.send().await {
            Ok(r) if r.status().is_success() => DeliveryOutcome::Delivered,
            Ok(r) if r.status().as_u16() == 429 || r.status().is_server_error() => {
                retry(format!("HTTP {}", r.status().as_u16()))
            }
            Ok(r) => DeliveryOutcome::Failed {
                error: format!("HTTP {}", r.status().as_u16()),
            },
            // Without the URL: a Slack webhook's URL is its secret.
            Err(e) => retry(e.without_url().to_string()),
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `t=<unix seconds>,v1=<hex HMAC-SHA256 of "<t>.<body>">`.
pub fn signature(secret: &str, t: u64, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    mac.update(format!("{t}.{body}").as_bytes());
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("t={t},v1={hex}")
}

/// Text for Slack's mrkdwn: hidden characters visible, and `&`, `<`, `>`
/// escaped, so a table name can't become a link or a mention.
fn mrkdwn(text: &str) -> String {
    visible(text)
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('`', "'")
}

/// The Slack reference message: who wants what, why it was held, and a
/// button to the change. Approving happens in the UI, behind sign-in.
pub fn slack_message(payload: &Value) -> Value {
    let c = &payload["change"];
    let s = |k: &str| c[k].as_str().unwrap_or("");
    let what = match (s("statement"), c["rows"].as_u64()) {
        ("ddl", _) => format!("change the schema of `{}`", mrkdwn(s("table"))),
        (kind, Some(1)) => format!("{kind} *1 row* in `{}`", mrkdwn(s("table"))),
        (kind, Some(n)) => format!("{kind} *{n} rows* in `{}`", mrkdwn(s("table"))),
        (kind, None) => format!("{kind} `{}`", mrkdwn(s("table"))),
    };
    let summary = format!(
        "*Held for review:* `{}` wants to {what}.",
        mrkdwn(s("agent"))
    );
    let why = match c["rule"].as_str() {
        Some(rule) => format!("Rule `{}`: {}", mrkdwn(rule), mrkdwn(s("reason"))),
        None => mrkdwn(s("reason")),
    };
    let mut blocks = vec![
        json!({ "type": "section", "text": { "type": "mrkdwn", "text": summary } }),
        json!({ "type": "context", "elements": [{ "type": "mrkdwn", "text": format!("Change #{} · {why}", c["id"]) }] }),
    ];
    if let Some(url) = c["url"].as_str() {
        blocks.push(json!({
            "type": "actions",
            "elements": [{ "type": "button", "text": { "type": "plain_text", "text": "Review" }, "url": url }],
        }));
    }
    json!({
        "text": format!("Held for review: {} wants to {}", visible(s("agent")), what.replace(['*', '`'], "")),
        "blocks": blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_follow_the_spec() {
        // python3 -c 'import hmac,hashlib; print(hmac.new(b"k", b"1700000000.{\"a\":1}", hashlib.sha256).hexdigest())'
        assert_eq!(
            signature("k", 1_700_000_000, r#"{"a":1}"#),
            "t=1700000000,v1=1b6ad1bc9bf48c52ce01939866d5877c6cc4e87ddad27e16673cb8c4964394a3"
        );
    }

    #[test]
    fn slack_text_cant_link_or_mention() {
        let payload = json!({ "change": {
            "id": 7, "agent": "bot", "table": "db.<!channel>", "statement": "update",
            "rows": 37, "rule": "big", "reason": "a & b", "url": "https://r.example.com/changes/7",
        }});
        let m = slack_message(&payload);
        let text = m.to_string();
        assert!(!text.contains("<!channel>"), "{text}");
        assert!(text.contains("&lt;!channel&gt;"), "{text}");
        assert!(text.contains("*37 rows*"), "{text}");
        assert!(text.contains("a &amp; b"), "{text}");
        assert_eq!(
            m["blocks"][2]["elements"][0]["url"],
            "https://r.example.com/changes/7"
        );
    }
}
