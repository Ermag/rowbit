//! Rowbit's review page: one plain HTML page that lists held changes with
//! their rows before and after, and lets a person approve or reject them,
//! and undo applied ones. No JavaScript.
//!
//! Reviewers sign in with the page's password, which `rowbit serve` prints
//! once when it first starts (or which the operator sets). Sign-in gives an
//! `HttpOnly` session cookie that carries the reviewer's name; every action
//! needs that cookie and the session's form token. Being able to reach the
//! port is not enough, so another user or an agent on the same host can't
//! approve. The page also answers only requests addressed to the host it
//! listens on (against DNS rebinding) and refuses forms from other origins.
//!
//! Sessions live in memory and end when the process stops. A shared
//! password with a typed name is the v0.1 stand-in for sign-in with an
//! identity provider (OIDC), planned for v0.2.

use std::collections::HashMap;
use std::fmt::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
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

/// Rows shown per change on the queue before the rest is linked.
const SHOWN_ROWS: usize = 50;
/// Recent changes shown below the held queue.
const RECENT: usize = 20;
/// How long a sign-in lasts.
const SESSION: Duration = Duration::from_secs(12 * 60 * 60);
const COOKIE: &str = "rowbit_session";

#[derive(Clone)]
struct App {
    engine: Arc<Engine>,
    hosts: Arc<Vec<String>>,
    password: Arc<String>,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    /// Wrong passwords in a row, across all clients. Held for the whole of
    /// a sign-in, so passwords are checked one at a time and each wrong one
    /// waits longer: the wait caps the guessing rate, and only one Argon2
    /// check uses memory at a time.
    failures: Arc<tokio::sync::Mutex<u32>>,
}

#[derive(Clone)]
struct Session {
    reviewer: String,
    /// Required in every form posted in this session.
    token: String,
    expires: Instant,
}

/// The review page as a router. `password_hash` is from [`hash_password`].
/// `hosts` are the `Host` header values it answers to, such as
/// `127.0.0.1:8080`.
pub fn router(engine: Arc<Engine>, password_hash: String, hosts: Vec<String>) -> Router {
    let app = App {
        engine,
        hosts: Arc::new(hosts),
        password: Arc::new(password_hash),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        failures: Arc::new(tokio::sync::Mutex::new(0)),
    };
    Router::new()
        .route("/", get(index))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route("/changes/{id}", get(change_page))
        .route("/changes/{id}/approve", post(approve))
        .route("/changes/{id}/reject", post(reject))
        .route("/changes/{id}/undo", post(undo))
        .layer(axum::middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

/// Serves the page on `addr` until the process stops.
pub async fn serve(
    engine: Arc<Engine>,
    password_hash: String,
    addr: SocketAddr,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    let mut hosts = vec![local.to_string()];
    if local.ip().is_loopback() {
        hosts.push(format!("localhost:{}", local.port()));
    }
    axum::serve(listener, router(engine, password_hash, hosts)).await
}

/// A random password for the page: 24 characters from an alphabet without
/// look-alikes, about 119 bits.
pub fn new_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    // Bytes from 248 up are dropped, so every letter is equally likely.
    let limit = 256 - 256 % ALPHABET.len();
    let mut chars = Vec::with_capacity(24);
    while chars.len() < 24 {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("the system has a random source");
        chars.extend(
            bytes
                .iter()
                .filter(|b| (**b as usize) < limit)
                .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char),
        );
    }
    chars.truncate(24);
    chars
        .chunks(6)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// An Argon2id hash of `password`, to store instead of the password.
pub fn hash_password(password: &str) -> String {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).expect("the system has a random source");
    let salt = SaltString::encode_b64(&salt).expect("16 bytes is a valid salt");
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .expect("Argon2 with default parameters hashes any password")
        .to_string()
}

fn password_matches(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
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
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The signed-in session, from the cookie.
fn session(app: &App, headers: &HeaderMap) -> Option<Session> {
    let id = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            pair.trim()
                .strip_prefix(&format!("{COOKIE}="))
                .map(str::to_owned)
        })?;
    let mut sessions = app.sessions.lock().unwrap_or_else(|p| p.into_inner());
    sessions.retain(|_, s| s.expires > Instant::now());
    sessions.get(&id).cloned()
}

fn same_origin(app: &App, headers: &HeaderMap) -> bool {
    match headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
        None => true,
        Some(origin) => app
            .hosts
            .iter()
            .any(|h| origin == format!("http://{h}") || origin == format!("https://{h}")),
    }
}

