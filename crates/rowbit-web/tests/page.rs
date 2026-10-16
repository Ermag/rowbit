//! The review page through its router, as a browser would use it, against
//! the test databases from `compose.yaml`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use mysql_async::prelude::Queryable;
use rowbit_engine::Engine;
use rowbit_mysql::MySqlAdapter;
use rowbit_policy::PolicyFile;
use rowbit_store::Store;
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
            router: rowbit_web::router(engine.clone(), hash, vec![HOST.to_owned()]),
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

    async fn post(&self, path: &str, form: &str, origin: Option<&str>) -> (StatusCode, String) {
        let mut request = Request::post(path)
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        if let Some(cookie) = &self.cookie {
            request = request.header(header::COOKIE, cookie);
        }
        self.send(request.body(Body::from(form.to_owned())).unwrap())
            .await
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

/// The value of the first `name="..." value="..."` hidden field.
fn field(html: &str, name: &str) -> String {
    let marker = format!("name={name} value=\"");
    let start = html.find(&marker).expect(name) + marker.len();
    html[start..start + html[start..].find('"').unwrap()].to_owned()
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

on_both!(approve_from_the_page_then_undo);
async fn approve_from_the_page_then_undo(var: &str, table: &str) {
    let Some((page, url, t)) = setup(var, &format!("{table}_a")).await else {
        return;
    };
    let id = page
        .engine
        .propose(
            "refund-bot",
            "rowbit_test",
            &format!("UPDATE {t} SET status = '<b>refunded</b>'"),
        )
        .await
        .unwrap()
        .id;

    let (status, html) = page.get(HOST).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("Waiting for a person (1)"), "{html}");
    // Everything the agent wrote is escaped.
    assert!(html.contains("&lt;b&gt;refunded&lt;/b&gt;"), "{html}");
    assert!(!html.contains("<b>refunded</b>"), "{html}");
    let token = field(&html, "token");
    let review = field(&html, "review");

    let approve = format!("/changes/{id}/approve");
    // No token, a wrong token, or another site's form: refused.
    let (status, _) = page.post(&approve, &format!("review={review}"), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = page
        .post(
            &approve,
            &format!("token={}&review={review}", "0".repeat(64)),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = page
        .post(
            &approve,
            &format!("token={token}&review={review}"),
            Some("https://evil.example"),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // A review hash that is not the stored one.
    let (status, _) = page
        .post(
            &approve,
            &format!("token={token}&review={}", "0".repeat(64)),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(statuses(&url, &t).await, ["approved", "approved"]);

    let (status, _) = page
        .post(
            &approve,
            &format!("token={token}&review={review}"),
            Some(&format!("http://{HOST}")),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        statuses(&url, &t).await,
        ["<b>refunded</b>", "<b>refunded</b>"]
    );

    let (_, html) = page.get(HOST).await;
    assert!(html.contains("Waiting for a person (0)"), "{html}");
    assert!(html.contains(&format!("/changes/{id}/undo")), "{html}");
    let (status, _) = page
        .post(
            &format!("/changes/{id}/undo"),
            &format!("token={token}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(statuses(&url, &t).await, ["approved", "approved"]);

    let history = page.engine.history(id).unwrap();
    assert!(history.iter().any(|e| e.actor == "human:philip"));
}

on_both!(reject_from_the_page);
async fn reject_from_the_page(var: &str, table: &str) {
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
    let (_, html) = page.get(HOST).await;
    let token = field(&html, "token");
    let reject = format!("/changes/{id}/reject");
    let (status, _) = page
        .post(&reject, &format!("token={token}&reason=+"), None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = page
        .post(&reject, &format!("token={token}&reason=not+today"), None)
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
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
async fn other_hosts_are_refused() {
    let engine = Arc::new(Engine::new(
        MySqlAdapter::new("mysql://nobody@127.0.0.1:1/none").unwrap(),
        PolicyFile::from_yaml(POLICY).unwrap(),
        Store::open_in_memory().unwrap(),
    ));
    let mut page = Page::new(engine);
    let (status, _) = page.get("rebind.evil.example:8080").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Signed out, the queue redirects to sign-in and shows nothing.
    let (status, html) = page.get(HOST).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
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
    let (status, html) = page.get(HOST).await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Signed in as philip"), "{html}");
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
    let (_, html) = reviewer.get(HOST).await;
    let token = field(&html, "token");
    let review = field(&html, "review");
    let form = format!("token={token}&review={review}");

    // An attacker on the same host: no cookie, even with a stolen token.
    let mut attacker = Page {
        router: reviewer.router.clone(),
        engine: reviewer.engine.clone(),
        cookie: None,
    };
    let (status, _) = attacker.get(HOST).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let (status, _) = attacker
        .post(&format!("/changes/{id}/approve"), &form, None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // A made-up cookie.
    attacker.cookie = Some(format!("rowbit_session={}", "a".repeat(64)));
    let (status, _) = attacker
        .post(&format!("/changes/{id}/approve"), &form, None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
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
        .post(&format!("/changes/{id}/approve"), &form, None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    assert_eq!(statuses(&url, &t).await, ["approved", "approved"]);
    let (status, _) = reviewer
        .post(&format!("/changes/{id}/approve"), &form, None)
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        reviewer
            .engine
            .history(id)
            .unwrap()
            .iter()
            .any(|e| e.event == "Approve" && e.actor == "human:philip")
    );
}

on_both!(big_changes_show_every_row_before_approval);
async fn big_changes_show_every_row_before_approval(var: &str, table: &str) {
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
    let (_, html) = page.get(HOST).await;
    assert!(html.contains("Approve and apply all 60 rows"), "{html}");
    assert!(
        html.contains(&format!("<a href=\"/changes/{id}\">See all 60</a>")),
        "{html}"
    );
    assert!(!html.contains("<td>60</td><td>status</td>"), "{html}");
    let (status, all) = page.get_path(HOST, &format!("/changes/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(all.contains("<td>60</td><td>status</td>"), "{all}");
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
