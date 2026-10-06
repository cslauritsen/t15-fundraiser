use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fulfillment {
    ScoutDelivery,
    DirectShip,
}

impl Fulfillment {
    pub fn as_str(self) -> &'static str {
        match self {
            Fulfillment::ScoutDelivery => "scout_delivery",
            Fulfillment::DirectShip => "direct_ship",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "scout_delivery" => Some(Fulfillment::ScoutDelivery),
            "direct_ship" => Some(Fulfillment::DirectShip),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogItem {
    pub id: String,
    pub name: String,
    pub description: String,
    pub price_cents: i64,
    pub fulfillment: Fulfillment,
    pub max_qty: u32,
    pub image_url: String,
    pub image_alt: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Support {
    pub email: String,
    pub phone: String,
}

impl Default for Support {
    fn default() -> Self {
        Self { email: String::new(), phone: String::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogResponse {
    pub open: bool,
    /// RFC 3339 timestamp; display only.
    pub closes_at: String,
    pub delivery_note: String,
    pub shipping_note: String,
    pub local_zip_prefixes: Vec<String>,
    pub local_zips: Vec<String>,
    pub support: Support,
    pub items: Vec<CatalogItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CartLine {
    pub item_id: String,
    pub qty: u32,
}

/// Local drop-off address for scout-delivery items.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Delivery {
    pub street: String,
    pub city: String,
    pub state: String,
    pub zip: String,
    #[serde(default)]
    pub notes: Option<String>,
}

/// Gift recipient's address for direct-ship items (contiguous US only).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ShipTo {
    pub name: String,
    pub line1: String,
    #[serde(default)]
    pub line2: Option<String>,
    pub city: String,
    pub state: String,
    pub postal_code: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckoutRequest {
    pub lines: Vec<CartLine>,
    pub email: String,
    pub buyer_name: String,
    pub phone: String,
    #[serde(default)]
    pub scout_name: Option<String>,
    #[serde(default)]
    pub delivery: Option<Delivery>,
    /// Required when the cart has direct-ship items.
    #[serde(default)]
    pub shipping: Option<ShipTo>,
    /// Short message for the gift label (direct-ship orders only).
    #[serde(default)]
    pub gift_message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckoutResponse {
    pub checkout_url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub fields: Vec<FieldError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Pending,
    Paid,
    Expired,
    Failed,
    /// Money arrived but something didn't add up (e.g. amount mismatch); a human should look.
    NeedsReview,
}

impl OrderStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderStatus::Pending => "pending",
            OrderStatus::Paid => "paid",
            OrderStatus::Expired => "expired",
            OrderStatus::Failed => "failed",
            OrderStatus::NeedsReview => "needs_review",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(OrderStatus::Pending),
            "paid" => Some(OrderStatus::Paid),
            "expired" => Some(OrderStatus::Expired),
            "failed" => Some(OrderStatus::Failed),
            "needs_review" => Some(OrderStatus::NeedsReview),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderLine {
    pub item_id: String,
    pub name: String,
    pub unit_price_cents: i64,
    pub qty: u32,
    pub fulfillment: Fulfillment,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderStatusResponse {
    pub order_id: String,
    pub status: OrderStatus,
    pub total_cents: i64,
    pub email: String,
    pub lines: Vec<OrderLine>,
    pub delivery: Option<Delivery>,
    pub ship_to: Option<ShipTo>,
    pub gift_message: Option<String>,
}

/// `3500` -> `"$35.00"`.
pub fn format_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let c = cents.abs();
    format!("{sign}${}.{:02}", c / 100, c % 100)
}

// ---------------------------------------------------------------------------------------------
// Annual camping fee
// ---------------------------------------------------------------------------------------------

/// `GET /api/annual-fee`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnnualFeeInfo {
    pub scouting_year: String,
    /// Per scout.
    pub amount_cents: i64,
    pub max_scouts: u32,
    #[serde(default)]
    pub note: Option<String>,
    /// RFC 3339 with the troop's local offset; display only.
    pub closes_at: String,
    pub open: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeScout {
    pub first_name: String,
    pub last_name: String,
}

impl FeeScout {
    pub fn full_name(&self) -> String {
        format!("{} {}", self.first_name, self.last_name)
    }
}

/// `GET /api/annual-fee/check-name?first_name=&last_name=`: paid scouts for the current scouting
/// year that look like the one being entered. Advisory only; it never blocks adding a scout.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeNameCheckResponse {
    /// A paid scout has this exact first and last name (case-insensitive).
    pub exact: bool,
    /// Paid scouts whose first name starts with the entered first name and whose last name
    /// matches exactly (case-insensitive), excluding an exact match. As recorded.
    pub similar: Vec<FeeScout>,
}

/// `POST /api/annual-fee/checkout`. There is deliberately no amount: it comes from the config.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FeeCheckoutRequest {
    pub payer_name: String,
    pub payer_email: String,
    pub scouts: Vec<FeeScout>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeStatus {
    Paid,
    /// Money arrived but the metadata or amount didn't check out; the treasurer confirms.
    NeedsReview,
    /// No payment recorded yet and the Stripe session is still open.
    Pending,
    /// The Stripe session expired unpaid.
    Expired,
}

impl FeeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FeeStatus::Paid => "paid",
            FeeStatus::NeedsReview => "needs_review",
            FeeStatus::Pending => "pending",
            FeeStatus::Expired => "expired",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "paid" => Some(FeeStatus::Paid),
            "needs_review" => Some(FeeStatus::NeedsReview),
            "pending" => Some(FeeStatus::Pending),
            "expired" => Some(FeeStatus::Expired),
            _ => None,
        }
    }
}

/// `GET /api/annual-fee/{payment_id}/status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeeStatusResponse {
    pub status: FeeStatus,
    pub scouting_year: String,
    pub scouts: Vec<FeeScout>,
    pub total_cents: i64,
    pub payer_email: String,
}

/// Format an RFC 3339 timestamp in its own offset, e.g.
/// `"2027-01-31T23:59:59-05:00"` -> `"January 31, 2027 at 11:59 PM"`.
/// The config writes the troop's local offset, so this is the troop's local time without
/// needing a time zone database in the browser. `None` if the string doesn't parse.
pub fn format_local_datetime(rfc3339: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September", "October",
        "November", "December",
    ];
    let (date, time) = rfc3339.split_once(['T', 't', ' '])?;
    let mut d = date.splitn(3, '-');
    let (year, month, day) = (d.next()?, d.next()?.parse::<usize>().ok()?, d.next()?.parse::<u32>().ok()?);
    let mut t = time.splitn(3, ':');
    let (hour, minute) = (t.next()?.parse::<u32>().ok()?, t.next()?.get(..2)?.parse::<u32>().ok()?);
    if year.len() != 4 || !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let (h12, ampm) = match hour {
        0 => (12, "AM"),
        1..=11 => (hour, "AM"),
        12 => (12, "PM"),
        _ => (hour - 12, "PM"),
    };
    Some(format!("{} {day}, {year} at {h12}:{minute:02} {ampm}", MONTHS[month - 1]))
}
