//! Signing in through an OpenID Connect provider, against a small provider
//! run by the test: discovery, keys, authorize and token endpoints. The
//! provider can be told to misbehave, to check that Rowbit refuses it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Form, Query, State};
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use openidconnect::core::{
    CoreGenderClaim, CoreJsonWebKeySet, CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm,
    CoreRsaPrivateSigningKey,
};
use openidconnect::{
    AdditionalClaims, Audience, EndUserEmail, IdToken, IdTokenClaims, IssuerUrl, JsonWebKeyId,
    Nonce, PrivateSigningKey, StandardClaims, SubjectIdentifier,
};
use rowbit_engine::Engine;
use rowbit_mysql::MySqlAdapter;
use rowbit_policy::PolicyFile;
use rowbit_store::Store;
use rowbit_web::config::ServerConfig;
use rowbit_web::oidc::OidcLogin;
use serde::{Deserialize, Serialize};
use tower::ServiceExt;

const HOST: &str = "127.0.0.1:8080";
const CLIENT_ID: &str = "rowbit-test";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Groups {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    groups: Vec<String>,
}
impl AdditionalClaims for Groups {}

/// Who the provider says signed in next, and how it misbehaves.
#[derive(Clone)]
struct Next {
    email: String,
    verified: Option<bool>,
    groups: Vec<String>,
    wrong_nonce: bool,
    foreign_key: bool,
}

impl Next {
    fn as_(email: &str) -> Next {
        Next {
            email: email.into(),
            verified: Some(true),
            groups: Vec::new(),
            wrong_nonce: false,
            foreign_key: false,
        }
    }
}

struct Provider {
    issuer: String,
    key: CoreRsaPrivateSigningKey,
    other_key: CoreRsaPrivateSigningKey,
    next: Mutex<Next>,
    /// Codes handed out: the nonce and PKCE challenge they carry.
    codes: Mutex<HashMap<String, (String, String)>>,
}

fn rsa_key(kid: &str) -> CoreRsaPrivateSigningKey {
    use rsa::pkcs1::EncodeRsaPrivateKey;
    let key = rsa::RsaPrivateKey::new(&mut rand_core::OsRng, 2048).unwrap();
    let pem = key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap();
    CoreRsaPrivateSigningKey::from_pem(&pem, Some(JsonWebKeyId::new(kid.into()))).unwrap()
}

/// Starts the provider on a free port.
async fn provider() -> Arc<Provider> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let p = Arc::new(Provider {
        issuer,
        key: rsa_key("good"),
        other_key: rsa_key("good"),
        next: Mutex::new(Next::as_("ann@example.com")),
        codes: Mutex::new(HashMap::new()),
    });
    let app = axum::Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .with_state(p.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    p
}

async fn discovery(State(p): State<Arc<Provider>>) -> Response {
    axum::Json(serde_json::json!({
        "issuer": p.issuer,
        "authorization_endpoint": format!("{}/authorize", p.issuer),
        "token_endpoint": format!("{}/token", p.issuer),
        "jwks_uri": format!("{}/jwks", p.issuer),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
    }))
    .into_response()
}

async fn jwks(State(p): State<Arc<Provider>>) -> Response {
    axum::Json(CoreJsonWebKeySet::new(vec![p.key.as_verification_key()])).into_response()
}

async fn authorize(
    State(p): State<Arc<Provider>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    assert_eq!(q["client_id"], CLIENT_ID);
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["scope"].split(' ').any(|s| s == "email"), "{q:?}");
    let code = format!("code-{}", p.codes.lock().unwrap().len());
    p.codes.lock().unwrap().insert(
        code.clone(),
        (q["nonce"].clone(), q["code_challenge"].clone()),
    );
    Redirect::to(&format!(
        "{}?code={code}&state={}",
        q["redirect_uri"], q["state"]
    ))
    .into_response()
}

