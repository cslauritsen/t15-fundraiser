use anyhow::{Context, Result, bail};
use std::sync::Arc;
use std::time::Duration;
use t15_fundraiser::{
    AppState, StaticDirs,
    catalog::Catalog,
    config::{CatalogPaths, Config},
    db::{self, Db},
    export,
    ratelimit::RateLimiter,
    router,
    stripe::StripeClient,
};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: t15-fundraiser [serve | export | check-catalog]

  serve          run the web server (default)
  export         print paid and needs_review orders as CSV to stdout
  check-catalog  validate catalog.yaml and list its items";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    match std::env::args().nth(1).as_deref() {
        None | Some("serve") => serve().await,
        Some("export") => export().await,
        Some("check-catalog") => check_catalog(),
        Some(_) => bail!("{USAGE}"),
    }
}

fn check_catalog() -> Result<()> {
    let p = CatalogPaths::from_env();
    let catalog = Catalog::load(&p.catalog_path, &p.images_dir, p.check_images).map_err(|e| anyhow::anyhow!("{e:#}"))?;
    let view = catalog.view(chrono::Utc::now());
    println!("OK: {} items, orders close {}", view.items.len(), view.closes_at);
    for i in &view.items {
        println!("  {:<24} {:>8}  {:?}  {}", i.id, shared::format_cents(i.price_cents), i.fulfillment, i.name);
    }
    Ok(())
}

async fn export() -> Result<()> {
    let db = Db::open(&Config::database_path_from_env())?;
    let rows = db.call(|c| db::list_orders_for_export(c)).await?;
    export::write_csv(&rows, std::io::stdout().lock())
}

async fn serve() -> Result<()> {
    let cfg = Config::from_env()?;
    let images_dir = cfg.static_dir.join("images");
    let catalog = Catalog::load(&cfg.catalog_path, &images_dir, !cfg.allow_missing_images)
        .map_err(|e| anyhow::anyhow!("{e:#}"))?;
    let db = Db::open(&cfg.database_path).context("opening database")?;
    let state = AppState {
        catalog: Arc::new(catalog),
        db,
        provider: Arc::new(StripeClient::new(cfg.stripe_secret_key.clone())),
        webhook_secret: cfg.stripe_webhook_secret.clone(),
        base_url: cfg.base_url.clone(),
        limiter: Arc::new(RateLimiter::new(10, Duration::from_secs(60))),
        now: Arc::new(chrono::Utc::now),
        trust_proxy: cfg.trust_proxy,
    };
    let app = router(state, Some(StaticDirs { static_dir: cfg.static_dir, frontend_dir: cfg.frontend_dir }));

    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await.with_context(|| format!("binding {}", cfg.bind_addr))?;
    tracing::info!("listening on http://{}", cfg.bind_addr);
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
