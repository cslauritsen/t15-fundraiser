use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset, Utc};
use serde::Deserialize;
use shared::{CatalogItem, CatalogResponse, Fulfillment, Support};
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

/// Immutable, validated catalog. Loaded once at startup.
#[derive(Debug)]
pub struct Catalog {
    pub closes_at: DateTime<FixedOffset>,
    view: CatalogResponse,
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

        if !problems.is_empty() {
            bail!("{} problem(s):\n  - {}", problems.len(), problems.join("\n  - "));
        }
        let closes_at = closes_at.expect("checked above");
        Ok(Catalog {
            closes_at,
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
    fn duplicate_ids_rejected() {
        let two = format!("{GOOD}  - id: wreath\n    name: W2\n    price_cents: 1\n    fulfillment: direct_ship\n    image: a.jpg\n    image_alt: a\n");
        assert!(Catalog::parse(&two, |_| true).unwrap_err().to_string().contains("duplicate id"));
    }
}
