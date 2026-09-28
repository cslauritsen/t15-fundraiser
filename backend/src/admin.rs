//! `/admin` routes: Google OIDC login gated by the `admins` list in catalog.yaml, and a CSV
//! export of orders (the same data as `t15-fundraiser export`).
//!
//! Flow:
//!   GET /admin           -> if a valid admin session cookie is present, show the download
//!                           page; otherwise redirect into Google's OIDC login.
//!   GET /admin/callback  -> Google redirects here with `code` and `state`; on success, sets
//!                           the admin session cookie and redirects back to /admin.
//!   GET /admin/logout    -> clears the session cookie.
//!   GET /admin/export.csv -> streams the orders CSV; requires a valid session cookie.
//!
//! Both the short-lived login flow state (CSRF token, nonce, PKCE verifier) and the admin
//! session are kept in encrypted, HttpOnly cookies (`axum-extra`'s `PrivateCookieJar`) rather
//! than server-side storage, so no session store is needed.

use crate::{AppState, db, export};
use axum::{
    Router,
    extract::{Query, State},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use axum_extra::extract::{
    PrivateCookieJar,
    cookie::{Cookie, SameSite},
};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EmptyAdditionalProviderMetadata, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, IssuerUrl, JsonWebKeySetUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl,
    Scope, TokenResponse,
    core::{
        CoreAuthenticationFlow, CoreClient, CoreJwsSigningAlgorithm, CoreProviderMetadata, CoreResponseType,
        CoreSubjectIdentifierType,
    },
};
use serde::{Deserialize, Serialize};

const FLOW_COOKIE: &str = "admin_oidc_flow";
const SESSION_COOKIE: &str = "admin_session";
const FLOW_TTL_SECS: i64 = 10 * 60;
const SESSION_TTL_SECS: i64 = 12 * 60 * 60;
const GOOGLE_ISSUER: &str = "https://accounts.google.com";

/// The concrete client type produced by `CoreClient::from_provider_metadata`: auth and issuer
/// endpoints are always present after discovery, while token/userinfo endpoints are checked at
/// request time (`EndpointMaybeSet`) since the trait bounds don't otherwise let us name this type.
type GoogleClient = CoreClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointMaybeSet, EndpointMaybeSet>;

/// Discovered Google OIDC settings, built once at startup. A fresh `CoreClient` is constructed
/// from these (cheaply, no network I/O) for each request that needs one.
pub struct AdminOidc {
    provider_metadata: CoreProviderMetadata,
    client_id: ClientId,
    client_secret: ClientSecret,
    redirect_uri: RedirectUrl,
    http_client: openidconnect::reqwest::Client,
}

impl AdminOidc {
    /// Fetches Google's OIDC discovery document. `base_url` is the site's public base URL
    /// (e.g. `https://troop15.org`); the callback is registered at `{base_url}/admin/callback`.
    pub async fn discover(client_id: String, client_secret: String, base_url: &str) -> anyhow::Result<Self> {
        let http_client = openidconnect::reqwest::ClientBuilder::new()
            .redirect(openidconnect::reqwest::redirect::Policy::none())
            .build()?;
        let provider_metadata =
            CoreProviderMetadata::discover_async(IssuerUrl::new(GOOGLE_ISSUER.to_string())?, &http_client).await?;
        Ok(AdminOidc {
            provider_metadata,
            client_id: ClientId::new(client_id),
            client_secret: ClientSecret::new(client_secret),
            redirect_uri: RedirectUrl::new(format!("{base_url}/admin/callback"))?,
            http_client,
        })
    }

