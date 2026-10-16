//! Rowbit's review page: one plain HTML page that lists held changes with
//! their rows before and after, and lets a person approve or reject them,
//! and undo applied ones. No JavaScript.
//!
//! v0.1 has no sign-in. The page acts as the reviewer named when it was
//! started, listens on localhost by default, refuses requests whose `Host`
//! is not one it serves (against DNS rebinding), and requires a per-process
//! token in every form (against cross-site requests).

use std::fmt::Write;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use rowbit_core::change::State as ChangeState;
use rowbit_core::preview::{Cell, Preview, WriteKind};
use rowbit_engine::Engine;
use rowbit_store::{Change, Review};
use serde::Deserialize;

/// Rows shown per change before the rest is summarised.
const SHOWN_ROWS: usize = 50;
/// Recent changes shown below the held queue.
const RECENT: usize = 20;

#[derive(Clone)]
struct App {
    engine: Arc<Engine>,
    reviewer: String,
    token: String,
    hosts: Arc<Vec<String>>,
}

/// The review page as a router, acting as `reviewer`. `hosts` are the
/// `Host` header values it answers to, such as `127.0.0.1:8080`.
pub fn router(engine: Arc<Engine>, reviewer: impl Into<String>, hosts: Vec<String>) -> Router {
    let app = App {
        engine,
        reviewer: reviewer.into(),
        token: random_token(),
        hosts: Arc::new(hosts),
    };
    Router::new()
        .route("/", get(index))
        .route("/changes/{id}/approve", post(approve))
        .route("/changes/{id}/reject", post(reject))
        .route("/changes/{id}/undo", post(undo))
        .layer(axum::middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

/// Serves the page on `addr` until the process stops.
pub async fn serve(
    engine: Arc<Engine>,
    reviewer: impl Into<String>,
    addr: SocketAddr,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let mut hosts = vec![local.to_string()];
    if local.ip().is_loopback() {
        hosts.push(format!("localhost:{}", local.port()));
    }
    axum::serve(listener, router(engine, reviewer, hosts)).await
}

/// Checks the `Host` header, and sets headers that keep the page from being
/// framed or running anything it did not mean to.
async fn guard(
    State(app): State<App>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    if !app.hosts.iter().any(|h| h == host) {
        return (StatusCode::FORBIDDEN, "unknown host").into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

#[derive(Deserialize)]
struct ApproveForm {
    token: String,
    review: String,
}

#[derive(Deserialize)]
struct RejectForm {
    token: String,
    reason: String,
}

#[derive(Deserialize)]
struct UndoForm {
    token: String,
}

async fn approve(
    State(app): State<App>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<ApproveForm>,
) -> Response {
    if let Some(refused) = check_form(&app, &headers, &form.token) {
        return refused;
    }
    let result = app.engine.approve(&app.reviewer, id, &form.review).await;
    done(result.map(|_| ()).map_err(|e| e.to_string()))
}

async fn reject(
    State(app): State<App>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<RejectForm>,
) -> Response {
    if let Some(refused) = check_form(&app, &headers, &form.token) {
        return refused;
    }
    let reason = form.reason.trim();
    if reason.is_empty() {
        return done(Err("give a reason".to_owned()));
    }
    let result = app.engine.reject(&app.reviewer, id, reason);
    done(result.map(|_| ()).map_err(|e| e.to_string()))
}

async fn undo(
    State(app): State<App>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<UndoForm>,
) -> Response {
    if let Some(refused) = check_form(&app, &headers, &form.token) {
        return refused;
    }
    let result = app.engine.undo(&app.reviewer, id).await;
    done(result.map(|_| ()).map_err(|e| e.to_string()))
}

/// Refuses a form without this process's token, or posted from another
/// origin.
fn check_form(app: &App, headers: &HeaderMap, token: &str) -> Option<Response> {
    let origin_ok = match headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
        None => true,
        Some(origin) => app
            .hosts
            .iter()
            .any(|h| origin == format!("http://{h}") || origin == format!("https://{h}")),
    };
    if !origin_ok || !constant_time_eq(token.as_bytes(), app.token.as_bytes()) {
        return Some((StatusCode::FORBIDDEN, "form refused; reload the page").into_response());
    }
    None
}

fn done(result: Result<(), String>) -> Response {
    match result {
        Ok(()) => Redirect::to("/").into_response(),
        Err(message) => (
            StatusCode::CONFLICT,
            Html(page(
                "Not done",
                &format!(
                    "<p class=error>{}</p><p><a href=\"/\">Back to the queue</a></p>",
                    esc(&message)
                ),
            )),
        )
            .into_response(),
    }
}

async fn index(State(app): State<App>) -> Response {
    match render_index(&app) {
        Ok(body) => Html(page("Rowbit review", &body)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, esc(&e)).into_response(),
    }
}

fn render_index(app: &App) -> Result<String, String> {
    let engine = &app.engine;
    let mut out = String::new();
    let held = engine.held().map_err(|e| e.to_string())?;
    let _ = write!(
        out,
        "<h1>Waiting for a person ({})</h1><p class=muted>Acting as {}.</p>",
        held.len(),
        esc(&app.reviewer)
    );
    if held.is_empty() {
        out.push_str("<p class=muted>Nothing is waiting.</p>");
    }
    for id in &held {
        let change = engine.change(*id).map_err(|e| e.to_string())?;
        out.push_str(&card(app, &change));
    }

    let interrupted = engine.interrupted().map_err(|e| e.to_string())?;
    if !interrupted.is_empty() {
        let _ = write!(
            out,
            "<p class=error>Changes {interrupted:?} were interrupted while applying or undoing. \
             Compare their rows with the preview before anything else.</p>"
        );
    }

    out.push_str("<h2>Recent changes</h2><table><tr><th>#</th><th>State</th><th>Agent</th><th>Table</th><th>Rows</th><th></th></tr>");
    let all = engine.list(None).map_err(|e| e.to_string())?;
    for id in all.iter().rev().take(RECENT) {
        let change = engine.change(*id).map_err(|e| e.to_string())?;
        let (table, rows) = match &change.review {
            Some(Review::Rows(p)) => (p.table.qualified_name(), p.rows.len().to_string()),
            Some(Review::Ddl { .. }) => ("schema change".to_owned(), String::new()),
            None => (String::new(), String::new()),
        };
        let action = if change.state == ChangeState::Applied
            && matches!(change.review, Some(Review::Rows(_)))
        {
            format!(
                "<form method=post action=\"/changes/{id}/undo\">{}<button>Undo</button></form>",
                token_field(app)
            )
        } else {
            String::new()
        };
        let _ = write!(
            out,
            "<tr><td>{id}</td><td>{}</td><td>{}</td><td>{}</td><td>{rows}</td><td>{action}</td></tr>",
            change.state.as_str(),
            esc(&change.agent),
            esc(&table),
        );
    }
    out.push_str("</table>");
    Ok(out)
}

/// One held change: what the agent sent, what it would do, and the forms.
fn card(app: &App, change: &Change) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "<section><h3>#{} from {}</h3><pre>{}</pre>",
        change.id,
        esc(&change.agent),
        esc(&change.sql)
    );
    match &change.review {
        Some(Review::Rows(p)) => out.push_str(&rows(p)),
        Some(Review::Ddl { sql }) => {
            let _ = write!(
                out,
                "<p>Runs this schema change, which cannot be undone:</p><pre>{}</pre>",
                esc(sql)
            );
        }
        None => {}
    }
    if let Some(reason) = held_reason(app, change.id) {
        let _ = write!(out, "<p class=muted>Held: {}</p>", esc(&reason));
    }
    let id = change.id;
    let review = change.review_hash.as_deref().unwrap_or_default();
    let _ = write!(
        out,
        "<div class=actions>\
         <form method=post action=\"/changes/{id}/approve\">{token}\
           <input type=hidden name=review value=\"{review}\">\
           <button class=approve>Approve and apply</button></form>\
         <form method=post action=\"/changes/{id}/reject\">{token}\
           <input name=reason placeholder=\"Why reject?\" required>\
           <button>Reject</button></form>\
         </div></section>",
        token = token_field(app),
        review = esc(review),
    );
    out
}

fn held_reason(app: &App, id: i64) -> Option<String> {
    app.engine
        .history(id)
        .ok()?
        .into_iter()
        .rev()
        .find(|e| e.event == "PolicyHeld")
        .and_then(|e| e.detail)
}

fn rows(p: &Preview) -> String {
    let mut out = String::new();
    let verb = match p.kind {
        WriteKind::Insert => "inserts",
        WriteKind::Update => "updates",
        WriteKind::Delete => "deletes",
    };
    let _ = write!(
        out,
        "<p>{verb} {} rows in <b>{}</b>:</p><table><tr><th>Row</th><th>Column</th><th>Before</th><th>After</th></tr>",
        p.rows.len(),
        esc(&p.table.qualified_name())
    );
    for row in p.rows.iter().take(SHOWN_ROWS) {
        let key = match p.key(row) {
            Some(key) => key.iter().map(show).collect::<Vec<_>>().join(", "),
            None => "new".to_owned(),
        };
        for (i, column) in p.table.columns.iter().enumerate() {
            let before = row.before.as_ref().map(|b| &b[i]);
            let after = row.after.as_ref().map(|a| &a[i]);
            if let (Some(b), Some(a)) = (before, after)
                && b == a
            {
                continue;
            }
            let _ = write!(
                out,
                "<tr><td>{}</td><td>{}</td><td class=before>{}</td><td class=after>{}</td></tr>",
                esc(&key),
                esc(&column.name),
                before.map(|c| esc(&show(c))).unwrap_or_default(),
                after.map(|c| esc(&show(c))).unwrap_or_default(),
            );
        }
    }
    out.push_str("</table>");
    if p.rows.len() > SHOWN_ROWS {
        let _ = write!(
            out,
            "<p class=muted>... and {} more rows. Use <code>rowbit show</code> for all of them.</p>",
            p.rows.len() - SHOWN_ROWS
        );
    }
    out
}

fn token_field(app: &App) -> String {
    format!("<input type=hidden name=token value=\"{}\">", app.token)
}

fn show(cell: &Cell) -> String {
    match cell {
        None => "NULL".to_owned(),
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => text.to_owned(),
            Err(_) => {
                let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                format!("0x{hex}")
            }
        },
    }
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{}</title><style>{STYLE}</style></head><body>{body}</body></html>",
        esc(title)
    )
}

