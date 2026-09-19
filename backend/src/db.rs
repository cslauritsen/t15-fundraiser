use crate::stripe::SessionInfo;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use shared::{Delivery, Fulfillment, OrderLine, OrderStatus, ShipTo, ValidatedOrder};
use std::sync::{Arc, Mutex};

const SCHEMA: &str = "
CREATE TABLE orders (
    id                       TEXT PRIMARY KEY,
    status                   TEXT NOT NULL CHECK (status IN ('pending','paid','expired','failed','needs_review')),
    email                    TEXT NOT NULL,
    buyer_name               TEXT NOT NULL,
    phone                    TEXT NOT NULL,
    scout_name               TEXT,
    total_cents              INTEGER NOT NULL,
    delivery_json            TEXT,
    shipping_json            TEXT,
    gift_message             TEXT,
    stripe_session_id        TEXT UNIQUE,
    stripe_payment_intent_id TEXT,
    review_reason            TEXT,
    created_at               TEXT NOT NULL,
    paid_at                  TEXT
);
CREATE TABLE order_items (
    order_id         TEXT NOT NULL REFERENCES orders(id),
    item_id          TEXT NOT NULL,
    name             TEXT NOT NULL,
    unit_price_cents INTEGER NOT NULL,
    qty              INTEGER NOT NULL,
    fulfillment      TEXT NOT NULL,
    PRIMARY KEY (order_id, item_id)
);
CREATE TABLE stripe_events (
    event_id    TEXT PRIMARY KEY,
    type        TEXT NOT NULL,
    received_at TEXT NOT NULL
);
";

/// One SQLite connection behind a mutex; all access runs on the blocking pool.
/// Plenty for a few dozen orders on a single node.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        if let Some(dir) = std::path::Path::new(path).parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version == 0 {
            conn.execute_batch(&format!("BEGIN; {SCHEMA} PRAGMA user_version = 1; COMMIT;"))?;
        }
        Ok(Db { conn: Arc::new(Mutex::new(conn)) })
    }

    pub async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut guard)
        })
        .await?
        .map_err(Into::into)
    }
}

pub fn ts(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[derive(Debug, Clone)]
pub struct OrderRow {
    pub id: String,
    pub status: OrderStatus,
    pub email: String,
    pub buyer_name: String,
    pub phone: String,
    pub scout_name: Option<String>,
    pub total_cents: i64,
    pub delivery: Option<Delivery>,
    pub ship_to: Option<ShipTo>,
    pub gift_message: Option<String>,
    pub stripe_session_id: Option<String>,
    pub review_reason: Option<String>,
    pub created_at: String,
    pub paid_at: Option<String>,
    pub lines: Vec<OrderLine>,
}

pub fn insert_order(conn: &mut Connection, id: &str, o: &ValidatedOrder, now: &str) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO orders (id, status, email, buyer_name, phone, scout_name, total_cents, delivery_json,
                             shipping_json, gift_message, created_at)
         VALUES (?1, 'pending', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            id,
            o.email,
            o.buyer_name,
            o.phone,
            o.scout_name,
            o.total_cents,
            o.delivery.as_ref().map(|d| serde_json::to_string(d).expect("serialize delivery")),
            o.shipping.as_ref().map(|s| serde_json::to_string(s).expect("serialize shipping")),
            o.gift_message,
            now,
        ],
    )?;
    for l in &o.lines {
        tx.execute(
            "INSERT INTO order_items (order_id, item_id, name, unit_price_cents, qty, fulfillment)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, l.item_id, l.name, l.unit_price_cents, l.qty, l.fulfillment.as_str()],
        )?;
    }
    tx.commit()
}

pub fn set_session_id(conn: &Connection, order_id: &str, session_id: &str) -> rusqlite::Result<()> {
    conn.execute("UPDATE orders SET stripe_session_id = ?2 WHERE id = ?1", params![order_id, session_id])?;
    Ok(())
}

pub fn mark_failed(conn: &Connection, order_id: &str) -> rusqlite::Result<()> {
    conn.execute("UPDATE orders SET status = 'failed' WHERE id = ?1 AND status = 'pending'", [order_id])?;
    Ok(())
}

fn load_lines(conn: &Connection, order_id: &str) -> rusqlite::Result<Vec<OrderLine>> {
    let mut stmt = conn.prepare(
        "SELECT name, unit_price_cents, qty, fulfillment FROM order_items WHERE order_id = ?1 ORDER BY rowid",
    )?;
    stmt.query_map([order_id], |r| {
        Ok(OrderLine {
            name: r.get(0)?,
            unit_price_cents: r.get(1)?,
            qty: r.get(2)?,
            fulfillment: Fulfillment::parse(&r.get::<_, String>(3)?).unwrap_or(Fulfillment::ScoutDelivery),
        })
    })?
    .collect()
}