    /// Builds a client without contacting Google. For tests only: the resulting client can't
    /// complete a real login (its endpoints are dummy URLs), but it lets `AppState` be built
    /// without network access.
    pub fn dummy_for_tests(base_url: &str) -> Self {
        let provider_metadata = CoreProviderMetadata::new(
            IssuerUrl::new(GOOGLE_ISSUER.to_string()).unwrap(),
            openidconnect::AuthUrl::new(format!("{GOOGLE_ISSUER}/o/oauth2/v2/auth")).unwrap(),
            JsonWebKeySetUrl::new(format!("{GOOGLE_ISSUER}/oauth2/v3/certs")).unwrap(),
            vec![openidconnect::ResponseTypes::new(vec![CoreResponseType::Code])],
            vec![CoreSubjectIdentifierType::Public],
            vec![CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256],
            EmptyAdditionalProviderMetadata {},
        )
        .set_token_endpoint(Some(openidconnect::TokenUrl::new(format!("{GOOGLE_ISSUER}/token")).unwrap()));
        AdminOidc {
            provider_metadata,
            client_id: ClientId::new("test-client-id".into()),
            client_secret: ClientSecret::new("test-client-secret".into()),
            redirect_uri: RedirectUrl::new(format!("{base_url}/admin/callback")).unwrap(),
            http_client: openidconnect::reqwest::Client::new(),
        }
    }

    fn client(&self) -> GoogleClient {
        CoreClient::from_provider_metadata(
            self.provider_metadata.clone(),
            self.client_id.clone(),
            Some(self.client_secret.clone()),
        )
        .set_redirect_uri(self.redirect_uri.clone())
    }
}

#[derive(Serialize, Deserialize)]
struct FlowData {
    csrf: String,
    nonce: String,
    pkce_verifier: String,
    expires_at: i64,
}

#[derive(Serialize, Deserialize)]
struct SessionData {
    email: String,
    expires_at: i64,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin", get(admin_page))
        .route("/admin/callback", get(admin_callback))
        .route("/admin/logout", get(admin_logout))
        .route("/admin/export.csv", get(admin_export))
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

fn cookie<'c>(st: &AppState, name: &'static str, value: String) -> Cookie<'c> {
    let mut c = Cookie::new(name, value);
    c.set_path("/admin");
    c.set_http_only(true);
    c.set_same_site(SameSite::Lax);
    c.set_secure(st.base_url.starts_with("https://"));
    c
}

fn expired_cookie<'c>(name: &'static str) -> Cookie<'c> {
    let mut c = Cookie::new(name, "");
    c.set_path("/admin");
    c.set_max_age(time::Duration::ZERO);
    c
}