const STYLE: &str = "\
body{font:15px/1.5 system-ui,sans-serif;max-width:960px;margin:24px auto;padding:0 16px;color:#141a2b;background:#f4f6fa}\
section{background:#fff;border:1px solid #d9deea;border-radius:8px;padding:16px;margin:16px 0}\
pre{white-space:pre-wrap;background:#f4f6fa;padding:8px;border-radius:4px}\
table{border-collapse:collapse;width:100%;margin:8px 0}th,td{text-align:left;padding:4px 8px;border-bottom:1px solid #d9deea;vertical-align:top}\
td.before{color:#a03030}td.after{color:#17794a}.muted{color:#59627a}.error{color:#a03030}\
.actions{display:flex;gap:12px;flex-wrap:wrap}.actions form{display:flex;gap:6px}\
button{font:inherit;padding:4px 12px;cursor:pointer}button.approve{background:#2b47c9;color:#fff;border:0;border-radius:4px}\
@media (prefers-color-scheme:dark){body{background:#0d111c;color:#e6e9f2}section{background:#151b2b;border-color:#283149}pre{background:#0d111c}th,td{border-color:#283149}.muted{color:#9aa3bb}}";

/// Escapes text for HTML content and double-quoted attributes.
fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the system has a random source");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_everything_that_matters() {
        assert_eq!(
            esc(r#"<script>"x" & 'y'</script>"#),
            "&lt;script&gt;&quot;x&quot; &amp; &#39;y&#39;&lt;/script&gt;"
        );
    }

    #[test]
    fn tokens_are_random_and_compared_whole() {
        let (a, b) = (random_token(), random_token());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(constant_time_eq(a.as_bytes(), a.as_bytes()));
        assert!(!constant_time_eq(a.as_bytes(), b.as_bytes()));
        assert!(!constant_time_eq(a.as_bytes(), &a.as_bytes()[..63]));
    }
}
