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
//!   GET /admin/orders    -> orders report: paid and needs-review orders, oldest payment first.
//!   GET /admin/annual-fees     -> annual camping fee report for one scouting year (`?year=`).
//!   GET /admin/annual-fees.csv -> annual fees CSV (all years, or `?year=`).
//!
//! Every page except the login flow requires the session cookie and redirects to /admin
//! without it. Pages with names or emails are sent with `Cache-Control: no-store`.
//!
//! Both the short-lived login flow state (CSRF token, nonce, PKCE verifier) and the admin
//! session are kept in encrypted, HttpOnly cookies (`axum-extra`'s `PrivateCookieJar`) rather
//! than server-side storage, so no session store is needed.

use crate::{AppState, db, export};
use chrono::{DateTime, Datelike, FixedOffset};
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
        .route("/admin/orders", get(orders_page))
        .route("/admin/annual-fees", get(annual_fees_page))
        .route("/admin/annual-fees.csv", get(annual_fees_csv))
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

const ADMIN_STYLE: &str = "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<style>body{font-family:system-ui,sans-serif;margin:1rem;line-height:1.4;color:#1d2521}\
.scroll{overflow-x:auto}table{border-collapse:collapse;margin:.5rem 0}\
th,td{border:1px solid #d6dbd4;padding:.3rem .6rem;text-align:left;vertical-align:top}\
td.n{text-align:right}th{background:#eef0ec}.alert{color:#b3261e;font-weight:700}\
.dup{color:#8a5a00;font-weight:600}a{color:#1f5c3a}</style>";

/// A signed-in page. `no-store` because these pages show names and emails.
fn private_page(title: &str, body: &str) -> Response {
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Html(format!("<!doctype html><title>{}</title>{ADMIN_STYLE}{body}", html_escape(title))),
    )
        .into_response()
}