/// The signed-in admin's email, if the session cookie is present, unexpired, and still in
/// catalog.yaml's `admins` list (so removing an admin from the catalog revokes access without
/// needing to wait for the cookie to expire).
fn current_admin(st: &AppState, jar: &PrivateCookieJar) -> Option<String> {
    let raw = jar.get(SESSION_COOKIE)?;
    let data: SessionData = serde_json::from_str(raw.value()).ok()?;
    if data.expires_at < now_ts() || !st.catalog.is_admin(&data.email) {
        return None;
    }
    Some(data.email)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn page(status: axum::http::StatusCode, body: impl Into<String>) -> Response {
    (status, Html(body.into())).into_response()
}

async fn admin_page(State(st): State<AppState>, jar: PrivateCookieJar) -> Response {
    if let Some(email) = current_admin(&st, &jar) {
        return page(
            axum::http::StatusCode::OK,
            format!(
                "<!doctype html><title>Admin</title><p>Signed in as {}.</p>\
                 <p><a href=\"/admin/export.csv\">Download orders CSV</a></p>\
                 <p><a href=\"/admin/logout\">Log out</a></p>",
                html_escape(&email)
            ),
        );
    }

    let client = st.admin_oidc.client();
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (auth_url, csrf_token, nonce) = client
        .authorize_url(CoreAuthenticationFlow::AuthorizationCode, CsrfToken::new_random, Nonce::new_random)
        .add_scope(Scope::new("email".to_string()))
        .add_scope(Scope::new("profile".to_string()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    let flow = FlowData {
        csrf: csrf_token.secret().clone(),
        nonce: nonce.secret().clone(),
        pkce_verifier: pkce_verifier.secret().clone(),
        expires_at: now_ts() + FLOW_TTL_SECS,
    };
    let Ok(flow_json) = serde_json::to_string(&flow) else {
        return page(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Could not start login.");
    };
    let jar = jar.add(cookie(&st, FLOW_COOKIE, flow_json));
    (jar, Redirect::to(auth_url.as_str())).into_response()
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn admin_callback(State(st): State<AppState>, jar: PrivateCookieJar, Query(q): Query<CallbackQuery>) -> Response {
    if let Some(e) = q.error {
        return page(axum::http::StatusCode::FORBIDDEN, format!("Google sign-in failed: {}", html_escape(&e)));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return page(axum::http::StatusCode::BAD_REQUEST, "Missing code or state.");
    };

    let Some(raw_flow) = jar.get(FLOW_COOKIE) else {
        return page(axum::http::StatusCode::BAD_REQUEST, "Login session expired. <a href=\"/admin\">Try again</a>.");
    };
    let jar = jar.add(expired_cookie(FLOW_COOKIE));
    let Ok(flow) = serde_json::from_str::<FlowData>(raw_flow.value()) else {
        return page(axum::http::StatusCode::BAD_REQUEST, "Invalid login session. <a href=\"/admin\">Try again</a>.");
    };
    if flow.expires_at < now_ts() {
        return page(axum::http::StatusCode::BAD_REQUEST, "Login session expired. <a href=\"/admin\">Try again</a>.");
    }
    if flow.csrf != state {
        return page(axum::http::StatusCode::BAD_REQUEST, "Invalid login state.");
    }

    let client = st.admin_oidc.client();
    let token_response = match client
        .exchange_code(AuthorizationCode::new(code))
        .map(|r| r.set_pkce_verifier(PkceCodeVerifier::new(flow.pkce_verifier)))
    {
        Ok(req) => match req.request_async(&st.admin_oidc.http_client).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("admin login: token exchange failed: {e:#}");
                return page(axum::http::StatusCode::BAD_GATEWAY, "Could not complete sign-in with Google.");
            }
        },
        Err(e) => {
            tracing::warn!("admin login: building token request failed: {e:#}");
            return page(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Could not complete sign-in with Google.");
        }
    };

    let Some(id_token) = token_response.id_token() else {
        return page(axum::http::StatusCode::BAD_GATEWAY, "Google did not return an ID token.");
    };
    let verifier = client.id_token_verifier();
    let claims = match id_token.claims(&verifier, &Nonce::new(flow.nonce)) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("admin login: ID token verification failed: {e:#}");
            return page(axum::http::StatusCode::FORBIDDEN, "Could not verify Google sign-in.");
        }
    };

    let email = match claims.email() {
        Some(e) if claims.email_verified() == Some(true) => e.as_str().to_string(),
        _ => return page(axum::http::StatusCode::FORBIDDEN, "Your Google account has no verified email address."),
    };

    if !st.catalog.is_admin(&email) {
        tracing::warn!(email, "admin login rejected: not in catalog admins list");
        return page(axum::http::StatusCode::FORBIDDEN, "You are not authorized to access this page.");
    }

    let session = SessionData { email: email.clone(), expires_at: now_ts() + SESSION_TTL_SECS };
    let Ok(session_json) = serde_json::to_string(&session) else {
        return page(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Could not start session.");
    };
    tracing::info!(email, "admin signed in");
    let jar = jar.add(cookie(&st, SESSION_COOKIE, session_json));
    (jar, Redirect::to("/admin")).into_response()
}

async fn admin_logout(jar: PrivateCookieJar) -> Response {
    let jar = jar.add(expired_cookie(SESSION_COOKIE));
    (jar, Redirect::to("/admin")).into_response()
}

async fn admin_export(State(st): State<AppState>, jar: PrivateCookieJar) -> Response {
    let Some(email) = current_admin(&st, &jar) else {
        return Redirect::to("/admin").into_response();
    };

    let rows = match st.db.call(|c| db::list_orders_for_export(c)).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("admin export: loading orders failed: {e:#}");
            return page(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Could not load orders.");
        }
    };
    let item_ids: Vec<String> = st.catalog.view((st.now)()).items.into_iter().map(|i| i.id).collect();
    let mut buf: Vec<u8> = Vec::new();
    if let Err(e) = export::write_csv(&rows, &item_ids, &mut buf) {
        tracing::error!("admin export: writing CSV failed: {e:#}");
        return page(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Could not build CSV.");
    }
    tracing::info!(email, order_count = rows.len(), "admin exported orders CSV");

    (
        [
            (axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (axum::http::header::CONTENT_DISPOSITION, "attachment; filename=\"orders.csv\""),
        ],
        buf,
    )
        .into_response()
}
