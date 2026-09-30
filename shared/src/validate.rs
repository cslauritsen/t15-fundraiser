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
    let scout_name = Some(errs.text("scout_name", req.scout_name.as_deref().unwrap_or(""), "Scout name", 100));

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

// ---------------------------------------------------------------------------------------------
// Annual camping fee
// ---------------------------------------------------------------------------------------------

/// Scout first/last name limit, in characters.
pub const MAX_SCOUT_NAME: usize = 50;
/// Payer name limit, in characters.
pub const MAX_PAYER_NAME: usize = 100;

/// Trim and collapse whitespace; 1..=`max` characters; no control characters. Control characters
/// are checked before collapsing, so a tab inside a name is rejected rather than turned into a
/// space (tab separates first and last name in the Stripe metadata).
pub fn normalize_person_name(input: &str, label: &str, max: usize) -> Result<String, String> {
    let t = input.trim();
    if t.is_empty() {
        return Err(format!("{label} is required."));
    }
    if t.chars().any(char::is_control) {
        return Err(format!("{label} contains invalid characters."));
    }
    let collapsed = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max {
        return Err(format!("{label} is too long (max {max} characters)."));
    }
    Ok(collapsed)
}

/// Validate one scout row. Field names are `first_name` and `last_name`.
pub fn validate_fee_scout(s: &FeeScout) -> Result<FeeScout, Vec<FieldError>> {
    let mut errs = Errs(Vec::new());
    let mut name = |field: &str, input: &str, label: &str| {
        normalize_person_name(input, label, MAX_SCOUT_NAME).unwrap_or_else(|m| {
            errs.add(field, m);
            String::new()
        })
    };
    let first_name = name("first_name", &s.first_name, "Scout first name");
    let last_name = name("last_name", &s.last_name, "Scout last name");
    if errs.0.is_empty() { Ok(FeeScout { first_name, last_name }) } else { Err(errs.0) }
}

/// Case-folded identity used for duplicate detection (names are already normalized).
pub fn fee_scout_key(s: &FeeScout) -> (String, String) {
    (s.first_name.to_lowercase(), s.last_name.to_lowercase())
}

/// Why `candidate` can't be added to `cart`, if it can't. Used by the SPA's "Add scout" button.
pub fn fee_cart_add_error(cart: &[FeeScout], candidate: &FeeScout, max_scouts: u32) -> Option<String> {
    if cart.len() >= max_scouts as usize {
        return Some(format!("You can pay for at most {max_scouts} scouts per checkout."));
    }
    let key = fee_scout_key(candidate);
    cart.iter()
        .any(|s| fee_scout_key(s) == key)
        .then(|| format!("{} is already in the cart.", candidate.full_name()))
}

/// A fee checkout request after normalization.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedFeeCheckout {
    pub payer_name: String,
    pub payer_email: String,
    pub scouts: Vec<FeeScout>,
}