async fn admin_page(State(st): State<AppState>, jar: PrivateCookieJar) -> Response {
    if let Some(email) = current_admin(&st, &jar) {
        let fees = match &st.catalog.annual_fee {
            None => String::new(),
            Some(cfg) => {
                let year = cfg.scouting_year.clone();
                match st.db.call(move |c| db::annual_fee_summary(c, &year)).await {
                    Ok(summary) => fee_admin_section(cfg, &summary, cfg.is_open((st.now)())),
                    Err(e) => {
                        tracing::error!("admin: loading annual fee summary failed: {e:#}");
                        "<h2>Annual camping fees</h2><p class=\"alert\">Could not load the annual fee summary.</p>".into()
                    }
                }
            }
        };
        return private_page(
            "Admin",
            &format!(
                "<p>Signed in as {}.</p>\
                 <p><a href=\"/admin/orders\">View orders report</a></p>\
                 <p><a href=\"/admin/export.csv\">Download orders CSV</a></p>\
                 {fees}\
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
    // Redirect to the homepage rather than back to /admin: some password managers auto-follow
    // a login redirect and would immediately sign back in, making logout look like a no-op loop.
    (jar, Redirect::to("/")).into_response()
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

// ---------------------------------------------------------------------------------------------
// Orders report
// ---------------------------------------------------------------------------------------------

async fn orders_page(State(st): State<AppState>, jar: PrivateCookieJar) -> Response {
    let Some(email) = current_admin(&st, &jar) else {
        return Redirect::to("/admin").into_response();
    };
    let rows = match st.db.call(|c| db::list_orders_for_export(c)).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("admin: loading orders report failed: {e:#}");
            return page(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Could not load orders.");
        }
    };
    tracing::info!(email, rows = rows.len(), "admin viewed orders report");
    // Dates are shown in the offset the catalog's cutoff is written in, like the fee report.
    let offset = *st.catalog.closes_at.offset();
    private_page("Orders", &orders_report_html(&rows, offset))
}

fn orders_report_html(rows: &[db::OrderRow], offset: FixedOffset) -> String {
    let esc = |s: &str| html_escape(s);
    let paid: Vec<&db::OrderRow> = rows.iter().filter(|o| o.status == shared::OrderStatus::Paid).collect();
    let review: Vec<&db::OrderRow> = rows.iter().filter(|o| o.status == shared::OrderStatus::NeedsReview).collect();
    let date = |o: &db::OrderRow| local_date(o.paid_at.as_deref().unwrap_or(&o.created_at), offset);
    let items = |o: &db::OrderRow| {
        o.lines.iter().map(|l| format!("{} × {}", l.qty, esc(&l.name))).collect::<Vec<_>>().join("<br>")
    };
    let mailto = |e: &str| {
        if e.is_empty() { String::new() } else { format!("<a href=\"mailto:{0}\">{0}</a>", esc(e)) }
    };
    let review_count = if review.is_empty() {
        "0".to_string()
    } else {
        format!("<span class=\"alert\">{}</span>", review.len())
    };

    let mut h = String::from("<h1>Orders</h1>");
    h += &format!(
        "<table>\
         <tr><th>Orders paid</th><td class=\"n\">{}</td></tr>\
         <tr><th>Total collected (before Stripe fees)</th><td class=\"n\">{}</td></tr>\
         <tr><th>Needs review</th><td class=\"n\">{review_count}</td></tr>\
         </table>",
        paid.len(),
        shared::format_cents(paid.iter().map(|o| o.total_cents).sum()),
    );

    if !review.is_empty() {
        h += "<h2 class=\"alert\">Needs review</h2><div class=\"scroll\"><table><tr><th>Buyer</th><th>Email</th>\
              <th>Items</th><th>Total</th><th>Paid at</th><th>Reason</th><th>Order</th></tr>";
        for o in review {
            h += &format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td class=\"n\">{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&o.buyer_name),
                mailto(&o.email),
                items(o),
                shared::format_cents(o.total_cents),
                esc(o.paid_at.as_deref().unwrap_or("")),
                esc(o.review_reason.as_deref().unwrap_or("")),
                esc(&o.id),
            );
        }
        h += "</table></div>";
    }

    h += &format!("<h2>Paid orders ({})</h2>", paid.len());
    if paid.is_empty() {
        h += "<p>No paid orders yet.</p>";
    } else {
        h += "<div class=\"scroll\"><table><tr><th>Date paid</th><th>Buyer</th><th>Email</th><th>Phone</th>\
              <th>Scout</th><th>Items</th><th>Total</th></tr>";
        for o in paid {
            h += &format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td class=\"n\">{}</td></tr>",
                date(o),
                esc(&o.buyer_name),
                mailto(&o.email),
                esc(&o.phone),
                esc(o.scout_name.as_deref().unwrap_or("")),
                items(o),
                shared::format_cents(o.total_cents),
            );
        }
        h += "</table></div>";
    }

    h += "<p><a href=\"/admin/export.csv\">Download orders CSV</a></p><p><a href=\"/admin\">Back to admin</a></p>";
    h
}

// ---------------------------------------------------------------------------------------------
// Annual camping fees
// ---------------------------------------------------------------------------------------------

/// Percent-encode a query-string value (scouting years are free text).
fn query_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The summary numbers, as a two-column table.
fn fee_summary_table(s: &db::AnnualFeeSummary) -> String {
    let review = if s.needs_review > 0 {
        format!("<span class=\"alert\">{}</span>", s.needs_review)
    } else {
        "0".into()
    };
    format!(
        "<table>\
         <tr><th>Scouts paid</th><td class=\"n\">{}</td></tr>\
         <tr><th>Total collected (before Stripe fees)</th><td class=\"n\">{}</td></tr>\
         <tr><th>Checkouts</th><td class=\"n\">{}</td></tr>\
         <tr><th>Needs review</th><td class=\"n\">{review}</td></tr>\
         </table>",
        s.scouts_paid,
        shared::format_cents(s.total_cents),
        s.checkouts,
    )
}

fn closing_line(cfg: &crate::catalog::AnnualFeeConfig, open: bool) -> String {
    let when = shared::format_local_datetime(&cfg.closes_at.to_rfc3339()).unwrap_or_else(|| cfg.closes_at.to_rfc3339());
    let state = if open { "open" } else { "closed" };
    format!("<p>Payments close {} — currently <strong>{state}</strong>.</p>", html_escape(&when))
}

/// The "Annual camping fees" section of `/admin`, for the configured scouting year.
fn fee_admin_section(cfg: &crate::catalog::AnnualFeeConfig, summary: &db::AnnualFeeSummary, open: bool) -> String {
    format!(
        "<h2>Annual camping fees {}</h2>{}{}\
         <p><a href=\"/admin/annual-fees\">View annual fee report</a></p>\
         <p><a href=\"/admin/annual-fees.csv\">Download annual fees CSV</a></p>",
        html_escape(&cfg.scouting_year),
        fee_summary_table(summary),
        closing_line(cfg, open),
    )
}

#[derive(Deserialize)]
struct YearQuery {
    year: Option<String>,
}

impl YearQuery {
    fn year(self) -> Option<String> {
        self.year.map(|y| y.trim().to_string()).filter(|y| !y.is_empty())
    }
}

/// `paid_at` (UTC) as a date in the troop's local offset: the one `closes_at` is written in.
fn local_date(paid_at: &str, offset: FixedOffset) -> String {
    match DateTime::parse_from_rfc3339(paid_at) {
        Ok(t) => {
            let t = t.with_timezone(&offset);
            format!("{:04}-{:02}-{:02}", t.year(), t.month(), t.day())
        }
        Err(_) => paid_at.to_string(),
    }
}

struct FeeReport {
    years: Vec<String>,
    summary: db::AnnualFeeSummary,
    rows: Vec<db::AnnualFeeRow>,
}

async fn annual_fees_page(State(st): State<AppState>, jar: PrivateCookieJar, Query(q): Query<YearQuery>) -> Response {
    let Some(email) = current_admin(&st, &jar) else {
        return Redirect::to("/admin").into_response();
    };
    let cfg = st.catalog.annual_fee.as_ref();
    let requested = q.year().or_else(|| cfg.map(|c| c.scouting_year.clone()));
    let loaded = st
        .db
        .call(move |c| {
            let years = db::annual_fee_years(c)?;
            // Without config or ?year=, show the newest year there is.
            let year = requested.or_else(|| years.first().cloned()).unwrap_or_default();
            let summary = db::annual_fee_summary(c, &year)?;
            let rows = db::list_annual_fees(c, Some(&year))?;
            Ok((year, FeeReport { years, summary, rows }))
        })
        .await;
    let (year, report) = match loaded {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("admin: loading annual fee report failed: {e:#}");
            return page(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Could not load annual fees.");
        }
    };
    tracing::info!(email, year, rows = report.rows.len(), "admin viewed annual fee report");
    let offset = cfg.map(|c| *c.closes_at.offset()).unwrap_or_else(|| FixedOffset::east_opt(0).expect("UTC"));
    private_page("Annual camping fees", &fee_report_html(&year, &report, cfg, (st.now)(), offset))
}

fn fee_report_html(
    year: &str,
    r: &FeeReport,
    cfg: Option<&crate::catalog::AnnualFeeConfig>,
    now: chrono::DateTime<chrono::Utc>,
    offset: FixedOffset,
) -> String {
    let esc = |s: &str| html_escape(s);
    let mut h = format!("<h1>Annual camping fees {}</h1>", esc(year));

    if !r.years.is_empty() {
        let links: Vec<String> = r
            .years
            .iter()
            .map(|y| {
                if y == year {
                    format!("<strong>{}</strong>", esc(y))
                } else {
                    format!("<a href=\"/admin/annual-fees?year={}\">{}</a>", query_escape(y), esc(y))
                }
            })
            .collect();
        h += &format!("<p>Scouting years: {}</p>", links.join(" · "));
    }
    h += &fee_summary_table(&r.summary);
    if let Some(c) = cfg.filter(|c| c.scouting_year == year) {
        h += &closing_line(c, c.is_open(now));
    }

    // Same scout (case-insensitively) under more than one checkout: probably paid twice.
    let key = |f: &db::AnnualFeeRow| (f.scout_first_name.to_lowercase(), f.scout_last_name.to_lowercase());
    let mut checkouts: std::collections::HashMap<(String, String), std::collections::HashSet<&str>> = Default::default();
    for f in &r.rows {
        checkouts.entry(key(f)).or_default().insert(&f.payment_id);
    }
    let dup_flag = |f: &db::AnnualFeeRow| {
        if checkouts.get(&key(f)).is_some_and(|p| p.len() > 1) {
            " <span class=\"dup\">possible duplicate</span>"
        } else {
            ""
        }
    };
    let mailto = |e: &str| {
        if e.is_empty() { String::new() } else { format!("<a href=\"mailto:{0}\">{0}</a>", esc(e)) }
    };

    let review: Vec<&db::AnnualFeeRow> = r.rows.iter().filter(|f| f.status == shared::FeeStatus::NeedsReview).collect();
    if !review.is_empty() {
        h += "<h2 class=\"alert\">Needs review</h2><div class=\"scroll\"><table><tr><th>Scout</th><th>Payer</th>\
              <th>Payer email</th><th>Amount</th><th>Paid at</th><th>Reason</th><th>Stripe payment intent</th></tr>";
        for f in review {
            h += &format!(
                "<tr><td>{} {}{}</td><td>{}</td><td>{}</td><td class=\"n\">{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&f.scout_first_name),
                esc(&f.scout_last_name),
                dup_flag(f),
                esc(&f.payer_name),
                mailto(&f.payer_email),
                shared::format_cents(f.amount_cents),
                esc(&f.paid_at),
                esc(f.review_reason.as_deref().unwrap_or("")),
                esc(f.stripe_payment_intent_id.as_deref().unwrap_or("")),
            );
        }
        h += "</table></div>";
    }

    let paid: Vec<&db::AnnualFeeRow> = r.rows.iter().filter(|f| f.status == shared::FeeStatus::Paid).collect();
    h += &format!("<h2>Paid scouts ({})</h2>", paid.len());
    if paid.is_empty() {
        h += "<p>No payments yet.</p>";
    } else {
        h += "<div class=\"scroll\"><table><tr><th>Last name</th><th>First name</th><th>Amount</th><th>Date paid</th>\
              <th>Payer</th><th>Payer email</th><th></th></tr>";
        for f in paid {
            h += &format!(
                "<tr><td>{}</td><td>{}</td><td class=\"n\">{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&f.scout_last_name),
                esc(&f.scout_first_name),
                shared::format_cents(f.amount_cents),
                local_date(&f.paid_at, offset),
                esc(&f.payer_name),
                mailto(&f.payer_email),
                dup_flag(f).trim_start(),
            );
        }
        h += "</table></div>";
    }

    h += &format!(
        "<p><a href=\"/admin/annual-fees.csv?year={}\">Download CSV for {}</a></p><p><a href=\"/admin\">Back to admin</a></p>",
        query_escape(year),
        esc(year)
    );
    h
}

async fn annual_fees_csv(State(st): State<AppState>, jar: PrivateCookieJar, Query(q): Query<YearQuery>) -> Response {
    let Some(email) = current_admin(&st, &jar) else {
        return Redirect::to("/admin").into_response();
    };
    let year = q.year();
    let y = year.clone();
    let rows = match st.db.call(move |c| db::list_annual_fees(c, y.as_deref())).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("admin export: loading annual fees failed: {e:#}");
            return page(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Could not load annual fees.");
        }
    };
    let mut buf: Vec<u8> = Vec::new();
    if let Err(e) = export::write_annual_fees_csv(&rows, &mut buf) {
        tracing::error!("admin export: writing annual fees CSV failed: {e:#}");
        return page(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Could not build CSV.");
    }
    tracing::info!(email, year, row_count = rows.len(), "admin exported annual fees CSV");

    // The year is free text; keep the header value to safe filename characters.
    let label: String = year
        .as_deref()
        .unwrap_or("all")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (axum::http::header::CONTENT_DISPOSITION, format!("attachment; filename=\"annual-fees-{label}.csv\"")),
            (axum::http::header::CACHE_CONTROL, "no-store".to_string()),
        ],
        buf,
    )
        .into_response()
}
