use anyhow::{Context, Result, bail};
use std::sync::Arc;
use std::time::Duration;
use t15_fundraiser::{
    AppState, StaticDirs,
    admin::AdminOidc,
    catalog::Catalog,
    config::{CatalogPaths, Config},
    db::{self, Db},
    export,
    ratelimit::RateLimiter,
    router,
    stripe::StripeClient,
};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: t15-fundraiser [serve | export | export-annual-fees [--year Y] | check-catalog]

  serve               run the web server (default)
  export              print paid and needs_review orders as CSV to stdout
  export-annual-fees  print annual camping fee payments (one row per scout) as CSV to stdout;
                      --year limits it to one scouting year, e.g. --year 2026-2027
  check-catalog       validate catalog.yaml and list its items";

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env before anything reads the environment (incl. RUST_LOG); real env vars take precedence.
    match dotenvy::dotenv() {
        Ok(_) => {}
        Err(e) if e.not_found() => {}
        Err(e) => return Err(e).context("loading .env"),
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    match std::env::args().nth(1).as_deref() {
        None | Some("serve") => serve().await,
        Some("export") => export().await,
        Some("export-annual-fees") => export_annual_fees(std::env::args().skip(2).collect()).await,
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
    match &catalog.annual_fee {
        None => println!("annual_fee: not configured (the /annual-fee page is disabled)"),
        Some(f) => {
            let open = if f.is_open(chrono::Utc::now()) { "open" } else { "closed" };
            println!(
                "annual_fee: {} at {} per scout, up to {} scouts per checkout; payments close {} ({open})",
                f.scouting_year,
                shared::format_cents(f.amount_cents),
                f.max_scouts,
                f.closes_at.to_rfc3339(),
            );
            if let Some(n) = &f.note {
                println!("  note: {n}");
            }
        }
    }
    Ok(())
}

async fn export_annual_fees(args: Vec<String>) -> Result<()> {
    let year = match args.as_slice() {
        [] => None,
        [flag, y] if flag == "--year" && !y.is_empty() => Some(y.clone()),
        [arg] if arg.starts_with("--year=") && arg.len() > 7 => Some(arg[7..].to_string()),
        _ => bail!("{USAGE}"),
    };
    let db = Db::open(&Config::database_path_from_env())?;
    let rows = db.call(move |c| db::list_annual_fees(c, year.as_deref())).await?;
    export::write_annual_fees_csv(&rows, std::io::stdout().lock())
}

async fn export() -> Result<()> {
    let db = Db::open(&Config::database_path_from_env())?;
    let rows = db.call(|c| db::list_orders_for_export(c)).await?;
    // Product columns follow catalog order; without a readable catalog they fall back to the ids in the orders.
    let p = CatalogPaths::from_env();
    let item_ids = match Catalog::load(&p.catalog_path, &p.images_dir, false) {
        Ok(c) => c.view(chrono::Utc::now()).items.into_iter().map(|i| i.id).collect(),
        Err(e) => {
            eprintln!("warning: catalog not loaded ({e:#}); product columns are in id order");
            Vec::new()
        }
    };
    export::write_csv(&rows, &item_ids, std::io::stdout().lock())
}

async fn serve() -> Result<()> {
    let cfg = Config::from_env()?;
    let images_dir = cfg.static_dir.join("images");
    let catalog = Catalog::load(&cfg.catalog_path, &images_dir, !cfg.allow_missing_images)
        .map_err(|e| anyhow::anyhow!("{e:#}"))?;
    let db = Db::open(&cfg.database_path).context("opening database")?;
    let admin_oidc = AdminOidc::discover(
        cfg.oidc_client_id.clone(),
        secrecy::ExposeSecret::expose_secret(&cfg.oidc_client_secret).to_string(),
        &cfg.base_url,
    )
    .await
    .context("discovering Google OIDC configuration for /admin")?;
    let state = AppState {
        catalog: Arc::new(catalog),
        db,
        provider: Arc::new(StripeClient::new(cfg.stripe_secret_key)),
        webhook_secret: Arc::new(cfg.stripe_webhook_secret),
        base_url: cfg.base_url.clone(),
        limiter: Arc::new(RateLimiter::new(10, Duration::from_secs(60))),
        now: Arc::new(chrono::Utc::now),
        trust_proxy: cfg.trust_proxy,
        admin_oidc: Arc::new(admin_oidc),
        cookie_key: axum_extra::extract::cookie::Key::generate(),
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
