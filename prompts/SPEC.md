# Scout Troop Fundraiser Stripe Commerce Site

## 1. Goal

A simple purchase form where supporters of a scout troop buy from ~15 fixed-price greenery items and pay by card via Stripe. Expected volume: a few dozen orders total. Single node, single process, single SQLite file.

## 2. Non-goals

- User accounts, logins, or an admin UI (admin = read the DB / CSV export).
- Inventory limits, discounts, coupons, sales tax calculation, or refunds inside the app (refunds are done in the Stripe dashboard).
- Multi-tenant or multi-fundraiser support.
- Horizontal scaling.

## 3. Stack

| Layer | Choice |
|---|---|
| Frontend | Leptos CSR SPA (WASM, built with Trunk), served as static files by the backend |
| Backend | Rust, Axum, one process |
| DB | SQLite (via `sqlx` or `rusqlite`), WAL mode, one file |
| Payments | Stripe Checkout (hosted), Stripe REST API called directly from the backend (`reqwest`) or `async-stripe` |
| Config | `catalog.yaml` (items) and env vars (secrets, URLs) |
| Shared types | A `shared` crate (workspace member) with request/response DTOs used by both frontend and backend |

Workspace layout: `shared/`, `backend/`, `frontend/`.

## 4. Catalog (`catalog.yaml`)

Loaded and validated at startup (`closes_at` must parse); the process refuses to start on a bad file. Catalog is immutable at runtime (change requires restart).

```yaml
closes_at: "2026-10-30T23:59:59-04:00"   # RFC 3339 with explicit offset; orders rejected after this
delivery_note: "Local delivery the week before Thanksgiving (Nov 16-24, 2026)."
shipping_note: "Gift items ship directly to the recipient during the first 2 weeks after Thanksgiving (contiguous U.S. only)."
fulfillment:
  local_zip_prefixes: ["441"]      # scout-delivery allowlist: a ZIP is local if it starts with any prefix...
  local_zips: []                   # ...or exactly matches any entry here
items:
  - id: wreath-24        # stable slug, used in orders
    name: 24" Fresh Wreath
    description: ...
    price_cents: 3500    # all-in: shipping and any tax are included
    fulfillment: scout_delivery   # or: direct_ship
    max_qty: 10          # optional, default 20
    image: wreath-24.jpg # required; file in static/images/, checked at startup
    image_alt: A 24-inch fresh balsam wreath with a red bow
```

Validation: every `image` file exists under `static/images/`, unique ids, price_cents > 0, fulfillment is one of the two values, at least one item.

## 5. Fulfillment rules

Each item is either `scout_delivery` or `direct_ship`. Carts may mix both in one order.

- Cart contains any `scout_delivery` item: form requires a **delivery address** (name, street, city, state, ZIP, **required phone**, optional delivery notes). ZIP must match `local_zip_prefixes` or `local_zips`; enforced in the form and again on the server.
- Cart contains any `direct_ship` item: form requires the gift **recipient's shipping address** (name, street, optional apartment, city, state, ZIP) and offers an optional **gift message** of at most 20 characters, printed on the shipping label. State must be in the **contiguous US** (48 states plus DC; Alaska, Hawaii and territories are rejected). All direct-ship items in one order go to one recipient with one message; separate recipients mean separate orders. Neither field is collected or stored when the cart has no direct-ship items.
- Phone is required on every order (simplifies validation and gives leaders a contact for delivery problems).
- Required field on the form: "Scout to credit" (free text, at most 100 characters), stored on the order.

Both addresses are collected by our own form, not by Stripe. Stripe Checkout can neither enforce a local ZIP allowlist nor exclude Alaska and Hawaii (it filters by country only) and reports the address only after the buyer has paid, so restrictions could not be applied before payment. Validating on our side rejects a bad address before any Stripe session exists. Stripe's hosted page collects only card details.

## 5a. Images

- One image per item, stored in `static/images/` and referenced by filename in the catalog. Served by the backend with long-lived cache headers (filenames are versioned by the operator: rename on change).
- Recommended format: JPEG or WebP, square, about 640x640, under ~150 KB each, so 15 items load fast on phones.
- `image_alt` is required (accessibility). The SPA lazy-loads images and reserves the aspect ratio to avoid layout shift.
- The Stripe session also receives the image URL (`price_data.product_data.images`, absolute `{BASE_URL}/images/...`) so it shows on the hosted checkout page. This requires `BASE_URL` to be publicly reachable; skipped in local dev.

## 6. Pricing

