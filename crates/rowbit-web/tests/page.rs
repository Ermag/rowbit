//! The review UI's API and pages through the router, as the browser would
//! use them, against the test databases from `compose.yaml`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use mysql_async::prelude::Queryable;
use rowbit_engine::Engine;
use rowbit_mysql::MySqlAdapter;
use rowbit_policy::PolicyFile;
use rowbit_store::Store;
use serde_json::{Value, json};
use tower::ServiceExt;

const HOST: &str = "127.0.0.1:8080";

/// The page's password for these tests, made up at run time.
fn password() -> &'static str {
    static PASSWORD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PASSWORD.get_or_init(rowbit_web::new_password)
}
const POLICY: &str = "version: 0\nagents:\n  refund-bot:\n    default: hold\n";

fn url(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(url) => Some(url),
        Err(_) if std::env::var("ROWBIT_REQUIRE_DB").as_deref() == Ok("1") => {
            panic!("{var} is not set and ROWBIT_REQUIRE_DB=1")
        }
        Err(_) => {
            eprintln!("skipping: {var} is not set (run `docker compose up -d --wait`)");
            None
        }
    }
}

struct Page {
    router: axum::Router,
    engine: Arc<Engine>,
    /// The session cookie, once signed in.
    cookie: Option<String>,
}

impl Page {
    fn new(engine: Arc<Engine>) -> Page {
        let hash = rowbit_web::hash_password(password());
        Page {
            router: rowbit_web::router(
                engine.clone(),
                rowbit_web::Web {
                    sign_in: rowbit_web::SignIn::Password { hash },
                    hosts: vec![HOST.to_owned()],
                    secure: false,
                    mcp: None,
                },
            ),
            engine,
            cookie: None,
        }
    }

