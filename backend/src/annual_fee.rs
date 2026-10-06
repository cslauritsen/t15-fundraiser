//! Annual camping fee: a parent pays for one or more scouts in a single Stripe Checkout.
//!
//! Nothing is written until money arrives. Checkout puts the scouts into the session metadata;
//! when Stripe reports the session paid (webhook, or the success page's status poll), the
//! scouts are read back from the session and one `annual_fees` row per scout is inserted.
//! Abandoned checkouts leave no trace.

use crate::{
    ApiError, AppState, SESSION_TTL_SECS, StatusQuery,
    catalog::AnnualFeeConfig,
    db::{self, AnnualFeeRow, FeeInsertOutcome, ts},
    stripe::{SessionInfo, SessionLine, SessionRequest},
};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use shared::{
    AnnualFeeInfo, CheckoutResponse, ErrorResponse, FeeCheckoutRequest, FeeScout, FeeStatus, FeeStatusResponse,
    FeeNameCheckResponse, FieldError, fee_scout_key, validate_fee_checkout, validate_fee_scout,
};
use std::collections::HashMap;

/// Fee payment ids (and so `client_reference_id`s) start with this; order ids never do.
pub const PAYMENT_ID_PREFIX: &str = "af_";
const KIND: &str = "annual_fee";
/// Stand-in for a name that couldn't be read back from the session metadata.
const UNKNOWN: &str = "(unknown)";
/// Never read more scouts than Stripe could hold in metadata.
const MAX_METADATA_SCOUTS: usize = 50;

pub fn is_fee_session(session: &SessionInfo) -> bool {
    session.order_id.as_deref().is_some_and(|id| id.starts_with(PAYMENT_ID_PREFIX))
}

fn config(st: &AppState) -> Result<&AnnualFeeConfig, ApiError> {
    st.catalog.annual_fee.as_ref().ok_or_else(ApiError::not_found)
}

// ---------------------------------------------------------------------------------------------
// Session metadata
// ---------------------------------------------------------------------------------------------

/// Everything needed to record the payment later, carried on the Checkout Session.
pub fn build_fee_metadata(scouting_year: &str, amount_cents: i64, scouts: &[FeeScout], payer_name: &str) -> Vec<(String, String)> {
    let mut m = vec![
        ("kind".to_string(), KIND.to_string()),
        ("scouting_year".to_string(), scouting_year.to_string()),
        ("amount_cents".to_string(), amount_cents.to_string()),
        ("scout_count".to_string(), scouts.len().to_string()),
    ];
    for (i, s) in scouts.iter().enumerate() {
        m.push((format!("scout_{i}"), format!("{}\t{}", s.first_name, s.last_name)));
    }
    m.push(("payer_name".to_string(), payer_name.to_string()));
    m
}

/// The metadata read back from a session. Missing or malformed entries are `None` (or an
/// `(unknown)` scout) and described in `problems`; parsing never fails outright.
#[derive(Debug, Clone, PartialEq)]
pub struct FeeMetadata {
    pub scouting_year: Option<String>,
    pub amount_cents: Option<i64>,
    pub scouts: Vec<FeeScout>,
    pub payer_name: Option<String>,
    pub problems: Vec<String>,
}

