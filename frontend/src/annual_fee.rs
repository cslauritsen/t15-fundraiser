//! `/annual-fee`: pay the troop's annual camping fee for one or more scouts in one checkout.
//! Shared by direct URL; not linked from the greenery shop.
//!
//! The cart lives only in this browser (`localStorage`); the server stores nothing until
//! Stripe reports the payment.

use crate::api::{self, ApiError};
use crate::pages::{Field, redirect};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::{components::A, hooks::use_query_map};
use serde::{Deserialize, Serialize};
use shared::{
    AnnualFeeInfo, FeeCheckoutRequest, FeeScout, FeeStatus, FieldError, MAX_SCOUT_NAME, fee_cart_add_error,
    format_cents, format_local_datetime, validate_fee_checkout, validate_fee_scout,
};

// ---------------------------------------------------------------------------------------------
// Cart persistence
// ---------------------------------------------------------------------------------------------

const CART_KEY: &str = "t15.annualFeeCart";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SavedCart {
    #[serde(default)]
    scouts: Vec<FeeScout>,
    #[serde(default)]
    payer_name: String,
    #[serde(default)]
    payer_email: String,
}

/// `None` when storage is unavailable (private mode, blocked site data); the page still works,
/// the cart just isn't saved.
fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn load_cart() -> SavedCart {
    storage()
        .and_then(|s| s.get_item(CART_KEY).ok().flatten())
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

fn save_cart(cart: &SavedCart) {
    if let (Some(s), Ok(json)) = (storage(), serde_json::to_string(cart)) {
        let _ = s.set_item(CART_KEY, &json);
    }
}

fn clear_cart() {
    if let Some(s) = storage() {
        let _ = s.remove_item(CART_KEY);
    }
}

/// "Alex Smith", "Alex Smith and Jamie Smith", "A, B and C".
fn join_names(scouts: &[FeeScout]) -> String {
    let names: Vec<String> = scouts.iter().map(FeeScout::full_name).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn field_error(errors: &[FieldError], field: &str) -> Option<String> {
    errors.iter().find(|e| e.field == field).map(|e| e.message.clone())
}

// ---------------------------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------------------------

#[component]
pub fn AnnualFee() -> impl IntoView {
    let info = LocalResource::new(api::fetch_annual_fee);
    view! {
        <Suspense fallback=|| view! { <p class="center">"Loading…"</p> }>
            {move || {
                info.get().map(|result| match result {
                    Ok(i) if i.open => view! { <FeeForm info=i /> }.into_any(),
                    Ok(i) => view! {
                        <div class="center">
                            <h2>"Annual camping fee"</h2>
                            <p>{format!("Annual fee payments for {} are closed.", i.scouting_year)}</p>
                        </div>
                    }.into_any(),
                    Err(ApiError::Server(e)) if e.code == "not_found" => view! {
                        <div class="center">
                            <h2>"Annual camping fee"</h2>
                            <p>"Annual fee payments are not available."</p>
                        </div>
                    }.into_any(),
                    Err(e) => view! { <p class="banner-error">{e.message()}</p> }.into_any(),
                })
            }}
        </Suspense>
    }
}

#[component]
fn FeeForm(info: AnnualFeeInfo) -> impl IntoView {
    let max = info.max_scouts;
    let amount = info.amount_cents;

    let saved = load_cart();
    let scouts = RwSignal::new(saved.scouts);
    let payer_name = RwSignal::new(saved.payer_name);
    let payer_email = RwSignal::new(saved.payer_email);
    Effect::new(move |_| {
        save_cart(&SavedCart { scouts: scouts.get(), payer_name: payer_name.get(), payer_email: payer_email.get() });
    });

    let first = RwSignal::new(String::new());
    let last = RwSignal::new(String::new());
    // Errors for the add-scout row: `first_name`, `last_name`, or `add` (limit / duplicate).
    let row_errors = RwSignal::new(Vec::<FieldError>::new());
    // Errors from checkout validation (client or server).
    let errors = RwSignal::new(Vec::<FieldError>::new());
    let form_error = RwSignal::new(None::<String>);
    let submitting = RwSignal::new(false);

    let add_scout = move || {
        let candidate = FeeScout { first_name: first.get_untracked(), last_name: last.get_untracked() };
        let scout = match validate_fee_scout(&candidate) {
            Ok(s) => s,
            Err(errs) => return row_errors.set(errs),
        };
        if let Some(message) = fee_cart_add_error(&scouts.get_untracked(), &scout, max) {
            return row_errors.set(vec![FieldError { field: "add".into(), message }]);
        }
        row_errors.set(vec![]);
        errors.update(|e| e.retain(|f| !f.field.starts_with("scouts")));
        scouts.update(|v| v.push(scout));
        first.set(String::new());
        last.set(String::new());
    };
    // Enter in a name box adds the scout rather than submitting the payment form.
    let add_on_enter = move |ev: leptos::ev::KeyboardEvent| {
        if ev.key() == "Enter" {
            ev.prevent_default();
            add_scout();
        }
    };

    let total = move || amount * scouts.with(Vec::len) as i64;

    let on_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        if submitting.get_untracked() {
            return;
        }
        form_error.set(None);
        let req = FeeCheckoutRequest {
            payer_name: payer_name.get_untracked(),
            payer_email: payer_email.get_untracked(),
            scouts: scouts.get_untracked(),
        };
        if let Err(errs) = validate_fee_checkout(&req, max) {
            form_error.set(Some("Please fix the highlighted fields.".into()));
            errors.set(errs);
            return;
        }
        errors.set(vec![]);
        submitting.set(true);
        spawn_local(async move {
            match api::fee_checkout(&req).await {
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

    let closes = format_local_datetime(&info.closes_at).unwrap_or_else(|| info.closes_at.clone());
    let year = info.scouting_year.clone();
    let note = info.note.clone();

    view! {
        <h2>{format!("Annual camping fee {year}")}</h2>
        <div class="notes">
            <p><strong>{format_cents(amount)}</strong>" per scout for the " {year.clone()} " scouting year."</p>
            <p>{format!("Payments accepted through {closes}.")}</p>
            {note.map(|n| view! { <p>{n}</p> })}
        </div>

        <form on:submit=on_submit novalidate>
            <fieldset>
                <legend>"Scouts"</legend>
                <div class="row fee-row">
                    <label class="field">
                        "Scout first name"
                        <input type="text" name="first_name" autocomplete="off" maxlength=MAX_SCOUT_NAME
                            prop:value=move || first.get()
                            on:input=move |ev| first.set(event_target_value(&ev))
                            on:keydown=add_on_enter />
                        {move || field_error(&row_errors.get(), "first_name").map(|m| view! { <p class="err">{m}</p> })}
                    </label>
                    <label class="field">
                        "Scout last name"
                        <input type="text" name="last_name" autocomplete="off" maxlength=MAX_SCOUT_NAME
                            prop:value=move || last.get()
                            on:input=move |ev| last.set(event_target_value(&ev))
                            on:keydown=add_on_enter />
                        {move || field_error(&row_errors.get(), "last_name").map(|m| view! { <p class="err">{m}</p> })}
                    </label>
                    <button class="secondary" type="button" on:click=move |_| add_scout()>"Add scout"</button>
                </div>
                {move || field_error(&row_errors.get(), "add").map(|m| view! { <p class="err" role="alert">{m}</p> })}
                <p class="hint">{format!("Up to {max} scouts per payment.")}</p>
            </fieldset>

            <div class="summary">
                <Show
                    when=move || scouts.with(|s| !s.is_empty())
                    fallback=|| view! { <p class="hint">"No scouts added yet."</p> }
                >
                    <ul class="fee-cart">
                        {move || {
                            scouts
                                .get()
                                .into_iter()
                                .enumerate()
                                .map(|(i, s)| view! {
                                    <li>
                                        <span>{s.full_name()}</span>
                                        <span class="fee-line">
                                            {format_cents(amount)}
                                            <button class="link" type="button"
                                                aria-label=format!("Remove {}", s.full_name())
                                                on:click=move |_| {
                                                    scouts.update(|v| if i < v.len() { v.remove(i); });
                                                    row_errors.set(vec![]);
                                                }>
                                                "Remove"
                                            </button>
                                        </span>
                                    </li>
                                })
                                .collect_view()
                        }}
                    </ul>
                </Show>
                {move || {
                    errors
                        .get()
                        .into_iter()
                        .filter(|e| e.field.starts_with("scouts"))
                        .map(|e| view! { <p class="err">{e.message}</p> })
                        .collect_view()
                }}
                <div class="total"><span>"Total"</span><span>{move || format_cents(total())}</span></div>
            </div>

            <fieldset>
                <legend>"Your details"</legend>
                <Field label="Your name" name="payer_name" value=payer_name errors=errors autocomplete="name" />
                <Field label="Email (your receipt goes here)" name="payer_email" value=payer_email errors=errors kind="email" autocomplete="email" />
            </fieldset>

            {move || form_error.get().map(|m| view! { <p class="banner-error" role="alert">{m}</p> })}
            <button class="primary" type="submit" disabled=move || submitting.get() || scouts.with(Vec::is_empty)>
                {move || if submitting.get() { "Opening secure payment…".to_string() } else { format!("Pay {}", format_cents(total())) }}
            </button>
            <p class="hint center">"Payments are processed securely by Stripe."</p>
        </form>
    }
}

// ---------------------------------------------------------------------------------------------
// Success / cancel
// ---------------------------------------------------------------------------------------------

#[component]
pub fn AnnualFeeSuccess() -> impl IntoView {
    let query = use_query_map();
    let status = LocalResource::new(move || {
        let q = query.get();
        let payment = q.get("payment").unwrap_or_default();
        let session = q.get("session_id").unwrap_or_default();
        async move {
            if payment.is_empty() || session.is_empty() {
                return Err(ApiError::Network);
            }
            // The webhook usually lands within a second or two; poll briefly while pending.
            let mut last = api::fee_status(&payment, &session).await;
            for _ in 0..6 {
                match &last {
                    Ok(s) if s.status == FeeStatus::Pending => {
                        gloo_timers::future::TimeoutFuture::new(2_000).await;
                        last = api::fee_status(&payment, &session).await;
                    }
                    _ => break,
                }
            }
            // The payment is recorded: the cart has done its job.
            if matches!(&last, Ok(s) if matches!(s.status, FeeStatus::Paid | FeeStatus::NeedsReview)) {
                clear_cart();
            }
            last
        }
    });

    view! {
        <Suspense fallback=|| view! { <p class="center">"Confirming your payment…"</p> }>
            {move || {
                status.get().map(|r| match r {
                    Ok(s) if s.status == FeeStatus::Paid => view! {
                        <div class="summary">
                            <h2>"Thank you!"</h2>
                            <p>{format!(
                                "Paid: {} camping fee for {}, total {}, receipt sent to {}.",
                                s.scouting_year,
                                join_names(&s.scouts),
                                format_cents(s.total_cents),
                                s.payer_email,
                            )}</p>
                        </div>
                    }.into_any(),
                    Ok(s) if s.status == FeeStatus::NeedsReview => view! {
                        <div class="center">
                            <h2>"Thank you"</h2>
                            <p>"Payment received; the treasurer will confirm."</p>
                        </div>
                    }.into_any(),
                    Ok(s) if s.status == FeeStatus::Pending => view! {
                        <div class="center">
                            <h2>"Payment processing"</h2>
                            <p>"Payment processing — check your email for the Stripe receipt."</p>
                        </div>
                    }.into_any(),
                    _ => view! {
                        <div class="center">
                            <h2>"We couldn't confirm this payment"</h2>
                            <p>"If you completed payment, Stripe has emailed you a receipt."</p>
                            <p><A href="/annual-fee">"Back to the annual fee page"</A></p>
                        </div>
                    }.into_any(),
                })
            }}
        </Suspense>
    }
}

#[component]
pub fn AnnualFeeCancel() -> impl IntoView {
    view! {
        <div class="center">
            <h2>"Payment cancelled"</h2>
            <p>"Payment cancelled. Your scouts are still in the cart."</p>
            <p><A href="/annual-fee">"Back to the annual fee page"</A></p>
        </div>
    }
}
