mod config;

use anyhow::Result;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // Step 1-2: Load config (dotenvy called inside Config::load)
    let cfg = config::Config::load()?;

    // Step 3: Init tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("mantis=info")),
        )
        .init();

    info!("MANTIS starting up");
    info!(
        capital_usd = %cfg.capital.total_usd,
        max_exposure_usd = %cfg.capital.max_total_exposure_usd,
        daily_loss_limit_usd = %cfg.capital.daily_loss_limit_usd,
        "Capital config loaded"
    );
    info!(
        flash_threshold_pct = %cfg.signal.flash_divergence_threshold_pct,
        standard_min_certainty = cfg.signal.standard_min_certainty,
        "Signal config loaded"
    );
    info!(
        clob_url = %cfg.network.clob_url,
        geoblock_url = %cfg.network.geoblock_url,
        "Network config loaded"
    );

    // Step 4: Geoblock check — exit(1) if blocked
    info!("Checking geoblock...");
    match check_geoblock(&cfg.network.geoblock_url).await {
        Ok(true) => {
            error!("GEOBLOCK: this IP is blocked by Polymarket. Run from EC2 eu-west-2.");
            std::process::exit(1);
        }
        Ok(false) => {
            info!("Geoblock check passed — IP is not blocked");
        }
        Err(e) => {
            warn!("Geoblock check failed (network error): {e} — proceeding with caution");
        }
    }

    info!("P1 scaffold complete. Feeds, signals, and trader not yet implemented.");
    Ok(())
}

/// Returns Ok(true) if this IP is geoblocked by Polymarket.
async fn check_geoblock(url: &str) -> Result<bool> {
    #[derive(serde::Deserialize)]
    struct GeoblockResponse {
        blocked: bool,
    }

    let client = reqwest::Client::new();
    let resp = client
        .get(url)
        .header("User-Agent", "mantis-trading-agent/0.1.0")
        .send()
        .await?;

    let body: GeoblockResponse = resp.json().await?;
    Ok(body.blocked)
}