pub fn parse_fee_metadata(m: &HashMap<String, String>) -> FeeMetadata {
    let mut problems = Vec::new();
    let get = |k: &str| m.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());

    if get("kind") != Some(KIND) {
        problems.push(format!("metadata kind is {:?}, expected {KIND:?}", get("kind")));
    }
    let scouting_year = get("scouting_year").map(str::to_string);
    if scouting_year.is_none() {
        problems.push("metadata scouting_year is missing".into());
    }
    let amount_cents = get("amount_cents").and_then(|v| v.parse::<i64>().ok()).filter(|a| *a > 0);
    if amount_cents.is_none() {
        problems.push(format!("metadata amount_cents {:?} is missing or invalid", get("amount_cents")));
    }
    let count = get("scout_count").and_then(|v| v.parse::<usize>().ok());
    if count.is_none() {
        problems.push(format!("metadata scout_count {:?} is missing or invalid", get("scout_count")));
    }
    // Without a usable count, read every consecutive scout_i that is there.
    let n = count
        .unwrap_or_else(|| (0..).take_while(|i| m.contains_key(&format!("scout_{i}"))).count())
        .min(MAX_METADATA_SCOUTS);
    let scouts: Vec<FeeScout> = (0..n)
        .map(|i| {
            let raw = m.get(&format!("scout_{i}"));
            match raw.and_then(|v| v.split_once('\t')) {
                Some((first, last)) if !first.trim().is_empty() && !last.trim().is_empty() => {
                    FeeScout { first_name: first.trim().to_string(), last_name: last.trim().to_string() }
                }
                _ => {
                    problems.push(format!("metadata scout_{i} {raw:?} is missing or malformed"));
                    // Keep whatever text there is so the treasurer has something to go on.
                    let last = raw.map(|v| v.trim()).filter(|v| !v.is_empty()).unwrap_or(UNKNOWN);
                    FeeScout { first_name: UNKNOWN.into(), last_name: last.replace('\t', " ") }
                }
            }
        })
        .collect();
    if scouts.is_empty() {
        problems.push("metadata lists no scouts".into());
    }
    let payer_name = get("payer_name").map(str::to_string);
    if payer_name.is_none() {
        problems.push("metadata payer_name is missing".into());
    }
    FeeMetadata { scouting_year, amount_cents, scouts, payer_name, problems }
}

// ---------------------------------------------------------------------------------------------
// Recording a paid session
// ---------------------------------------------------------------------------------------------

/// The rows to insert for a paid fee session. Money has arrived, so this always yields at least
/// one row: anything that doesn't check out is recorded as `needs_review` with a reason.
/// Returns the rows plus the metadata problems and amount mismatch (for logging).
pub fn fee_rows_from_session(
    session: &SessionInfo,
    fallback_year: &str,
    paid_at: &str,
) -> (Vec<AnnualFeeRow>, Vec<String>, Option<String>) {
    let meta = parse_fee_metadata(&session.metadata);
    let mut problems = meta.problems;
    let mut scouts = meta.scouts;
    if scouts.is_empty() {
        scouts.push(FeeScout { first_name: UNKNOWN.into(), last_name: UNKNOWN.into() });
    }
    let payer_email = session.customer_email.clone().unwrap_or_else(|| {
        problems.push("session has no customer email".into());
        String::new()
    });

    let n = scouts.len() as i64;
    let mismatch = match meta.amount_cents {
        Some(a) if a.checked_mul(n) == Some(session.amount_total) => None,
        Some(a) => Some(format!("Stripe amount {} != {n} x {a}", session.amount_total)),
        None => None, // already a metadata problem
    };
    // Per-scout amounts: from the metadata, or else Stripe's total split across the rows so
    // the rows still add up to what was actually paid.
    let amounts: Vec<i64> = match meta.amount_cents {
        Some(a) => vec![a; scouts.len()],
        None => {
            let total = session.amount_total.max(0);
            (0..n).map(|i| total / n + i64::from(i < total % n)).collect()
        }
    };

    let reasons: Vec<&str> = problems.iter().chain(&mismatch).map(String::as_str).collect();
    let (status, review_reason) = if reasons.is_empty() {
        (FeeStatus::Paid, None)
    } else {
        (FeeStatus::NeedsReview, Some(reasons.join("; ")))
    };
    let payment_id = session.order_id.clone().unwrap_or_default();
    let rows = scouts
        .into_iter()
        .zip(amounts)
        .enumerate()
        .map(|(i, (s, amount_cents))| AnnualFeeRow {
            payment_id: payment_id.clone(),
            line_no: i as i64,
            status,
            review_reason: review_reason.clone(),
            scouting_year: meta.scouting_year.clone().unwrap_or_else(|| fallback_year.to_string()),
            scout_first_name: s.first_name,
            scout_last_name: s.last_name,
            amount_cents,
            payer_name: meta.payer_name.clone().unwrap_or_default(),
            payer_email: payer_email.clone(),
            stripe_session_id: session.id.clone(),
            stripe_payment_intent_id: session.payment_intent.clone(),
            paid_at: paid_at.to_string(),
        })
        .collect();
    (rows, problems, mismatch)
}

