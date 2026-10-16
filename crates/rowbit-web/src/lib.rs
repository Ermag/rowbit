//! Rowbit's review UI: a Svelte app, built into `dist/` and embedded in the
//! binary, that lists held changes with their rows before and after, lets a
//! person approve or reject them and undo applied ones, and shows the log.
//! It talks to the JSON API in [`api`]. The sign-in pages are plain HTML.
//!
//! Reviewers sign in through an OpenID Connect provider ([`oidc`]), or, on a
//! loopback address only, with the page's password, which `rowbit serve`
//! prints once when it first starts (or which the operator sets). Either
//! gives the same `HttpOnly` session cookie that carries the reviewer's
//! identity; every action needs that cookie and the session's form token. Being able to reach the
//! port is not enough, so another user or an agent on the same host can't
//! approve. The page also answers only requests addressed to the host it
//! listens on (against DNS rebinding) and refuses forms from other origins.
//!
//! Sessions live in memory and end when the process stops.

mod api;
pub mod config;
pub mod notify;
pub mod oidc;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::Router;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use rowbit_engine::Engine;
use serde::Deserialize;

/// The built UI. `web/` holds its source; CI checks these match a build.
const INDEX_HTML: &str = include_str!("../dist/index.html");
const APP_JS: &str = include_str!("../dist/assets/app.js");
const APP_CSS: &str = include_str!("../dist/assets/app.css");
/// For the UI: its own scripts, styles and API, nothing inline, no eval.
const UI_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
     img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";
/// For the plain HTML sign-in pages.
const PAGE_CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'";
/// How long a sign-in lasts.
const SESSION: Duration = Duration::from_secs(12 * 60 * 60);
const COOKIE: &str = "rowbit_session";
/// Brings the browser back from the provider to the login it started.
const LOGIN_COOKIE: &str = "rowbit_login";

/// How reviewers sign in.
pub enum SignIn {
    /// The page's password and a typed name: for one machine only.
    Password { hash: String },
    /// An OpenID Connect provider.
    Oidc(Box<oidc::OidcLogin>),
}

/// What the review page needs besides the engine.
pub struct Web {
    pub sign_in: SignIn,
    /// The `Host` header values it answers to, such as `127.0.0.1:8080`.
    pub hosts: Vec<String>,
    /// Mark cookies `Secure`, behind HTTPS.
    pub secure: bool,
    /// Serve agents over MCP at `/mcp`, with this default database.
    pub mcp: Option<String>,
}

