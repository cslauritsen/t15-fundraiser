//! End-to-end HTTP tests: real router and SQLite (in memory), fake Stripe.

use anyhow::Result;
use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use chrono::{DateTime, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, atomic::{AtomicI64, Ordering}};
use std::time::Duration;
use t15_fundraiser::{
    AppState,
    admin::AdminOidc,
    catalog::Catalog,
    db::{self, Db},
    ratelimit::RateLimiter,
    router,
    stripe::{CreatedSession, PaymentProvider, SessionInfo, SessionRequest, sign},
};
use axum_extra::extract::cookie::{Cookie, Key, PrivateCookieJar};
use axum::response::IntoResponse;
use tower::ServiceExt;

const CATALOG: &str = r#"
closes_at: "2026-10-30T23:59:59-04:00"
delivery_note: "Delivery Nov 16-24."
shipping_note: "Ships when it ships."
fulfillment:
  local_zip_prefixes: ["441"]
items:
  - id: wreath
    name: Wreath
    price_cents: 3500
    fulfillment: scout_delivery
    max_qty: 5
    image: wreath.jpg
    image_alt: A wreath
  - id: box
    name: Boxed Wreath
    price_cents: 5500
    fulfillment: direct_ship
    image: box.jpg
    image_alt: A boxed wreath
admins: ["admin@example.com"]
annual_fee:
  scouting_year: "2026-2027"
  amount_cents: 5000
  closes_at: "2027-01-31T23:59:59-05:00"
  max_scouts: 3
  note: "Covers campouts."
"#;

const ADMIN: &str = "admin@example.com";

const SECRET: &str = "whsec_test";

#[derive(Default)]
struct FakeStripe {
    created: Mutex<Vec<(String, i64)>>, // (order_id, total)
    requests: Mutex<Vec<SessionRequest>>,
    to_retrieve: Mutex<Option<SessionInfo>>,
    fail_create: Mutex<bool>,
}

#[async_trait]
impl PaymentProvider for FakeStripe {
    async fn create_session(&self, req: &SessionRequest) -> Result<CreatedSession> {
        if *self.fail_create.lock().unwrap() {
            anyhow::bail!("stripe down");
        }
        let total = req.lines.iter().map(|l| l.unit_amount * i64::from(l.qty)).sum();
        self.created.lock().unwrap().push((req.order_id.clone(), total));
        self.requests.lock().unwrap().push(req.clone());
        let id = format!("cs_{}", req.order_id);
        Ok(CreatedSession { url: format!("https://checkout.stripe.test/{id}"), id })
    }

    async fn retrieve_session(&self, _id: &str) -> Result<SessionInfo> {
        self.to_retrieve.lock().unwrap().clone().ok_or_else(|| anyhow::anyhow!("no session"))
    }
}

struct Harness {
    app: Router,
    db: Db,
    stripe: Arc<FakeStripe>,
    clock: Arc<AtomicI64>,
    cookie_key: Key,
}

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn harness() -> Harness {
    harness_with(CATALOG)
}

fn harness_with(catalog: &str) -> Harness {
    let stripe = Arc::new(FakeStripe::default());
    let db = Db::open_in_memory().unwrap();
    let clock = Arc::new(AtomicI64::new(utc("2026-10-01T12:00:00Z").timestamp()));
    let c = clock.clone();
    let cookie_key = Key::generate();
    let state = AppState {
        catalog: Arc::new(Catalog::parse(catalog, |_| true).unwrap()),
        db: db.clone(),
        provider: stripe.clone(),
        webhook_secret: Arc::new(SECRET.into()),
        base_url: "https://fundraiser.test".into(),
        limiter: Arc::new(RateLimiter::new(10, Duration::from_secs(60))),
        now: Arc::new(move || Utc.timestamp_opt(c.load(Ordering::SeqCst), 0).unwrap()),
        trust_proxy: false,
        admin_oidc: Arc::new(AdminOidc::dummy_for_tests("https://fundraiser.test")),
        cookie_key: cookie_key.clone(),
    };
    Harness { app: router(state, None), db, stripe, clock, cookie_key }
}

