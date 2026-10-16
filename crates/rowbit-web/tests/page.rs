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
}

impl Page {
    async fn get(&self, host: &str) -> (StatusCode, String) {
        let request = Request::get("/")
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap();
        self.send(request).await
    }

    async fn post(&self, path: &str, form: &str, origin: Option<&str>) -> (StatusCode, String) {
        let mut request = Request::post(path)
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
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
    let router = rowbit_web::router(engine.clone(), "philip", vec![HOST.to_owned()]);
    Some((Page { router, engine }, url, table.to_owned()))
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
    let page = Page {
        router: rowbit_web::router(engine.clone(), "philip", vec![HOST.to_owned()]),
        engine,
    };
    let (status, _) = page.get("rebind.evil.example:8080").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, html) = page.get(HOST).await;
    assert_eq!(status, StatusCode::OK, "{html}");
}
