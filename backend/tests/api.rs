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
    catalog::Catalog,
    db::{self, Db},
    ratelimit::RateLimiter,
    router,
    stripe::{CreatedSession, PaymentProvider, SessionInfo, SessionRequest, sign},
};
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
"#;

const SECRET: &str = "whsec_test";

#[derive(Default)]
struct FakeStripe {
    created: Mutex<Vec<(String, i64)>>, // (order_id, total)
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
}

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn harness() -> Harness {
    let stripe = Arc::new(FakeStripe::default());
    let db = Db::open_in_memory().unwrap();
    let clock = Arc::new(AtomicI64::new(utc("2026-10-01T12:00:00Z").timestamp()));
    let c = clock.clone();
    let state = AppState {
        catalog: Arc::new(Catalog::parse(CATALOG, |_| true).unwrap()),
        db: db.clone(),
        provider: stripe.clone(),
        webhook_secret: SECRET.into(),
        base_url: "https://fundraiser.test".into(),
        limiter: Arc::new(RateLimiter::new(10, Duration::from_secs(60))),
        now: Arc::new(move || Utc.timestamp_opt(c.load(Ordering::SeqCst), 0).unwrap()),
        trust_proxy: false,
    };
    Harness { app: router(state, None), db, stripe, clock }
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
async fn export_lists_paid_orders() {
    let h = harness();
    let (order_id, sid) = place(&h, json!([{"item_id": "wreath", "qty": 2}, {"item_id": "box", "qty": 1}])).await;
    place(&h, wreaths(1)).await; // stays pending: must not appear
    webhook(&h, &completed_event("evt_e", &order_id, &sid, 12500)).await;

    let rows = h.db.call(|c| db::list_orders_for_export(c)).await.unwrap();
    assert_eq!(rows.len(), 1);
    let mut out = Vec::new();
    t15_fundraiser::export::write_csv(&rows, &mut out).unwrap();
    let csv = String::from_utf8(out).unwrap();
    assert!(csv.contains("2x Wreath"), "{csv}");
    assert!(csv.contains("1x Boxed Wreath"), "{csv}");
    assert!(csv.contains("Sam Jones, 9 Elm St, Apt 2, Akron, OH 44301"), "{csv}");
    assert!(csv.contains("Merry Christmas!"), "{csv}");
    assert!(csv.contains("$125.00"), "{csv}");
    assert!(csv.contains("1 Main St, Cleveland, OH 44101"), "{csv}");
}