- The client sends only `{item_id, qty}` lines plus contact/address fields. **Prices are never accepted from the client.**
- The backend computes the total from the catalog, in integer cents, and stores line-item snapshots (name, unit price, qty) on the order so later catalog edits don't rewrite history.
- Stripe Checkout Session is built with `line_items` using inline `price_data` (currency `usd`, `unit_amount`, product name) so no Stripe Product objects need to exist.
- Minimum one line, qty 1..=max_qty per line.

## 7. Data model (SQLite)

```
orders
  id                  TEXT PK            -- ULID or UUIDv4, also used as client_reference_id
  status              TEXT NOT NULL      -- pending | paid | expired | failed | needs_review
  review_reason       TEXT               -- why an order is needs_review
  email               TEXT NOT NULL
  buyer_name          TEXT NOT NULL
  phone               TEXT NOT NULL
  scout_name          TEXT               -- scout to credit
  total_cents         INTEGER NOT NULL
  delivery_json       TEXT               -- local drop-off address, if any
  shipping_json       TEXT               -- gift recipient's address (direct-ship orders)
  gift_message        TEXT               -- optional, max 20 characters (direct-ship orders)
  stripe_session_id   TEXT UNIQUE
  stripe_payment_intent_id TEXT
  created_at          TEXT NOT NULL
  paid_at             TEXT

order_items
  order_id            TEXT FK
  item_id             TEXT
  name                TEXT               -- snapshot
  unit_price_cents    INTEGER            -- snapshot
  qty                 INTEGER
  fulfillment         TEXT               -- snapshot

stripe_events
  event_id            TEXT PK            -- for webhook idempotency
  type                TEXT
  received_at         TEXT
```

