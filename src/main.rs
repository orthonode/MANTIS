mod arb;
mod config;
mod dashboard;
mod feeds;
mod maker;
mod markets;
mod risk;
mod signal;
mod trader;

use anyhow::Result;
use feeds::gamma::GammaFilters;
use maker::fee::FeeCache;
use maker::inventory::InventoryTracker;
use maker::rebate::RebateTracker;
use markets::state::{MarketEvent, MarketState};
use risk::drawdown::DrawdownTracker;
use risk::regime::RegimeState;
use tokio::sync::{broadcast, mpsc};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // Step 1: Load config (dotenvy called inside Config::load)
    let cfg = config::Config::load()?;

    // Step 2: Init tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("mantis=info")),
        )
        .init();

    info!("MANTIS starting up — v2 (Maker + Standard AI + Kalshi Arb)");
    info!(
        capital_usd = %cfg.capital.total_usd,
        max_exposure_usd = %cfg.capital.max_total_exposure_usd,
        daily_loss_limit_usd = %cfg.capital.daily_loss_limit_usd,
        "Capital config loaded"
    );
    info!(
        standard_enabled = cfg.standard.enabled,
        maker_enabled = cfg.maker.enabled,
        arb_enabled = cfg.arb.enabled,
        "Strategy config loaded"
    );
    info!(
        clob_url = %cfg.network.clob_url,
        geoblock_url = %cfg.network.geoblock_url,
        "Network config loaded"
    );

    // Step 3: Geoblock check — exit(1) if blocked
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

    // Step 4: Init shared FeeCache (shared by maker engine + standard trader).
    let fee_cache = FeeCache::new(cfg.network.clob_url.clone());

    // Step 5: Init shared MarketState
    let market_state = MarketState::new();
    info!("MarketState initialised");

    // Step 6: Init broadcast channel for MarketEvents (capacity 1024).
    let (event_tx, _event_rx): (broadcast::Sender<MarketEvent>, _) =
        broadcast::channel(1024);

    // mpsc channel for CLOB WS token subscriptions (gamma → clob_ws).
    let (clob_sub_tx, clob_sub_rx) = mpsc::channel::<Vec<String>>(64);

    // Step 7: Spawn RTDS Binance feed — separate WS connection.
    let (binance_tx, _binance_rx) =
        broadcast::channel::<feeds::rtds_binance::BinanceTick>(256);
    tokio::spawn(feeds::rtds_binance::run(
        cfg.network.rtds_url.clone(),
        cfg.network.rtds_ping_interval_secs,
        binance_tx.clone(),
    ));

    // Step 8: Spawn RTDS Chainlink feed — SEPARATE WS connection from Binance.
    let (chainlink_tx, _chainlink_rx) =
        broadcast::channel::<feeds::rtds_chainlink::ChainlinkTick>(256);
    tokio::spawn(feeds::rtds_chainlink::run(
        cfg.network.rtds_url.clone(),
        cfg.network.rtds_ping_interval_secs,
        chainlink_tx.clone(),
    ));

    // Step 9: Spawn CLOB WS feed.
    tokio::spawn(feeds::clob_ws::run(
        cfg.network.clob_ws_url.clone(),
        market_state.clone(),
        event_tx.clone(),
        clob_sub_rx,
    ));

    // Step 10: Spawn Gamma poller (market discovery, 60s poll).
    tokio::spawn(feeds::gamma::run(
        cfg.network.gamma_url.clone(),
        cfg.filters.gamma_poll_interval_secs,
        market_state.clone(),
        event_tx.clone(),
        clob_sub_tx,
        GammaFilters {
            max_hours_to_resolution: 24.0,
            min_volume: cfg.filters.standard_min_volume_usd,
        },
    ));

    info!("Feeds spawned — Binance RTDS, Chainlink RTDS, CLOB WS, Gamma poll");

    // Step 11: Init DrawdownTracker + RegimeState.
    let drawdown = DrawdownTracker::new(cfg.capital.total_usd);
    let regime_state = RegimeState::new();

    // Spawn regime detector — reads from both RTDS feeds every 30s.
    tokio::spawn(risk::regime::run(
        binance_tx.subscribe(),
        chainlink_tx.subscribe(),
        regime_state.clone(),
        cfg.regime.clone(),
    ));

    // Step 12: Init shared dashboard state and spawn TUI task.
    let dash_state = std::sync::Arc::new(std::sync::Mutex::new(
        dashboard::tui::DashboardState::new(cfg.capital.total_usd),
    ));
    tokio::spawn({
        let ds = dash_state.clone();
        let ms = market_state.clone();
        let dd = drawdown.clone();
        let rs = regime_state.clone();
        async move { dashboard::tui::run(ds, ms, dd, rs).await }
    });

    // Step 13: Spawn standard AI signal trader (non-fee markets only).
    let trader_deps = trader::task::TaskDeps {
        market_state: market_state.clone(),
        event_rx: event_tx.subscribe(),
        binance_rx: binance_tx.subscribe(),
        chainlink_rx: chainlink_tx.subscribe(),
        standard: cfg.standard.clone(),
        capital: cfg.capital.clone(),
        kelly: cfg.kelly.clone(),
        exit: cfg.exit.clone(),
        ai: cfg.ai.clone(),
        fee_cache: fee_cache.clone(),
        drawdown: drawdown.clone(),
        regime: regime_state.clone(),
        dash_state: dash_state.clone(),
        groq_api_key: cfg.groq_api_key.clone(),
        anthropic_api_key: cfg.anthropic_api_key.clone(),
        private_key: cfg.private_key.clone(),
        clob_url: cfg.network.clob_url.clone(),
    };
    tokio::spawn(trader::task::run(trader_deps));

    // Step 14: Spawn maker engine (5-min/15-min Flash markets, bid/ask spread capture).
    let inventory = InventoryTracker::new(cfg.maker.max_imbalance_shares);
    let rebate = RebateTracker::new();
    let maker_deps = maker::engine::MakerDeps {
        market_state: market_state.clone(),
        event_rx: event_tx.subscribe(),
        cfg: cfg.maker.clone(),
        clob_url: cfg.network.clob_url.clone(),
        private_key: cfg.private_key.clone(),
        drawdown: drawdown.clone(),
        inventory: inventory.clone(),
        rebate: rebate.clone(),
        fee_cache: fee_cache.clone(),
    };
    tokio::spawn(maker::engine::run(maker_deps));

    // Step 15: Spawn auto-redeemer (redeems winning shares within 60s of resolution).
    {
        let positions: std::sync::Arc<dashmap::DashMap<String, trader::executor::OpenPosition>> =
            std::sync::Arc::new(dashmap::DashMap::new());
        tokio::spawn(trader::redeemer::run(
            event_tx.subscribe(),
            positions,
            cfg.network.clob_url.clone(),
            cfg.private_key.clone(),
            dash_state.clone(),
        ));
    }

    // Step 16: Spawn position merger (merge YES+NO pairs every 30s).
    tokio::spawn(trader::merger::run(
        inventory.clone(),
        cfg.network.clob_url.clone(),
        cfg.private_key.clone(),
    ));

    // Step 17: Spawn Kalshi arb scanner (alert-only until config.arb.enabled = true).
    tokio::spawn(arb::scanner::run(arb::scanner::ArbScannerDeps {
        market_state: market_state.clone(),
        arb_cfg: cfg.arb.clone(),
        clob_url: cfg.network.clob_url.clone(),
        kalshi_email: cfg.kalshi_email.clone(),
        kalshi_password: cfg.kalshi_password.clone(),
        private_key: cfg.private_key.clone(),
        dash_state: dash_state.clone(),
    }));

    info!("All tasks spawned — Feeds, Maker, Standard AI, Arb Scanner, Redeemer, Merger, Dashboard");
    info!("MANTIS is live. Waiting for Ctrl-C to shut down...");

    tokio::signal::ctrl_c().await?;
    info!("Ctrl-C received. Shutting down cleanly.");
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