/// A posted form that isn't from a signed-in session of this page.
struct Refused;

impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        (
            StatusCode::FORBIDDEN,
            "form refused; sign in and reload the page",
        )
            .into_response()
    }
}

/// The session for a posted form: signed in, same origin, and the
/// session's own token.
fn form_session(app: &App, headers: &HeaderMap, token: &str) -> Result<Session, Refused> {
    let session = session(app, headers).ok_or(Refused)?;
    if !same_origin(app, headers) || !constant_time_eq(token.as_bytes(), session.token.as_bytes()) {
        return Err(Refused);
    }
    Ok(session)
}

#[derive(Deserialize)]
struct LoginForm {
    name: String,
    password: String,
}

async fn login_page() -> Response {
    Html(page(
        "Sign in to Rowbit",
        "<h1>Sign in</h1>\
         <form method=post action=\"/login\" class=login>\
           <label>Your name <input name=name autocomplete=username required maxlength=64></label>\
           <label>Page password <input name=password type=password autocomplete=current-password required></label>\
           <button class=approve>Sign in</button></form>\
         <p class=muted>The password was printed when <code>rowbit serve</code> first started, \
         or set with <code>rowbit serve --set-password</code>.</p>",
    ))
    .into_response()
}

async fn login(
    State(app): State<App>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    if !same_origin(&app, &headers) {
        return (StatusCode::FORBIDDEN, "sign-in refused").into_response();
    }
    let name: String = form
        .name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    // One sign-in at a time, wait included.
    let mut failures = app.failures.lock().await;
    let password = Arc::clone(&app.password);
    let given = form.password;
    let ok = tokio::task::spawn_blocking(move || password_matches(&password, &given))
        .await
        .unwrap_or(false);
    if !ok || name.is_empty() {
        // Each wrong try in a row waits longer, up to 8 seconds, and no
        // other sign-in is checked meanwhile.
        *failures = failures.saturating_add(1);
        tokio::time::sleep(Duration::from_secs(1 << (*failures - 1).min(3))).await;
        return (
            StatusCode::FORBIDDEN,
            Html(page(
                "Not signed in",
                "<p class=error>Wrong password, or no name.</p><p><a href=\"/login\">Try again</a></p>",
            )),
        )
            .into_response();
    }
    *failures = 0;
    drop(failures);
    let id = random_token();
    let session = Session {
        reviewer: name,
        token: random_token(),
        expires: Instant::now() + SESSION,
    };
    app.sessions
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id.clone(), session);
    let cookie = format!(
        "{COOKIE}={id}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        SESSION.as_secs()
    );
    let mut response = Redirect::to("/").into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

#[derive(Deserialize)]
struct TokenForm {
    token: String,
}

async fn logout(
    State(app): State<App>,
    headers: HeaderMap,
    Form(form): Form<TokenForm>,
) -> Response {
    if form_session(&app, &headers, &form.token).is_ok() {
        let token = form.token;
        app.sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|_, s| s.token != token);
    }
    let mut response = Redirect::to("/login").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("rowbit_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"),
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

async fn approve(
    State(app): State<App>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<ApproveForm>,
) -> Response {
    let session = match form_session(&app, &headers, &form.token) {
        Ok(s) => s,
        Err(refused) => return refused.into_response(),
    };
    let result = app
        .engine
        .approve(&session.reviewer, id, &form.review)
        .await;
    done(result.map(|_| ()).map_err(|e| e.to_string()))
}

async fn reject(
    State(app): State<App>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<RejectForm>,
) -> Response {
    let session = match form_session(&app, &headers, &form.token) {
        Ok(s) => s,
        Err(refused) => return refused.into_response(),
    };
    let reason = form.reason.trim();
    if reason.is_empty() {
        return done(Err("give a reason".to_owned()));
    }
    let result = app.engine.reject(&session.reviewer, id, reason);
    done(result.map(|_| ()).map_err(|e| e.to_string()))
}

