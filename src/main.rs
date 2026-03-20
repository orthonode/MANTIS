mod config;
mod feeds;
mod markets;
mod risk;
mod signal;
mod trader;

use anyhow::Result;
use feeds::gamma::GammaFilters;
use markets::state::{MarketEvent, MarketState};
use tokio::sync::{broadcast, mpsc};
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

    // Step 7: Init shared MarketState
    let market_state = MarketState::new();
    info!("MarketState initialised");

    // Step 8: Init broadcast channel for MarketEvents (capacity 1024).
    // Multiple consumers: signal task, trader task, dashboard task.
    let (event_tx, _event_rx): (broadcast::Sender<MarketEvent>, _) =
        broadcast::channel(1024);

    // mpsc channel for CLOB WS token subscriptions (gamma → clob_ws).
    let (clob_sub_tx, clob_sub_rx) = mpsc::channel::<Vec<String>>(64);

    // Step 10: Spawn RTDS Binance feed — separate WS connection.
    let (binance_tx, _binance_rx) =
        broadcast::channel::<feeds::rtds_binance::BinanceTick>(256);
    tokio::spawn(feeds::rtds_binance::run(
        cfg.network.rtds_url.clone(),
        cfg.network.rtds_ping_interval_secs,
        binance_tx,
    ));

    // Step 11: Spawn RTDS Chainlink feed — SEPARATE WS connection from Binance.
    // Critical: two symbols on one RTDS WS silently kills the first feed.
    let (chainlink_tx, _chainlink_rx) =
        broadcast::channel::<feeds::rtds_chainlink::ChainlinkTick>(256);
    tokio::spawn(feeds::rtds_chainlink::run(
        cfg.network.rtds_url.clone(),
        cfg.network.rtds_ping_interval_secs,
        chainlink_tx,
    ));

    // Step 12: Spawn CLOB WS feed.
    let clob_state = market_state.clone();
    let clob_event_tx = event_tx.clone();
    tokio::spawn(feeds::clob_ws::run(
        cfg.network.clob_ws_url.clone(),
        clob_state,
        clob_event_tx,
        clob_sub_rx,
    ));

    // Step 13: Spawn Gamma poller (market discovery, 60s poll).
    let gamma_state = market_state.clone();
    let gamma_event_tx = event_tx.clone();
    tokio::spawn(feeds::gamma::run(
        cfg.network.gamma_url.clone(),
        cfg.filters.gamma_poll_interval_secs,
        gamma_state,
        gamma_event_tx,
        clob_sub_tx,
        GammaFilters {
            max_hours_to_resolution: 24.0,
            min_flash_volume: cfg.filters.flash_min_volume_usd,
            min_standard_volume: cfg.filters.standard_min_volume_usd,
        },
    ));

    info!("P2 feeds spawned — Binance RTDS, Chainlink RTDS, CLOB WS, Gamma poll");
    info!("Signal, trader, and dashboard not yet implemented. Waiting for Ctrl-C...");

    // Step 17: Wait for clean shutdown signal.
    tokio::signal::ctrl_c().await?;
    info!("Ctrl-C received. Shutting down.");

    // Step 18-20: Trader cancel_all + exit (implemented in P4).
    // For now, exit cleanly.
    info!("Clean shutdown. Goodbye.");
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
