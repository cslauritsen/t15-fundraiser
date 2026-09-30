# Annual Camping Fee Page — Implementation Spec

Add a page where a parent pays the troop's annual camping fee for one or more scouts in a single
Stripe Checkout. It reuses the existing stack and payment plumbing described in `prompts/SPEC.md`
(Leptos CSR SPA, Axum, SQLite via `backend/src/db.rs`, Stripe Checkout via
`backend/src/stripe.rs`, signed webhook in `backend/src/lib.rs`). Read SPEC.md first; everything
it says about Stripe sessions, webhook verification, idempotency, and validation applies here
unless this document says otherwise.

**Core design:** the app writes nothing to the database until a payment succeeds. The cart lives
in the browser. At checkout, the server puts the scouts into the Stripe Checkout Session's
metadata. When Stripe reports the session paid, the server reads the scouts back from the
session and inserts one `paid` row per scout. Abandoned or cancelled checkouts leave no trace in
the app.

## 1. Scope

**MVP (build this):**
- SPA route `/annual-fee` with a scout cart and checkout.
- `annual_fee` config block in `catalog.yaml`, including a closing date.
- New `annual_fees` table (paid fees only) and a schema migration.
- API endpoints for checkout and status; webhook handling for fee payments.
- Success and cancel pages for fees.
- Admin report of fee payments: a summary on `/admin`, a full report page, and CSV export
  (web and CLI).
- Tests.

**Out of scope:**
- Storing pending, abandoned, or failed checkouts.
- App-sent email. Stripe's receipt is the payment confirmation (see §8).
- Editing or refunding fees in the app. Refunds happen in the Stripe dashboard.
- Checking scout names against a roster. Names are free text.
- Collecting anything per scout besides first and last name.
- Discounts, sibling pricing, partial payments, or passing Stripe fees on to parents.

## 2. Configuration (`catalog.yaml`)

Add a top-level block, loaded and validated at startup like the rest of the catalog:

```yaml
annual_fee:
  scouting_year: "2026-2027"               # free text, 1..=20 chars; stored on every row and shown on the page
  amount_cents: 5000                       # per scout, > 0; placeholder, the operator sets the real fee
  closes_at: "2027-01-31T23:59:59-05:00"   # required; RFC 3339 with explicit offset (placeholder date)
  max_scouts: 8                            # max scouts per checkout, 1..=10
  note: "Covers campouts from September through August."   # optional, shown on the page
```

- If the block is missing, the feature is disabled: `/api/annual-fee/*` returns 404 and the SPA
  shows "not available".
- The troop absorbs Stripe's processing fees. Parents pay exactly
  `amount_cents × number of scouts`, with no surcharge line.
- `t15-fundraiser check-catalog` also validates and prints this block. The process refuses to
  start if `closes_at` doesn't parse.

**Closing date.** This follows the same rules as the greenery `closes_at` (SPEC.md §8.1 step 0)
but is a separate setting; the greenery cutoff does not apply to fees.
- `GET /api/annual-fee` returns `open = now <= closes_at`, plus `closes_at` itself.
- The page shows "Payments accepted through {closes_at, formatted in the troop's local time}".
  After the cutoff it shows "Annual fee payments for {scouting_year} are closed." instead of the
  form.
- After the cutoff, `POST /api/annual-fee/checkout` returns 409 `closed`. This check is enforced
  on the server, regardless of what the SPA shows.
- A session created before the cutoff may still be paid after it; its `expires_at` is capped at
  30 minutes. Webhooks and status reconciliation are always processed.

## 3. User flow

1. Parent opens `/annual-fee`. The page shows the scouting year, the per-scout amount, the
   closing date, and the optional note.
2. The form has one row: **Scout first name** and **Scout last name** (both required) and an
   **Add scout** button.
3. Clicking **Add scout** validates the row, appends it to the cart, and clears the inputs. The
   cart lists each scout with the per-scout amount, a **Remove** button, and a running total.
4. Adding is blocked, with an inline message, when the cart already has `max_scouts` entries or
   already has the same name (case-insensitive after trimming).
5. Below the cart, the parent enters **their name** and **email** (both required). The receipt
   goes to that email.
6. **Pay $X.XX** is enabled when the cart has at least one scout. It calls the checkout API and
   redirects to Stripe.
7. Stripe redirects back to `/annual-fee/success` or `/annual-fee/cancel` (§6).

