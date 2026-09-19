pub mod catalog;
pub mod config;
pub mod db;
pub mod export;
pub mod ratelimit;
pub mod stripe;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use catalog::Catalog;
use chrono::{DateTime, Utc};
use db::{Db, PaidOutcome, ts};
use ratelimit::RateLimiter;
use serde::Deserialize;
use shared::{
    CatalogResponse, CheckoutRequest, CheckoutResponse, ErrorResponse, FieldError, OrderStatus,
    OrderStatusResponse, validate_checkout,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use stripe::{PaymentProvider, SessionInfo, SessionLine, SessionRequest};
use tower::ServiceBuilder;
use tower_http::{
    compression::CompressionLayer,
    services::{ServeDir, ServeFile},
    set_header::SetResponseHeaderLayer,
    trace::TraceLayer,
};

/// Stripe requires a session to live at least 30 minutes; add a minute of slack.
const SESSION_TTL_SECS: i64 = 31 * 60;
const WEBHOOK_TOLERANCE_SECS: i64 = 300;

#[derive(Clone)]
pub struct AppState {
    pub catalog: Arc<Catalog>,
    pub db: Db,
    pub provider: Arc<dyn PaymentProvider>,
    pub webhook_secret: String,
    pub base_url: String,
    pub limiter: Arc<RateLimiter>,
    pub now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    pub trust_proxy: bool,
}

/// Where static files live; `None` in tests.
pub struct StaticDirs {
    /// Contains `images/`.
    pub static_dir: PathBuf,
    /// The built SPA (`index.html`, wasm, js, css).
    pub frontend_dir: PathBuf,
}

pub struct ApiError {
    status: StatusCode,
    body: ErrorResponse,
}

impl ApiError {
    fn new(status: StatusCode, code: &str, message: &str) -> Self {
        ApiError { status, body: ErrorResponse { code: code.into(), message: message.into(), fields: vec![] } }
    }

    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "Not found.")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        tracing::error!("internal error: {:#}", e.into());
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "Something went wrong. Please try again.")
    }
}

pub fn router(state: AppState, dirs: Option<StaticDirs>) -> Router {
    let checkout = Router::new()
        .route("/api/checkout", post(checkout))
        .layer(middleware::from_fn_with_state(state.clone(), rate_limit))
        .layer(DefaultBodyLimit::max(64 * 1024));

    let mut app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/catalog", get(catalog))
        .route("/api/orders/{id}/status", get(order_status))
        .route("/api/stripe/webhook", post(webhook))
        .merge(checkout)
        .route("/api/{*rest}", any(|| async { ApiError::not_found() }));

    if let Some(d) = dirs {
        let images = ServiceBuilder::new()
            .layer(SetResponseHeaderLayer::if_not_present(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("public, max-age=86400"),
            ))
            .service(ServeDir::new(d.static_dir.join("images")));
        // Unknown paths get index.html (with 200) so client-side routes like /success work.
        let spa = ServeDir::new(&d.frontend_dir).fallback(ServeFile::new(d.frontend_dir.join("index.html")));
        app = app
            .nest_service("/images", images)
            .fallback_service(spa)
            .layer(middleware::from_fn(static_cache_headers));
    }

    app.layer(CompressionLayer::new()).layer(TraceLayer::new_for_http()).with_state(state)
}

/// Trunk content-hashes the wasm/js/css filenames, so those can be cached forever;
/// the HTML shell must be revalidated so a new deploy is picked up.
async fn static_cache_headers(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let mut resp = next.run(req).await;
    if path.starts_with("/api") || resp.headers().contains_key(header::CACHE_CONTROL) {
        return resp;
    }
    let value = if path.ends_with(".wasm") || path.ends_with(".js") || path.ends_with(".css") {
        Some("public, max-age=31536000, immutable")
    } else if resp.headers().get(header::CONTENT_TYPE).is_some_and(|t| t.as_bytes().starts_with(b"text/html")) {
        Some("no-cache")
    } else {
        None
    };
    if let Some(v) = value {
        resp.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static(v));
    }
    resp
}

fn client_key(st: &AppState, req: &Request) -> String {
    if st.trust_proxy {
        // The rightmost entry is the one our own proxy appended; earlier ones are client-controlled.
        if let Some(ip) = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            return ip;
        }
    }
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip().to_string())
        .unwrap_or_else(|| "unknown".into())
}

async fn rate_limit(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let key = client_key(&st, &req);
    if !st.limiter.allow(&key, Instant::now()) {
        return ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "Too many requests. Please wait a minute and try again.")
            .into_response();
    }
    next.run(req).await
}

async fn catalog(State(st): State<AppState>) -> Json<CatalogResponse> {
    Json(st.catalog.view((st.now)()))
}

