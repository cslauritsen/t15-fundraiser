use anyhow::{Context, Result, bail};
use secrecy::SecretString;
use std::path::PathBuf;

pub struct Config {
    pub bind_addr: String,
    pub base_url: String,
    pub database_path: String,
    pub catalog_path: PathBuf,
    /// Contains `images/`.
    pub static_dir: PathBuf,
    /// Trunk build output (the SPA).
    pub frontend_dir: PathBuf,
    pub stripe_secret_key: SecretString,
    pub stripe_webhook_secret: SecretString,
    /// Trust `X-Forwarded-For` for rate limiting (only behind a reverse proxy).
    pub trust_proxy: bool,
    pub allow_missing_images: bool,
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn required(name: &str) -> Result<String> {
    var(name).with_context(|| format!("missing required environment variable {name}"))
}

fn flag(name: &str) -> bool {
    matches!(var(name).as_deref(), Some("1" | "true" | "yes"))
}

impl Config {
    pub fn database_path_from_env() -> String {
        var("DATABASE_PATH").unwrap_or_else(|| "./data/fundraiser.db".into())
    }

    pub fn from_env() -> Result<Self> {
        let base_url = required("BASE_URL")?.trim_end_matches('/').to_string();
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            bail!("BASE_URL must start with http:// or https://");
        }
        let stripe_secret_key = required("STRIPE_SECRET_KEY")?;
        if !stripe_secret_key.starts_with("sk_") && !stripe_secret_key.starts_with("rk_") {
            bail!("STRIPE_SECRET_KEY must be a Stripe secret (sk_...) or restricted (rk_...) key");
        }
        Ok(Config {
            bind_addr: var("BIND_ADDR").unwrap_or_else(|| "127.0.0.1:8080".into()),
            base_url,
            database_path: Self::database_path_from_env(),
            catalog_path: var("CATALOG_PATH").unwrap_or_else(|| "./catalog.yaml".into()).into(),
            static_dir: var("STATIC_DIR").unwrap_or_else(|| "./static".into()).into(),
            frontend_dir: var("FRONTEND_DIR").unwrap_or_else(|| "./frontend/dist".into()).into(),
            stripe_secret_key: stripe_secret_key.into(),
            stripe_webhook_secret: required("STRIPE_WEBHOOK_SECRET")?.into(),
            trust_proxy: flag("TRUST_PROXY"),
            allow_missing_images: flag("ALLOW_MISSING_IMAGES"),
        })
    }
}

/// Just what the catalog checker needs (no Stripe secrets required).
pub struct CatalogPaths {
    pub catalog_path: PathBuf,
    pub images_dir: PathBuf,
    pub check_images: bool,
}

impl CatalogPaths {
    pub fn from_env() -> Self {
        let static_dir: PathBuf = var("STATIC_DIR").unwrap_or_else(|| "./static".into()).into();
        CatalogPaths {
            catalog_path: var("CATALOG_PATH").unwrap_or_else(|| "./catalog.yaml".into()).into(),
            images_dir: static_dir.join("images"),
            check_images: !flag("ALLOW_MISSING_IMAGES"),
        }
    }
}