**Cart persistence:** the cart and payer fields live only in the browser, in `localStorage`
under key `t15.annualFeeCart`. A parent who leaves and returns sees the same cart. The cart is
cleared only when the success page confirms the payment. Wrap all storage access in error
handling. If storage is unavailable, the page still works; the cart just isn't saved.

## 4. Data model

Migration: bump `PRAGMA user_version` from 1 to 2 in `db.rs`. Keep the migration additive: an
existing v1 database gets the new table, a fresh database gets the v1 schema followed by v2.

```sql
CREATE TABLE annual_fees (
    id                        INTEGER PRIMARY KEY,
    payment_id                TEXT NOT NULL,      -- "af_" + UUIDv4, generated at checkout; groups one checkout's scouts
    line_no                   INTEGER NOT NULL,   -- 0-based position of the scout in that checkout
    status                    TEXT NOT NULL CHECK (status IN ('paid','needs_review')),
    review_reason             TEXT,
    scouting_year             TEXT NOT NULL,      -- as sent in the session metadata
    scout_first_name          TEXT NOT NULL,
    scout_last_name           TEXT NOT NULL,
    amount_cents              INTEGER NOT NULL,   -- as sent in the session metadata
    payer_name                TEXT NOT NULL,
    payer_email               TEXT NOT NULL,
    stripe_session_id         TEXT NOT NULL,
    stripe_payment_intent_id  TEXT,
    paid_at                   TEXT NOT NULL,      -- RFC 3339 UTC, via db::ts
    UNIQUE (payment_id, line_no)
);
CREATE INDEX annual_fees_session ON annual_fees(stripe_session_id);
```

- Rows exist only for money that arrived. There is one row per scout, and all rows from one
  checkout share `payment_id`.
- `needs_review` means the money arrived but the checks in §6 failed. The treasurer resolves
  these from the export.
- The unique `(payment_id, line_no)` key makes recording a payment idempotent. The webhook and
  the success-page reconciliation may both try to record the same session; use
  `INSERT OR IGNORE` so the second attempt is a no-op.
- The same scout may appear under different `payment_id`s (for example, if two parents both pay).
  Don't block this; the export shows it so the treasurer can refund one.

## 5. API

All JSON. Handlers go in a new `backend/src/annual_fee.rs` module, merged into the router in
`lib.rs`. DTOs go in `shared/src/api.rs`, and validation goes in `shared/src/validate.rs` so the
SPA and server share it.

| Method | Path | Purpose |
|---|---|---|
| GET  | `/api/annual-fee` | `{scouting_year, amount_cents, max_scouts, note, closes_at, open}` |
| POST | `/api/annual-fee/checkout` | Body `{payer_name, payer_email, scouts: [{first_name, last_name}]}` → `{checkout_url}` |
| GET  | `/api/annual-fee/{payment_id}/status?session_id=...` | `{status: "paid" \| "needs_review" \| "pending" \| "expired", scouting_year, scouts, total_cents, payer_email}` (see §6) |

**Checkout handler:**
1. If fees are disabled, return 404. If `now > closes_at`, return 409 `closed`.
2. Validate the request (§7). Return 400 with a field-level error on failure.
3. Generate `payment_id = "af_" + uuid_v4`. **Don't write to the database.**
4. Create the Stripe Checkout Session through the existing `PaymentProvider::create_session`,
   taking the amount and scouting year from config. **Never accept an amount from the client.**
   - Use one line item per scout, quantity 1, `unit_amount = amount_cents`, named
     `"Annual camping fee {scouting_year} — {first} {last}"`, with no image.
   - Set `customer_email = payer_email` and `client_reference_id = metadata[order_id] = payment_id`.
   - Set `success_url = {BASE_URL}/annual-fee/success?payment={payment_id}&session_id={CHECKOUT_SESSION_ID}`
     and `cancel_url = {BASE_URL}/annual-fee/cancel`.
   - Use the same 30-minute `expires_at` as orders.
   - Set session metadata that carries everything needed to record the payment later:

     | Key | Value |
     |---|---|
     | `kind` | `annual_fee` |
     | `scouting_year` | from config |
     | `amount_cents` | per-scout amount from config |
     | `scout_count` | N |
     | `scout_{i}` | `"{first}\t{last}"` for i in 0..N |
     | `payer_name` | payer name |

     Stripe metadata allows 50 keys of at most 500 characters each. With `max_scouts ≤ 10` and
     names ≤ 50 characters, this stays well within limits. The payer email comes from the
     session's `customer_email`.
5. Return the checkout URL. If Stripe fails, log it and return a generic 502. There is nothing
   to clean up.