/// Record a paid fee session. Called by the webhook (with its `(event_id, event_type)`, stored
/// in the same transaction) and by the status endpoint (`None`). Idempotent.
pub async fn record_fee_payment(
    st: &AppState,
    event: Option<(&str, &str)>,
    session: &SessionInfo,
) -> anyhow::Result<FeeInsertOutcome> {
    let now = ts((st.now)());
    let fallback_year = st.catalog.annual_fee.as_ref().map(|f| f.scouting_year.as_str()).unwrap_or(UNKNOWN);
    let (rows, problems, mismatch) = fee_rows_from_session(session, fallback_year, &now);
    let payment_id = rows[0].payment_id.clone();
    if !problems.is_empty() {
        tracing::error!(payment_id, session_id = session.id, ?problems, "fee session metadata malformed; recording as needs_review");
    }
    if let Some(m) = &mismatch {
        tracing::warn!(payment_id, session_id = session.id, "FEE AMOUNT MISMATCH: {m}; recording as needs_review");
    }
    let event = event.map(|(e, k)| (e.to_string(), k.to_string()));
    let outcome = st
        .db
        .call(move |c| db::insert_fee_payment(c, event.as_ref().map(|(e, k)| (e.as_str(), k.as_str())), &rows, &now))
        .await?;
    tracing::info!(payment_id, session_id = session.id, ?outcome, "fee payment recorded");
    Ok(outcome)
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

pub(crate) async fn info(State(st): State<AppState>) -> Result<Json<AnnualFeeInfo>, ApiError> {
    Ok(Json(config(&st)?.view((st.now)())))
}

#[derive(serde::Deserialize)]
pub(crate) struct NameQuery {
    first_name: String,
    last_name: String,
}

/// Advisory duplicate check for the add-scout row. Names are normalized the same way checkout
/// does; a name that wouldn't validate simply has no matches.
pub(crate) async fn check_name(
    State(st): State<AppState>,
    Query(q): Query<NameQuery>,
) -> Result<Json<FeeNameCheckResponse>, ApiError> {
    let cfg = config(&st)?;
    let Ok(scout) = validate_fee_scout(&FeeScout { first_name: q.first_name, last_name: q.last_name }) else {
        return Ok(Json(FeeNameCheckResponse::default()));
    };
    let year = cfg.scouting_year.clone();
    let (first, last) = (scout.first_name.clone(), scout.last_name.clone());
    let rows = st.db.call(move |c| db::fee_name_matches(c, &year, &first, &last)).await?;
    let key = fee_scout_key(&scout);
    let mut resp = FeeNameCheckResponse::default();
    for (first_name, last_name) in rows {
        let found = FeeScout { first_name, last_name };
        if fee_scout_key(&found) == key {
            resp.exact = true;
        } else {
            resp.similar.push(found);
        }
    }
    Ok(Json(resp))
}

pub(crate) async fn checkout(
    State(st): State<AppState>,
    Json(req): Json<FeeCheckoutRequest>,
) -> Result<Json<CheckoutResponse>, ApiError> {
    let cfg = config(&st)?;
    let now = (st.now)();
    if !cfg.is_open(now) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "closed",
            &format!("Annual fee payments for {} are closed.", cfg.scouting_year),
        ));
    }
    let v = validate_fee_checkout(&req, cfg.max_scouts).map_err(|fields: Vec<FieldError>| ApiError {
        status: StatusCode::BAD_REQUEST,
        body: ErrorResponse { code: "invalid".into(), message: "Please fix the highlighted fields.".into(), fields },
    })?;

    // No database write: the session metadata carries everything until the money arrives.
    let payment_id = format!("{PAYMENT_ID_PREFIX}{}", uuid::Uuid::new_v4());
    let session_req = SessionRequest {
        order_id: payment_id.clone(),
        email: v.payer_email.clone(),
        description: format!("Troop 15 annual camping fee {payment_id}"),
        lines: v
            .scouts
            .iter()
            .map(|s| SessionLine {
                name: format!("Annual camping fee {} — {}", cfg.scouting_year, s.full_name()),
                unit_amount: cfg.amount_cents,
                qty: 1,
                image_url: None,
            })
            .collect(),
        metadata: build_fee_metadata(&cfg.scouting_year, cfg.amount_cents, &v.scouts, &v.payer_name),
        success_url: format!(
            "{}/annual-fee/success?payment={payment_id}&session_id={{CHECKOUT_SESSION_ID}}",
            st.base_url
        ),
        cancel_url: format!("{}/annual-fee/cancel", st.base_url),
        expires_at: now.timestamp() + SESSION_TTL_SECS,
    };

    match st.provider.create_session(&session_req).await {
        Ok(session) => {
            tracing::info!(payment_id, session_id = session.id, scouts = v.scouts.len(), "fee checkout session created");
            Ok(Json(CheckoutResponse { checkout_url: session.url }))
        }
        Err(e) => {
            tracing::error!(payment_id, "creating Stripe fee session failed: {e:#}");
            Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "payment_unavailable",
                "We couldn't start the payment page. Please try again in a moment.",
            ))
        }
    }
}