fn row_to_order(r: &rusqlite::Row) -> rusqlite::Result<OrderRow> {
    let json = |i: usize| -> rusqlite::Result<Option<String>> { r.get(i) };
    Ok(OrderRow {
        id: r.get(0)?,
        status: OrderStatus::parse(&r.get::<_, String>(1)?).unwrap_or(OrderStatus::NeedsReview),
        email: r.get(2)?,
        buyer_name: r.get(3)?,
        phone: r.get(4)?,
        scout_name: r.get(5)?,
        total_cents: r.get(6)?,
        delivery: json(7)?.and_then(|s| serde_json::from_str(&s).ok()),
        ship_to: json(8)?.and_then(|s| serde_json::from_str(&s).ok()),
        stripe_session_id: r.get(9)?,
        review_reason: r.get(10)?,
        created_at: r.get(11)?,
        paid_at: r.get(12)?,
        gift_message: r.get(13)?,
        lines: Vec::new(),
    })
}

const ORDER_COLS: &str = "id, status, email, buyer_name, phone, scout_name, total_cents, delivery_json,
    shipping_json, stripe_session_id, review_reason, created_at, paid_at, gift_message";

pub fn get_order(conn: &Connection, id: &str) -> rusqlite::Result<Option<OrderRow>> {
    let row = conn
        .query_row(&format!("SELECT {ORDER_COLS} FROM orders WHERE id = ?1"), [id], row_to_order)
        .optional()?;
    row.map(|mut o| {
        o.lines = load_lines(conn, &o.id)?;
        Ok(o)
    })
    .transpose()
}

/// Orders that need a human: paid ones (to fulfil) and needs_review ones (to check).
pub fn list_orders_for_export(conn: &Connection) -> rusqlite::Result<Vec<OrderRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ORDER_COLS} FROM orders WHERE status IN ('paid','needs_review') ORDER BY created_at, id"
    ))?;
    let mut rows = stmt.query_map([], row_to_order)?.collect::<rusqlite::Result<Vec<_>>>()?;
    for o in &mut rows {
        o.lines = load_lines(conn, &o.id)?;
    }
    Ok(rows)
}

#[derive(Debug, PartialEq, Eq)]
pub enum PaidOutcome {
    Paid,
    AlreadyPaid,
    /// This webhook event id was already processed.
    Duplicate,
    UnknownOrder,
    /// Stripe's total differs from ours; order flagged `needs_review`.
    AmountMismatch,
}

/// Insert the event id; false if we've seen it before.
fn record_event(tx: &rusqlite::Transaction, event: Option<(&str, &str)>, now: &str) -> rusqlite::Result<bool> {
    let Some((id, kind)) = event else { return Ok(true) };
    let n = tx.execute(
        "INSERT OR IGNORE INTO stripe_events (event_id, type, received_at) VALUES (?1, ?2, ?3)",
        params![id, kind, now],
    )?;
    Ok(n == 1)
}

/// Record a paid Checkout Session. `event` is `(event_id, event_type)` for webhooks
/// and `None` for the success-page reconcile. Idempotent.
pub fn apply_paid(
    conn: &mut Connection,
    event: Option<(&str, &str)>,
    s: &SessionInfo,
    now: &str,
) -> rusqlite::Result<PaidOutcome> {
    let tx = conn.transaction()?;
    if !record_event(&tx, event, now)? {
        return Ok(PaidOutcome::Duplicate);
    }
    let Some(order_id) = s.order_id.as_deref() else { return Ok(PaidOutcome::UnknownOrder) };
    let found: Option<(String, i64, Option<String>)> = tx
        .query_row(
            "SELECT status, total_cents, stripe_session_id FROM orders WHERE id = ?1",
            [order_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((status, total, session_id)) = found else {
        tx.commit()?;
        return Ok(PaidOutcome::UnknownOrder);
    };
    if status == "paid" {
        tx.commit()?;
        return Ok(PaidOutcome::AlreadyPaid);
    }
    let mismatch = if s.amount_total != total {
        Some(format!("Stripe amount {} != order total {}", s.amount_total, total))
    } else if session_id.as_deref().is_some_and(|id| id != s.id) {
        Some(format!("Stripe session {} != recorded session {:?}", s.id, session_id))
    } else {
        None
    };
    if let Some(reason) = mismatch {
        tx.execute(
            "UPDATE orders SET status = 'needs_review', review_reason = ?2,
                    stripe_payment_intent_id = ?3, paid_at = ?4 WHERE id = ?1",
            params![order_id, reason, s.payment_intent, now],
        )?;
        tx.commit()?;
        return Ok(PaidOutcome::AmountMismatch);
    }
    tx.execute(
        "UPDATE orders SET status = 'paid', stripe_payment_intent_id = ?2, paid_at = ?3,
                stripe_session_id = COALESCE(stripe_session_id, ?4), review_reason = NULL WHERE id = ?1",
        params![order_id, s.payment_intent, now, s.id],
    )?;
    tx.commit()?;
    Ok(PaidOutcome::Paid)
}

/// Mark a pending order expired. Idempotent; never touches an order in any other state.
pub fn apply_expired(
    conn: &mut Connection,
    event: Option<(&str, &str)>,
    order_id: &str,
    now: &str,
) -> rusqlite::Result<bool> {
    let tx = conn.transaction()?;
    if !record_event(&tx, event, now)? {
        return Ok(false);
    }
    let n = tx.execute("UPDATE orders SET status = 'expired' WHERE id = ?1 AND status = 'pending'", [order_id])?;
    tx.commit()?;
    Ok(n == 1)
}
