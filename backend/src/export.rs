use crate::db::OrderRow;
use anyhow::Result;
use shared::format_cents;
use std::collections::BTreeSet;
use std::io::Write;

/// One row per order with one quantity column per product, headed by its item id (blank = none).
/// Columns follow `item_ids` (catalog order); ids that appear in orders but not in `item_ids`
/// (e.g. items since removed from the catalog) are appended in id order.
pub fn write_csv(orders: &[OrderRow], item_ids: &[String], mut w: impl Write) -> Result<()> {
    let mut products: Vec<&str> = item_ids.iter().map(String::as_str).collect();
    let extra: BTreeSet<&str> = orders
        .iter()
        .flat_map(|o| o.lines.iter().map(|l| l.item_id.as_str()))
        .filter(|id| !products.contains(id))
        .collect();
    products.extend(extra);

    // Byte-order mark: without it Excel reads the file as MacRoman/Windows-1252 and mangles
    // non-ASCII characters (curly quotes, accents).
    w.write_all(b"\xEF\xBB\xBF")?;
    let mut out = csv::Writer::from_writer(w);
    let mut header: Vec<&str> = vec!["order_id", "status", "paid_at", "buyer_name", "email", "phone", "scout_name", "total"];
    header.extend(&products);
    header.extend(["delivery_address", "delivery_notes", "ship_to", "gift_message", "review_reason"]);
    out.write_record(&header)?;
    for o in orders {
        let delivery_address = o
            .delivery
            .as_ref()
            .map(|d| format!("{}, {}, {} {}", d.street, d.city, d.state, d.zip))
            .unwrap_or_default();
        let ship_to = o
            .ship_to
            .as_ref()
            .map(|s| {
                let street = match &s.line2 {
                    Some(l2) => format!("{}, {}", s.line1, l2),
                    None => s.line1.clone(),
                };
                format!("{}, {}, {}, {} {}", s.name, street, s.city, s.state, s.postal_code)
            })
            .unwrap_or_default();
        let mut row: Vec<String> = vec![
            o.id.clone(),
            o.status.as_str().to_string(),
            o.paid_at.clone().unwrap_or_default(),
            o.buyer_name.clone(),
            o.email.clone(),
            o.phone.clone(),
            o.scout_name.clone().unwrap_or_default(),
            format_cents(o.total_cents),
        ];
        row.extend(products.iter().map(|id| {
            let qty: u32 = o.lines.iter().filter(|l| l.item_id == *id).map(|l| l.qty).sum();
            if qty == 0 { String::new() } else { qty.to_string() }
        }));
        row.extend([
            delivery_address,
            o.delivery.as_ref().and_then(|d| d.notes.clone()).unwrap_or_default(),
            ship_to,
            o.gift_message.clone().unwrap_or_default(),
            o.review_reason.clone().unwrap_or_default(),
        ]);
        out.write_record(&row)?;
    }
    out.flush()?;
    Ok(())
}