async fn token(State(p): State<Arc<Provider>>, Form(f): Form<HashMap<String, String>>) -> Response {
    use base64::Engine;
    use sha2::Digest;
    let Some((nonce, challenge)) = p.codes.lock().unwrap().remove(&f["code"]) else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "invalid_grant"})),
        )
            .into_response();
    };
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(f["code_verifier"].as_bytes()));
    if verifier != challenge {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "invalid_grant"})),
        )
            .into_response();
    }
    let next = p.next.lock().unwrap().clone();
    let now = chrono::Utc::now();
    let claims = IdTokenClaims::new(
        IssuerUrl::new(p.issuer.clone()).unwrap(),
        vec![Audience::new(CLIENT_ID.into())],
        now + chrono::Duration::minutes(5),
        now,
        StandardClaims::<CoreGenderClaim>::new(SubjectIdentifier::new(next.email.clone()))
            .set_email(Some(EndUserEmail::new(next.email.clone())))
            .set_email_verified(next.verified),
        Groups {
            groups: next.groups,
        },
    )
    .set_nonce(Some(Nonce::new(if next.wrong_nonce {
        "other".into()
    } else {
        nonce
    })));
    let key = if next.foreign_key {
        &p.other_key
    } else {
        &p.key
    };
    let id_token = IdToken::<
        Groups,
        CoreGenderClaim,
        CoreJweContentEncryptionAlgorithm,
        CoreJwsSigningAlgorithm,
    >::new(
        claims,
        key,
        CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
        None,
        None,
    )
    .unwrap();
    axum::Json(serde_json::json!({
        "access_token": "unused",
        "token_type": "Bearer",
        "expires_in": 300,
        "id_token": id_token.to_string(),
    }))
    .into_response()
}

/// Rowbit's review page, signing in through `p`.
async fn rowbit(p: &Provider) -> axum::Router {
    let config = ServerConfig::from_yaml(
        &format!(
            "version: 0\npublic_url: http://{HOST}\nreviewers:\n  oidc:\n    issuer: {}\n    client_id: {CLIENT_ID}\n    client_secret: ${{SECRET}}\n  allow:\n    domains: [example.com]\n    groups: [db-reviewers]\n",
            p.issuer
        ),
        |name| (name == "SECRET").then(|| format!("{:?}", std::time::Instant::now())),
    )
    .unwrap();
    let login = OidcLogin::discover(
        config.oidc.as_ref().unwrap(),
        config.public_url.as_deref().unwrap(),
    )
    .await
    .unwrap();
    let engine = Arc::new(Engine::new(
        MySqlAdapter::new("mysql://nobody@127.0.0.1:1/none").unwrap(),
        PolicyFile::from_yaml("version: 0\n").unwrap(),
        Store::open_in_memory().unwrap(),
    ));
    rowbit_web::router(
        engine,
        rowbit_web::Web {
            sign_in: rowbit_web::SignIn::Oidc(Box::new(login)),
            hosts: vec![HOST.into()],
            secure: false,
            mcp: None,
        },
    )
}

async fn send(router: &axum::Router, path: &str, cookie: Option<&str>) -> Response {
    let mut request = Request::get(path).header(header::HOST, HOST);
    if let Some(c) = cookie {
        request = request.header(header::COOKIE, c);
    }
    router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn set_cookie(r: &Response, name: &str) -> Option<String> {
    r.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(&format!("{name}=")) && !v.starts_with(&format!("{name}=;")))
        .map(str::to_owned)
}

fn location(r: &Response) -> String {
    r.headers()[header::LOCATION].to_str().unwrap().to_owned()
}