async fn send(h: &Harness, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let b = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn order_body(lines: Value) -> Value {
    json!({
        "lines": lines,
        "email": "Pat@Example.com",
        "buyer_name": "Pat Smith",
        "phone": "(216) 555-0142",
        "scout_name": "Alex",
        "delivery": {"street": "1 Main St", "city": "Cleveland", "state": "OH", "zip": "44101"},
        "shipping": {"name": "Sam Jones", "line1": "9 Elm St", "line2": "Apt 2", "city": "Akron", "state": "OH", "postal_code": "44301"},
        "gift_message": "Merry Christmas!"
    })
}

fn wreaths(n: u32) -> Value {
    json!([{"item_id": "wreath", "qty": n}])
}

/// Place an order; returns (order_id, session_id).
async fn place(h: &Harness, lines: Value) -> (String, String) {
    let (status, body) = send(h, Method::POST, "/api/checkout", Some(order_body(lines))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (order_id, _) = h.stripe.created.lock().unwrap().last().cloned().unwrap();
    assert_eq!(body["checkout_url"], format!("https://checkout.stripe.test/cs_{order_id}"));
    let sid = format!("cs_{order_id}");
    (order_id, sid)
}

async fn order(h: &Harness, id: &str) -> db::OrderRow {
    let id = id.to_string();
    h.db.call(move |c| db::get_order(c, &id)).await.unwrap().unwrap()
}

fn completed_event(event_id: &str, order_id: &str, session_id: &str, amount: i64) -> Value {
    json!({
        "id": event_id,
        "type": "checkout.session.completed",
        "data": {"object": {
            "id": session_id, "client_reference_id": order_id, "status": "complete",
            "payment_status": "paid", "amount_total": amount, "payment_intent": "pi_123"
        }}
    })
}

async fn webhook(h: &Harness, event: &Value) -> StatusCode {
    let payload = event.to_string();
    let sig = sign(SECRET, h.clock.load(Ordering::SeqCst), payload.as_bytes());
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/stripe/webhook")
        .header("stripe-signature", sig)
        .body(Body::from(payload))
        .unwrap();
    h.app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn catalog_is_served_and_open() {
    let h = harness();
    let (s, body) = send(&h, Method::GET, "/api/catalog", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["open"], true);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["items"][0]["image_url"], "/images/wreath.jpg");
}

#[tokio::test]
async fn checkout_creates_pending_order_with_server_side_total() {
    let h = harness();
    let (order_id, _) = place(&h, wreaths(2)).await;
    let (_, total) = h.stripe.created.lock().unwrap()[0].clone();
    assert_eq!(total, 7000);
    let o = order(&h, &order_id).await;
    assert_eq!(o.status.as_str(), "pending");
    assert_eq!(o.email, "pat@example.com");
    assert_eq!(o.phone, "216-555-0142");
    assert_eq!(o.total_cents, 7000);
    assert_eq!(o.lines.len(), 1);
    // Delivery-only cart: the shipping address and gift message sent anyway are not stored.
    assert!(o.ship_to.is_none() && o.gift_message.is_none());
    assert_eq!(o.delivery.unwrap().zip, "44101");
    assert_eq!(o.stripe_session_id.as_deref(), Some(format!("cs_{order_id}").as_str()));
}

#[tokio::test]
async fn client_supplied_prices_are_ignored() {
    let h = harness();
    let mut body = order_body(wreaths(1));
    body["lines"][0]["price_cents"] = json!(1);
    body["total_cents"] = json!(1);
    let (s, _) = send(&h, Method::POST, "/api/checkout", Some(body)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.stripe.created.lock().unwrap()[0].1, 3500);
}

#[tokio::test]
async fn mixed_cart_stores_delivery_shipping_and_gift_message() {
    let h = harness();
    let (order_id, _) = place(&h, json!([{"item_id": "wreath", "qty": 1}, {"item_id": "box", "qty": 1}])).await;
    let o = order(&h, &order_id).await;
    assert_eq!(o.delivery.unwrap().street, "1 Main St");
    let ship = o.ship_to.unwrap();
    assert_eq!((ship.name.as_str(), ship.line2.as_deref(), ship.state.as_str()), ("Sam Jones", Some("Apt 2"), "OH"));
    assert_eq!(o.gift_message.as_deref(), Some("Merry Christmas!"));
}

#[tokio::test]
async fn shipping_outside_contiguous_us_or_long_gift_message_is_rejected_before_payment() {
    let h = harness();
    let mut body = order_body(json!([{"item_id": "box", "qty": 1}]));
    body["shipping"]["state"] = json!("HI");
    body["gift_message"] = json!("This message is way too long");
    let (s, resp) = send(&h, Method::POST, "/api/checkout", Some(body)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let fields: Vec<&str> = resp["fields"].as_array().unwrap().iter().map(|f| f["field"].as_str().unwrap()).collect();
    assert_eq!(fields, ["shipping.state", "gift_message"], "{resp}");
    assert!(h.stripe.created.lock().unwrap().is_empty(), "no Stripe session, so nothing to refund");
}

#[tokio::test]
async fn invalid_checkout_returns_field_errors_and_creates_nothing() {
    let h = harness();
    let mut body = order_body(wreaths(1));
    body["delivery"]["zip"] = json!("90210");
    body["email"] = json!("nope");
    let (s, resp) = send(&h, Method::POST, "/api/checkout", Some(body)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let fields: Vec<&str> = resp["fields"].as_array().unwrap().iter().map(|f| f["field"].as_str().unwrap()).collect();
    assert!(fields.contains(&"email") && fields.contains(&"delivery.zip"), "{resp}");
    assert!(h.stripe.created.lock().unwrap().is_empty());
    let n: i64 = h.db.call(|c| c.query_row("SELECT COUNT(*) FROM orders", [], |r| r.get(0))).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn checkout_closes_at_cutoff() {
    let h = harness();
    h.clock.store(utc("2026-10-30T23:59:59-04:00").timestamp(), Ordering::SeqCst);
    let (s, _) = send(&h, Method::POST, "/api/checkout", Some(order_body(wreaths(1)))).await;
    assert_eq!(s, StatusCode::OK);
    h.clock.store(utc("2026-10-31T00:00:00-04:00").timestamp(), Ordering::SeqCst);
    let (s, body) = send(&h, Method::POST, "/api/checkout", Some(order_body(wreaths(1)))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(body["code"], "closed");
    let (_, cat) = send(&h, Method::GET, "/api/catalog", None).await;
    assert_eq!(cat["open"], false);
}

#[tokio::test]
async fn stripe_failure_marks_order_failed() {
    let h = harness();
    *h.stripe.fail_create.lock().unwrap() = true;
    let (s, body) = send(&h, Method::POST, "/api/checkout", Some(order_body(wreaths(1)))).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(body["code"], "payment_unavailable");
    let status: String = h.db.call(|c| c.query_row("SELECT status FROM orders", [], |r| r.get(0))).await.unwrap();
    assert_eq!(status, "failed");
}

#[tokio::test]
async fn webhook_rejects_bad_signature() {
    let h = harness();
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/stripe/webhook")
        .header("stripe-signature", "t=1,v1=00")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(h.app.clone().oneshot(req).await.unwrap().status(), StatusCode::BAD_REQUEST);
    let req = Request::builder().method(Method::POST).uri("/api/stripe/webhook").body(Body::from("{}")).unwrap();
    assert_eq!(h.app.clone().oneshot(req).await.unwrap().status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn webhook_marks_paid_once_and_is_idempotent() {
    let h = harness();
    let (order_id, sid) = place(&h, wreaths(2)).await;
    let ev = completed_event("evt_1", &order_id, &sid, 7000);
    assert_eq!(webhook(&h, &ev).await, StatusCode::OK);
    let o = order(&h, &order_id).await;
    assert_eq!(o.status.as_str(), "paid");
    assert!(o.paid_at.is_some());

    // Redelivery of the same event, and a different event for the same session, are no-ops.
    let paid_at = o.paid_at.clone();
    h.clock.fetch_add(60, Ordering::SeqCst);
    assert_eq!(webhook(&h, &ev).await, StatusCode::OK);
    assert_eq!(webhook(&h, &completed_event("evt_2", &order_id, &sid, 7000)).await, StatusCode::OK);
    assert_eq!(order(&h, &order_id).await.paid_at, paid_at);
    let events: i64 = h.db.call(|c| c.query_row("SELECT COUNT(*) FROM stripe_events", [], |r| r.get(0))).await.unwrap();
    assert_eq!(events, 2, "each distinct event id is recorded once; the redelivery added nothing");
}

#[tokio::test]
async fn amount_mismatch_flags_order_instead_of_marking_paid() {
    let h = harness();
    let (order_id, sid) = place(&h, wreaths(2)).await;
    assert_eq!(webhook(&h, &completed_event("evt_m", &order_id, &sid, 100)).await, StatusCode::OK);
    let o = order(&h, &order_id).await;
    assert_eq!(o.status.as_str(), "needs_review");
    assert!(o.review_reason.unwrap().contains("100"));
}

#[tokio::test]
async fn unpaid_completed_session_is_ignored_until_async_success() {
    let h = harness();
    let (order_id, sid) = place(&h, wreaths(1)).await;
    let mut ev = completed_event("evt_u", &order_id, &sid, 3500);
    ev["data"]["object"]["payment_status"] = json!("unpaid");
    assert_eq!(webhook(&h, &ev).await, StatusCode::OK);
    assert_eq!(order(&h, &order_id).await.status.as_str(), "pending");

    let mut ev = completed_event("evt_ok", &order_id, &sid, 3500);
    ev["type"] = json!("checkout.session.async_payment_succeeded");
    assert_eq!(webhook(&h, &ev).await, StatusCode::OK);
    assert_eq!(order(&h, &order_id).await.status.as_str(), "paid");
}

#[tokio::test]
async fn expired_event_expires_pending_but_never_paid_orders() {
    let h = harness();
    let (pending_id, pending_sid) = place(&h, wreaths(1)).await;
    let (paid_id, paid_sid) = place(&h, wreaths(1)).await;
    webhook(&h, &completed_event("evt_p", &paid_id, &paid_sid, 3500)).await;

    for (n, (id, sid)) in [(&pending_id, &pending_sid), (&paid_id, &paid_sid)].into_iter().enumerate() {
        let ev = json!({"id": format!("evt_x{n}"), "type": "checkout.session.expired",
            "data": {"object": {"id": sid, "client_reference_id": id, "status": "expired", "payment_status": "unpaid"}}});
        assert_eq!(webhook(&h, &ev).await, StatusCode::OK);
    }
    assert_eq!(order(&h, &pending_id).await.status.as_str(), "expired");
    assert_eq!(order(&h, &paid_id).await.status.as_str(), "paid");
}

#[tokio::test]
async fn success_page_reconciles_when_webhook_is_late() {
    let h = harness();
    let (order_id, sid) = place(&h, wreaths(2)).await;
    let uri = format!("/api/orders/{order_id}/status?session_id={sid}");

    // Stripe says still open: order stays pending.
    *h.stripe.to_retrieve.lock().unwrap() = Some(SessionInfo {
        id: sid.clone(), order_id: Some(order_id.clone()), status: "open".into(),
        payment_status: "unpaid".into(), amount_total: 7000, payment_intent: None,
        ..Default::default()
    });
    let (s, body) = send(&h, Method::GET, &uri, None).await;
    assert_eq!((s, body["status"].as_str()), (StatusCode::OK, Some("pending")));

    // Stripe says paid: reconciled without any webhook.
    h.stripe.to_retrieve.lock().unwrap().as_mut().unwrap().payment_status = "paid".into();
    let (_, body) = send(&h, Method::GET, &uri, None).await;
    assert_eq!(body["status"], "paid");
    assert_eq!(body["total_cents"], 7000);
    assert_eq!(body["lines"][0]["name"], "Wreath");
    assert_eq!(order(&h, &order_id).await.status.as_str(), "paid");
}

#[tokio::test]
async fn status_requires_matching_session_id() {
    let h = harness();
    let (order_id, _) = place(&h, wreaths(1)).await;
    let (s, _) = send(&h, Method::GET, &format!("/api/orders/{order_id}/status?session_id=cs_wrong"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = send(&h, Method::GET, "/api/orders/nope/status?session_id=cs_x", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn checkout_is_rate_limited() {
    let h = harness();
    for _ in 0..10 {
        let (s, _) = send(&h, Method::POST, "/api/checkout", Some(order_body(wreaths(1)))).await;
        assert_eq!(s, StatusCode::OK);
    }
    let (s, body) = send(&h, Method::POST, "/api/checkout", Some(order_body(wreaths(1)))).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["code"], "rate_limited");
}

#[tokio::test]
async fn unknown_api_paths_are_json_404() {
    let h = harness();
    let (s, body) = send(&h, Method::GET, "/api/nope", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_found");
}

#[tokio::test]
async fn admin_page_without_session_redirects_to_google_login() {
    let h = harness();
    let req = Request::builder().method(Method::GET).uri("/admin").body(Body::empty()).unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    assert!(location.starts_with("https://accounts.google.com/"), "{location}");
    // A short-lived, encrypted flow cookie is set to carry CSRF/nonce/PKCE state to the callback.
    let set_cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap();
    assert!(set_cookie.starts_with("admin_oidc_flow="), "{set_cookie}");
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
}

#[tokio::test]
async fn admin_export_without_session_redirects_to_admin() {
    let h = harness();
    let req = Request::builder().method(Method::GET).uri("/admin/export.csv").body(Body::empty()).unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(resp.headers().get("location").unwrap(), "/admin");
}

#[tokio::test]
async fn admin_export_with_bogus_cookie_is_treated_as_signed_out() {
    let h = harness();
    let req = Request::builder()
        .method(Method::GET)
        .uri("/admin/export.csv")
        .header("cookie", "admin_session=not-a-valid-encrypted-value")
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(resp.headers().get("location").unwrap(), "/admin");
}

#[tokio::test]
async fn admin_logout_clears_session_and_redirects() {
    let h = harness();
    let req = Request::builder().method(Method::GET).uri("/admin/logout").body(Body::empty()).unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(resp.headers().get("location").unwrap(), "/");
    let set_cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap();
    assert!(set_cookie.starts_with("admin_session="), "{set_cookie}");
    assert!(set_cookie.contains("Max-Age=0"), "{set_cookie}");
}

#[tokio::test]
async fn export_lists_paid_orders() {
    let h = harness();
    let (order_id, sid) = place(&h, json!([{"item_id": "wreath", "qty": 2}, {"item_id": "box", "qty": 1}])).await;
    place(&h, wreaths(1)).await; // stays pending: must not appear
    webhook(&h, &completed_event("evt_e", &order_id, &sid, 12500)).await;

    let rows = h.db.call(|c| db::list_orders_for_export(c)).await.unwrap();
    assert_eq!(rows.len(), 1);
    let mut out = Vec::new();
    t15_fundraiser::export::write_csv(&rows, &["box".to_string(), "wreath".to_string()], &mut out).unwrap();
    assert!(out.starts_with(b"\xEF\xBB\xBF"), "UTF-8 BOM for Excel");
    let csv = String::from_utf8(out).unwrap();
    let mut rdr = csv::ReaderBuilder::new().from_reader(csv.trim_start_matches('\u{feff}').as_bytes());
    let headers = rdr.headers().unwrap().clone();
    let record = rdr.records().next().unwrap().unwrap();
    let col = |name: &str| record.get(headers.iter().position(|h| h == name).expect(name)).unwrap().to_string();
    assert_eq!((col("wreath"), col("box")), ("2".to_string(), "1".to_string()), "{csv}");
    assert!(headers.iter().position(|h| h == "box") < headers.iter().position(|h| h == "wreath"), "catalog order");
    assert!(!csv.contains("Boxed Wreath"), "product names are not exported: {csv}");
    assert!(csv.contains("Sam Jones, 9 Elm St, Apt 2, Akron, OH 44301"), "{csv}");
    assert!(csv.contains("Merry Christmas!"), "{csv}");
    assert!(csv.contains("$125.00"), "{csv}");
    assert!(csv.contains("1 Main St, Cleveland, OH 44101"), "{csv}");
}

// ---------------------------------------------------------------------------------------------
// Annual camping fee
// ---------------------------------------------------------------------------------------------

fn fee_body(scouts: &[(&str, &str)]) -> Value {
    json!({
        "payer_name": "Pat Smith",
        "payer_email": "Pat@Example.com",
        "scouts": scouts.iter().map(|(f, l)| json!({"first_name": f, "last_name": l})).collect::<Vec<_>>(),
    })
}

/// Start a fee checkout; returns (payment_id, session_id, what was sent to Stripe).
async fn start_fee(h: &Harness, scouts: &[(&str, &str)]) -> (String, String, SessionRequest) {
    let (status, body) = send(h, Method::POST, "/api/annual-fee/checkout", Some(fee_body(scouts))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let req = h.stripe.requests.lock().unwrap().last().cloned().unwrap();
    let sid = format!("cs_{}", req.order_id);
    assert_eq!(body["checkout_url"], format!("https://checkout.stripe.test/{sid}"));
    (req.order_id.clone(), sid, req)
}

/// The Checkout Session object Stripe would send back for `req`.
fn fee_session(req: &SessionRequest, amount_total: i64, payment_status: &str) -> Value {
    let mut metadata: serde_json::Map<String, Value> =
        req.metadata.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    metadata.insert("order_id".into(), json!(req.order_id));
    json!({
        "id": format!("cs_{}", req.order_id),
        "client_reference_id": req.order_id,
        "status": if payment_status == "paid" { "complete" } else { "open" },
        "payment_status": payment_status,
        "amount_total": amount_total,
        "payment_intent": "pi_fee",
        "customer_email": req.email,
        "customer_details": {"email": req.email},
        "metadata": metadata,
    })
}

fn event(event_id: &str, kind: &str, object: Value) -> Value {
    json!({"id": event_id, "type": kind, "data": {"object": object}})
}

async fn fee_rows(h: &Harness) -> Vec<db::AnnualFeeRow> {
    h.db.call(|c| db::list_annual_fees(c, None)).await.unwrap()
}

async fn count(h: &Harness, table: &'static str) -> i64 {
    h.db.call(move |c| c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))).await.unwrap()
}

#[tokio::test]
async fn fee_info_is_served_with_its_own_cutoff() {
    let h = harness();
    let (s, body) = send(&h, Method::GET, "/api/annual-fee", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["scouting_year"], "2026-2027");
    assert_eq!(body["amount_cents"], 5000);
    assert_eq!(body["max_scouts"], 3);
    assert_eq!(body["note"], "Covers campouts.");
    assert_eq!(body["closes_at"], "2027-01-31T23:59:59-05:00");
    assert_eq!(body["open"], true);

    // The greenery cutoff doesn't apply to fees.
    h.clock.store(utc("2026-12-01T12:00:00Z").timestamp(), Ordering::SeqCst);
    let (_, cat) = send(&h, Method::GET, "/api/catalog", None).await;
    let (_, fee) = send(&h, Method::GET, "/api/annual-fee", None).await;
    assert_eq!((cat["open"].as_bool(), fee["open"].as_bool()), (Some(false), Some(true)));
}

#[tokio::test]
async fn fee_endpoints_404_when_not_configured() {
    let without = CATALOG.split("annual_fee:").next().unwrap();
    let h = harness_with(without);
    let (s, body) = send(&h, Method::GET, "/api/annual-fee", None).await;
    assert_eq!((s, body["code"].as_str()), (StatusCode::NOT_FOUND, Some("not_found")));
    let (s, _) = send(&h, Method::POST, "/api/annual-fee/checkout", Some(fee_body(&[("Alex", "Smith")]))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = send(&h, Method::GET, "/api/annual-fee/af_x/status?session_id=cs_x", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(h.stripe.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn fee_checkout_creates_session_per_scout_and_writes_nothing() {
    let h = harness();
    let now = h.clock.load(Ordering::SeqCst);
    let (payment_id, _, req) = start_fee(&h, &[(" Alex ", "Smith"), ("Jamie", "Smith")]).await;

    assert!(payment_id.starts_with("af_") && payment_id.len() == 3 + 36, "{payment_id}");
    assert_eq!(req.email, "pat@example.com");
    assert_eq!(req.description, format!("Troop 15 annual camping fee {payment_id}"));
    assert_eq!(req.lines.len(), 2);
    for (l, name) in req.lines.iter().zip(["Alex Smith", "Jamie Smith"]) {
        assert_eq!(l.name, format!("Annual camping fee 2026-2027 — {name}"));
        assert_eq!((l.unit_amount, l.qty, l.image_url.as_deref()), (5000, 1, None));
    }
    let meta: std::collections::HashMap<&str, &str> = req.metadata.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(meta["kind"], "annual_fee");
    assert_eq!(meta["scouting_year"], "2026-2027");
    assert_eq!(meta["amount_cents"], "5000");
    assert_eq!(meta["scout_count"], "2");
    assert_eq!(meta["scout_0"], "Alex\tSmith");
    assert_eq!(meta["scout_1"], "Jamie\tSmith");
    assert_eq!(meta["payer_name"], "Pat Smith");
    assert_eq!(
        req.success_url,
        format!("https://fundraiser.test/annual-fee/success?payment={payment_id}&session_id={{CHECKOUT_SESSION_ID}}")
    );
    assert_eq!(req.cancel_url, "https://fundraiser.test/annual-fee/cancel");
    assert_eq!(req.expires_at, now + 31 * 60);

    assert_eq!((count(&h, "annual_fees").await, count(&h, "orders").await), (0, 0), "nothing stored before payment");
}

#[tokio::test]
async fn fee_checkout_ignores_client_supplied_amounts() {
    let h = harness();
    let mut body = fee_body(&[("Alex", "Smith")]);
    body["amount_cents"] = json!(1);
    body["total_cents"] = json!(1);
    body["scouts"][0]["amount_cents"] = json!(1);
    let (s, _) = send(&h, Method::POST, "/api/annual-fee/checkout", Some(body)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.stripe.created.lock().unwrap()[0].1, 5000);
}

#[tokio::test]
async fn invalid_fee_checkout_is_400_with_fields_and_no_session() {
    let h = harness();
    let mut body = fee_body(&[("Alex", "Smith"), ("alex", "SMITH"), ("", "Jones")]);
    body["payer_email"] = json!("nope");
    let (s, resp) = send(&h, Method::POST, "/api/annual-fee/checkout", Some(body)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(resp["code"], "invalid");
    let fields: Vec<&str> = resp["fields"].as_array().unwrap().iter().map(|f| f["field"].as_str().unwrap()).collect();
    assert_eq!(fields, ["payer_email", "scouts", "scouts[2].first_name"], "{resp}");

    // max_scouts is 3 in the test config.
    let four = fee_body(&[("A", "Smith"), ("B", "Smith"), ("C", "Smith"), ("D", "Smith")]);
    let (s, resp) = send(&h, Method::POST, "/api/annual-fee/checkout", Some(four)).await;
    assert_eq!((s, resp["fields"][0]["field"].as_str()), (StatusCode::BAD_REQUEST, Some("scouts")));
    assert!(h.stripe.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn fee_checkout_closes_at_its_cutoff() {
    let h = harness();
    h.clock.store(utc("2027-01-31T23:59:59-05:00").timestamp(), Ordering::SeqCst);
    start_fee(&h, &[("Alex", "Smith")]).await;
    h.clock.store(utc("2027-02-01T00:00:00-05:00").timestamp(), Ordering::SeqCst);
    let (s, body) = send(&h, Method::POST, "/api/annual-fee/checkout", Some(fee_body(&[("Alex", "Smith")]))).await;
    assert_eq!((s, body["code"].as_str()), (StatusCode::CONFLICT, Some("closed")));
    let (_, info) = send(&h, Method::GET, "/api/annual-fee", None).await;
    assert_eq!(info["open"], false);
    assert_eq!(h.stripe.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn fee_name_check_flags_exact_and_prefix_matches_case_insensitively() {
    let h = harness();
    let (_, _, req) = start_fee(&h, &[("Alexander", "Smith"), ("Alex", "Smith")]).await;
    let ev = event("evt_n1", "checkout.session.completed", fee_session(&req, 10000, "paid"));
    assert_eq!(webhook(&h, &ev).await, StatusCode::OK);

    let check = |f: &'static str, l: &'static str| {
        let h = &h;
        async move { send(h, Method::GET, &format!("/api/annual-fee/check-name?first_name={f}&last_name={l}"), None).await }
    };
    let (s, b) = check("aLEX", "smith").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["exact"], true);
    assert_eq!(b["similar"], json!([{"first_name": "Alexander", "last_name": "Smith"}]));

    let (_, b) = check("Al", "SMITH").await;
    assert_eq!((b["exact"].as_bool(), b["similar"].as_array().unwrap().len()), (Some(false), 2));

    // Different last name, or a first name that merely contains the text, is not a match;
    // LIKE wildcards are literal.
    for (f, l) in [("Alex", "Smyth"), ("lex", "Smith"), ("%", "Smith"), ("A_ex", "Smith")] {
        let (_, b) = check(f, l).await;
        assert_eq!((b["exact"].as_bool(), b["similar"].as_array().unwrap().len()), (Some(false), 0), "{f} {l}");
    }
}

#[tokio::test]
async fn paid_fee_webhook_records_one_paid_row_per_scout() {
    let h = harness();
    let (payment_id, sid, req) = start_fee(&h, &[("Alex", "Smith"), ("Jamie", "Smith")]).await;
    let ev = event("evt_f1", "checkout.session.completed", fee_session(&req, 10000, "paid"));
    assert_eq!(webhook(&h, &ev).await, StatusCode::OK);

    let rows = fee_rows(&h).await;
    assert_eq!(rows.len(), 2);
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r.status, shared::FeeStatus::Paid);
        assert_eq!((r.payment_id.as_str(), r.line_no), (payment_id.as_str(), i as i64));
        assert_eq!((r.scouting_year.as_str(), r.amount_cents), ("2026-2027", 5000));
        assert_eq!((r.payer_name.as_str(), r.payer_email.as_str()), ("Pat Smith", "pat@example.com"));
        assert_eq!((r.stripe_session_id.as_str(), r.stripe_payment_intent_id.as_deref()), (sid.as_str(), Some("pi_fee")));
        assert_eq!(r.paid_at, "2026-10-01T12:00:00Z");
        assert!(r.review_reason.is_none());
    }
    assert_eq!((rows[0].scout_first_name.as_str(), rows[1].scout_first_name.as_str()), ("Alex", "Jamie"));
    assert_eq!(count(&h, "orders").await, 0, "fees never touch orders");

    let (s, body) = send(&h, Method::GET, &format!("/api/annual-fee/{payment_id}/status?session_id={sid}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["status"], "paid");
    assert_eq!(body["total_cents"], 10000);
    assert_eq!(body["payer_email"], "pat@example.com");
    assert_eq!(body["scouting_year"], "2026-2027");
    assert_eq!(body["scouts"][1], json!({"first_name": "Jamie", "last_name": "Smith"}));
}

#[tokio::test]
async fn duplicate_fee_webhooks_are_no_ops() {
    let h = harness();
    let (_, _, req) = start_fee(&h, &[("Alex", "Smith"), ("Jamie", "Smith")]).await;
    let ev = event("evt_d1", "checkout.session.completed", fee_session(&req, 10000, "paid"));
    webhook(&h, &ev).await;
    let before = fee_rows(&h).await;

    h.clock.fetch_add(60, Ordering::SeqCst);
    assert_eq!(webhook(&h, &ev).await, StatusCode::OK, "redelivery");
    let other = event("evt_d2", "checkout.session.async_payment_succeeded", fee_session(&req, 10000, "paid"));
    assert_eq!(webhook(&h, &other).await, StatusCode::OK, "a second event for the same session");

    assert_eq!(fee_rows(&h).await, before, "same rows, same paid_at");
    assert_eq!(count(&h, "stripe_events").await, 2);
}

#[tokio::test]
async fn status_reconciliation_and_webhook_record_the_session_once() {
    let h = harness();
    let (payment_id, sid, req) = start_fee(&h, &[("Alex", "Smith"), ("Jamie", "Smith")]).await;
    let uri = format!("/api/annual-fee/{payment_id}/status?session_id={sid}");

    // Still open at Stripe: pending, described from the session metadata, nothing stored.
    let open = SessionInfo::from_json(&fee_session(&req, 10000, "unpaid")).unwrap();
    *h.stripe.to_retrieve.lock().unwrap() = Some(open);
    let (s, body) = send(&h, Method::GET, &uri, None).await;
    assert_eq!((s, body["status"].as_str()), (StatusCode::OK, Some("pending")));
    assert_eq!((body["total_cents"].as_i64(), body["scouts"].as_array().map(Vec::len)), (Some(10000), Some(2)));
    assert_eq!(count(&h, "annual_fees").await, 0);

    // Paid at Stripe, webhook not here yet: the status poll records it.
    let paid = SessionInfo::from_json(&fee_session(&req, 10000, "paid")).unwrap();
    *h.stripe.to_retrieve.lock().unwrap() = Some(paid);
    let (_, body) = send(&h, Method::GET, &uri, None).await;
    assert_eq!(body["status"], "paid");
    assert_eq!(count(&h, "annual_fees").await, 2);

    // Then the webhook arrives: no new rows.
    webhook(&h, &event("evt_r1", "checkout.session.completed", fee_session(&req, 10000, "paid"))).await;
    assert_eq!(count(&h, "annual_fees").await, 2);

    // Once rows exist, Stripe isn't asked again.
    *h.stripe.to_retrieve.lock().unwrap() = None;
    let (s, body) = send(&h, Method::GET, &uri, None).await;
    assert_eq!((s, body["status"].as_str()), (StatusCode::OK, Some("paid")));
}

#[tokio::test]
async fn fee_status_checks_the_session_belongs_to_the_payment() {
    let h = harness();
    let (payment_id, sid, req) = start_fee(&h, &[("Alex", "Smith")]).await;
    let (other_id, other_sid, other_req) = start_fee(&h, &[("Jamie", "Smith")]).await;

    // Stripe returns a session for a different payment: rejected, nothing recorded.
    *h.stripe.to_retrieve.lock().unwrap() = Some(SessionInfo::from_json(&fee_session(&other_req, 5000, "paid")).unwrap());
    let (s, _) = send(&h, Method::GET, &format!("/api/annual-fee/{payment_id}/status?session_id={other_sid}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(count(&h, "annual_fees").await, 0);

    // Recorded rows are only shown with their own session id.
    webhook(&h, &event("evt_s1", "checkout.session.completed", fee_session(&req, 5000, "paid"))).await;
    let (s, _) = send(&h, Method::GET, &format!("/api/annual-fee/{payment_id}/status?session_id={other_sid}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = send(&h, Method::GET, &format!("/api/annual-fee/{payment_id}/status?session_id={sid}"), None).await;
    assert_eq!(s, StatusCode::OK);

    // Order ids aren't fee payments.
    let (s, _) = send(&h, Method::GET, "/api/annual-fee/some-order-id/status?session_id=cs_x", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // An expired session reports expired.
    let mut expired = fee_session(&other_req, 5000, "unpaid");
    expired["status"] = json!("expired");
    *h.stripe.to_retrieve.lock().unwrap() = Some(SessionInfo::from_json(&expired).unwrap());
    let (_, body) = send(&h, Method::GET, &format!("/api/annual-fee/{other_id}/status?session_id={other_sid}"), None).await;
    assert_eq!(body["status"], "expired");
}

#[tokio::test]
async fn fee_amount_mismatch_records_needs_review() {
    let h = harness();
    let (payment_id, sid, req) = start_fee(&h, &[("Alex", "Smith"), ("Jamie", "Smith")]).await;
    webhook(&h, &event("evt_m1", "checkout.session.completed", fee_session(&req, 5000, "paid"))).await;
    let rows = fee_rows(&h).await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.status == shared::FeeStatus::NeedsReview));
    assert!(rows[0].review_reason.as_deref().unwrap().contains("5000"), "{:?}", rows[0].review_reason);
    let (_, body) = send(&h, Method::GET, &format!("/api/annual-fee/{payment_id}/status?session_id={sid}"), None).await;
    assert_eq!(body["status"], "needs_review");
}

#[tokio::test]
async fn expired_or_unpaid_fee_webhooks_write_nothing() {
    let h = harness();
    let (_, _, req) = start_fee(&h, &[("Alex", "Smith")]).await;
    let mut expired = fee_session(&req, 5000, "unpaid");
    expired["status"] = json!("expired");
    assert_eq!(webhook(&h, &event("evt_x1", "checkout.session.expired", expired)).await, StatusCode::OK);
    let unpaid = fee_session(&req, 5000, "unpaid");
    assert_eq!(webhook(&h, &event("evt_x2", "checkout.session.completed", unpaid)).await, StatusCode::OK);
    assert_eq!((count(&h, "annual_fees").await, count(&h, "stripe_events").await), (0, 0));
}

#[tokio::test]
async fn paid_fee_webhook_after_cutoff_is_still_recorded() {
    let h = harness();
    h.clock.store(utc("2027-01-31T23:50:00-05:00").timestamp(), Ordering::SeqCst);
    let (_, _, req) = start_fee(&h, &[("Alex", "Smith")]).await;
    h.clock.store(utc("2027-02-01T00:10:00-05:00").timestamp(), Ordering::SeqCst);
    assert_eq!(webhook(&h, &event("evt_l1", "checkout.session.completed", fee_session(&req, 5000, "paid"))).await, StatusCode::OK);
    let rows = fee_rows(&h).await;
    assert_eq!((rows.len(), rows[0].status), (1, shared::FeeStatus::Paid));
}

#[tokio::test]
async fn order_and_fee_webhooks_are_routed_by_reference_prefix() {
    let h = harness();
    let (order_id, order_sid) = place(&h, wreaths(1)).await;
    let (_, _, req) = start_fee(&h, &[("Alex", "Smith")]).await;
    webhook(&h, &completed_event("evt_o", &order_id, &order_sid, 3500)).await;
    webhook(&h, &event("evt_f", "checkout.session.completed", fee_session(&req, 5000, "paid"))).await;
    assert_eq!(order(&h, &order_id).await.status.as_str(), "paid");
    assert_eq!(count(&h, "annual_fees").await, 1);
    assert_eq!(count(&h, "orders").await, 1);
}

// ---- admin ----------------------------------------------------------------------------------

/// A `Cookie` header value for a signed-in admin, encrypted with the app's key.
fn admin_cookie(h: &Harness) -> String {
    let session = json!({"email": ADMIN, "expires_at": Utc::now().timestamp() + 3600}).to_string();
    let resp = PrivateCookieJar::new(h.cookie_key.clone()).add(Cookie::new("admin_session", session)).into_response();
    let set_cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap();
    set_cookie.split(';').next().unwrap().to_string()
}

/// GET as a signed-in admin (or signed out); returns status, headers, and body text.
async fn admin_get(h: &Harness, uri: &str, signed_in: bool) -> (StatusCode, axum::http::HeaderMap, String) {
    let mut b = Request::builder().method(Method::GET).uri(uri);
    if signed_in {
        b = b.header("cookie", admin_cookie(h));
    }
    let resp = h.app.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
}

/// Pay for `scouts` through the webhook; `amount` overrides the correct total.
async fn pay_fee(h: &Harness, scouts: &[(&str, &str)], amount: Option<i64>) {
    let (payment_id, _, req) = start_fee(h, scouts).await;
    let total = amount.unwrap_or(5000 * scouts.len() as i64);
    webhook(h, &event(&format!("evt_{payment_id}"), "checkout.session.completed", fee_session(&req, total, "paid"))).await;
}

async fn insert_old_year_fee(h: &Harness, first: &str, last: &str) {
    let row = db::AnnualFeeRow {
        payment_id: "af_old".into(),
        line_no: 0,
        status: shared::FeeStatus::Paid,
        review_reason: None,
        scouting_year: "2025-2026".into(),
        scout_first_name: first.into(),
        scout_last_name: last.into(),
        amount_cents: 4000,
        payer_name: "Old Payer".into(),
        payer_email: "old@example.com".into(),
        stripe_session_id: "cs_old".into(),
        stripe_payment_intent_id: None,
        paid_at: "2025-10-01T12:00:00Z".into(),
    };
    h.db.call(move |c| db::insert_fee_payment(c, None, &[row], "now")).await.unwrap();
}

#[tokio::test]
async fn admin_fee_routes_redirect_when_signed_out() {
    let h = harness();
    for uri in ["/admin/annual-fees", "/admin/annual-fees.csv", "/admin/annual-fees?year=2026-2027"] {
        let (s, headers, _) = admin_get(&h, uri, false).await;
        assert_eq!(s, StatusCode::SEE_OTHER, "{uri}");
        assert_eq!(headers.get("location").unwrap(), "/admin", "{uri}");
    }
}

#[tokio::test]
async fn admin_page_shows_fee_summary_and_links() {
    let h = harness();
    pay_fee(&h, &[("Alex", "Smith"), ("Jamie", "Smith")], None).await;
    pay_fee(&h, &[("Sam", "Jones")], Some(1)).await;
    let (s, headers, html) = admin_get(&h, "/admin", true).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");
    assert!(html.contains("Signed in as admin@example.com"), "{html}");
    assert!(html.contains("Download orders CSV"), "{html}");
    assert!(html.contains("Annual camping fees 2026-2027"), "{html}");
    assert!(html.contains("<th>Scouts paid</th><td class=\"n\">2</td>"), "{html}");
    // Sum of the recorded per-scout amounts, needs_review rows included (their money arrived).
    assert!(html.contains("$150.00"), "{html}");
    assert!(html.contains("<th>Checkouts</th><td class=\"n\">2</td>"), "{html}");
    assert!(html.contains("<span class=\"alert\">1</span>"), "needs review is highlighted: {html}");
    assert!(html.contains("January 31, 2027 at 11:59 PM") && html.contains("<strong>open</strong>"), "{html}");
    assert!(html.contains("href=\"/admin/annual-fees\">View annual fee report"), "{html}");
    assert!(html.contains("href=\"/admin/annual-fees.csv\">Download annual fees CSV"), "{html}");
    assert!(html.contains("Log out"));

    // Without the config block the section is omitted.
    let h = harness_with(CATALOG.split("annual_fee:").next().unwrap());
    let (_, _, html) = admin_get(&h, "/admin", true).await;
    assert!(html.contains("Download orders CSV") && !html.contains("Annual camping fees"), "{html}");
}

#[tokio::test]
async fn admin_fee_report_shows_totals_review_table_and_duplicates() {
    let h = harness();
    pay_fee(&h, &[("Zed", "Adams"), ("Alex", "Smith")], None).await;
    pay_fee(&h, &[("ALEX", "smith")], None).await; // a second parent paid for the same scout
    pay_fee(&h, &[("<b>Bobby</b>", "Tables & Co")], Some(1)).await;

    let (s, headers, html) = admin_get(&h, "/admin/annual-fees", true).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");
    assert!(html.contains("<h1>Annual camping fees 2026-2027</h1>"), "{html}");
    assert!(html.contains("<th>Scouts paid</th><td class=\"n\">3</td>"), "{html}");
    assert!(html.contains("<th>Checkouts</th><td class=\"n\">3</td>"), "{html}");
    assert!(html.contains("$200.00"), "{html}");

    // Needs review comes first and is escaped.
    let review = html.find("Needs review</h2>").expect("needs review table");
    let paid = html.find("Paid scouts (3)").expect("paid table");
    assert!(review < paid);
    assert!(html.contains("&lt;b&gt;Bobby&lt;/b&gt; Tables &amp; Co"), "{html}");
    assert!(!html.contains("<b>Bobby"), "{html}");
    assert!(html.contains("Stripe amount 1 != 1 x 5000") && html.contains("pi_fee"), "{html}");

    // Paid table: sorted by last then first name; dates local; mailto; duplicates flagged.
    let adams = html.find("<td>Adams</td>").unwrap();
    let smith = html.find("<td>Smith</td>").or_else(|| html.find("<td>smith</td>")).unwrap();
    assert!(adams < smith, "{html}");
    assert!(html.contains("<td>2026-10-01</td>"), "{html}");
    assert!(html.contains("<a href=\"mailto:pat@example.com\">pat@example.com</a>"), "{html}");
    assert_eq!(html.matches("possible duplicate").count(), 2, "{html}");
    assert!(html.contains("href=\"/admin/annual-fees.csv?year=2026-2027\""), "{html}");
    assert!(html.contains("href=\"/admin\""), "{html}");
}

#[tokio::test]
async fn admin_fee_report_and_csv_filter_by_year() {
    let h = harness();
    pay_fee(&h, &[("Alex", "Smith")], None).await;
    insert_old_year_fee(&h, "Old", "Timer").await;

    let (_, _, html) = admin_get(&h, "/admin/annual-fees", true).await;
    assert!(html.contains("Smith") && !html.contains("Timer"), "defaults to the configured year: {html}");
    assert!(html.contains("<a href=\"/admin/annual-fees?year=2025-2026\">2025-2026</a>"), "{html}");

    let (_, _, html) = admin_get(&h, "/admin/annual-fees?year=2025-2026", true).await;
    assert!(html.contains("Timer") && !html.contains(">Smith<"), "{html}");
    assert!(html.contains("$40.00") && !html.contains("Payments close"), "{html}");

    // CSV: one year, or all years sorted by year.
    let (s, headers, csv) = admin_get(&h, "/admin/annual-fees.csv?year=2026-2027", true).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(headers.get("content-disposition").unwrap(), "attachment; filename=\"annual-fees-2026-2027.csv\"");
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");
    let csv = csv.trim_start_matches('\u{feff}');
    let mut lines = csv.lines();
    assert_eq!(
        lines.next().unwrap(),
        "scouting_year,scout_last_name,scout_first_name,amount,status,paid_at,payer_name,payer_email,payment_id,stripe_payment_intent_id,review_reason"
    );
    let row = lines.next().unwrap();
    assert!(row.starts_with("2026-2027,Smith,Alex,$50.00,paid,2026-10-01T12:00:00Z,Pat Smith,pat@example.com,af_"), "{row}");
    assert!(lines.next().is_none());

    let (_, headers, csv) = admin_get(&h, "/admin/annual-fees.csv", true).await;
    assert_eq!(headers.get("content-disposition").unwrap(), "attachment; filename=\"annual-fees-all.csv\"");
    let years: Vec<&str> = csv.lines().skip(1).map(|l| l.split(',').next().unwrap()).collect();
    assert_eq!(years, ["2025-2026", "2026-2027"]);
}
