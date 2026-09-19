//! Stripe Checkout integration: a small trait (so tests can stub it), the real REST client,
//! and webhook signature verification.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

pub struct SessionLine {
    pub name: String,
    pub unit_amount: i64,
    pub qty: u32,
    pub image_url: Option<String>,
}

pub struct SessionRequest {
    pub order_id: String,
    pub email: String,
    pub lines: Vec<SessionLine>,
    pub success_url: String,
    pub cancel_url: String,
    /// Unix seconds; Stripe requires 30 minutes to 24 hours from now.
    pub expires_at: i64,
}

pub struct CreatedSession {
    pub id: String,
    pub url: String,
}

/// What we need from a Checkout Session object (webhook payload or API response).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub order_id: Option<String>,
    /// `open`, `complete` or `expired`.
    pub status: String,
    /// `paid`, `unpaid` or `no_payment_required`.
    pub payment_status: String,
    pub amount_total: i64,
    pub payment_intent: Option<String>,
}

impl SessionInfo {
    pub fn from_json(v: &Value) -> Option<Self> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(SessionInfo {
            id: s("id")?,
            order_id: s("client_reference_id")
                .or_else(|| v.pointer("/metadata/order_id").and_then(Value::as_str).map(str::to_string)),
            status: s("status").unwrap_or_default(),
            payment_status: s("payment_status").unwrap_or_default(),
            amount_total: v.get("amount_total").and_then(Value::as_i64).unwrap_or(-1),
            payment_intent: s("payment_intent"),
        })
    }
}

#[async_trait]
pub trait PaymentProvider: Send + Sync {
    async fn create_session(&self, req: &SessionRequest) -> Result<CreatedSession>;
    async fn retrieve_session(&self, session_id: &str) -> Result<SessionInfo>;
}

pub struct StripeClient {
    http: reqwest::Client,
    secret_key: String,
}

impl StripeClient {
    pub fn new(secret_key: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("reqwest client");
        StripeClient { http, secret_key }
    }

    async fn parse(resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let body: Value = resp.json().await.context("decoding Stripe response")?;
        if !status.is_success() {
            let msg = body.pointer("/error/message").and_then(Value::as_str).unwrap_or("unknown error");
            return Err(anyhow!("Stripe returned {status}: {msg}"));
        }
        Ok(body)
    }
}

/// Form parameters for `POST /v1/checkout/sessions`.
pub fn build_session_params(req: &SessionRequest) -> Vec<(String, String)> {
    let mut p: Vec<(String, String)> = Vec::new();
    let mut add = |k: String, v: String| p.push((k, v));
    add("mode".into(), "payment".into());
    add("customer_email".into(), req.email.clone());
    add("client_reference_id".into(), req.order_id.clone());
    add("metadata[order_id]".into(), req.order_id.clone());
    add("payment_intent_data[metadata][order_id]".into(), req.order_id.clone());
    add("payment_intent_data[description]".into(), format!("Troop fundraiser order {}", req.order_id));
    add("success_url".into(), req.success_url.clone());
    add("cancel_url".into(), req.cancel_url.clone());
    add("expires_at".into(), req.expires_at.to_string());
    for (i, l) in req.lines.iter().enumerate() {
        let b = format!("line_items[{i}]");
        add(format!("{b}[quantity]"), l.qty.to_string());
        add(format!("{b}[price_data][currency]"), "usd".into());
        add(format!("{b}[price_data][unit_amount]"), l.unit_amount.to_string());
        add(format!("{b}[price_data][product_data][name]"), l.name.clone());
        if let Some(img) = &l.image_url {
            add(format!("{b}[price_data][product_data][images][0]"), img.clone());
        }
    }
    p
}

#[async_trait]
impl PaymentProvider for StripeClient {
    async fn create_session(&self, req: &SessionRequest) -> Result<CreatedSession> {
        let resp = self
            .http
            .post("https://api.stripe.com/v1/checkout/sessions")
            .bearer_auth(&self.secret_key)
            .header("Idempotency-Key", format!("order-{}", req.order_id))
            .form(&build_session_params(req))
            .send()
            .await
            .context("calling Stripe")?;
        let body = Self::parse(resp).await?;
        Ok(CreatedSession {
            id: body.get("id").and_then(Value::as_str).context("session id missing")?.to_string(),
            url: body.get("url").and_then(Value::as_str).context("session url missing")?.to_string(),
        })
    }