async fn body(r: Response) -> String {
    String::from_utf8(
        axum::body::to_bytes(r.into_body(), 1 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

/// Goes to the provider and back. Returns the login cookie and the
/// callback's path and query.
async fn round_trip(router: &axum::Router) -> (String, String) {
    let start = send(router, "/auth/start", None).await;
    assert!(start.status().is_redirection());
    let login = set_cookie(&start, "rowbit_login").expect("login cookie");
    assert!(
        login.contains("SameSite=Lax") && login.contains("HttpOnly"),
        "{login}"
    );
    let login = login.split(';').next().unwrap().to_owned();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let back = http.get(location(&start)).send().await.unwrap();
    let back = back.headers()[header::LOCATION.as_str()]
        .to_str()
        .unwrap()
        .to_owned();
    let path = back
        .strip_prefix(&format!("http://{HOST}"))
        .unwrap()
        .to_owned();
    assert!(path.starts_with("/auth/callback?"), "{path}");
    (login, path)
}

/// Signs in as whoever the provider says; returns the callback's status,
/// the session cookie if one was set, and the page.
async fn sign_in(router: &axum::Router) -> (StatusCode, Option<String>, String) {
    let (login, path) = round_trip(router).await;
    let r = send(router, &path, Some(&login)).await;
    let status = r.status();
    let session = set_cookie(&r, "rowbit_session");
    (status, session, body(r).await)
}

#[tokio::test]
async fn a_verified_allowed_reviewer_signs_in() {
    let p = provider().await;
    let router = rowbit(&p).await;
    let page = body(send(&router, "/login", None).await).await;
    assert!(page.contains("/auth/start"), "{page}");

    let (status, session, page) = sign_in(&router).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let session = session.expect("a session");
    assert!(
        session.contains("SameSite=Strict") && session.contains("HttpOnly"),
        "{session}"
    );
    // Moves on with a page of its own, so the Strict cookie is sent.
    assert!(page.contains("http-equiv=refresh"), "{page}");
    let cookie = session.split(';').next().unwrap();
    let me = body(send(&router, "/api/session", Some(cookie)).await).await;
    assert!(me.contains(r#""reviewer":"ann@example.com""#), "{me}");

    // By group, from outside the allowed domains.
    let mut next = Next::as_("contractor@elsewhere.dev");
    next.groups = vec!["db-reviewers".into()];
    *p.next.lock().unwrap() = next;
    let (status, session, page) = sign_in(&router).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(session.is_some());
}

#[tokio::test]
async fn the_provider_cant_vouch_for_the_wrong_people() {
    let p = provider().await;
    let router = rowbit(&p).await;
    let cases: Vec<(Next, &str)> = vec![
        (Next::as_("eve@elsewhere.dev"), "may not review"),
        (Next::as_("ann@example.com.evil.dev"), "may not review"),
        (
            Next {
                verified: Some(false),
                ..Next::as_("ann@example.com")
            },
            "verified email",
        ),
        (
            Next {
                verified: None,
                ..Next::as_("ann@example.com")
            },
            "verified email",
        ),
        (
            Next {
                wrong_nonce: true,
                ..Next::as_("ann@example.com")
            },
            "check out",
        ),
        (
            Next {
                foreign_key: true,
                ..Next::as_("ann@example.com")
            },
            "check out",
        ),
    ];
    for (next, needle) in cases {
        let email = next.email.clone();
        *p.next.lock().unwrap() = next;
        let (status, session, page) = sign_in(&router).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{email}: {page}");
        assert!(session.is_none(), "{email}");
        assert!(page.contains(needle), "{email}: {page}");
    }
}

#[tokio::test]
async fn a_callback_only_finishes_its_own_login_once() {
    let p = provider().await;
    let router = rowbit(&p).await;

    // Replayed.
    let (login, path) = round_trip(&router).await;
    assert_eq!(
        send(&router, &path, Some(&login)).await.status(),
        StatusCode::OK
    );
    let again = send(&router, &path, Some(&login)).await;
    assert_eq!(again.status(), StatusCode::FORBIDDEN);
    assert!(body(again).await.contains("already used"));

    // Without the browser's login cookie: someone else's link.
    let (_, path) = round_trip(&router).await;
    let r = send(&router, &path, None).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    // Another login's cookie.
    let (login, _) = round_trip(&router).await;
    let (_, path) = round_trip(&router).await;
    let r = send(&router, &path, Some(&login)).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    // A changed state.
    let (login, path) = round_trip(&router).await;
    let tampered = path.replace("state=", "state=x");
    let r = send(&router, &tampered, Some(&login)).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert!(body(r).await.contains("belong to this sign-in"));

    // The page password doesn't exist here.
    let r = router
        .clone()
        .oneshot(
            Request::post("/login")
                .header(header::HOST, HOST)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("name=eve&password=x"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}