/// Validate a fee checkout. Fields: `payer_name`, `payer_email`, `scouts` (count and duplicates),
/// and `scouts[i].first_name` / `scouts[i].last_name`.
pub fn validate_fee_checkout(req: &FeeCheckoutRequest, max_scouts: u32) -> Result<ValidatedFeeCheckout, Vec<FieldError>> {
    let mut errs = Errs(Vec::new());

    let payer_name = normalize_person_name(&req.payer_name, "Your name", MAX_PAYER_NAME).unwrap_or_else(|m| {
        errs.add("payer_name", m);
        String::new()
    });
    let payer_email = normalize_email(&req.payer_email).unwrap_or_else(|m| {
        errs.add("payer_email", m);
        String::new()
    });

    if req.scouts.is_empty() {
        errs.add("scouts", "Add at least one scout.");
    } else if req.scouts.len() > max_scouts as usize {
        errs.add("scouts", format!("You can pay for at most {max_scouts} scouts per checkout."));
    }
    let mut scouts: Vec<FeeScout> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (i, s) in req.scouts.iter().enumerate() {
        match validate_fee_scout(s) {
            Ok(s) => {
                if !seen.insert(fee_scout_key(&s)) {
                    errs.add("scouts", format!("{} is listed more than once.", s.full_name()));
                }
                scouts.push(s);
            }
            Err(fs) => {
                for f in fs {
                    errs.add(&format!("scouts[{i}].{}", f.field), f.message);
                }
            }
        }
    }

    if errs.0.is_empty() { Ok(ValidatedFeeCheckout { payer_name, payer_email, scouts }) } else { Err(errs.0) }
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
            support: Default::default(),
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
            scout_name: Some("Alex".into()),
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
    fn scout_name_is_required() {
        for missing in [None, Some("   ".to_string())] {
            let mut r = request();
            r.scout_name = missing;
            assert_eq!(fields(validate_checkout(&r, &catalog())), ["scout_name"]);
        }
        assert_eq!(validate_checkout(&request(), &catalog()).unwrap().scout_name.as_deref(), Some("Alex"));
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

    fn scout(first: &str, last: &str) -> FeeScout {
        FeeScout { first_name: first.into(), last_name: last.into() }
    }

    fn fee_request(scouts: Vec<FeeScout>) -> FeeCheckoutRequest {
        FeeCheckoutRequest { payer_name: "  Pat   Smith ".into(), payer_email: " Pat@Example.COM ".into(), scouts }
    }

    fn fee_fields(r: Result<ValidatedFeeCheckout, Vec<FieldError>>) -> Vec<String> {
        r.unwrap_err().into_iter().map(|e| e.field).collect()
    }

    #[test]
    fn person_names_are_trimmed_collapsed_and_capped() {
        assert_eq!(normalize_person_name("  Mary   Ann  ", "Name", 50).unwrap(), "Mary Ann");
        assert_eq!(normalize_person_name("O'Brien-Smith Jr.", "Name", 50).unwrap(), "O'Brien-Smith Jr.");
        // Other printable Unicode is accepted.
        assert_eq!(normalize_person_name("Zoë 李", "Name", 50).unwrap(), "Zoë 李");
        assert!(normalize_person_name("   ", "Name", 50).is_err());
        assert!(normalize_person_name(&"é".repeat(50), "Name", 50).is_ok(), "characters, not bytes");
        assert!(normalize_person_name(&"a".repeat(51), "Name", 50).is_err());
        // Collapsing happens before the length check.
        assert!(normalize_person_name(&format!("{}      {}", "a".repeat(24), "b".repeat(25)), "Name", 50).is_ok());
        // Control characters, including the metadata's tab separator, are rejected.
        for bad in ["Pat\tSmith", "Pat\nSmith", "Pat\u{0}"] {
            assert!(normalize_person_name(bad, "Name", 50).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn valid_fee_checkout_is_normalized() {
        let v = validate_fee_checkout(&fee_request(vec![scout(" Alex ", "Smith"), scout("Jamie", " Smith")]), 8).unwrap();
        assert_eq!(v.payer_name, "Pat Smith");
        assert_eq!(v.payer_email, "pat@example.com");
        assert_eq!(v.scouts, [scout("Alex", "Smith"), scout("Jamie", "Smith")]);
    }

    #[test]
    fn fee_checkout_requires_payer_and_scouts() {
        let r = FeeCheckoutRequest { payer_name: " ".into(), payer_email: "nope".into(), scouts: vec![] };
        assert_eq!(fee_fields(validate_fee_checkout(&r, 8)), ["payer_name", "payer_email", "scouts"]);
        let r = fee_request(vec![scout("", "Smith"), scout("Al\tex", &"x".repeat(51))]);
        assert_eq!(
            fee_fields(validate_fee_checkout(&r, 8)),
            ["scouts[0].first_name", "scouts[1].first_name", "scouts[1].last_name"]
        );
    }

    #[test]
    fn fee_checkout_rejects_case_insensitive_duplicates() {
        let r = fee_request(vec![scout("Alex", "Smith"), scout(" ALEX ", "smith")]);
        assert_eq!(fee_fields(validate_fee_checkout(&r, 8)), ["scouts"]);
        // Same first name, different last name is fine.
        assert!(validate_fee_checkout(&fee_request(vec![scout("Alex", "Smith"), scout("Alex", "Jones")]), 8).is_ok());
    }

    #[test]
    fn fee_checkout_respects_max_scouts() {
        let three: Vec<FeeScout> = (0..3).map(|i| scout(&format!("S{i}"), "Smith")).collect();
        assert!(validate_fee_checkout(&fee_request(three[..2].to_vec()), 2).is_ok());
        assert_eq!(fee_fields(validate_fee_checkout(&fee_request(three), 2)), ["scouts"]);
    }

    #[test]
    fn cart_add_checks_limit_and_duplicates() {
        let cart = vec![scout("Alex", "Smith")];
        assert!(fee_cart_add_error(&cart, &scout("Jamie", "Smith"), 2).is_none());
        assert!(fee_cart_add_error(&cart, &scout("alex", "SMITH"), 2).unwrap().contains("already"));
        assert!(fee_cart_add_error(&cart, &scout("Jamie", "Smith"), 1).unwrap().contains("at most 1"));
    }

    #[test]
    fn fee_scout_row_errors_use_row_field_names() {
        let errs = validate_fee_scout(&scout(" ", "")).unwrap_err();
        assert_eq!(errs.iter().map(|e| e.field.as_str()).collect::<Vec<_>>(), ["first_name", "last_name"]);
    }

    #[test]
    fn local_datetime_format() {
        assert_eq!(format_local_datetime("2027-01-31T23:59:59-05:00").as_deref(), Some("January 31, 2027 at 11:59 PM"));
        assert_eq!(format_local_datetime("2026-09-05T00:05:00Z").as_deref(), Some("September 5, 2026 at 12:05 AM"));
        assert_eq!(format_local_datetime("2026-09-05T12:00:00+00:00").as_deref(), Some("September 5, 2026 at 12:00 PM"));
        assert!(format_local_datetime("tomorrow").is_none());
        assert!(format_local_datetime("2026-13-05T12:00:00Z").is_none());
    }
}