fn response_from_rows(rows: &[AnnualFeeRow]) -> FeeStatusResponse {
    let status = if rows.iter().any(|r| r.status == FeeStatus::NeedsReview) { FeeStatus::NeedsReview } else { FeeStatus::Paid };
    FeeStatusResponse {
        status,
        scouting_year: rows[0].scouting_year.clone(),
        scouts: rows
            .iter()
            .map(|r| FeeScout { first_name: r.scout_first_name.clone(), last_name: r.scout_last_name.clone() })
            .collect(),
        total_cents: rows.iter().map(|r| r.amount_cents).sum(),
        payer_email: rows[0].payer_email.clone(),
    }
}

/// Unpaid session: describe it from its metadata.
fn response_from_session(status: FeeStatus, session: &SessionInfo) -> FeeStatusResponse {
    let meta = parse_fee_metadata(&session.metadata);
    FeeStatusResponse {
        status,
        scouting_year: meta.scouting_year.unwrap_or_default(),
        total_cents: meta.amount_cents.unwrap_or(0) * meta.scouts.len() as i64,
        scouts: meta.scouts,
        payer_email: session.customer_email.clone().unwrap_or_default(),
    }
}

async fn load_rows(st: &AppState, payment_id: &str) -> anyhow::Result<Vec<AnnualFeeRow>> {
    let id = payment_id.to_string();
    st.db.call(move |c| db::fee_rows_for_payment(c, &id)).await
}

