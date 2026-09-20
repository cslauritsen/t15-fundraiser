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
    t15-fundraiser check-catalog validate catalog.yaml, list items

## Tests

    cargo test                                        # shared, backend unit + HTTP integration tests
    cargo check -p frontend --target wasm32-unknown-unknown