async fn undo(
    State(app): State<App>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Form(form): Form<TokenForm>,
) -> Response {
    let session = match form_session(&app, &headers, &form.token) {
        Ok(s) => s,
        Err(refused) => return refused.into_response(),
    };
    let result = app.engine.undo(&session.reviewer, id).await;
    done(result.map(|_| ()).map_err(|e| e.to_string()))
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

/// One change with every row, for changes too big for the queue.
async fn change_page(State(app): State<App>, Path(id): Path<i64>, headers: HeaderMap) -> Response {
    let Some(session) = session(&app, &headers) else {
        return Redirect::to("/login").into_response();
    };
    match app.engine.change(id) {
        Ok(change) => Html(page(
            &format!("Change {id}"),
            &format!(
                "<p><a href=\"/\">Back to the queue</a></p>{}",
                card(&app, &session, &change, true)
            ),
        ))
        .into_response(),
        Err(e) => (StatusCode::NOT_FOUND, esc(&e.to_string())).into_response(),
    }
}

async fn index(State(app): State<App>, headers: HeaderMap) -> Response {
    let Some(session) = session(&app, &headers) else {
        return Redirect::to("/login").into_response();
    };
    match render_index(&app, &session) {
        Ok(body) => Html(page("Rowbit review", &body)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, esc(&e)).into_response(),
    }
}

fn render_index(app: &App, session: &Session) -> Result<String, String> {
    let engine = &app.engine;
    let mut out = String::new();
    let held = engine.held().map_err(|e| e.to_string())?;
    let _ = write!(
        out,
        "<h1>Waiting for a person ({})</h1>\
         <form method=post action=\"/logout\" class=muted>Signed in as {}. {}<button>Sign out</button></form>",
        held.len(),
        esc(&session.reviewer),
        token_field(session)
    );
    if held.is_empty() {
        out.push_str("<p class=muted>Nothing is waiting.</p>");
    }
    for id in &held {
        let change = engine.change(*id).map_err(|e| e.to_string())?;
        out.push_str(&card(app, session, &change, false));
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
                token_field(session)
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

/// One change: what the agent sent, what it would do, and, while it is
/// held, the forms. `all` shows every row instead of the first
/// [`SHOWN_ROWS`].
fn card(app: &App, session: &Session, change: &Change, all: bool) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "<section><h3>#{} from {}</h3><pre>{}</pre>",
        change.id,
        esc(&change.agent),
        esc(&statement(&change.sql))
    );
    match &change.review {
        Some(Review::Rows(p)) => out.push_str(&rows(p, change.id, all)),
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
    if change.state != ChangeState::Held {
        out.push_str("</section>");
        return out;
    }
    let id = change.id;
    let review = change.review_hash.as_deref().unwrap_or_default();
    // The button names what an approval covers: every row, not the ones shown.
    let approve = match &change.review {
        Some(Review::Rows(p)) if p.rows.len() == 1 => "Approve and apply 1 row".to_owned(),
        Some(Review::Rows(p)) => format!("Approve and apply all {} rows", p.rows.len()),
        _ => "Approve and run".to_owned(),
    };
    let _ = write!(
        out,
        "<div class=actions>\
         <form method=post action=\"/changes/{id}/approve\">{token}\
           <input type=hidden name=review value=\"{review}\">\
           <button class=approve>{approve}</button></form>\
         <form method=post action=\"/changes/{id}/reject\">{token}\
           <input name=reason placeholder=\"Why reject?\" required>\
           <button>Reject</button></form>\
         </div></section>",
        token = token_field(session),
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

fn rows(p: &Preview, id: i64, all: bool) -> String {
    let mut out = String::new();
    let verb = match p.kind {
        WriteKind::Insert => "inserts",
        WriteKind::Update => "updates",
        WriteKind::Delete => "deletes",
    };
    let _ = write!(
        out,
        "<p>{verb} {} in <b>{}</b>:</p><table><tr><th>Row</th><th>Column</th><th>Before</th><th>After</th></tr>",
        if p.rows.len() == 1 {
            "1 row".to_owned()
        } else {
            format!("{} rows", p.rows.len())
        },
        esc(&p.table.qualified_name())
    );
    let shown = if all { p.rows.len() } else { SHOWN_ROWS };
    for row in p.rows.iter().take(shown) {
        let key = match p.key(row) {
            Some(key) => key.iter().map(cell_html).collect::<Vec<_>>().join(", "),
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
                key,
                esc(&column.name),
                before.map(cell_html).unwrap_or_default(),
                after.map(cell_html).unwrap_or_default(),
            );
        }
    }
    out.push_str("</table>");
    if p.rows.len() > shown {
        let _ = write!(
            out,
            "<p class=muted>... and {} more rows. <a href=\"/changes/{id}\">See all {}</a> before approving.</p>",
            p.rows.len() - shown,
            p.rows.len()
        );
    }
    out
}

fn token_field(session: &Session) -> String {
    format!("<input type=hidden name=token value=\"{}\">", session.token)
}

/// A value as HTML that can't be mistaken for another: SQL NULL is marked
/// up apart from the text "NULL", text is quoted (so '' and ' x ' show),
/// numbers are plain, and bytes that aren't text are hex.
fn cell_html(cell: &Cell) -> String {
    match cell {
        None => "<i class=null>NULL</i>".to_owned(),
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) if !text.is_empty() && text.parse::<f64>().is_ok() => esc(text),
            Ok(text) => format!("<span class=text>&quot;{}&quot;</span>", esc(text)),
            Err(_) => {
                let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                format!("<span class=bytes>0x{hex}</span>")
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
td.before{color:#a03030}td.after{color:#17794a}td{white-space:pre-wrap}i.null{color:#59627a}.muted{color:#59627a}.error{color:#a03030}\
.actions{display:flex;gap:12px;flex-wrap:wrap}.actions form{display:flex;gap:6px}\
form.login{display:flex;flex-direction:column;gap:10px;max-width:320px}form.login input{font:inherit;padding:4px}\
button{font:inherit;padding:4px 12px;cursor:pointer}button.approve{background:#2b47c9;color:#fff;border:0;border-radius:4px}\
@media (prefers-color-scheme:dark){body{background:#0d111c;color:#e6e9f2}section{background:#151b2b;border-color:#283149}pre{background:#0d111c}th,td{border-color:#283149}.muted{color:#9aa3bb}}";

/// The statement as Rowbit understood it, without the agent's comments
/// (which could say anything, such as "reviewed, safe"), or the agent's
/// text if it never parsed.
fn statement(sql: &str) -> String {
    rowbit_mysql::statement::classify(sql)
        .map(|c| c.canonical)
        .unwrap_or_else(|_| sql.to_owned())
}

/// Escapes text for HTML content and double-quoted attributes. Hidden
/// characters (see [`rowbit_core::text::is_hidden`]) other than newlines
/// are shown as `\u{..}`, so agent text can't hide, reorder or smuggle
/// anything past a reviewer.
fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if rowbit_core::text::is_hidden(c) && c != '\n' {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
            continue;
        }
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
    fn hidden_characters_show() {
        assert_eq!(
            esc("a\u{202e}b\u{200b}c\u{1b}"),
            "a\\u{202e}b\\u{200b}c\\u{1b}"
        );
        // Found in review 4.
        assert_eq!(
            esc("ok\u{61c}X\u{2028}Y\u{ad}Z\u{e0041}"),
            "ok\\u{61c}X\\u{2028}Y\\u{ad}Z\\u{e0041}"
        );
        assert_eq!(
            statement("UPDATE t SET a = 1 /* reviewed, safe */ WHERE id = 2"),
            "UPDATE t SET a = 1 WHERE id = 2"
        );
    }

    // Found in review 4: the text 'NULL' looked like NULL, '' like nothing.
    #[test]
    fn values_cannot_pass_for_other_values() {
        let text = |s: &str| Some(s.as_bytes().to_vec());
        assert_eq!(cell_html(&None), "<i class=null>NULL</i>");
        assert_eq!(
            cell_html(&text("NULL")),
            "<span class=text>&quot;NULL&quot;</span>"
        );
        assert_eq!(cell_html(&text("")), "<span class=text>&quot;&quot;</span>");
        assert_eq!(
            cell_html(&text(" x ")),
            "<span class=text>&quot; x &quot;</span>"
        );
        assert_eq!(cell_html(&text("12.50")), "12.50");
        assert_eq!(
            cell_html(&Some(vec![0xff])),
            "<span class=bytes>0xff</span>"
        );
    }

    #[test]
    fn escapes_everything_that_matters() {
        assert_eq!(
            esc(r#"<script>"x" & 'y'</script>"#),
            "&lt;script&gt;&quot;x&quot; &amp; &#39;y&#39;&lt;/script&gt;"
        );
    }

    #[test]
    fn passwords_are_random_and_only_their_hash_matches() {
        let p = new_password();
        assert_eq!(p.len(), 24 + 3);
        assert_ne!(p, new_password());
        let hash = hash_password(&p);
        assert!(hash.starts_with("$argon2id$"), "{hash}");
        assert!(!hash.contains(&p));
        assert!(password_matches(&hash, &p));
        assert!(!password_matches(&hash, &new_password()));
        assert!(!password_matches("not a hash", &p));
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