    async fn sign_in(&mut self, name: &str, password: &str) -> StatusCode {
        let request = Request::post("/login")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "name={name}&password={}",
                password.replace(' ', "+")
            )))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        if let Some(cookie) = response.headers().get(header::SET_COOKIE) {
            let cookie = cookie.to_str().unwrap();
            assert!(
                cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"),
                "{cookie}"
            );
            self.cookie = Some(cookie.split(';').next().unwrap().to_owned());
        }
        response.status()
    }

    async fn get(&self, host: &str) -> (StatusCode, String) {
        self.get_path(host, "/").await
    }

    async fn get_path(&self, host: &str, path: &str) -> (StatusCode, String) {
        let mut request = Request::get(path).header(header::HOST, host);
        if let Some(cookie) = &self.cookie {
            request = request.header(header::COOKIE, cookie);
        }
        self.send(request.body(Body::empty()).unwrap()).await
    }

    /// A JSON API read.
    async fn api(&self, path: &str) -> (StatusCode, Value) {
        let (status, body) = self.get_path(HOST, path).await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    /// This session's token, from the API.
    async fn token(&self) -> String {
        self.api("/api/session").await.1["token"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// A JSON API action, with `token` in the header (if any) and `origin`.
    async fn act(
        &self,
        path: &str,
        body: Value,
        token: Option<&str>,
        origin: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut request = Request::post(path)
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            request = request.header("x-rowbit-token", token);
        }
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        if let Some(cookie) = &self.cookie {
            request = request.header(header::COOKIE, cookie);
        }
        let (status, body) = self
            .send(request.body(Body::from(body.to_string())).unwrap())
            .await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    async fn send(&self, request: Request<Body>) -> (StatusCode, String) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }
}

async fn setup(var: &str, table: &str) -> Option<(Page, String, String)> {
    let url = url(var)?;
    let mut conn = mysql_async::Conn::from_url(&url).await.unwrap();
    conn.query_drop(format!(
        "DROP TABLE IF EXISTS {table}; \
         CREATE TABLE {table} (id INT PRIMARY KEY, status VARCHAR(40) NOT NULL) ENGINE=InnoDB; \
         INSERT INTO {table} VALUES (1, 'approved'), (2, 'approved')"
    ))
    .await
    .unwrap();
    conn.disconnect().await.unwrap();
    let engine = Arc::new(Engine::new(
        MySqlAdapter::new(&url).unwrap(),
        PolicyFile::from_yaml(POLICY).unwrap(),
        Store::open_in_memory().unwrap(),
    ));
    let mut page = Page::new(engine);
    assert_eq!(
        page.sign_in("philip", password()).await,
        StatusCode::SEE_OTHER
    );
    Some((page, url, table.to_owned()))
}

async fn statuses(url: &str, table: &str) -> Vec<String> {
    let mut conn = mysql_async::Conn::from_url(url).await.unwrap();
    let s = conn
        .query(format!("SELECT status FROM {table} ORDER BY id"))
        .await
        .unwrap();
    conn.disconnect().await.unwrap();
    s
}

macro_rules! on_both {
    ($scenario:ident) => {
        mod $scenario {
            #[tokio::test]
            async fn mariadb() {
                super::$scenario("ROWBIT_TEST_MARIADB_URL", "web_m").await
            }
            #[tokio::test]
            async fn mysql() {
                super::$scenario("ROWBIT_TEST_MYSQL_URL", "web_y").await
            }
        }
    };
}

on_both!(approve_from_the_ui_then_undo);
async fn approve_from_the_ui_then_undo(var: &str, table: &str) {
    let Some((page, url, t)) = setup(var, &format!("{table}_a")).await else {
        return;
    };
    let id = page
        .engine
        .propose(
            "refund-bot",
            "rowbit_test",
            &format!("UPDATE {t} SET status = '<b>refunded</b>\u{202e}'"),
        )
        .await
        .unwrap()
        .id;

    let (status, list) = page.api("/api/changes?state=held").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["changes"].as_array().unwrap().len(), 1, "{list}");
    let (_, change) = page.api(&format!("/api/changes/{id}")).await;
    let review = change["review_hash"].as_str().unwrap().to_owned();
    assert_eq!(change["preview"]["total_rows"], 2);
    // Values come typed, hidden characters made visible; the UI inserts
    // them only as text.
    let after = &change["preview"]["rows"][0]["cells"][1]["after"];
    assert_eq!(
        after,
        &json!({"kind": "text", "text": "<b>refunded</b>\\u{202e}"}),
        "{change}"
    );
    let token = page.token().await;

    let approve = format!("/api/changes/{id}/approve");
    let body = json!({ "review": review });
    // No token, a wrong token, or another site: refused.
    let (status, _) = page.act(&approve, body.clone(), None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = page
        .act(&approve, body.clone(), Some(&"0".repeat(64)), None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = page
        .act(
            &approve,
            body.clone(),
            Some(&token),
            Some("https://evil.example"),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // A review hash that is not the stored one.
    let (status, _) = page
        .act(
            &approve,
            json!({ "review": "0".repeat(64) }),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(statuses(&url, &t).await, ["approved", "approved"]);

    let (status, done) = page
        .act(
            &approve,
            body,
            Some(&token),
            Some(&format!("http://{HOST}")),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "applied");
    assert_eq!(done["undoable"], true);
    assert_eq!(
        statuses(&url, &t).await,
        ["<b>refunded</b>\u{202e}", "<b>refunded</b>\u{202e}"]
    );

    let (status, done) = page
        .act(
            &format!("/api/changes/{id}/undo"),
            json!({}),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "undone");
    assert_eq!(statuses(&url, &t).await, ["approved", "approved"]);
    let actors: Vec<&str> = done["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["actor"].as_str().unwrap())
        .collect();
    assert!(actors.contains(&"human:philip"), "{actors:?}");
    let (_, verify) = page.api("/api/verify").await;
    assert_eq!(verify["ok"], true);
}

on_both!(reject_from_the_ui);
async fn reject_from_the_ui(var: &str, table: &str) {
    let Some((page, url, t)) = setup(var, &format!("{table}_r")).await else {
        return;
    };
    let id = page
        .engine
        .propose(
            "refund-bot",
            "rowbit_test",
            &format!("DELETE FROM {t} WHERE id = 1"),
        )
        .await
        .unwrap()
        .id;
    let token = page.token().await;
    let reject = format!("/api/changes/{id}/reject");
    let (status, _) = page
        .act(&reject, json!({ "reason": " " }), Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, done) = page
        .act(
            &reject,
            json!({ "reason": "not today" }),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(done["reason"], "not today");
    assert_eq!(
        page.engine
            .status("refund-bot", id)
            .unwrap()
            .reason
            .as_deref(),
        Some("not today")
    );
    assert_eq!(statuses(&url, &t).await, ["approved", "approved"]);
}

#[tokio::test]
async fn other_hosts_are_refused_and_the_ui_needs_a_session() {
    let engine = Arc::new(Engine::new(
        MySqlAdapter::new("mysql://nobody@127.0.0.1:1/none").unwrap(),
        PolicyFile::from_yaml(POLICY).unwrap(),
        Store::open_in_memory().unwrap(),
    ));
    let mut page = Page::new(engine);
    let (status, _) = page.get("rebind.evil.example:8080").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Signed out, the UI redirects to sign-in and the API says nothing.
    let (status, html) = page.get(HOST).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
    let (status, _) = page.api("/api/changes").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Found by a browser test: with `Referrer-Policy: no-referrer`, browsers
    // send `Origin: null` on the page's own forms, so the origin check
    // refused every sign-in. Same-origin requests must carry their origin.
    let request = Request::get("/login")
        .header(header::HOST, HOST)
        .body(Body::empty())
        .unwrap();
    let response = page.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.headers()[header::REFERRER_POLICY], "same-origin");
    let (status, html) = page.get_path(HOST, "/login").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("type=password"), "{html}");
    assert_eq!(
        page.sign_in("philip", password()).await,
        StatusCode::SEE_OTHER
    );
    let request = Request::get("/")
        .header(header::HOST, HOST)
        .header(header::COOKIE, page.cookie.clone().unwrap())
        .body(Body::empty())
        .unwrap();
    let response = page.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let csp = response.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        csp.contains("script-src 'self'") && !csp.contains("unsafe"),
        "{csp}"
    );
    let (_, me) = page.api("/api/session").await;
    assert_eq!(me["reviewer"], "philip");
    let (status, js) = page.get_path(HOST, "/assets/app.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!js.is_empty());
}

// Found in review: any local user could fetch the page, take the form token
// and approve. Reaching the port is no longer enough.
on_both!(reaching_the_port_is_not_enough_to_approve);
async fn reaching_the_port_is_not_enough_to_approve(var: &str, table: &str) {
    let Some((reviewer, url, t)) = setup(var, &format!("{table}_x")).await else {
        return;
    };
    let id = reviewer
        .engine
        .propose(
            "refund-bot",
            "rowbit_test",
            &format!("UPDATE {t} SET status = 'x'"),
        )
        .await
        .unwrap()
        .id;
    let token = reviewer.token().await;
    let review = reviewer.api(&format!("/api/changes/{id}")).await.1["review_hash"].clone();
    let body = json!({ "review": review });
    let approve = format!("/api/changes/{id}/approve");

    // An attacker on the same host: no cookie, even with a stolen token.
    let mut attacker = Page {
        router: reviewer.router.clone(),
        engine: reviewer.engine.clone(),
        cookie: None,
    };
    let (status, _) = attacker.get(HOST).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _) = attacker
        .act(&approve, body.clone(), Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A made-up cookie.
    attacker.cookie = Some(format!("rowbit_session={}", "a".repeat(64)));
    let (status, _) = attacker
        .act(&approve, body.clone(), Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Guessing the password: refused, and slowly.
    attacker.cookie = None;
    let started = std::time::Instant::now();
    assert_eq!(
        attacker.sign_in("evil", &rowbit_web::new_password()).await,
        StatusCode::FORBIDDEN
    );
    assert!(started.elapsed() >= std::time::Duration::from_secs(1));
    assert!(attacker.cookie.is_none());
    // Another session's token doesn't work with your own cookie.
    assert_eq!(
        attacker.sign_in("other", password()).await,
        StatusCode::SEE_OTHER
    );
    let (status, _) = attacker
        .act(&approve, body.clone(), Some(&token), None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    assert_eq!(statuses(&url, &t).await, ["approved", "approved"]);
    let (status, _) = reviewer.act(&approve, body, Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        reviewer
            .engine
            .history(id)
            .unwrap()
            .iter()
            .any(|e| e.event == "Approve" && e.actor == "human:philip")
    );
}

on_both!(big_changes_need_the_row_count);
async fn big_changes_need_the_row_count(var: &str, table: &str) {
    let Some((page, url, t)) = setup(var, &format!("{table}_b")).await else {
        return;
    };
    let mut conn = mysql_async::Conn::from_url(&url).await.unwrap();
    for id in 3..=60 {
        conn.query_drop(format!("INSERT INTO {t} VALUES ({id}, 'approved')"))
            .await
            .unwrap();
    }
    conn.disconnect().await.unwrap();
    let id = page
        .engine
        .propose(
            "refund-bot",
            "rowbit_test",
            &format!("UPDATE {t} SET status = 'x'"),
        )
        .await
        .unwrap()
        .id;
    let (_, first) = page.api(&format!("/api/changes/{id}")).await;
    assert_eq!(first["confirm_rows"], true);
    assert_eq!(first["preview"]["rows"].as_array().unwrap().len(), 50);
    let (_, rest) = page.api(&format!("/api/changes/{id}?offset=50")).await;
    let rows = rest["preview"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 10);
    assert_eq!(rows[9]["key"][0], json!({"kind": "number", "text": "60"}));

    let token = page.token().await;
    let approve = format!("/api/changes/{id}/approve");
    let review = first["review_hash"].clone();
    for rows in [None, Some(59)] {
        let (status, body) = page
            .act(
                &approve,
                json!({ "review": review, "rows": rows }),
                Some(&token),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].as_str().unwrap().contains("60"));
    }
    let (status, done) = page
        .act(
            &approve,
            json!({ "review": review, "rows": 60 }),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "applied");
}

// Found in review 4: wrong guesses were checked in parallel, so the wait
// didn't cap the rate. Now they queue: two at once take 1 s + 2 s.
#[tokio::test]
async fn wrong_passwords_are_checked_one_at_a_time() {
    let engine = Arc::new(Engine::new(
        MySqlAdapter::new("mysql://nobody@127.0.0.1:1/none").unwrap(),
        PolicyFile::from_yaml(POLICY).unwrap(),
        Store::open_in_memory().unwrap(),
    ));
    let page = Page::new(engine);
    let started = std::time::Instant::now();
    let guess = |p: &Page| {
        let mut p = Page {
            router: p.router.clone(),
            engine: p.engine.clone(),
            cookie: None,
        };
        async move { p.sign_in("evil", &rowbit_web::new_password()).await }
    };
    let (a, b) = tokio::join!(guess(&page), guess(&page));
    assert_eq!((a, b), (StatusCode::FORBIDDEN, StatusCode::FORBIDDEN));
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

on_both!(shadowed_changes_are_listed_apart);
async fn shadowed_changes_are_listed_apart(var: &str, table: &str) {
    let Some(url) = url(var) else { return };
    let t = format!("{table}_sh");
    let mut conn = mysql_async::Conn::from_url(&url).await.unwrap();
    conn.query_drop(format!(
        "DROP TABLE IF EXISTS {t}; CREATE TABLE {t} (id INT PRIMARY KEY, status VARCHAR(40) NOT NULL) ENGINE=InnoDB; \
         INSERT INTO {t} VALUES (1, 'approved'), (2, 'approved')"
    ))
    .await
    .unwrap();
    conn.disconnect().await.unwrap();
    let engine = Arc::new(Engine::new(
        MySqlAdapter::new(&url).unwrap(),
        PolicyFile::from_yaml("version: 0\nagents:\n  refund-bot:\n    mode: shadow\n").unwrap(),
        Store::open_in_memory().unwrap(),
    ));
    let mut page = Page::new(engine.clone());
    page.sign_in("philip", password()).await;
    engine
        .propose(
            "refund-bot",
            "rowbit_test",
            &format!("UPDATE {t} SET status = 'x' WHERE id = 1"),
        )
        .await
        .unwrap();
    let (_, all) = page.api("/api/changes").await;
    assert_eq!(all["changes"].as_array().unwrap().len(), 1);
    let (_, shadowed) = page.api("/api/changes?shadowed=true").await;
    let list = shadowed["changes"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{shadowed}");
    assert_eq!(list[0]["state"], "applied");
    assert_eq!(
        list[0]["shadowed"],
        json!({ "by": "policy:default", "would": "would hold: no rule matched" })
    );
}