    async fn retrieve_session(&self, session_id: &str) -> Result<SessionInfo> {
        let resp = self
            .http
            .get(format!("https://api.stripe.com/v1/checkout/sessions/{session_id}"))
            .bearer_auth(&self.secret_key)
            .send()
            .await
            .context("calling Stripe")?;
        let body = Self::parse(resp).await?;
        SessionInfo::from_json(&body).context("unexpected session shape")
    }
}

/// Verify a `Stripe-Signature` header (`t=...,v1=...`) over the raw body.
pub fn verify_signature(secret: &str, header: &str, payload: &[u8], now_unix: i64, tolerance_secs: i64) -> bool {
    let mut timestamp: Option<i64> = None;
    let mut sigs: Vec<Vec<u8>> = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => timestamp = v.parse().ok(),
            Some(("v1", v)) => {
                if let Ok(b) = hex::decode(v) {
                    sigs.push(b);
                }
            }
            _ => {}
        }
    }
    let Some(t) = timestamp else { return false };
    if (now_unix - t).abs() > tolerance_secs {
        return false;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(t.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    sigs.iter().any(|s| mac.clone().verify_slice(s).is_ok())
}

/// Build a valid header for tests and local tooling.
pub fn sign(secret: &str, timestamp: i64, payload: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    format!("t={timestamp},v1={}", hex::encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_roundtrip_and_tamper() {
        let body = br#"{"id":"evt_1"}"#;
        let h = sign("whsec_x", 1_000, body);
        assert!(verify_signature("whsec_x", &h, body, 1_100, 300));
        assert!(!verify_signature("whsec_y", &h, body, 1_100, 300), "wrong secret");
        assert!(!verify_signature("whsec_x", &h, b"{}", 1_100, 300), "tampered body");
        assert!(!verify_signature("whsec_x", &h, body, 2_000, 300), "stale timestamp");
        assert!(!verify_signature("whsec_x", "garbage", body, 1_000, 300));
        assert!(!verify_signature("whsec_x", "t=1000", body, 1_000, 300), "no v1");
    }

    #[test]
    fn accepts_any_of_multiple_v1() {
        let body = b"x";
        let good = sign("s", 5, body);
        let h = format!("t=5,v1=deadbeef,{}", good.split_once(',').unwrap().1);
        assert!(verify_signature("s", &h, body, 5, 300));
    }

    #[test]
    fn parses_session_and_falls_back_to_metadata_order_id() {
        let full = serde_json::json!({
            "id": "cs_1", "client_reference_id": "o1", "status": "complete",
            "payment_status": "paid", "amount_total": 7000, "payment_intent": "pi_1"
        });
        let s = SessionInfo::from_json(&full).unwrap();
        assert_eq!((s.order_id.as_deref(), s.amount_total, s.payment_intent.as_deref()), (Some("o1"), 7000, Some("pi_1")));

        let sparse = serde_json::json!({"id": "cs_2", "metadata": {"order_id": "o2"}});
        let s = SessionInfo::from_json(&sparse).unwrap();
        assert_eq!((s.order_id.as_deref(), s.amount_total), (Some("o2"), -1));
        assert!(SessionInfo::from_json(&serde_json::json!({})).is_none());
    }

    #[test]
    fn session_params_shape() {
        let req = SessionRequest {
            order_id: "o1".into(),
            email: "a@b.co".into(),
            lines: vec![SessionLine { name: "Wreath".into(), unit_amount: 3500, qty: 2, image_url: None }],
            success_url: "https://x/success".into(),
            cancel_url: "https://x/cancel".into(),
            expires_at: 99,
        };
        let p = build_session_params(&req);
        let get = |k: &str| p.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        assert_eq!(get("line_items[0][price_data][unit_amount]"), Some("3500"));
        assert_eq!(get("line_items[0][quantity]"), Some("2"));
        assert!(!p.iter().any(|(k, _)| k.starts_with("shipping_address_collection")));
        assert_eq!(get("client_reference_id"), Some("o1"));
        assert!(get("line_items[0][price_data][product_data][images][0]").is_none());
    }
}
