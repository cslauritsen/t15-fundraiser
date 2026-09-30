# Troop 15 Greenery Fundraiser

Single-binary Rust site: Leptos (CSR) order form + Axum API + SQLite + Stripe Checkout.
See `prompts/SPEC.md` for the full design.

```
shared/     API types + validation used by both sides
backend/    Axum server, SQLite, Stripe client, webhook, CSV export
frontend/   Leptos SPA (built with trunk)
catalog.yaml, static/images/   the items and their photos
```

## Set up

    cargo install trunk                       # once
    cp .env.example .env                      # fill in test-mode Stripe keys
    ./target/debug/t15-fundraiser check-catalog   # validate catalog.yaml + images

## Develop

    # terminal 1: API + static images on :8080
    cargo run -p t15-fundraiser
    # terminal 2: SPA on :3000, proxying /api and /images to :8080
    cd frontend && trunk serve
    # terminal 3: forward Stripe webhooks (prints the whsec_ secret for .env)
    stripe listen --forward-to localhost:8080/api/stripe/webhook \
      --events checkout.session.completed,checkout.session.async_payment_succeeded,checkout.session.expired

The server loads `.env` itself at startup (searching the current directory and its parents), so
don't `source` it. Variables already set in your shell take precedence over `.env`, so an old
exported `STRIPE_SECRET_KEY` will silently win; `unset` it if Stripe returns 401. Restart the
server after editing `.env`.

Test card: `4242 4242 4242 4242`, any future date and CVC.

## Production

    cd frontend && trunk build --release      # -> frontend/dist
    cargo build --release                     # -> target/release/t15-fundraiser

Run the binary with the env vars from `.env.example` behind HTTPS, and add a webhook endpoint in
the Stripe dashboard for `{BASE_URL}/api/stripe/webhook` with events `checkout.session.completed`,
`checkout.session.async_payment_succeeded` and `checkout.session.expired`.
Back up `DATABASE_PATH` (e.g. `sqlite3 data/fundraiser.db ".backup backup.db"`).

### Docker

    docker build -t t15-fundraiser --build-arg GIT_DESCRIBE=$(git describe --always --dirty --tags) .
    docker run -d --name t15 -p 8080:8080 -v t15-data:/data --env-file .env \
      -e BASE_URL=https://example.org t15-fundraiser

The image bundles the binary, the SPA, `catalog.yaml` and `static/`; paths and `BIND_ADDR` are
preset, so only `BASE_URL` and the Stripe keys are required. The SQLite database lives on the
`/data` volume. Other commands: `docker exec t15 t15-fundraiser export`. Put HTTPS in front
(and set `TRUST_PROXY=1`).

### Go live

Live runs from `docker-compose.live.yml` (project `t15-fundraiser-live`, port 8081, named volume
`t15-live-data`), separate from the sandbox in `docker-compose.yml`.

1. Stripe: finish account verification, add the payout bank account, and pick a payout schedule.
2. Stripe (live mode): add a webhook endpoint `https://troop15.org/api/stripe/webhook` for
   `checkout.session.completed`, `checkout.session.async_payment_succeeded` and
   `checkout.session.expired`; copy its `whsec_`.
3. Create `~/secrets/greenery-live.env` (never commit it):

        STRIPE_SECRET_KEY=sk_live_...
        STRIPE_WEBHOOK_SECRET=whsec_...   # the live endpoint's, not the sandbox one

   `BASE_URL`, `TRUST_PROXY` and the data path are set in the compose file.
4. Point the reverse proxy for `troop15.org` (HTTPS) at port 8081.
5. `docker compose -f docker-compose.live.yml up -d --build`
6. Smoke test: buy one small item with a real card, confirm the webhook delivery succeeded in the
   Stripe dashboard and the order appears in `t15-fundraiser export`, then refund it.
7. Reconcile: match each Stripe payout to a deposit in the troop account, using Stripe's payout
   report and the export CSV.

## Commands

    t15-fundraiser [serve]       run the server
    t15-fundraiser export        paid + needs_review orders as CSV on stdout
    t15-fundraiser export-annual-fees [--year 2026-2027]
                                 annual camping fee payments (one row per scout) as CSV on stdout
    t15-fundraiser check-catalog validate catalog.yaml (incl. the annual_fee block), list items

## Admin CSV export (web)

`GET /admin` (bookmark it; it's not linked from the site) offers the same CSV export as
`t15-fundraiser export`, gated by Google sign-in:

1. Create an OAuth2 client at https://console.cloud.google.com/apis/credentials (type
   "Web application") with an authorized redirect URI of `{BASE_URL}/admin/callback`.
2. Set `OIDC_CLIENT_ID` / `OIDC_CLIENT_SECRET` (see `.env.example`), or mount them as files at
   `/run/secrets/client_id` and `/run/secrets/client_secret` (e.g. Docker/Podman secrets) —
   an env var takes precedence over the secret file if both are present.
3. Add the Google account email(s) allowed in to catalog.yaml's `admins` list.

Visiting `/admin` while signed out redirects into the Google login; on success it lands on a page
with a "Download orders CSV" link (`/admin/export.csv`) and a logout link. The session is kept in
an encrypted, HttpOnly cookie (no server-side session store), and is re-checked against the
`admins` list on every request, so removing an email from catalog.yaml revokes access immediately.

## Annual camping fee

`/annual-fee` lets a parent pay the troop's annual camping fee for one or more scouts in a single
Stripe Checkout. It isn't linked from the shop; share the URL directly. Configure it with a
top-level block in `catalog.yaml` (remove the block to disable the page and its API):

```yaml
annual_fee:
  scouting_year: "2026-2027"               # free text, 1-20 characters; stored on every payment
  amount_cents: 5000                       # per scout, > 0; the troop absorbs Stripe's fees
  closes_at: "2027-01-31T23:59:59-05:00"   # required, RFC 3339 with explicit offset
  max_scouts: 8                            # scouts per checkout, 1-10
  note: "Covers campouts from September through August."   # optional
```

`closes_at` is separate from the greenery `closes_at`: after it the page says payments are closed
and `POST /api/annual-fee/checkout` returns 409. Sessions started before the cutoff can still be
paid, and webhooks are always processed. The offset written in `closes_at` is treated as the
troop's local time zone for displayed dates. The server refuses to start if `closes_at` doesn't parse.

The cart lives only in the parent's browser (`localStorage`); nothing is stored until Stripe
reports the payment, then one `annual_fees` row per scout is written (by the webhook, or by the
success page's status check if the webhook is late). Payments whose amount or metadata don't
check out are stored as `needs_review`. Stripe's receipt (one line per scout) is the only
confirmation email, so make sure "Successful payments" customer emails are enabled in the Stripe
dashboard (live mode). Refunds are done in the Stripe dashboard.

The existing webhook endpoint and events cover fees; no Stripe configuration changes are needed.

**Admin.** When signed in, `/admin` also shows an "Annual camping fees" summary for the configured
year (scouts paid, total collected before Stripe fees, checkouts, needs-review count, closing
date) with links to:

- `/admin/annual-fees?year=…`: the full report for a year (defaults to the configured one; links
  to every year on file). Needs-review rows are listed first; paid scouts are sorted by last name
  and flagged "possible duplicate" when the same scout was paid for in more than one checkout.
- `/admin/annual-fees.csv?year=…`: the same CSV as `t15-fundraiser export-annual-fees`
  (all years without `year`).

## Tests

    cargo test                                        # shared, backend unit + HTTP integration tests
    cargo check -p frontend --target wasm32-unknown-unknown