async fn checkout(
    State(st): State<AppState>,
    Json(req): Json<CheckoutRequest>,
) -> Result<Json<CheckoutResponse>, ApiError> {
    let now = (st.now)();
    if !st.catalog.is_open(now) {
        return Err(ApiError::new(StatusCode::CONFLICT, "closed", "Orders are closed. Thank you for your support!"));
    }
    let view = st.catalog.view(now);
    let order = validate_checkout(&req, &view).map_err(|fields: Vec<FieldError>| ApiError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        body: ErrorResponse { code: "invalid".into(), message: "Please fix the highlighted fields.".into(), fields },
    })?;

    let order_id = uuid::Uuid::new_v4().to_string();
    {
        let (id, o, t) = (order_id.clone(), order.clone(), ts(now));
        st.db.call(move |c| db::insert_order(c, &id, &o, &t)).await?;
    }

    // Product images only work if Stripe can fetch them, i.e. a public https BASE_URL.
    let public_images = st.base_url.starts_with("https://");
    let session_req = SessionRequest {
        order_id: order_id.clone(),
        email: order.email.clone(),
        lines: order
            .lines
            .iter()
            .map(|l| SessionLine {
                name: l.name.clone(),
                unit_amount: l.unit_price_cents,
                qty: l.qty,
                image_url: public_images.then(|| format!("{}{}", st.base_url, l.image_url)),
            })
            .collect(),
        success_url: format!("{}/success?order={order_id}&session_id={{CHECKOUT_SESSION_ID}}", st.base_url),
        cancel_url: format!("{}/cancel?order={order_id}", st.base_url),
        expires_at: now.timestamp() + SESSION_TTL_SECS,
    };

    match st.provider.create_session(&session_req).await {
        Ok(session) => {
            let (id, sid) = (order_id.clone(), session.id.clone());
            st.db.call(move |c| db::set_session_id(c, &id, &sid)).await?;
            tracing::info!(order_id, session_id = session.id, total_cents = order.total_cents, "checkout session created");
            Ok(Json(CheckoutResponse { checkout_url: session.url }))
        }
        Err(e) => {
            tracing::error!(order_id, "creating Stripe session failed: {e:#}");
            let id = order_id.clone();
            st.db.call(move |c| db::mark_failed(c, &id)).await?;
            Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "payment_unavailable",
                "We couldn't start the payment page. Please try again in a moment.",
            ))
        }
    }
}

#[derive(Deserialize)]
struct StatusQuery {
    session_id: String,
}

async fn load_order(st: &AppState, id: &str) -> anyhow::Result<Option<db::OrderRow>> {
    let id = id.to_string();
    st.db.call(move |c| db::get_order(c, &id)).await
}

/// Success-page poll. If the webhook hasn't landed yet, ask Stripe directly.
async fn order_status(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<StatusQuery>,
) -> Result<Json<OrderStatusResponse>, ApiError> {
    let matches = |o: &db::OrderRow| o.stripe_session_id.as_deref() == Some(q.session_id.as_str());
    let mut order = load_order(&st, &id).await?.filter(matches).ok_or_else(ApiError::not_found)?;

    if order.status == OrderStatus::Pending {
        match st.provider.retrieve_session(&q.session_id).await {
            Ok(session) => {
                reconcile(&st, &session).await?;
                order = load_order(&st, &id).await?.ok_or_else(ApiError::not_found)?;
            }
            Err(e) => tracing::warn!(order_id = id, "reconcile with Stripe failed: {e:#}"),
        }
    }

    Ok(Json(OrderStatusResponse {
        order_id: order.id,
        status: order.status,
        total_cents: order.total_cents,
        email: order.email,
        lines: order.lines,
        delivery: order.delivery,
        ship_to: order.ship_to,
        gift_message: order.gift_message,
    }))
}

async fn reconcile(st: &AppState, session: &SessionInfo) -> anyhow::Result<()> {
    let now = ts((st.now)());
    if session.payment_status == "paid" {
        let s = session.clone();
        let outcome = st.db.call(move |c| db::apply_paid(c, None, &s, &now)).await?;
        log_outcome(&session.id, &outcome);
    } else if session.status == "expired" {
        if let Some(order_id) = session.order_id.clone() {
            st.db.call(move |c| db::apply_expired(c, None, &order_id, &now)).await?;
        }
    }
    Ok(())
}

fn log_outcome(session_id: &str, outcome: &PaidOutcome) {
    match outcome {
        PaidOutcome::AmountMismatch => tracing::error!(session_id, "PAYMENT AMOUNT MISMATCH: order flagged needs_review"),
        PaidOutcome::UnknownOrder => tracing::warn!(session_id, "paid session for unknown order"),
        other => tracing::info!(session_id, ?other, "payment recorded"),
    }
}

async fn webhook(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Result<StatusCode, ApiError> {
    let now = (st.now)();
    let signature = headers.get("stripe-signature").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !stripe::verify_signature(&st.webhook_secret, signature, &body, now.timestamp(), WEBHOOK_TOLERANCE_SECS) {
        tracing::warn!("webhook with bad signature rejected");
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "bad_signature", "Invalid signature."));
    }
    let event: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "bad_payload", "Invalid payload."))?;
    let (Some(event_id), Some(kind)) = (event["id"].as_str(), event["type"].as_str()) else {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "bad_payload", "Invalid payload."));
    };
    let (event_id, kind) = (event_id.to_string(), kind.to_string());
    let now_s = ts(now);

    match kind.as_str() {
        "checkout.session.completed" | "checkout.session.async_payment_succeeded" => {
            let session = SessionInfo::from_json(&event["data"]["object"])
                .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "bad_payload", "Invalid payload."))?;
            if session.payment_status == "paid" {
                let s = session.clone();
                let (e, k) = (event_id.clone(), kind.clone());
                let outcome = st.db.call(move |c| db::apply_paid(c, Some((&e, &k)), &s, &now_s)).await?;
                log_outcome(&session.id, &outcome);
            } else {
                // e.g. delayed payment methods; async_payment_succeeded follows later.
                tracing::info!(session_id = session.id, payment_status = session.payment_status, "session completed but not yet paid");
            }
        }
        "checkout.session.expired" => {
            if let Some(session) = SessionInfo::from_json(&event["data"]["object"]) {
                if let Some(order_id) = session.order_id {
                    let (e, k) = (event_id.clone(), kind.clone());
                    st.db.call(move |c| db::apply_expired(c, Some((&e, &k)), &order_id, &now_s)).await?;
                }
            }
        }
        "checkout.session.async_payment_failed" => {
            tracing::warn!(event_id, "delayed payment failed; order stays pending until the session expires");
        }
        _ => {}
    }
    Ok(StatusCode::OK)
}
