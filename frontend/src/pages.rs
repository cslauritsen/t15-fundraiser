use crate::api::{self, ApiError};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::{components::A, hooks::use_query_map};
use shared::{
    CartLine, CatalogItem, CatalogResponse, CheckoutRequest, Delivery, FieldError, Fulfillment, MAX_GIFT_MESSAGE,
    OrderStatus, ShipTo, format_cents, validate_checkout,
};
use std::sync::Arc;

fn redirect(url: &str) {
    if let Some(w) = web_sys::window() {
        let _ = w.location().set_href(url);
    }
}

fn opt(s: String) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

// ---------------------------------------------------------------------------------------------
// Shop
// ---------------------------------------------------------------------------------------------

#[component]
pub fn Shop() -> impl IntoView {
    let catalog = LocalResource::new(api::fetch_catalog);
    view! {
        <Suspense fallback=|| view! { <p class="center">"Loading…"</p> }>
            {move || {
                catalog.get().map(|result| match result {
                    Ok(c) if c.open => view! { <OrderForm catalog=c /> }.into_any(),
                    Ok(c) => view! { <Closed catalog=c /> }.into_any(),
                    Err(e) => view! { <p class="banner-error">{e.message()}</p> }.into_any(),
                })
            }}
        </Suspense>
    }
}

#[component]
fn Closed(catalog: CatalogResponse) -> impl IntoView {
    let _ = catalog;
    view! {
        <div class="center">
            <h2>"Orders are closed"</h2>
            <p>"Thank you for supporting our troop! The fundraiser has ended."</p>
        </div>
    }
}

#[component]
fn Field(
    label: &'static str,
    name: &'static str,
    value: RwSignal<String>,
    errors: RwSignal<Vec<FieldError>>,
    #[prop(default = "text")] kind: &'static str,
    #[prop(optional)] autocomplete: &'static str,
    #[prop(optional)] hint: &'static str,
) -> impl IntoView {
    view! {
        <label class="field">
            {label}
            <input
                type=kind
                name=name
                autocomplete=autocomplete
                prop:value=move || value.get()
                on:input=move |ev| value.set(event_target_value(&ev))
            />
            {(!hint.is_empty()).then(|| view! { <p class="hint">{hint}</p> })}
            {move || {
                errors.get().into_iter().find(|e| e.field == name).map(|e| view! { <p class="err">{e.message}</p> })
            }}
        </label>
    }
}

#[component]
fn ItemCard(item: CatalogItem, qty: RwSignal<u32>) -> impl IntoView {
    let max = item.max_qty;
    let (badge_class, badge_text) = match item.fulfillment {
        Fulfillment::ScoutDelivery => ("badge", "Delivered by our scouts"),
        Fulfillment::DirectShip => ("badge ship", "Shipped to you"),
    };
    view! {
        <div class="item">
            <img src=item.image_url.clone() alt=item.image_alt.clone() loading="lazy" />
            <div class="body">
                <h3>{item.name.clone()}</h3>
                <span class=badge_class>{badge_text}</span>
                <p class="desc">{item.description.clone()}</p>
                <div class="price">{format_cents(item.price_cents)}</div>
                <div class="qty">
                    <button type="button" aria-label="Fewer" on:click=move |_| qty.update(|q| *q = q.saturating_sub(1))>"−"</button>
                    <span class="n" aria-live="polite">{move || qty.get()}</span>
                    <button type="button" aria-label="More" on:click=move |_| qty.update(|q| if *q < max { *q += 1 })>"+"</button>
                </div>
            </div>
        </div>
    }
}