#[derive(Clone)]
struct App {
    engine: Arc<Engine>,
    hosts: Arc<Vec<String>>,
    sign_in: Arc<SignIn>,
    secure: bool,
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

/// The review page as a router.
pub fn router(engine: Arc<Engine>, web: Web) -> Router {
    let mcp = web.mcp.clone().map(|database| {
        let origins = web
            .hosts
            .iter()
            .flat_map(|h| [format!("http://{h}"), format!("https://{h}")])
            .collect();
        rowbit_mcp::RowbitMcp::http_service(engine.clone(), database, web.hosts.clone(), origins)
    });
    let app = App {
        engine,
        hosts: Arc::new(web.hosts),
        sign_in: Arc::new(web.sign_in),
        secure: web.secure,
        sessions: Arc::new(Mutex::new(HashMap::new())),
        failures: Arc::new(tokio::sync::Mutex::new(0)),
    };
    Router::new()
        .route("/", get(index))
        .route("/assets/app.js", get(app_js))
        .route("/assets/app.css", get(app_css))
        .route("/login", get(login_page).post(login))
        .route("/auth/start", get(auth_start))
        .route("/auth/callback", get(auth_callback))
        .route("/api/session", get(api::me))
        .route("/api/logout", post(api::logout))
        .route("/api/changes", get(api::list))
        .route("/api/changes/{id}", get(api::detail))
        .route("/api/changes/{id}/approve", post(api::approve))
        .route("/api/changes/{id}/reject", post(api::reject))
        .route("/api/changes/{id}/undo", post(api::undo))
        .route("/api/verify", get(api::verify))
        .route("/api/deliveries", get(api::deliveries))
        .merge(match mcp {
            // Every request signs in with its agent's token first.
            Some(service) => Router::new().nest_service(
                "/mcp",
                tower::ServiceBuilder::new()
                    .layer(axum::middleware::from_fn_with_state(
                        app.clone(),
                        mcp_sign_in,
                    ))
                    .service(service),
            ),
            None => Router::new(),
        })
        .layer(axum::middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

/// Serves the page on `addr` until the process stops. `web.hosts` gains
/// the address it listens on.
pub async fn serve(engine: Arc<Engine>, mut web: Web, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    web.hosts.push(local.to_string());
    if local.ip().is_loopback() {
        web.hosts.push(format!("localhost:{}", local.port()));
    }
    axum::serve(listener, router(engine, web)).await
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
    if !headers.contains_key(header::CONTENT_SECURITY_POLICY) {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(PAGE_CSP),
        );
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // Not `no-referrer`: with it, browsers send `Origin: null` on our own
    // forms, and the origin check refuses every sign-in and button.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Signs an MCP request in by its `Authorization: Bearer` token, as the
/// agent the token belongs to. Anything else gets `401` and nothing more.
async fn mcp_sign_in(
    State(app): State<App>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let agent = match token {
        Some(t) if !t.is_empty() => app.engine.agent_for_token(t).ok().flatten(),
        _ => None,
    };
    match agent {
        Some(agent) => {
            request
                .extensions_mut()
                .insert(rowbit_mcp::SignedInAgent(agent));
            next.run(request).await
        }
        None => {
            // Tokens can't be guessed; the wait just keeps a flood slow.
            tokio::time::sleep(Duration::from_millis(500)).await;
            let mut r =
                (StatusCode::UNAUTHORIZED, "sign in with your agent's token").into_response();
            r.headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            r
        }
    }
}

/// The signed-in session, from the cookie.
fn session(app: &App, headers: &HeaderMap) -> Option<Session> {
    let id = cookie(headers, COOKIE)?;
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

#[derive(Deserialize)]
struct LoginForm {
    name: String,
    password: String,
}

async fn login_page(State(app): State<App>) -> Response {
    if let SignIn::Oidc(login) = &*app.sign_in {
        return Html(page(
            "Sign in to Rowbit",
            &format!(
                "<h1>Sign in</h1><p><a class=button href=\"/auth/start\">Sign in with {}</a></p>",
                esc(&login.provider)
            ),
        ))
        .into_response();
    }
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
    let SignIn::Password { hash } = &*app.sign_in else {
        return (
            StatusCode::NOT_FOUND,
            "sign in through the identity provider",
        )
            .into_response();
    };
    let name: String = form
        .name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    // One sign-in at a time, wait included.
    let mut failures = app.failures.lock().await;
    let password = hash.clone();
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
    let mut response = Redirect::to("/").into_response();
    start_session(&app, name, &mut response);
    response
}

/// Signs `reviewer` in: a new session, and its cookie on `response`.
fn start_session(app: &App, reviewer: String, response: &mut Response) {
    let id = random_token();
    let session = Session {
        reviewer,
        token: random_token(),
        expires: Instant::now() + SESSION,
    };
    app.sessions
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(id.clone(), session);
    let cookie = format!(
        "{COOKIE}={id}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{}",
        SESSION.as_secs(),
        if app.secure { "; Secure" } else { "" }
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|pair| {
            pair.trim()
                .strip_prefix(name)
                .and_then(|r| r.strip_prefix('='))
                .map(str::to_owned)
        })
}

/// Sends the browser to the provider, with a cookie that brings it back to
/// this login. `SameSite=Lax`, since coming back is a cross-site navigation.
async fn auth_start(State(app): State<App>) -> Response {
    let SignIn::Oidc(login) = &*app.sign_in else {
        return Redirect::to("/login").into_response();
    };
    let id = random_token();
    let url = login.start(id.clone());
    let mut response = Redirect::to(&url).into_response();
    let cookie = format!(
        "{LOGIN_COOKIE}={id}; HttpOnly; SameSite=Lax; Path=/auth; Max-Age={}{}",
        oidc::LOGIN_WINDOW.as_secs(),
        if app.secure { "; Secure" } else { "" }
    );
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

#[derive(Deserialize)]
struct Callback {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

/// Where the provider sends the browser back. On success the session
/// starts, and a page of our own moves on to the queue: the browser
/// doesn't send a `SameSite=Strict` cookie on a redirect that began at
/// another site, but does on a navigation this page starts.
async fn auth_callback(
    State(app): State<App>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<Callback>,
) -> Response {
    let SignIn::Oidc(login) = &*app.sign_in else {
        return Redirect::to("/login").into_response();
    };
    let refused = |why: &str| {
        let mut r = (
            StatusCode::FORBIDDEN,
            Html(page(
                "Not signed in",
                &format!(
                    "<p class=error>Sign-in refused: {}</p><p><a href=\"/login\">Start again</a></p>",
                    esc(why)
                ),
            )),
        )
            .into_response();
        r.headers_mut().insert(
            header::SET_COOKIE,
            HeaderValue::from_static(
                "rowbit_login=; HttpOnly; SameSite=Lax; Path=/auth; Max-Age=0",
            ),
        );
        r
    };
    if let Some(error) = q.error {
        let error: String = error.chars().take(100).collect();
        return refused(&format!("the provider said {error}"));
    }
    let (Some(state), Some(code), Some(id)) = (q.state, q.code, cookie(&headers, LOGIN_COOKIE))
    else {
        return refused("this sign-in didn't start here, or took too long; start again");
    };
    match login.finish(&id, &state, &code).await {
        Err(why) => refused(&why),
        Ok(reviewer) => {
            let mut response = Html(page(
                "Signed in",
                "<meta http-equiv=refresh content=\"0;url=/\"><p>Signed in. <a href=\"/\">Continue</a></p>",
            ))
            .into_response();
            response.headers_mut().insert(
                header::SET_COOKIE,
                HeaderValue::from_static(
                    "rowbit_login=; HttpOnly; SameSite=Lax; Path=/auth; Max-Age=0",
                ),
            );
            start_session(&app, reviewer.email, &mut response);
            response
        }
    }
}

/// The UI, for a signed-in person; the sign-in page for anyone else.
async fn index(State(app): State<App>, headers: HeaderMap) -> Response {
    if session(&app, &headers).is_none() {
        return Redirect::to("/login").into_response();
    }
    let mut r = Html(INDEX_HTML).into_response();
    r.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(UI_CSP),
    );
    r
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    let mut r = body.into_response();
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    r.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(UI_CSP),
    );
    r
}

async fn app_js() -> Response {
    asset("text/javascript; charset=utf-8", APP_JS)
}

async fn app_css() -> Response {
    asset("text/css; charset=utf-8", APP_CSS)
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
button{font:inherit;padding:4px 12px;cursor:pointer}button.approve,a.button{background:#2b47c9;color:#fff;border:0;border-radius:4px}a.button{display:inline-block;padding:6px 14px;text-decoration:none}\
@media (prefers-color-scheme:dark){body{background:#0d111c;color:#e6e9f2}section{background:#151b2b;border-color:#283149}pre{background:#0d111c}th,td{border-color:#283149}.muted{color:#9aa3bb}}";

/// The statement as Rowbit understood it, without the agent's comments
/// (which could say anything, such as "reviewed, safe"), or the agent's
/// text if it never parsed.
fn statement(sql: &str) -> String {
    rowbit_mysql::statement::classify(sql)
        .map(|c| c.canonical)
        .unwrap_or_else(|_| sql.to_owned())
}

/// Text with hidden characters (see [`rowbit_core::text::is_hidden`])
/// other than newlines shown as `\u{..}`, so agent text can't hide,
/// reorder or smuggle anything past a reviewer.
pub(crate) fn visible(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if rowbit_core::text::is_hidden(c) && c != '\n' {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// Escapes text for HTML content and double-quoted attributes, after
/// making hidden characters [`visible`].
fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in visible(text).chars() {
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
