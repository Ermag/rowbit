//! Signing reviewers in through an OpenID Connect provider: the
//! authorization code flow with PKCE, a `state` and a `nonce`, as a
//! confidential client. See "Signing in" in `docs/spec/server.md`.
//!
//! The `openidconnect` crate does discovery, the token exchange and the ID
//! token checks (signature against the provider's keys, issuer, audience,
//! expiry, nonce). This module keeps the state of logins in progress and
//! decides who may sign in.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    TokenResponse,
};

use crate::config::{Allow, Oidc};

/// How long a login may take at the provider.
pub const LOGIN_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Logins in progress kept at once; the oldest go first.
const MAX_PENDING: usize = 1_000;
/// How long a request to the provider may take.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(10);

type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// A login started here and not yet back from the provider.
struct Pending {
    state: CsrfToken,
    nonce: Nonce,
    verifier: PkceCodeVerifier,
    started: Instant,
}

pub struct OidcLogin {
    client: Client,
    http: openidconnect::reqwest::Client,
    scopes: Vec<String>,
    allow: Allow,
    /// Provider host, for the sign-in button.
    pub provider: String,
    pending: Mutex<HashMap<String, Pending>>,
}

/// Who signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reviewer {
    pub email: String,
}

impl OidcLogin {
    /// Reads the provider's discovery document and keys. `public_url` is
    /// Rowbit's, without a trailing `/`.
    pub async fn discover(config: &Oidc, public_url: &str) -> Result<OidcLogin, String> {
        let http = openidconnect::reqwest::ClientBuilder::new()
            // Following redirects would let a provider's answer send Rowbit
            // somewhere else.
            .redirect(openidconnect::reqwest::redirect::Policy::none())
            .timeout(PROVIDER_TIMEOUT)
            .build()
            .map_err(|e| format!("can't build an HTTP client: {e}"))?;
        let issuer = IssuerUrl::new(config.issuer.clone()).map_err(|e| format!("issuer: {e}"))?;
        let metadata = CoreProviderMetadata::discover_async(issuer, &http)
            .await
            .map_err(|e| format!("reading {}'s discovery document failed: {e}", config.issuer))?;
        let redirect = RedirectUrl::new(format!("{public_url}/auth/callback"))
            .map_err(|e| format!("public_url: {e}"))?;
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(config.client_id.clone()),
            Some(ClientSecret::new(config.client_secret.expose().to_owned())),
        )
        .set_redirect_uri(redirect);
        let provider = config
            .issuer
            .split_once("://")
            .map(|(_, rest)| rest.split('/').next().unwrap_or(rest).to_owned())
            .unwrap_or_else(|| config.issuer.clone());
        Ok(OidcLogin {
            client,
            http,
            scopes: config.scopes.clone(),
            allow: config.allow.clone(),
            provider,
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// Starts a login. Returns the login's id, for a cookie that brings the
    /// browser back to the same login, and where to send the browser.
    pub fn start(&self, login_id: String) -> String {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut request = self
            .client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("email".into()))
            .set_pkce_challenge(challenge);
        for scope in &self.scopes {
            request = request.add_scope(Scope::new(scope.clone()));
        }
        let (url, state, nonce) = request.url();
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending.retain(|_, p| p.started.elapsed() < LOGIN_WINDOW);
        if pending.len() >= MAX_PENDING
            && let Some(oldest) = pending
                .iter()
                .min_by_key(|(_, p)| p.started)
                .map(|(k, _)| k.clone())
        {
            pending.remove(&oldest);
        }
        pending.insert(
            login_id,
            Pending {
                state,
                nonce,
                verifier,
                started: Instant::now(),
            },
        );
        url.to_string()
    }

    /// Finishes the login `login_id` with what the provider sent back.
    /// Each login can be finished once. `Err` is shown to the person.
    pub async fn finish(
        &self,
        login_id: &str,
        state: &str,
        code: &str,
    ) -> Result<Reviewer, String> {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(login_id)
            .filter(|p| p.started.elapsed() < LOGIN_WINDOW)
            .ok_or("this sign-in expired or was already used; start again")?;
        if !constant_time_eq(state.as_bytes(), pending.state.secret().as_bytes()) {
            return Err("the provider's answer doesn't belong to this sign-in; start again".into());
        }
        let token = self
            .client
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .map_err(|e| format!("the provider has no token endpoint: {e}"))?
            .set_pkce_verifier(pending.verifier)
            .request_async(&self.http)
            .await
            .map_err(|e| format!("the provider refused the sign-in: {e}"))?;
        let id_token = token.id_token().ok_or("the provider sent no ID token")?;
        let claims = id_token
            .claims(&self.client.id_token_verifier(), &pending.nonce)
            .map_err(|e| format!("the ID token didn't check out: {e}"))?;
        let email = claims
            .email()
            .map(|e| e.as_str().to_owned())
            .ok_or("the provider sent no email address; ask for the email scope")?;
        if claims.email_verified() != Some(true) {
            return Err(format!(
                "the provider doesn't vouch for {email}; Rowbit needs a verified email address"
            ));
        }
        // The signature was checked above, so the token's other claims can
        // be read from it as they are.
        let groups = groups_claim(&id_token.to_string());
        if !self.allow.admits(&email, &groups) {
            return Err(format!("{email} may not review changes here"));
        }
        Ok(Reviewer {
            email: email.to_ascii_lowercase(),
        })
    }
}

/// The `groups` claim of a JWT whose signature was already checked: a list
/// of strings, or nothing.
fn groups_claim(jwt: &str) -> Vec<String> {
    use base64::Engine;
    let Some(payload) = jwt.split('.').nth(1) else {
        return Vec::new();
    };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) else {
        return Vec::new();
    };
    #[derive(serde::Deserialize)]
    struct Groups {
        #[serde(default)]
        groups: Vec<String>,
    }
    serde_json::from_slice::<Groups>(&bytes)
        .map(|g| g.groups)
        .unwrap_or_default()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use base64::Engine;

    use super::*;

    #[test]
    fn groups_come_from_the_payload() {
        let enc = |v: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        let jwt = format!(
            "{}.{}.sig",
            enc("{}"),
            enc(r#"{"groups":["a","b"],"sub":"1"}"#)
        );
        assert_eq!(groups_claim(&jwt), ["a", "b"]);
        let jwt = format!("{}.{}.sig", enc("{}"), enc(r#"{"groups":"a"}"#));
        assert!(groups_claim(&jwt).is_empty(), "not a list: no groups");
        assert!(groups_claim("garbage").is_empty());
    }
}
