use crate::db::OrderRow;
use anyhow::Result;
use shared::{Fulfillment, OrderLine, format_cents};
use std::io::Write;

fn items(lines: &[OrderLine], f: Fulfillment) -> String {
    lines
        .iter()
        .filter(|l| l.fulfillment == f)
        .map(|l| format!("{}x {}", l.qty, l.name))
        .collect::<Vec<_>>()
        .join("; ")
}

/// One row per order, with scout-delivery and direct-ship items split for planning.
pub fn write_csv(orders: &[OrderRow], w: impl Write) -> Result<()> {
    let mut out = csv::Writer::from_writer(w);
    out.write_record([
        "order_id", "status", "paid_at", "buyer_name", "email", "phone", "scout_name", "total",
        "delivery_items", "delivery_address", "delivery_notes", "ship_items", "ship_to", "gift_message",
        "review_reason",
    ])?;
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
        out.write_record([
            o.id.as_str(),
            o.status.as_str(),
            o.paid_at.as_deref().unwrap_or(""),
            &o.buyer_name,
            &o.email,
            &o.phone,
            o.scout_name.as_deref().unwrap_or(""),
            &format_cents(o.total_cents),
            &items(&o.lines, Fulfillment::ScoutDelivery),
            &delivery_address,
            o.delivery.as_ref().and_then(|d| d.notes.as_deref()).unwrap_or(""),
            &items(&o.lines, Fulfillment::DirectShip),
            &ship_to,
            o.gift_message.as_deref().unwrap_or(""),
            o.review_reason.as_deref().unwrap_or(""),
        ])?;
    }
    out.flush()?;
    Ok(())
}
