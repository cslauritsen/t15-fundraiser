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
