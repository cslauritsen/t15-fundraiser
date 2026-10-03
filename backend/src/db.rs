use crate::stripe::SessionInfo;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use shared::{Delivery, FeeStatus, Fulfillment, OrderLine, OrderStatus, ShipTo, ValidatedOrder};
use std::sync::{Arc, Mutex};

/// Version 1: greenery orders.
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

/// Version 2: annual camping fees. Rows exist only for money that arrived.
const SCHEMA_V2: &str = "
CREATE TABLE annual_fees (
    id                        INTEGER PRIMARY KEY,
    payment_id                TEXT NOT NULL,
    line_no                   INTEGER NOT NULL,
    status                    TEXT NOT NULL CHECK (status IN ('paid','needs_review')),
    review_reason             TEXT,
    scouting_year             TEXT NOT NULL,
    scout_first_name          TEXT NOT NULL,
    scout_last_name           TEXT NOT NULL,
    amount_cents              INTEGER NOT NULL,
    payer_name                TEXT NOT NULL,
    payer_email               TEXT NOT NULL,
    stripe_session_id         TEXT NOT NULL,
    stripe_payment_intent_id  TEXT,
    paid_at                   TEXT NOT NULL,
    UNIQUE (payment_id, line_no)
);
CREATE INDEX annual_fees_session ON annual_fees(stripe_session_id);
";