6. Apply the existing per-IP rate limiter used by `/api/checkout`.

**Changes to `backend/src/stripe.rs`:**
- `SessionRequest`: add a `description` field (fees use
  `"Troop 15 annual camping fee {payment_id}"`; orders keep their current text) and a
  `metadata: Vec<(String, String)>` field, added as `metadata[key]` parameters. `order_id` is
  still set as today.
- `SessionInfo`: add `metadata: HashMap<String, String>` and `customer_email: Option<String>`,
  parsed from the session JSON. Use `customer_details.email`, falling back to `customer_email`.

## 6. Recording a paid fee

One function, `record_fee_payment(session: &SessionInfo, now)`, is called by both the webhook
and the status endpoint. It:
1. Parses the metadata listed in §5. If the metadata is missing or malformed, log at error
   level and still record whatever rows it can, as `needs_review`. Money has arrived, so never
   drop it.
2. Checks that `amount_total == amount_cents × scout_count`. On a mismatch, record the rows as
   `needs_review` with a `review_reason`, and log at warn level.
3. In one transaction, `INSERT OR IGNORE`s one row per scout with `paid_at`,
   `stripe_session_id`, `stripe_payment_intent_id`, `payer_name`, and `payer_email`.

**Webhook:** keep the single `/api/stripe/webhook` endpoint. After signature verification,
route on `client_reference_id`: an `af_` prefix goes to the fee path and anything else goes to
the existing order handlers.
- On `checkout.session.completed` or `async_payment_succeeded` with `payment_status = paid`,
  call `record_fee_payment`. Insert the event into `stripe_events` in the same transaction.
- For fees, `checkout.session.expired` is acknowledged with 200 and nothing else happens.

**Status endpoint:**
- If rows exist for `payment_id`, return their status and details.
- Otherwise, retrieve the session from Stripe by `session_id`. Reject it unless its
  `client_reference_id` equals `payment_id`.
  - If it is paid, call `record_fee_payment` and return `paid`.
  - If it is still open, return `pending`.
  - If it has expired, return `expired`.
- Don't call Stripe again once rows exist.

**Success page** `/annual-fee/success`: poll the status endpoint a few times, as `/success`
does.
- `paid`: show "Paid: {scouting_year} camping fee for {names}, total $X, receipt sent to
  {email}" and clear the `localStorage` cart.
- `needs_review`: show "Payment received; the treasurer will confirm" and clear the cart.
- `pending` after polling ends: show "Payment processing — check your email for the Stripe
  receipt."
- Anything else: show a message and a link back to `/annual-fee`.

**Cancel page** `/annual-fee/cancel`: "Payment cancelled. Your scouts are still in the cart."
with a link back. There is no API call.

## 7. Validation (shared crate, enforced on client and server)

- **Scout first/last name:** trimmed and whitespace-collapsed, 1..=50 characters, no control
  characters (this also excludes the tab used as the metadata separator). Letters, spaces,
  `'`, `-`, and `.` are expected, but don't reject other printable Unicode.
- **Scouts:** 1..=`max_scouts`; no duplicate (first, last) pairs after case-folding.
- **Payer name:** 1..=100 characters, same rules as scout names.
- **Payer email:** the existing email validator.

## 8. Receipts and email

With one Stripe line item per scout, Stripe's automatic receipt already lists each scout and the
total. That covers confirmation email without adding mail infrastructure.
`build_session_params` sets `payment_intent_data[receipt_email]` to the payer's email. In live
mode, Stripe then sends the receipt even when the dashboard's "Successful payments" customer
email setting is off. Test mode never sends receipts.

## 9. Admin report and export

Everything in this section uses the existing `/admin` Google sign-in in `backend/src/admin.rs`.
Every new route checks `current_admin` and redirects to `/admin` when the viewer isn't signed
in, just like `/admin/export.csv`. The new pages are server-rendered HTML like the current
`/admin` page (no SPA, no JavaScript). Escape every value with `html_escape`, and send
`Cache-Control: no-store` because the pages contain names and emails.

**`GET /admin` (existing page):** keep the sign-in line, the orders CSV link, and the logout
link. Add an "Annual camping fees" section that shows:
- A summary for the configured `scouting_year`:
  - scouts paid
  - total collected (a sum of `amount_cents`, before Stripe fees)
  - number of checkouts (distinct `payment_id`)
  - number of `needs_review` rows, highlighted when greater than 0
  - the closing date, and whether payments are open or closed
