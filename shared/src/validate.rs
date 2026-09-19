use crate::api::*;
use std::collections::BTreeMap;

/// Gift label message limit, in characters.
pub const MAX_GIFT_MESSAGE: usize = 20;

const US_STATES: [&str; 51] = [
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "DC", "FL", "GA", "HI", "ID", "IL", "IN", "IA",
    "KS", "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM",
    "NY", "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA",
    "WV", "WI", "WY",
];

/// Trim, lowercase and syntax-check an email address.
pub fn normalize_email(input: &str) -> Result<String, &'static str> {
    let e = input.trim().to_lowercase();
    if e.is_empty() {
        return Err("Enter your email address.");
    }
    const BAD: &str = "That doesn't look like a valid email address.";
    if e.len() > 254 || e.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(BAD);
    }
    let mut parts = e.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(BAD);
    };
    if local.is_empty() || local.len() > 64 || local.starts_with('.') || local.ends_with('.') || local.contains("..") {
        return Err(BAD);
    }
    if !local
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+/=?^_`{|}~.-".contains(c))
    {
        return Err(BAD);
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return Err(BAD);
    }
    for l in &labels {
        if l.is_empty()
            || l.len() > 63
            || l.starts_with('-')
            || l.ends_with('-')
            || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(BAD);
        }
    }
    let tld = labels[labels.len() - 1];
    if tld.len() < 2 || !tld.chars().all(|c| c.is_ascii_alphabetic()) {
        return Err(BAD);
    }
    Ok(e)
}

/// US phone number to `NNN-NNN-NNNN`.
pub fn normalize_phone(input: &str) -> Result<String, &'static str> {
    let mut digits: String = input.chars().filter(|c| c.is_ascii_digit()).collect();
    if input.trim().is_empty() {
        return Err("Enter a phone number.");
    }
    if digits.len() == 11 && digits.starts_with('1') {
        digits.remove(0);
    }
    let valid = digits.len() == 10
        && !digits.starts_with(['0', '1'])
        && !digits[3..].starts_with(['0', '1']);
    if !valid {
        return Err("Enter a 10-digit US phone number.");
    }
    Ok(format!("{}-{}-{}", &digits[..3], &digits[3..6], &digits[6..]))
}

/// Accepts `12345` or `12345-6789`.
pub fn normalize_zip(input: &str) -> Result<String, &'static str> {
    let z = input.trim();
    let ok = match z.len() {
        5 => z.chars().all(|c| c.is_ascii_digit()),
        10 => {
            let (a, b) = z.split_at(5);
            a.chars().all(|c| c.is_ascii_digit())
                && b.starts_with('-')
                && b[1..].chars().all(|c| c.is_ascii_digit())
        }
        _ => false,
    };
    if ok { Ok(z.to_string()) } else { Err("Enter a 5-digit ZIP code.") }
}

pub fn normalize_state(input: &str) -> Result<String, &'static str> {
    let s = input.trim().to_uppercase();
    if US_STATES.contains(&s.as_str()) { Ok(s) } else { Err("Choose a US state.") }
}

/// Gift items ship only within the contiguous US: the 48 states plus DC.
pub fn is_contiguous_state(code: &str) -> bool {
    !matches!(code, "AK" | "HI")
}

/// A ZIP is local if its first five digits start with any prefix or equal any listed ZIP.
pub fn is_local_zip(zip: &str, prefixes: &[String], zips: &[String]) -> bool {
    let z5: String = zip.chars().take(5).collect();
    prefixes.iter().any(|p| z5.starts_with(p.as_str())) || zips.iter().any(|x| *x == z5)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidLine {
    pub item_id: String,
    pub name: String,
    pub unit_price_cents: i64,
    pub qty: u32,
    pub fulfillment: Fulfillment,
    pub image_url: String,
}

/// A checkout request after normalization, with prices taken from the catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedOrder {
    pub email: String,
    pub buyer_name: String,
    pub phone: String,
    pub scout_name: Option<String>,
    pub delivery: Option<Delivery>,
    pub shipping: Option<ShipTo>,
    pub gift_message: Option<String>,
    pub lines: Vec<ValidLine>,
    pub total_cents: i64,
}

struct Errs(Vec<FieldError>);

impl Errs {
    fn add(&mut self, field: &str, message: impl Into<String>) {
        self.0.push(FieldError { field: field.into(), message: message.into() });
    }

    /// Required trimmed text with a length cap and no control characters.
    fn text(&mut self, field: &str, input: &str, label: &str, max: usize) -> String {
        let t = input.trim();
        if t.is_empty() {
            self.add(field, format!("{label} is required."));
        } else if t.chars().count() > max {
            self.add(field, format!("{label} is too long (max {max} characters)."));
        } else if t.chars().any(|c| c.is_control()) {
            self.add(field, format!("{label} contains invalid characters."));
        }
        t.to_string()
    }

    fn optional_text(&mut self, field: &str, input: Option<&str>, label: &str, max: usize) -> Option<String> {
        let t = input.map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))?;
        if t.is_empty() {
            return None;
        }
        if t.chars().count() > max {
            self.add(field, format!("{label} is too long (max {max} characters)."));
        } else if t.chars().any(|c| c.is_control()) {
            self.add(field, format!("{label} contains invalid characters."));
        }
        Some(t)
    }
}

/// Validate and normalize a checkout request against the catalog.
/// Prices always come from the catalog, never from the request.
pub fn validate_checkout(
    req: &CheckoutRequest,
    catalog: &CatalogResponse,
) -> Result<ValidatedOrder, Vec<FieldError>> {
    let mut errs = Errs(Vec::new());

    // Cart lines: merge duplicates, check ids and quantities.
    let mut qty_by_item: BTreeMap<&str, u32> = BTreeMap::new();
    for (i, l) in req.lines.iter().enumerate() {
        let field = format!("lines[{i}]");
        if catalog.items.iter().all(|it| it.id != l.item_id) {
            errs.add(&field, "Unknown item.");
        } else if l.qty == 0 {
            errs.add(&field, "Quantity must be at least 1.");
        } else {
            let q = qty_by_item.entry(l.item_id.as_str()).or_insert(0);
            *q = q.saturating_add(l.qty);
        }
    }
    let mut lines = Vec::new();
    let mut total: i64 = 0;
    for item in &catalog.items {
        let Some(&qty) = qty_by_item.get(item.id.as_str()) else { continue };
        if qty > item.max_qty {
            errs.add("lines", format!("At most {} of \"{}\" per order.", item.max_qty, item.name));
            continue;
        }
        total += item.price_cents * i64::from(qty);
        lines.push(ValidLine {
            item_id: item.id.clone(),
            name: item.name.clone(),
            unit_price_cents: item.price_cents,
            qty,
            fulfillment: item.fulfillment,
            image_url: item.image_url.clone(),
        });
    }
    if req.lines.is_empty() {
        errs.add("lines", "Choose at least one item.");
    }

    let needs_delivery = lines.iter().any(|l| l.fulfillment == Fulfillment::ScoutDelivery);
    let needs_shipping = lines.iter().any(|l| l.fulfillment == Fulfillment::DirectShip);

    let email = match normalize_email(&req.email) {
        Ok(e) => e,
        Err(m) => {
            errs.add("email", m);
            String::new()
        }
    };
    let buyer_name = errs.text("buyer_name", &req.buyer_name, "Name", 100);
    let phone = match normalize_phone(&req.phone) {
        Ok(p) => p,
        Err(m) => {
            errs.add("phone", m);
            String::new()
        }
    };
    let scout_name = errs.optional_text("scout_name", req.scout_name.as_deref(), "Scout name", 100);

    let delivery = if needs_delivery {
        let d = req.delivery.clone().unwrap_or_default();
        let street = errs.text("delivery.street", &d.street, "Street address", 120);
        let city = errs.text("delivery.city", &d.city, "City", 60);
        let state = normalize_state(&d.state).unwrap_or_else(|m| {
            errs.add("delivery.state", m);
            String::new()
        });
        let zip = match normalize_zip(&d.zip) {
            Ok(z) if is_local_zip(&z, &catalog.local_zip_prefixes, &catalog.local_zips) => z,
            Ok(z) => {
                errs.add(
                    "delivery.zip",
                    "Scout delivery is only available in our local area. Use a local address or remove the delivery items.",
                );
                z
            }
            Err(m) => {
                errs.add("delivery.zip", m);
                String::new()
            }
        };
        let notes = errs.optional_text("delivery.notes", d.notes.as_deref(), "Delivery notes", 300);
        Some(Delivery { street, city, state, zip, notes })
    } else {
        None
    };

    let (shipping, gift_message) = if needs_shipping {
        let s = req.shipping.clone().unwrap_or_default();
        let name = errs.text("shipping.name", &s.name, "Recipient name", 100);
        let line1 = errs.text("shipping.line1", &s.line1, "Street address", 120);
        let line2 = errs.optional_text("shipping.line2", s.line2.as_deref(), "Apartment or suite", 120);
        let city = errs.text("shipping.city", &s.city, "City", 60);
        let state = match normalize_state(&s.state) {
            Ok(st) if is_contiguous_state(&st) => st,
            Ok(st) => {
                errs.add("shipping.state", "Gift items can only be shipped within the contiguous United States.");
                st
            }
            Err(m) => {
                errs.add("shipping.state", m);
                String::new()
            }
        };
        let postal_code = normalize_zip(&s.postal_code).unwrap_or_else(|m| {
            errs.add("shipping.postal_code", m);
            String::new()
        });
        let gift = errs.optional_text("gift_message", req.gift_message.as_deref(), "Gift message", MAX_GIFT_MESSAGE);
        (Some(ShipTo { name, line1, line2, city, state, postal_code }), gift)
    } else {
        (None, None)
    };

    if errs.0.is_empty() {
        Ok(ValidatedOrder {
            email,
            buyer_name,
            phone,
            scout_name,
            delivery,
            shipping,
            gift_message,
            lines,
            total_cents: total,
        })
    } else {
        Err(errs.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> CatalogResponse {
        let item = |id: &str, price, f| CatalogItem {
            id: id.into(),
            name: id.into(),
            description: String::new(),
            price_cents: price,
            fulfillment: f,
            max_qty: 5,
            image_url: format!("/images/{id}.jpg"),
            image_alt: "x".into(),
        };
        CatalogResponse {
            open: true,
            closes_at: String::new(),
            delivery_note: String::new(),
            shipping_note: String::new(),
            local_zip_prefixes: vec!["441".into()],
            local_zips: vec!["44001".into()],
            items: vec![
                item("wreath", 3500, Fulfillment::ScoutDelivery),
                item("box", 5500, Fulfillment::DirectShip),
            ],
        }
    }

    fn request() -> CheckoutRequest {
        CheckoutRequest {
            lines: vec![CartLine { item_id: "wreath".into(), qty: 2 }],
            email: " Pat@Example.COM ".into(),
            buyer_name: "Pat Smith".into(),
            phone: "(216) 555-0142".into(),
            scout_name: None,
            delivery: Some(Delivery {
                street: "1 Main St".into(),
                city: "Cleveland".into(),
                state: "oh".into(),
                zip: "44101".into(),
                notes: None,
            }),
            shipping: Some(ShipTo {
                name: "Sam Jones".into(),
                line1: "9 Elm St".into(),
                line2: None,
                city: "Akron".into(),
                state: "oh".into(),
                postal_code: "44301".into(),
            }),
            gift_message: Some("Happy Holidays!".into()),
        }
    }

    fn fields(r: Result<ValidatedOrder, Vec<FieldError>>) -> Vec<String> {
        r.unwrap_err().into_iter().map(|e| e.field).collect()
    }

    #[test]
    fn valid_order_is_normalized_and_priced_from_catalog() {
        let o = validate_checkout(&request(), &catalog()).unwrap();
        assert_eq!(o.email, "pat@example.com");
        assert_eq!(o.phone, "216-555-0142");
        assert_eq!(o.delivery.unwrap().state, "OH");
        assert_eq!(o.total_cents, 7000);
        // Delivery-only cart: shipping address and gift message are dropped.
        assert!(o.shipping.is_none() && o.gift_message.is_none());
    }

    #[test]
    fn mixed_cart_needs_both() {
        let mut r = request();
        r.lines.push(CartLine { item_id: "box".into(), qty: 1 });
        let o = validate_checkout(&r, &catalog()).unwrap();
        assert!(o.shipping.is_some() && o.delivery.is_some());
        assert_eq!(o.shipping.unwrap().state, "OH");
        assert_eq!(o.gift_message.as_deref(), Some("Happy Holidays!"));
        assert_eq!(o.total_cents, 12500);
    }

    #[test]
    fn direct_ship_only_ignores_delivery_address() {
        let mut r = request();
        r.lines = vec![CartLine { item_id: "box".into(), qty: 1 }];
        r.delivery = None;
        let o = validate_checkout(&r, &catalog()).unwrap();
        assert!(o.delivery.is_none() && o.shipping.is_some());
    }

    fn ship_only() -> CheckoutRequest {
        let mut r = request();
        r.lines = vec![CartLine { item_id: "box".into(), qty: 1 }];
        r.delivery = None;
        r
    }

    #[test]
    fn shipping_rejects_alaska_hawaii_and_bad_states() {
        for st in ["AK", "hi"] {
            let mut r = ship_only();
            r.shipping.as_mut().unwrap().state = st.into();
            assert_eq!(fields(validate_checkout(&r, &catalog())), ["shipping.state"], "{st}");
        }
        let mut r = ship_only();
        r.shipping.as_mut().unwrap().state = "DC".into();
        assert!(validate_checkout(&r, &catalog()).is_ok());
        r.shipping.as_mut().unwrap().state = "PR".into();
        assert!(validate_checkout(&r, &catalog()).is_err());
    }

    #[test]
    fn missing_shipping_reports_each_field() {
        let mut r = ship_only();
        r.shipping = None;
        assert_eq!(
            fields(validate_checkout(&r, &catalog())),
            ["shipping.name", "shipping.line1", "shipping.city", "shipping.state", "shipping.postal_code"]
        );
    }

    #[test]
    fn gift_message_limit_is_twenty_characters() {
        let mut r = ship_only();
        r.gift_message = Some("12345678901234567890".into()); // exactly 20
        assert_eq!(validate_checkout(&r, &catalog()).unwrap().gift_message.unwrap().len(), 20);
        r.gift_message = Some("123456789012345678901".into());
        assert_eq!(fields(validate_checkout(&r, &catalog())), ["gift_message"]);
        // Multi-byte characters count as one each; blank means no message.
        r.gift_message = Some("Joyeux Noël ❄❄❄❄❄❄❄❄".into());
        assert!(validate_checkout(&r, &catalog()).is_ok());
        r.gift_message = Some("   ".into());
        assert_eq!(validate_checkout(&r, &catalog()).unwrap().gift_message, None);
        r.gift_message = Some("Bell\u{7}".into());
        assert_eq!(fields(validate_checkout(&r, &catalog())), ["gift_message"]);
    }

    #[test]
    fn duplicate_lines_merge_and_respect_max_qty() {
        let mut r = request();
        r.lines = vec![
            CartLine { item_id: "wreath".into(), qty: 3 },
            CartLine { item_id: "wreath".into(), qty: 2 },
        ];
        assert_eq!(validate_checkout(&r, &catalog()).unwrap().total_cents, 17500);
        r.lines.push(CartLine { item_id: "wreath".into(), qty: 1 });
        assert_eq!(fields(validate_checkout(&r, &catalog())), ["lines"]);
    }

    #[test]
    fn rejects_bad_cart() {
        let mut r = request();
        r.lines = vec![];
        assert_eq!(fields(validate_checkout(&r, &catalog())), ["lines"]);
        r.lines = vec![CartLine { item_id: "nope".into(), qty: 1 }];
        assert!(fields(validate_checkout(&r, &catalog())).contains(&"lines[0]".to_string()));
        r.lines = vec![CartLine { item_id: "wreath".into(), qty: 0 }];
        assert!(fields(validate_checkout(&r, &catalog())).contains(&"lines[0]".to_string()));
    }

    #[test]
    fn rejects_non_local_zip_for_delivery() {
        let mut r = request();
        r.delivery.as_mut().unwrap().zip = "90210".into();
        assert_eq!(fields(validate_checkout(&r, &catalog())), ["delivery.zip"]);
        r.delivery.as_mut().unwrap().zip = "44001-1234".into();
        assert!(validate_checkout(&r, &catalog()).is_ok());
    }

    #[test]
    fn missing_delivery_reports_each_field() {
        let mut r = request();
        r.delivery = None;
        let f = fields(validate_checkout(&r, &catalog()));
        assert_eq!(f, ["delivery.street", "delivery.city", "delivery.state", "delivery.zip"]);
    }

    #[test]
    fn emails() {
        assert!(normalize_email("a.b+c@sub.example.org").is_ok());
        for bad in ["", "a", "a@b", "a@@b.com", "a b@c.com", "@c.com", "a@-c.com", "a@c.c", "a..b@c.com", "a@c..com"] {
            assert!(normalize_email(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn phones() {
        assert_eq!(normalize_phone("+1 (216) 555-0142").unwrap(), "216-555-0142");
        assert_eq!(normalize_phone("216.555.0142").unwrap(), "216-555-0142");
        for bad in ["", "555-0142", "116-555-0142", "216-155-0142", "216555014299"] {
            assert!(normalize_phone(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn zips_and_states() {
        assert!(normalize_zip("44101").is_ok() && normalize_zip("44101-1234").is_ok());
        for bad in ["4410", "441011", "44101-12", "abcde", "44101 1234"] {
            assert!(normalize_zip(bad).is_err(), "{bad}");
        }
        assert_eq!(normalize_state(" oh ").unwrap(), "OH");
        assert!(normalize_state("ZZ").is_err());
    }

    #[test]
    fn control_chars_rejected() {
        let mut r = request();
        r.buyer_name = "Pat\u{0}".into();
        assert_eq!(fields(validate_checkout(&r, &catalog())), ["buyer_name"]);
    }

    #[test]
    fn cents_format() {
        assert_eq!(format_cents(3500), "$35.00");
        assert_eq!(format_cents(1205), "$12.05");
    }
}
