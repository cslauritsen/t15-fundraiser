use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset, Utc};
use serde::Deserialize;
use shared::{AnnualFeeInfo, CatalogItem, CatalogResponse, Fulfillment, Support};
use std::path::Path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalog {
    closes_at: String,
    #[serde(default)]
    delivery_note: String,
    #[serde(default)]
    shipping_note: String,
    #[serde(default)]
    support: Support,
    fulfillment: RawFulfillment,
    items: Vec<RawItem>,
    /// Google account emails allowed to sign in at /admin (case-insensitive).
    #[serde(default)]
    admins: Vec<String>,
    /// Annual camping fee page; the feature is disabled when absent.
    #[serde(default)]
    annual_fee: Option<RawAnnualFee>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAnnualFee {
    scouting_year: String,
    amount_cents: i64,
    closes_at: String,
    max_scouts: u32,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFulfillment {
    #[serde(default)]
    local_zip_prefixes: Vec<String>,
    #[serde(default)]
    local_zips: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawItem {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    price_cents: i64,
    fulfillment: Fulfillment,
    max_qty: Option<u32>,
    image: String,
    image_alt: String,
}

const DEFAULT_MAX_QTY: u32 = 20;
/// Stripe metadata allows 50 keys; each scout takes one, so keep well clear.
const MAX_FEE_SCOUTS: u32 = 10;

/// The validated `annual_fee` block. Its `closes_at` is independent of the greenery cutoff.
#[derive(Debug, Clone)]
pub struct AnnualFeeConfig {
    pub scouting_year: String,
    /// Per scout.
    pub amount_cents: i64,
    /// Written with the troop's local offset, which is also used to show local dates.
    pub closes_at: DateTime<FixedOffset>,
    pub max_scouts: u32,
    pub note: Option<String>,
}

impl AnnualFeeConfig {
    pub fn is_open(&self, now: DateTime<Utc>) -> bool {
        now <= self.closes_at
    }

    pub fn view(&self, now: DateTime<Utc>) -> AnnualFeeInfo {
        AnnualFeeInfo {
            scouting_year: self.scouting_year.clone(),
            amount_cents: self.amount_cents,
            max_scouts: self.max_scouts,
            note: self.note.clone(),
            closes_at: self.closes_at.to_rfc3339(),
            open: self.is_open(now),
        }
    }

    fn parse(raw: RawAnnualFee, problems: &mut Vec<String>) -> Option<Self> {
        let n = problems.len();
        let year = raw.scouting_year.trim().to_string();
        if !(1..=20).contains(&year.chars().count()) || year.chars().any(char::is_control) {
            problems.push("annual_fee.scouting_year must be 1-20 characters".into());
        }
        if raw.amount_cents <= 0 {
            problems.push("annual_fee.amount_cents must be > 0".into());
        }
        if !(1..=MAX_FEE_SCOUTS).contains(&raw.max_scouts) {
            problems.push(format!("annual_fee.max_scouts must be 1-{MAX_FEE_SCOUTS}"));
        }
        let closes_at = DateTime::parse_from_rfc3339(&raw.closes_at)
            .map_err(|e| {
                problems.push(format!("annual_fee.closes_at {:?} is not RFC 3339 with an offset: {e}", raw.closes_at))
            })
            .ok();
        let note = raw.note.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        (problems.len() == n).then(|| AnnualFeeConfig {
            scouting_year: year,
            amount_cents: raw.amount_cents,
            closes_at: closes_at.expect("checked above"),
            max_scouts: raw.max_scouts,
            note,
        })
    }
}

/// Immutable, validated catalog. Loaded once at startup.
#[derive(Debug)]
pub struct Catalog {
    pub closes_at: DateTime<FixedOffset>,
    view: CatalogResponse,
    /// Lowercased admin emails from catalog.yaml; not part of the public `view`.
    admins: Vec<String>,
    pub annual_fee: Option<AnnualFeeConfig>,
}

impl Catalog {
    /// Load from a YAML file; `images_dir` is where each item's `image` must exist
    /// (skipped when `check_images` is false).
    pub fn load(path: &Path, images_dir: &Path, check_images: bool) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading catalog {}", path.display()))?;
        Self::parse(&text, |name| !check_images || images_dir.join(name).is_file())
            .with_context(|| format!("invalid catalog {}", path.display()))
    }

    pub fn parse(yaml: &str, image_exists: impl Fn(&str) -> bool) -> Result<Self> {
        let raw: RawCatalog = serde_yaml::from_str(yaml).context("parsing YAML")?;
        let mut problems: Vec<String> = Vec::new();

        let closes_at = match DateTime::parse_from_rfc3339(&raw.closes_at) {
            Ok(t) => Some(t),
            Err(e) => {
                problems.push(format!("closes_at {:?} is not RFC 3339 with an offset: {e}", raw.closes_at));
                None
            }
        };

        for p in &raw.fulfillment.local_zip_prefixes {
            if p.is_empty() || p.len() > 5 || !p.chars().all(|c| c.is_ascii_digit()) {
                problems.push(format!("local_zip_prefixes entry {p:?} must be 1-5 digits"));
            }
        }
        for z in &raw.fulfillment.local_zips {
            if z.len() != 5 || !z.chars().all(|c| c.is_ascii_digit()) {
                problems.push(format!("local_zips entry {z:?} must be exactly 5 digits"));
            }
        }
        let has_delivery = raw.items.iter().any(|i| i.fulfillment == Fulfillment::ScoutDelivery);
        if has_delivery && raw.fulfillment.local_zip_prefixes.is_empty() && raw.fulfillment.local_zips.is_empty() {
            problems.push("scout_delivery items exist but no local_zip_prefixes or local_zips are configured".into());
        }

        if raw.items.is_empty() {
            problems.push("items must not be empty".into());
        }
        let mut seen = std::collections::HashSet::new();
        let mut items = Vec::new();
        for it in &raw.items {
            let who = format!("item {:?}", it.id);
            if it.id.is_empty() || !it.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
                problems.push(format!("{who}: id must be lowercase letters, digits and '-'"));
            }
            if !seen.insert(it.id.clone()) {
                problems.push(format!("{who}: duplicate id"));
            }
            if it.name.trim().is_empty() {
                problems.push(format!("{who}: name is empty"));
            }
            if it.price_cents <= 0 {
                problems.push(format!("{who}: price_cents must be > 0"));
            }
            let max_qty = it.max_qty.unwrap_or(DEFAULT_MAX_QTY);
            if !(1..=100).contains(&max_qty) {
                problems.push(format!("{who}: max_qty must be 1-100"));
            }
            if it.image.is_empty() || it.image.contains('/') || it.image.contains("..") || it.image.starts_with('.') {
                problems.push(format!("{who}: image must be a plain filename in static/images/"));
            } else if !image_exists(&it.image) {
                problems.push(format!("{who}: image file {:?} not found in static/images/", it.image));
            }
            if it.image_alt.trim().is_empty() {
                problems.push(format!("{who}: image_alt is required"));
            }
            items.push(CatalogItem {
                id: it.id.clone(),
                name: it.name.clone(),
                description: it.description.clone(),
                price_cents: it.price_cents,
                fulfillment: it.fulfillment,
                max_qty,
                image_url: format!("/images/{}", it.image),
                image_alt: it.image_alt.clone(),
            });
        }

        let annual_fee = raw.annual_fee.and_then(|f| AnnualFeeConfig::parse(f, &mut problems));

        if !problems.is_empty() {
            bail!("{} problem(s):\n  - {}", problems.len(), problems.join("\n  - "));
        }
        let closes_at = closes_at.expect("checked above");
        Ok(Catalog {
            closes_at,
            admins: raw.admins.iter().map(|e| e.to_lowercase()).collect(),
            annual_fee,
            view: CatalogResponse {
                open: true,
                closes_at: closes_at.to_rfc3339(),
                delivery_note: raw.delivery_note,
                shipping_note: raw.shipping_note,
                local_zip_prefixes: raw.fulfillment.local_zip_prefixes,
                local_zips: raw.fulfillment.local_zips,
                support: raw.support,
                items,
            },
        })
    }

    pub fn is_open(&self, now: DateTime<Utc>) -> bool {
        now <= self.closes_at
    }

    /// Whether `email` (compared case-insensitively) is in the catalog's `admins` list.
    pub fn is_admin(&self, email: &str) -> bool {
        self.admins.iter().any(|a| a == &email.to_lowercase())
    }

    /// The public view, with `open` set for the given time.
    pub fn view(&self, now: DateTime<Utc>) -> CatalogResponse {
        CatalogResponse { open: self.is_open(now), ..self.view.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
closes_at: "2026-10-30T23:59:59-04:00"
delivery_note: "d"
fulfillment:
  local_zip_prefixes: ["441"]
items:
  - id: wreath
    name: Wreath
    price_cents: 3500
    fulfillment: scout_delivery
    image: wreath.jpg
    image_alt: A wreath
"#;

    #[test]
    fn parses_good_catalog() {
        let c = Catalog::parse(GOOD, |_| true).unwrap();
        let v = c.view(Utc::now());
        assert_eq!(v.items[0].max_qty, 20);
        assert_eq!(v.items[0].image_url, "/images/wreath.jpg");
    }

    #[test]
    fn cutoff_is_inclusive_at_the_second() {
        let c = Catalog::parse(GOOD, |_| true).unwrap();
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        assert!(c.is_open(at("2026-10-30T23:59:59-04:00")));
        assert!(!c.is_open(at("2026-10-31T00:00:00-04:00")));
    }

    #[test]
    fn reports_all_problems_at_once() {
        let bad = GOOD
            .replace("2026-10-30T23:59:59-04:00", "tomorrow")
            .replace("price_cents: 3500", "price_cents: 0")
            .replace("id: wreath", "id: Wreath!");
        let err = Catalog::parse(&bad, |_| false).unwrap_err().to_string();
        assert!(err.contains("3 problem") || err.contains("4 problem"), "{err}");
        assert!(err.contains("closes_at") && err.contains("price_cents") && err.contains("id must be"));
    }

    #[test]
    fn missing_image_and_typos_are_errors() {
        let err = Catalog::parse(GOOD, |_| false).unwrap_err().to_string();
        assert!(err.contains("not found"), "{err}");
        let typo = GOOD.replace("image_alt", "image_atl");
        assert!(Catalog::parse(&typo, |_| true).is_err());
    }

    #[test]
    fn admin_emails_match_case_insensitively() {
        let with_admins = GOOD.replacen("items:", "admins: [\"Admin@Example.com\"]\nitems:", 1);
        let c = Catalog::parse(&with_admins, |_| true).unwrap();
        assert!(c.is_admin("admin@example.com"));
        assert!(c.is_admin("ADMIN@EXAMPLE.COM"));
        assert!(!c.is_admin("other@example.com"));
    }

    #[test]
    fn defaults_to_no_admins() {
        let c = Catalog::parse(GOOD, |_| true).unwrap();
        assert!(!c.is_admin("anyone@example.com"));
    }

    const FEE: &str = r#"
annual_fee:
  scouting_year: "2026-2027"
  amount_cents: 5000
  closes_at: "2027-01-31T23:59:59-05:00"
  max_scouts: 8
  note: "Covers campouts."
"#;

    #[test]
    fn annual_fee_is_optional() {
        assert!(Catalog::parse(GOOD, |_| true).unwrap().annual_fee.is_none());
    }

    #[test]
    fn parses_annual_fee_block_with_its_own_cutoff() {
        let c = Catalog::parse(&format!("{GOOD}{FEE}"), |_| true).unwrap();
        let f = c.annual_fee.clone().unwrap();
        assert_eq!((f.scouting_year.as_str(), f.amount_cents, f.max_scouts), ("2026-2027", 5000, 8));
        assert_eq!(f.note.as_deref(), Some("Covers campouts."));
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        // Greenery is closed by November, fees are still open.
        assert!(!c.is_open(at("2026-11-15T12:00:00Z")) && f.is_open(at("2026-11-15T12:00:00Z")));
        assert!(f.is_open(at("2027-01-31T23:59:59-05:00")));
        assert!(!f.is_open(at("2027-02-01T00:00:00-05:00")));
        let v = f.view(at("2027-02-01T00:00:00-05:00"));
        assert_eq!((v.open, v.closes_at.as_str()), (false, "2027-01-31T23:59:59-05:00"));
    }

    #[test]
    fn bad_annual_fee_block_is_rejected() {
        let bad = FEE
            .replace("2027-01-31T23:59:59-05:00", "2027-01-31 midnight")
            .replace("amount_cents: 5000", "amount_cents: 0")
            .replace("max_scouts: 8", "max_scouts: 11")
            .replace("2026-2027", "");
        let err = Catalog::parse(&format!("{GOOD}{bad}"), |_| true).unwrap_err().to_string();
        assert!(err.contains("4 problem"), "{err}");
        for want in ["annual_fee.closes_at", "amount_cents", "max_scouts", "scouting_year"] {
            assert!(err.contains(want), "{want}: {err}");
        }
        // closes_at is required.
        let missing = FEE.replace("  closes_at: \"2027-01-31T23:59:59-05:00\"\n", "");
        assert!(Catalog::parse(&format!("{GOOD}{missing}"), |_| true).is_err());
        // Typos are rejected like the rest of the catalog.
        let typo = FEE.replace("max_scouts", "max_scout");
        assert!(Catalog::parse(&format!("{GOOD}{typo}"), |_| true).is_err());
    }

    #[test]
    fn duplicate_ids_rejected() {
        let two = format!("{GOOD}  - id: wreath\n    name: W2\n    price_cents: 1\n    fulfillment: direct_ship\n    image: a.jpg\n    image_alt: a\n");
        assert!(Catalog::parse(&two, |_| true).unwrap_err().to_string().contains("duplicate id"));
    }
}
