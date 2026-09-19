# Builds the API binary and the Leptos SPA with cargo-chef so dependency layers are cached
# until Cargo.toml / Cargo.lock change.

FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS chef
WORKDIR /app

# ---- dependency recipe ----
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- backend ----
FROM chef AS backend
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --locked -p t15-fundraiser --recipe-path recipe.json
COPY . .
RUN cargo build --release --locked -p t15-fundraiser

# ---- frontend (wasm, built with trunk) ----
FROM chef AS frontend
RUN rustup target add wasm32-unknown-unknown && cargo install trunk --locked
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --locked --target wasm32-unknown-unknown -p frontend --recipe-path recipe.json
COPY . .
RUN cd frontend && trunk build --release

# ---- runtime ----
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /app app \
    && mkdir /data && chown app /data
WORKDIR /app
COPY --from=backend /app/target/release/t15-fundraiser /usr/local/bin/t15-fundraiser
COPY --from=frontend /app/frontend/dist ./frontend/dist
COPY catalog.yaml ./catalog.yaml
COPY static ./static

# Secrets (STRIPE_SECRET_KEY, STRIPE_WEBHOOK_SECRET) and BASE_URL are supplied at run time.
ENV BIND_ADDR=0.0.0.0:8080 \
    DATABASE_PATH=/data/fundraiser.db \
    CATALOG_PATH=/app/catalog.yaml \
    STATIC_DIR=/app/static \
    FRONTEND_DIR=/app/frontend/dist
VOLUME /data
EXPOSE 8080
USER app
ENTRYPOINT ["t15-fundraiser"]
CMD ["serve"]