- A link to the full report, "View annual fee report" (`/admin/annual-fees`).
- A link to download the CSV, "Download annual fees CSV" (`/admin/annual-fees.csv`).

If the `annual_fee` config block is missing, omit the section.

**`GET /admin/annual-fees?year=2026-2027` (new report page):**
- `year` defaults to the configured `scouting_year`. Show a list of links for every
  `scouting_year` found in the table, so earlier years stay viewable after the config changes.
- Show the same summary numbers as above, for the selected year.
- Show a **Needs review** table, only when there are any such rows: scout, payer name and
  email, amount, `paid_at`, `review_reason`, and the Stripe payment intent id. Put this table
  above the main one.
- Show a **Paid scouts** table with one row per scout, sorted by last name then first name.
  Columns: scout last name, scout first name, amount, date paid (the troop's local date),
  payer name, and payer email (as a `mailto:` link). When the same scout appears in more than
  one checkout, flag the rows with "possible duplicate", so the treasurer can refund one.
- Link to the CSV for the same year (`/admin/annual-fees.csv?year=…`) and back to `/admin`.
- Money is formatted from integer cents.

**`GET /admin/annual-fees.csv?year=…` (new CSV):** without `year`, it includes every year.
- One row per scout with columns `scouting_year, scout_last_name, scout_first_name, amount,
  status, paid_at, payer_name, payer_email, payment_id, stripe_payment_intent_id,
  review_reason`, sorted by year, then last name, then first name.
- File name: `annual-fees-{year|all}.csv`.
- Log the admin email and row count, as the orders export does.

**CLI:** `t15-fundraiser export-annual-fees [--year Y]` prints the same CSV to stdout.

**Implementation notes:**
- Put the queries in `db.rs`:
  - `list_annual_fees(year: Option<&str>)`
  - `annual_fee_summary(year)`
  - `annual_fee_years()`
- Put CSV writing next to `export.rs` so the CLI and `/admin` share it.

Update README.md: the config block (including `closes_at`), the new command, and the new admin
report and links.

## 10. Frontend

- Add routes in `frontend/src/main.rs`: `/annual-fee`, `/annual-fee/success`,
  `/annual-fee/cancel`. Put the page components in a new `frontend/src/annual_fee.rs` and the
  API calls in `frontend/src/api.rs`.
- Don't link the page from the greenery shop; it is shared by direct URL.
- Match the existing styles in `style.css`. The page must work at phone width.
- Format money from integer cents; never use floats.

## 11. Testing

- **Unit (shared):** name/payer validation, duplicate detection, `max_scouts`.
- **Unit (backend):**
  - Config parsing and validation, including a bad `closes_at`.
  - v1 → v2 migration on a v1 database that already has orders.
  - Metadata building and parsing round-trip.
- **Integration (`backend/tests/api.rs`, stubbed `PaymentProvider`, injected clock):**
  - Checkout creates a session with N line items and correct metadata, and writes no rows.
  - A client-supplied amount is ignored.
  - Checkout after `closes_at` returns 409; `GET /api/annual-fee` reports `open: false`.
  - A paid webhook inserts N `paid` rows.
  - A duplicate webhook is a no-op.
  - Webhook plus status reconciliation for the same session inserts the rows once.
  - An amount mismatch records `needs_review`.
  - An expired webhook writes nothing.
  - A webhook for a paid session that arrives after `closes_at` is still recorded.
  - Order webhooks still work (routing by prefix).
  - Admin routes (`/admin/annual-fees` and `/admin/annual-fees.csv`):
    - Both redirect to `/admin` when signed out.
    - The report shows the summary totals and the needs-review table.
    - Scout names containing HTML are escaped.
    - The `year` filter works.
    - A possible duplicate is flagged.
- **Manual (test mode):**
  - Pay for 2 scouts with `4242…`. Check that the Stripe receipt lists both scouts and the
    export shows both `paid`.
  - Abandon a checkout and confirm nothing is recorded.
  - Set `closes_at` in the past, restart, and confirm the page shows closed.

## 12. Decisions

- The operator sets `amount_cents` and `closes_at` in `catalog.yaml`; the agent leaves the
  placeholders as they are.
- The scouting-year label format is `"2026-2027"`.
- The page collects scout first and last names only.
- The troop absorbs Stripe's processing fees.
- The cart lives in the browser. Only paid fees are stored.
- Fee payments stop at the `annual_fee.closes_at` cutoff, which is separate from the greenery
  cutoff.