/// Success-page poll. Recorded rows answer directly; otherwise ask Stripe and record the
/// payment if the webhook hasn't landed yet.
pub(crate) async fn status(
    State(st): State<AppState>,
    Path(payment_id): Path<String>,
    Query(q): Query<StatusQuery>,
) -> Result<Json<FeeStatusResponse>, ApiError> {
    config(&st)?;
    if !payment_id.starts_with(PAYMENT_ID_PREFIX) {
        return Err(ApiError::not_found());
    }
    let rows = load_rows(&st, &payment_id).await?;
    if !rows.is_empty() {
        // The session id is the caller's proof they came back from this checkout.
        if rows[0].stripe_session_id != q.session_id {
            return Err(ApiError::not_found());
        }
        return Ok(Json(response_from_rows(&rows)));
    }

    let session = match st.provider.retrieve_session(&q.session_id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(payment_id, "fee status: retrieving session from Stripe failed: {e:#}");
            return Ok(Json(FeeStatusResponse {
                status: FeeStatus::Pending,
                scouting_year: String::new(),
                scouts: Vec::new(),
                total_cents: 0,
                payer_email: String::new(),
            }));
        }
    };
    if session.order_id.as_deref() != Some(payment_id.as_str()) {
        tracing::warn!(payment_id, session_id = q.session_id, "fee status: session belongs to a different payment");
        return Err(ApiError::not_found());
    }
    if session.payment_status == "paid" {
        record_fee_payment(&st, None, &session).await?;
        return Ok(Json(response_from_rows(&load_rows(&st, &payment_id).await?)));
    }
    let status = if session.status == "expired" { FeeStatus::Expired } else { FeeStatus::Pending };
    Ok(Json(response_from_session(status, &session)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scout(first: &str, last: &str) -> FeeScout {
        FeeScout { first_name: first.into(), last_name: last.into() }
    }

    fn session(metadata: Vec<(String, String)>, amount_total: i64) -> SessionInfo {
        SessionInfo {
            id: "cs_1".into(),
            order_id: Some("af_1".into()),
            status: "complete".into(),
            payment_status: "paid".into(),
            amount_total,
            payment_intent: Some("pi_1".into()),
            metadata: metadata.into_iter().collect(),
            customer_email: Some("pat@example.com".into()),
        }
    }

    #[test]
    fn metadata_round_trip() {
        let scouts = vec![scout("Alex", "Smith"), scout("Zoë", "O'Brien-Lee")];
        let m = build_fee_metadata("2026-2027", 5000, &scouts, "Pat Smith");
        let keys: Vec<&str> = m.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["kind", "scouting_year", "amount_cents", "scout_count", "scout_0", "scout_1", "payer_name"]);
        assert_eq!(m[4].1, "Alex\tSmith");
        let parsed = parse_fee_metadata(&m.into_iter().collect());
        assert_eq!(
            parsed,
            FeeMetadata {
                scouting_year: Some("2026-2027".into()),
                amount_cents: Some(5000),
                scouts,
                payer_name: Some("Pat Smith".into()),
                problems: vec![],
            }
        );
    }

    #[test]
    fn metadata_stays_within_stripe_limits() {
        let long = "x".repeat(shared::MAX_SCOUT_NAME);
        let scouts: Vec<FeeScout> = (0..10).map(|_| scout(&long, &long)).collect();
        let m = build_fee_metadata("2026-2027", 5000, &scouts, &"p".repeat(shared::MAX_PAYER_NAME));
        assert!(m.len() < 50, "plus order_id");
        assert!(m.iter().all(|(k, v)| k.len() <= 40 && v.chars().count() <= 500));
    }

    #[test]
    fn good_session_gives_paid_rows() {
        let m = build_fee_metadata("2026-2027", 5000, &[scout("Alex", "Smith"), scout("Jamie", "Smith")], "Pat Smith");
        let (rows, problems, mismatch) = fee_rows_from_session(&session(m, 10000), "fallback", "2026-10-01T12:00:00Z");
        assert!(problems.is_empty() && mismatch.is_none());
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.status == FeeStatus::Paid && r.review_reason.is_none()));
        assert_eq!((rows[1].line_no, rows[1].scout_first_name.as_str(), rows[1].amount_cents), (1, "Jamie", 5000));
        assert_eq!((rows[0].payment_id.as_str(), rows[0].payer_email.as_str()), ("af_1", "pat@example.com"));
    }

    #[test]
    fn amount_mismatch_needs_review() {
        let m = build_fee_metadata("2026-2027", 5000, &[scout("Alex", "Smith")], "Pat");
        let (rows, _, mismatch) = fee_rows_from_session(&session(m, 100), "fallback", "t");
        assert!(mismatch.unwrap().contains("100"));
        assert_eq!(rows[0].status, FeeStatus::NeedsReview);
        assert!(rows[0].review_reason.as_deref().unwrap().contains("100"));
    }

    #[test]
    fn malformed_metadata_still_records_what_it_can() {
        let m = vec![
            ("kind".to_string(), "annual_fee".to_string()),
            ("scout_0".to_string(), "Alex\tSmith".to_string()),
            ("scout_1".to_string(), "no tab here".to_string()),
        ];
        let (rows, problems, _) = fee_rows_from_session(&session(m, 7001), "2026-2027", "t");
        assert!(!problems.is_empty());
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.status == FeeStatus::NeedsReview));
        assert_eq!(rows[0].scouting_year, "2026-2027", "falls back to the configured year");
        assert_eq!((rows[0].scout_first_name.as_str(), rows[0].scout_last_name.as_str()), ("Alex", "Smith"));
        assert_eq!((rows[1].scout_first_name.as_str(), rows[1].scout_last_name.as_str()), (UNKNOWN, "no tab here"));
        assert_eq!(rows.iter().map(|r| r.amount_cents).sum::<i64>(), 7001, "rows add up to what was paid");

        // No metadata at all: still one row, so the money is never dropped.
        let (rows, _, _) = fee_rows_from_session(&session(vec![], 5000), "2026-2027", "t");
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].amount_cents, rows[0].status), (5000, FeeStatus::NeedsReview));
    }
}