#[component]
fn OrderForm(catalog: CatalogResponse) -> impl IntoView {
    let catalog = Arc::new(catalog);
    let qtys: Vec<(CatalogItem, RwSignal<u32>)> =
        catalog.items.iter().map(|i| (i.clone(), RwSignal::new(0u32))).collect();
    let qtys_store = StoredValue::new(qtys.clone());
    let catalog_store = StoredValue::new(catalog.clone());

    let email = RwSignal::new(String::new());
    let name = RwSignal::new(String::new());
    let phone = RwSignal::new(String::new());
    let scout = RwSignal::new(String::new());
    let street = RwSignal::new(String::new());
    let city = RwSignal::new(String::new());
    let state = RwSignal::new("OH".to_string());
    let zip = RwSignal::new(String::new());
    let notes = RwSignal::new(String::new());
    let ship_name = RwSignal::new(String::new());
    let ship_line1 = RwSignal::new(String::new());
    let ship_line2 = RwSignal::new(String::new());
    let ship_city = RwSignal::new(String::new());
    let ship_state = RwSignal::new("OH".to_string());
    let ship_zip = RwSignal::new(String::new());
    let gift = RwSignal::new(String::new());

    let errors = RwSignal::new(Vec::<FieldError>::new());
    let form_error = RwSignal::new(None::<String>);
    let submitting = RwSignal::new(false);

    // (name, unit price, qty, fulfillment) for lines with qty > 0.
    let selected = move || {
        qtys_store
            .get_value()
            .into_iter()
            .filter_map(|(item, q)| {
                let q = q.get();
                (q > 0).then_some((item, q))
            })
            .collect::<Vec<_>>()
    };
    let total = move || selected().iter().map(|(i, q)| i.price_cents * i64::from(*q)).sum::<i64>();
    let needs_delivery = move || selected().iter().any(|(i, _)| i.fulfillment == Fulfillment::ScoutDelivery);
    let needs_shipping = move || selected().iter().any(|(i, _)| i.fulfillment == Fulfillment::DirectShip);

    let build_request = move || CheckoutRequest {
        lines: selected().into_iter().map(|(i, q)| CartLine { item_id: i.id, qty: q }).collect(),
        email: email.get(),
        buyer_name: name.get(),
        phone: phone.get(),
        scout_name: opt(scout.get()),
        delivery: needs_delivery().then(|| Delivery {
            street: street.get(),
            city: city.get(),
            state: state.get(),
            zip: zip.get(),
            notes: opt(notes.get()),
        }),
        shipping: needs_shipping().then(|| ShipTo {
            name: ship_name.get(),
            line1: ship_line1.get(),
            line2: opt(ship_line2.get()),
            city: ship_city.get(),
            state: ship_state.get(),
            postal_code: ship_zip.get(),
        }),
        gift_message: needs_shipping().then(|| opt(gift.get())).flatten(),
    };

    let on_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        if submitting.get() {
            return;
        }
        form_error.set(None);
        let req = build_request();
        if let Err(errs) = validate_checkout(&req, &catalog_store.get_value()) {
            form_error.set(Some("Please fix the highlighted fields.".into()));
            errors.set(errs);
            return;
        }
        errors.set(vec![]);
        submitting.set(true);
        spawn_local(async move {
            match api::checkout(&req).await {
                Ok(r) => redirect(&r.checkout_url),
                Err(e) => {
                    if let ApiError::Server(s) = &e {
                        errors.set(s.fields.clone());
                    }
                    form_error.set(Some(e.message()));
                    submitting.set(false);
                }
            }
        });
    };

    let has_error = move |field: &'static str| errors.get().iter().any(|e| e.field == field);
    let delivery_note = catalog.delivery_note.clone();
    let shipping_note = catalog.shipping_note.clone();
    let states = STATES;
    let ship_states = STATES.iter().copied().filter(|s| *s != "AK" && *s != "HI").collect::<Vec<_>>();

    view! {
        <div class="notes">
            {(!delivery_note.is_empty()).then(|| view! { <p><strong>"Local delivery: "</strong>{delivery_note.clone()}</p> })}
            {(!shipping_note.is_empty()).then(|| view! { <p><strong>"Shipped items: "</strong>{shipping_note.clone()}</p> })}
        </div>

        <form on:submit=on_submit novalidate>
            <h2>"Choose your greenery"</h2>
            <div class="items">
                {qtys.into_iter().map(|(item, q)| view! { <ItemCard item=item qty=q /> }).collect_view()}
            </div>
            {move || has_error("lines").then(|| view! { <p class="err">{errors.get().iter().find(|e| e.field == "lines").map(|e| e.message.clone())}</p> })}

            <fieldset>
                <legend>"Your details"</legend>
                <Field label="Your name" name="buyer_name" value=name errors=errors autocomplete="name" />
                <Field label="Email (your receipt goes here)" name="email" value=email errors=errors kind="email" autocomplete="email" />
                <Field label="Phone" name="phone" value=phone errors=errors kind="tel" autocomplete="tel" hint="In case there's a delivery question." />
                <Field label="Scout to credit" name="scout_name" value=scout errors=errors />
            </fieldset>

            <Show when=needs_delivery>
                <fieldset>
                    <legend>"Delivery address"</legend>
                    <p class="hint">"Your scout-delivered items will be dropped off here. Local addresses only."</p>
                    <Field label="Street address" name="delivery.street" value=street errors=errors autocomplete="street-address" />
                    <div class="row">
                        <Field label="City" name="delivery.city" value=city errors=errors autocomplete="address-level2" />
                        <label class="field">
                            "State"
                            <select on:change=move |ev| state.set(event_target_value(&ev)) prop:value=move || state.get()>
                                {states.iter().map(|s| view! { <option value=*s>{*s}</option> }).collect_view()}
                            </select>
                        </label>
                        <Field label="ZIP" name="delivery.zip" value=zip errors=errors autocomplete="postal-code" />
                    </div>
                    <Field label="Delivery notes (optional)" name="delivery.notes" value=notes errors=errors hint="Gate code, porch instructions, etc." />
                </fieldset>
            </Show>

            <Show when=needs_shipping>
                <fieldset>
                    <legend>"Gift shipping address"</legend>
                    <p class="hint">"Gift items are shipped directly to the recipient (contiguous U.S. only)."</p>
                    <Field label="Recipient name" name="shipping.name" value=ship_name errors=errors autocomplete="shipping name" />
                    <Field label="Street address" name="shipping.line1" value=ship_line1 errors=errors autocomplete="shipping address-line1" />
                    <Field label="Apartment, suite, etc. (optional)" name="shipping.line2" value=ship_line2 errors=errors autocomplete="shipping address-line2" />
                    <div class="row">
                        <Field label="City" name="shipping.city" value=ship_city errors=errors autocomplete="shipping address-level2" />
                        <label class="field">
                            "State"
                            <select on:change=move |ev| ship_state.set(event_target_value(&ev)) prop:value=move || ship_state.get()>
                                {ship_states.iter().map(|s| view! { <option value=*s>{*s}</option> }).collect_view()}
                            </select>
                            {move || errors.get().into_iter().find(|e| e.field == "shipping.state").map(|e| view! { <p class="err">{e.message}</p> })}
                        </label>
                        <Field label="ZIP" name="shipping.postal_code" value=ship_zip errors=errors autocomplete="shipping postal-code" />
                    </div>
                    <label class="field">
                        "Gift message for the label (optional)"
                        <input
                            type="text"
                            name="gift_message"
                            maxlength=MAX_GIFT_MESSAGE
                            prop:value=move || gift.get()
                            on:input=move |ev| gift.set(event_target_value(&ev))
                        />
                        <p class="hint">{move || format!("{} of {MAX_GIFT_MESSAGE} characters", gift.get().chars().count())}</p>
                        {move || errors.get().into_iter().find(|e| e.field == "gift_message").map(|e| view! { <p class="err">{e.message}</p> })}
                    </label>
                </fieldset>
            </Show>

            <div class="summary">
                <ul>
                    {move || {
                        selected()
                            .into_iter()
                            .map(|(i, q)| view! {
                                <li><span>{format!("{q} × {}", i.name)}</span><span>{format_cents(i.price_cents * i64::from(q))}</span></li>
                            })
                            .collect_view()
                    }}
                </ul>
                <div class="total"><span>"Total"</span><span>{move || format_cents(total())}</span></div>
            </div>

            {move || form_error.get().map(|m| view! { <p class="banner-error" role="alert">{m}</p> })}
            <button class="primary" type="submit" disabled=move || submitting.get() || total() == 0>
                {move || if submitting.get() { "Opening secure payment…" } else { "Continue to payment" }}
            </button>
            <p class="hint center">"Payments are processed securely by Stripe."</p>
        </form>
    }
}

