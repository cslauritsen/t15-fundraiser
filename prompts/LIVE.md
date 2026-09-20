  In Stripe (only you can do this)
  1. Finish account activation: business details, bank account for payouts, and identity verification. Live charges stay off until this is done.
  2. Create a live-mode restricted key, or use the live secret key. It needs write access to Checkout Sessions and read access to Events. Live keys are separate from the sandbox
     ones.
  3. Add a live webhook endpoint at https://<live-domain>/api/stripe/webhook. Subscribe it to checkout.session.completed, checkout.session.async_payment_succeeded and
     checkout.session.expired. Copy its whsec_…, which differs from the sandbox one.
  4. Check the Checkout settings in live mode: statement descriptor, receipt emails, and which payment methods are enabled. Live mode has its own settings, separate from the
     sandbox.

  On the server
  1. Create ~/secrets/greenery-live.env with:
     - STRIPE_SECRET_KEY=sk_live_…
     - STRIPE_WEBHOOK_SECRET=whsec_…
     - BASE_URL=https://<live-domain>
     - TRUST_PROXY=1
  2. Give live its own database. The sandbox orders in fundraiser.db shouldn't mix with real ones.
  3. Make sure HTTPS terminates in front of the container.

  Problems in the current docker-compose.yml
  - It has no named volume for /data. Docker creates an anonymous volume, so the database is easy to lose or orphan on a recreate. Live needs something like t15-live-data:/data.
  - RUST_LOG: TRACE and OTEL_ENABLED are set. TRACE logging in production is noisy and could log request details, so I'd use info.
  - Your uncommitted edit points the file at dev.troop15.org and greenery-test.env. That's the sandbox setup, so live should be a separate service or compose file so the two can't
    get crossed.

  Questions for you
  1. What's the live hostname? Is it the real troop15.org name, or should dev.troop15.org stay as the sandbox?
  2. Should live run on the same host as a second container with its own port, env file and volume, or on a different machine?
  3. Is your reverse proxy already set up for TLS on the live name?

  Once you answer, I'll write the live compose file with a named volume and sane logging. I'll also add a short "go-live checklist" to the README, plus a smoke test: one real
  small purchase, refund it, then check the webhook delivery and the CSV export.