/// Bring the schema up to date. Each step is additive and runs in its own transaction, so a
/// fresh database gets v1 then v2, and an existing v1 database just gains the new table.
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        conn.execute_batch(&format!("BEGIN; {SCHEMA} PRAGMA user_version = 1; COMMIT;"))?;
    }
    if version < 2 {
        conn.execute_batch(&format!("BEGIN; {SCHEMA_V2} PRAGMA user_version = 2; COMMIT;"))?;
    }
    Ok(())
}

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
        migrate(&conn)?;
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
        "SELECT item_id, name, unit_price_cents, qty, fulfillment FROM order_items WHERE order_id = ?1 ORDER BY rowid",
    )?;
    stmt.query_map([order_id], |r| {
        Ok(OrderLine {
            item_id: r.get(0)?,
            name: r.get(1)?,
            unit_price_cents: r.get(2)?,
            qty: r.get(3)?,
            fulfillment: Fulfillment::parse(&r.get::<_, String>(4)?).unwrap_or(Fulfillment::ScoutDelivery),
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

// ---------------------------------------------------------------------------------------------
// Annual camping fees
// ---------------------------------------------------------------------------------------------

/// One scout's fee: a row of `annual_fees` (minus the rowid).
#[derive(Debug, Clone, PartialEq)]
pub struct AnnualFeeRow {
    pub payment_id: String,
    pub line_no: i64,
    /// `Paid` or `NeedsReview`.
    pub status: FeeStatus,
    pub review_reason: Option<String>,
    pub scouting_year: String,
    pub scout_first_name: String,
    pub scout_last_name: String,
    pub amount_cents: i64,
    pub payer_name: String,
    pub payer_email: String,
    pub stripe_session_id: String,
    pub stripe_payment_intent_id: Option<String>,
    pub paid_at: String,
}

const FEE_COLS: &str = "payment_id, line_no, status, review_reason, scouting_year, scout_first_name, scout_last_name,
    amount_cents, payer_name, payer_email, stripe_session_id, stripe_payment_intent_id, paid_at";

fn row_to_fee(r: &rusqlite::Row) -> rusqlite::Result<AnnualFeeRow> {
    Ok(AnnualFeeRow {
        payment_id: r.get(0)?,
        line_no: r.get(1)?,
        status: FeeStatus::parse(&r.get::<_, String>(2)?).unwrap_or(FeeStatus::NeedsReview),
        review_reason: r.get(3)?,
        scouting_year: r.get(4)?,
        scout_first_name: r.get(5)?,
        scout_last_name: r.get(6)?,
        amount_cents: r.get(7)?,
        payer_name: r.get(8)?,
        payer_email: r.get(9)?,
        stripe_session_id: r.get(10)?,
        stripe_payment_intent_id: r.get(11)?,
        paid_at: r.get(12)?,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum FeeInsertOutcome {
    /// Number of new rows (0 when the session was already recorded by another path).
    Inserted(usize),
    /// This webhook event id was already processed.
    Duplicate,
}

/// Record a paid fee checkout's rows and, for webhooks, the event id, in one transaction.
/// `INSERT OR IGNORE` on `(payment_id, line_no)` makes a second recording a no-op.
pub fn insert_fee_payment(
    conn: &mut Connection,
    event: Option<(&str, &str)>,
    rows: &[AnnualFeeRow],
    now: &str,
) -> rusqlite::Result<FeeInsertOutcome> {
    let tx = conn.transaction()?;
    if !record_event(&tx, event, now)? {
        return Ok(FeeInsertOutcome::Duplicate);
    }
    let mut n = 0;
    for f in rows {
        n += tx.execute(
            &format!("INSERT OR IGNORE INTO annual_fees ({FEE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"),
            params![
                f.payment_id,
                f.line_no,
                f.status.as_str(),
                f.review_reason,
                f.scouting_year,
                f.scout_first_name,
                f.scout_last_name,
                f.amount_cents,
                f.payer_name,
                f.payer_email,
                f.stripe_session_id,
                f.stripe_payment_intent_id,
                f.paid_at,
            ],
        )?;
    }
    tx.commit()?;
    Ok(FeeInsertOutcome::Inserted(n))
}

/// One checkout's rows, in cart order.
pub fn fee_rows_for_payment(conn: &Connection, payment_id: &str) -> rusqlite::Result<Vec<AnnualFeeRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {FEE_COLS} FROM annual_fees WHERE payment_id = ?1 ORDER BY line_no"))?;
    stmt.query_map([payment_id], row_to_fee)?.collect()
}

/// Every fee row, or one scouting year's, sorted by year, last name, first name.
pub fn list_annual_fees(conn: &Connection, year: Option<&str>) -> rusqlite::Result<Vec<AnnualFeeRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {FEE_COLS} FROM annual_fees WHERE ?1 IS NULL OR scouting_year = ?1
         ORDER BY scouting_year, scout_last_name COLLATE NOCASE, scout_first_name COLLATE NOCASE, paid_at, id"
    ))?;
    stmt.query_map([year], row_to_fee)?.collect()
}

/// Distinct scouts already paid for `year` whose last name equals `last` and whose first name
/// starts with `first`, both case-insensitively. `%`, `_` and `\` in `first` are literal.
pub fn fee_name_matches(conn: &Connection, year: &str, first: &str, last: &str) -> rusqlite::Result<Vec<(String, String)>> {
    let escaped = first.to_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    let mut stmt = conn.prepare(
        "SELECT DISTINCT scout_first_name, scout_last_name FROM annual_fees
         WHERE scouting_year = ?1 AND lower(scout_last_name) = lower(?2)
           AND lower(scout_first_name) LIKE ?3 || '%' ESCAPE '\\'
         ORDER BY scout_first_name COLLATE NOCASE, scout_last_name COLLATE NOCASE",
    )?;
    stmt.query_map(params![year, last, escaped], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnnualFeeSummary {
    /// Rows with status `paid`.
    pub scouts_paid: i64,
    /// Sum of `amount_cents` over every row (all of it is money that arrived), before Stripe fees.
    pub total_cents: i64,
    /// Distinct `payment_id`s.
    pub checkouts: i64,
    pub needs_review: i64,
}

pub fn annual_fee_summary(conn: &Connection, year: &str) -> rusqlite::Result<AnnualFeeSummary> {
    conn.query_row(
        "SELECT COALESCE(SUM(status = 'paid'), 0), COALESCE(SUM(amount_cents), 0), COUNT(DISTINCT payment_id),
                COALESCE(SUM(status = 'needs_review'), 0)
         FROM annual_fees WHERE scouting_year = ?1",
        [year],
        |r| Ok(AnnualFeeSummary { scouts_paid: r.get(0)?, total_cents: r.get(1)?, checkouts: r.get(2)?, needs_review: r.get(3)? }),
    )
}

/// Scouting years that have fee rows, newest first.
pub fn annual_fee_years(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT scouting_year FROM annual_fees ORDER BY scouting_year DESC")?;
    stmt.query_map([], |r| r.get(0))?.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_gets_both_versions() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 2);
        assert_eq!(annual_fee_years(&conn).unwrap(), Vec::<String>::new());
        // Idempotent.
        migrate(&conn).unwrap();
    }

    #[test]
    fn v1_database_with_orders_migrates_to_v2() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("{SCHEMA} PRAGMA user_version = 1;")).unwrap();
        conn.execute(
            "INSERT INTO orders (id, status, email, buyer_name, phone, total_cents, created_at)
             VALUES ('o1', 'paid', 'a@b.co', 'Pat', '216-555-0142', 3500, '2026-10-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO order_items VALUES ('o1', 'w2', 'Wreath', 3500, 1, 'scout_delivery')", []).unwrap();

        migrate(&conn).unwrap();

        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 2);
        let o = get_order(&conn, "o1").unwrap().unwrap();
        assert_eq!((o.status, o.total_cents, o.lines.len()), (OrderStatus::Paid, 3500, 1));
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM annual_fees", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }
}