const STATES: [&str; 51] = [
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "DC", "FL", "GA", "HI", "ID", "IL", "IN", "IA", "KS", "KY",
    "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM", "NY", "NC", "ND", "OH",
    "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA", "WV", "WI", "WY",
];

// ---------------------------------------------------------------------------------------------
// Success / cancel
// ---------------------------------------------------------------------------------------------

#[component]
pub fn Success() -> impl IntoView {
    let query = use_query_map();
    let status = LocalResource::new(move || {
        let q = query.get();
        let order = q.get("order").unwrap_or_default();
        let session = q.get("session_id").unwrap_or_default();
        async move {
            if order.is_empty() || session.is_empty() {
                return Err(ApiError::Network);
            }
            // The webhook usually lands within a second or two; poll briefly while pending.
            let mut last = api::order_status(&order, &session).await;
            for _ in 0..6 {
                match &last {
                    Ok(s) if s.status == OrderStatus::Pending => {
                        gloo_timers::future::TimeoutFuture::new(2_000).await;
                        last = api::order_status(&order, &session).await;
                    }
                    _ => break,
                }
            }
            last
        }
    });

    view! {
        <Suspense fallback=|| view! { <p class="center">"Confirming your payment…"</p> }>
            {move || {
                status.get().map(|r| match r {
                    Err(_) => view! {
                        <div class="center">
                            <h2>"We couldn't load your order"</h2>
                            <p>"If you completed payment, Stripe has emailed you a receipt."</p>
                            <p><A href="/">"Back to the shop"</A></p>
                        </div>
                    }.into_any(),
                    Ok(s) => match s.status {
                        OrderStatus::Paid => view! { <Confirmation order=s /> }.into_any(),
                        OrderStatus::NeedsReview => view! {
                            <div class="center">
                                <h2>"Thank you — we received your payment"</h2>
                                <p>"Something needs a quick manual check on our side. We'll follow up at " <strong>{s.email}</strong> " if we need anything."</p>
                            </div>
                        }.into_any(),
                        OrderStatus::Pending => view! {
                            <div class="center">
                                <h2>"Still confirming your payment"</h2>
                                <p>"This is taking longer than usual. Your receipt will arrive by email at " <strong>{s.email}</strong> " once it goes through. You can refresh this page in a minute."</p>
                            </div>
                        }.into_any(),
                        OrderStatus::Expired | OrderStatus::Failed => view! {
                            <div class="center">
                                <h2>"This order wasn't completed"</h2>
                                <p><A href="/">"Start a new order"</A></p>
                            </div>
                        }.into_any(),
                    },
                })
            }}
        </Suspense>
    }
}

