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
    set -a; source .env; set +a; cargo run -p t15-fundraiser
    # terminal 2: SPA on :3000, proxying /api and /images to :8080
    cd frontend && trunk serve
    # terminal 3: forward Stripe webhooks (prints the whsec_ secret for .env)
    stripe listen --forward-to localhost:8080/api/stripe/webhook

Test card: `4242 4242 4242 4242`, any future date and CVC.

## Production

    cd frontend && trunk build --release      # -> frontend/dist
    cargo build --release                     # -> target/release/t15-fundraiser

Run the binary with the env vars from `.env.example` behind HTTPS, and add a webhook endpoint in
the Stripe dashboard for `{BASE_URL}/api/stripe/webhook` with events `checkout.session.completed`,
`checkout.session.async_payment_succeeded` and `checkout.session.expired`.
Back up `DATABASE_PATH` (e.g. `sqlite3 data/fundraiser.db ".backup backup.db"`).

## Commands

    t15-fundraiser [serve]       run the server
    t15-fundraiser export        paid + needs_review orders as CSV on stdout
    t15-fundraiser check-catalog validate catalog.yaml, list items

## Tests

    cargo test                                        # shared, backend unit + HTTP integration tests
    cargo check -p frontend --target wasm32-unknown-unknown