Status transitions (`needs_review` = money arrived but the amount or session didn't match; a human checks it in the export): `pending -> paid` (on `checkout.session.completed` with `payment_status = paid`), `pending -> expired` (on `checkout.session.expired`). Paid orders never transition back; refunds are handled outside the app (optionally recorded later via `charge.refunded`).

## 8. Backend API

All JSON, all under `/api`.

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/catalog` | Items, local ZIP list, `closes_at`, delivery/shipping notes, and an `open` flag for the form |
| POST | `/api/checkout` | Validate cart and contact info, create `pending` order, create Stripe session, return `{checkout_url}` |
| GET | `/api/orders/{id}/status?session_id=...` | Success-page poll. If still `pending`, backend fetches the session from Stripe and finalizes if paid (fallback for slow/missed webhook) |
| POST | `/api/stripe/webhook` | Stripe webhook receiver |
| GET | `/healthz` | Liveness |

Non-API: static SPA files with fallback to `index.html` for client-side routes (`/`, `/success`, `/cancel`).

### 8.1 Checkout flow

0. **Cutoff:** if now > `closes_at`, `/api/checkout` returns 409 `closed`. The SPA shows a "orders closed" page instead of the form. Sessions already created before the cutoff may still complete (their `expires_at` is capped at 30 min), and webhooks are always processed.
1. SPA submits cart, email, name, delivery address (if needed).
2. Server validates everything, computes total, inserts `orders` and `order_items` as `pending` in one transaction.
3. Server creates the Checkout Session: `mode=payment`, `customer_email` (locks the validated email), `client_reference_id = order.id`, `metadata.order_id`, `line_items`, `success_url = {BASE_URL}/success?order={id}&session_id={CHECKOUT_SESSION_ID}`, `cancel_url = {BASE_URL}/cancel?order={id}`, `expires_at` = 30 minutes, and an `Idempotency-Key` header derived from the order id.
4. Server stores `stripe_session_id` and returns the URL; the SPA redirects.
5. If the Stripe call fails, the order is marked `failed` and a generic error is returned.

### 8.2 Payment confirmation

- **Webhook** is the source of truth. Verify the `Stripe-Signature` header against the raw body with `STRIPE_WEBHOOK_SECRET` (with timestamp tolerance). Handle `checkout.session.completed`, `checkout.session.async_payment_succeeded` (mark paid) and `checkout.session.expired` (mark expired). Ignore others with 200.
- Idempotency: insert `event_id` into `stripe_events` first; duplicate is a no-op 200. Order update and event insert happen in one transaction.
- On paid: set `status`, `paid_at`, `stripe_payment_intent_id`. Verify the session amount equals `orders.total_cents`; if not, log loudly and leave the order flagged rather than silently marking paid.
- **Success page** polls `/api/orders/{id}/status` a few times; server may reconcile directly against Stripe as described above. The page shows a confirmation summary only for `paid` orders.

## 9. Validation

- **Email**: syntactic validation on client and server (trimmed, lowercased, length ≤ 254, one `@`, sane domain). It is passed to Stripe as `customer_email`, and Stripe sends the receipt. No verification-link flow; a typo'd address is mitigated by showing the email on the confirmation page.
- **Name, address fields**: required, length-capped; state is a 2-letter US code; ZIP is 5 digits (or ZIP+4). Delivery ZIP must match the local allowlist; shipping state must be in the contiguous US.
- **Gift message**: optional, at most 20 characters (counted as characters, not bytes), whitespace collapsed, no control characters.
- **Phone**: required; normalized to digits, 10-digit US number (optional leading `+1`/`1`).
- All input is length-limited; SQL uses bound parameters; output rendered by Leptos is escaped by default.
- Basic abuse control: simple per-IP rate limit on `/api/checkout` (e.g., 10/min) via `tower-governor` or hand-rolled.

## 10. Configuration and secrets

Environment variables (never committed):

```
BIND_ADDR=127.0.0.1:8080
BASE_URL=https://fundraiser.example.org
DATABASE_PATH=./data/fundraiser.db
CATALOG_PATH=./catalog.yaml
STRIPE_SECRET_KEY=sk_live_...        # use a restricted key: Checkout Sessions write, read only
STRIPE_WEBHOOK_SECRET=whsec_...
STATIC_DIR=./static            # contains images/
FRONTEND_DIR=./frontend/dist   # trunk output
TRUST_PROXY=1                  # optional: rate-limit by X-Forwarded-For behind a proxy
ALLOW_MISSING_IMAGES=1         # optional, dev only
```

Use test-mode keys for development. Use `stripe listen --forward-to localhost:8080/api/stripe/webhook` locally.

## 11. Deployment

- One binary plus `catalog.yaml`, `static/`, and the SQLite file. Run under systemd or a container on one host.
- HTTPS required (Stripe webhook and Checkout redirect); terminate with Caddy/nginx or a tunnel.
- SQLite in WAL mode; nightly copy of the DB file (`sqlite3 .backup`) to somewhere off the host.

## 12. Operations and reporting

- CLI subcommands: `t15-fundraiser check-catalog` validates the YAML and images (run it while building the catalog); `t15-fundraiser export` prints paid orders as CSV: one row per order with buyer, email, scout, fulfillment groups, items, totals, and addresses. This is what the troop uses to plan deliveries and shipments.
- Structured logs (`tracing`), including order id and Stripe session id on every payment-related line.

## 13. Stripe account (personal) considerations

- The account is in your personal name. Funds pay out to your bank account, and card statements show your account's descriptor. Set a clear **statement descriptor** (e.g. `TROOP15 FUNDRAISER`) and business name/support email so buyers don't dispute unfamiliar charges.
- Stripe may issue a 1099-K to you for the gross volume. Keep the CSV export and payout records so the pass-through to the troop is documented. This is worth a word with the troop treasurer or a tax adviser; it is not something the app can solve.
- Stripe's terms restrict some uses of personal accounts to actual business activity; a fundraiser you're collecting on behalf of a troop is generally fine, but confirm the account's business type matches how you registered it.
- Fees (about 2.9% + 30¢ per charge) come out of each payment. Decide whether prices absorb them (spec assumes yes).

## 14. Testing

- Unit tests: catalog validation, pricing, ZIP rule, email validation, order state transitions.
- Integration tests: HTTP handlers against in-memory SQLite with Stripe stubbed behind a trait (`PaymentProvider`); webhook signature verification with known fixtures; duplicate webhook delivery.
- Manual: end-to-end in Stripe test mode with card `4242 4242 4242 4242`, including an abandoned session (expiry) and a mixed cart.

## 15. Delivery messaging

- Order form and success page show `delivery_note` when the cart has scout-delivery items, and `shipping_note` when it has direct-ship items (direct-ship timing is explicitly not guaranteed).
- No emails are sent by the app; Stripe's receipt is the only email. The success page is therefore the buyer's record of what to expect, so it repeats the items, addresses, and the notes above.

## 16. Open questions

1. Local delivery area: the config has `441` only; add `440`, `442`, `443` (and any exact ZIPs) before launch.

Answered: direct-ship to the contiguous US only, enforced on our form before payment; cutoff 2026-10-30 23:59:59 Eastern (EDT, -04:00); local delivery Nov 16-24; no app-sent email; phone required; optional 20-character gift message; Leptos CSR; Stripe Checkout.