#[component]
fn Confirmation(order: shared::OrderStatusResponse) -> impl IntoView {
    let has_delivery = order.lines.iter().any(|l| l.fulfillment == Fulfillment::ScoutDelivery);
    let has_shipping = order.lines.iter().any(|l| l.fulfillment == Fulfillment::DirectShip);
    let delivery = order.delivery.clone();
    let ship_to = order.ship_to.clone();
    let gift_message = order.gift_message.clone();
    view! {
        <div class="summary">
            <h2>"Thank you! Your order is confirmed."</h2>
            <p>"A receipt is on its way to " <strong>{order.email.clone()}</strong> "."</p>
            <ul>
                {order.lines.iter().map(|l| view! {
                    <li><span>{format!("{} × {}", l.qty, l.name)}</span><span>{format_cents(l.unit_price_cents * i64::from(l.qty))}</span></li>
                }).collect_view()}
            </ul>
            <div class="total"><span>"Total paid"</span><span>{format_cents(order.total_cents)}</span></div>
        </div>
        {has_delivery.then(|| view! {
            <div class="notes">
                <p><strong>"Scout delivery"</strong></p>
                {delivery.map(|d| view! { <p>{format!("{}, {}, {} {}", d.street, d.city, d.state, d.zip)}</p> })}
                <p>"Our scouts will drop these off during our local delivery window."</p>
            </div>
        })}
        {has_shipping.then(|| view! {
            <div class="notes">
                <p><strong>"Shipped items"</strong></p>
                {ship_to.map(|s| view! {
                    <p>{format!("To: {}, {}{}, {}, {} {}", s.name, s.line1, s.line2.map(|l| format!(", {l}")).unwrap_or_default(), s.city, s.state, s.postal_code)}</p>
                })}
                {gift_message.map(|m| view! { <p>{format!("Gift message: “{m}”")}</p> })}
                <p>"These ship directly from the grower; delivery dates are not guaranteed."</p>
            </div>
        })}
        <p class="center"><A href="/">"Place another order"</A></p>
    }
}

#[component]
pub fn Cancel() -> impl IntoView {
    view! {
        <div class="center">
            <h2>"Payment cancelled"</h2>
            <p>"No charge was made."</p>
            <p><A href="/">"Back to the shop"</A></p>
        </div>
    }
}
