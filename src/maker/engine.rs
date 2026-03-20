#![allow(dead_code)]
//! Market-maker engine — main tokio task.
//!
//! Runs as a separate task alongside the directional signal engine (P1-P5).
//! Targets 5-min and 15-min BTC/ETH crypto markets only.
//!
//! CRITICAL RULE CHANGES (Feb 2026 Polymarket updates):
//!   - feeRateBps REQUIRED in every signed order on fee-enabled markets.
//!   - Cancel ALL maker orders at T-10s before resolution — non-negotiable.
//!   - Cancel/replace loop target: <100ms (taker orders now execute instantly).
//!
//! Strategy:
//!   - Select markets: Flash, volume >$10K, p outside [0.35, 0.65] high-fee zone.
//!   - Quote bid/ask around mid with dynamic spread widening.
//!   - Cancel/replace every 80ms when quotes drift >1 tick.
//!   - Track inventory; skew quotes when imbalanced; flatten at T-30s.
//!   - Merge YES+NO pairs immediately for risk-free $1/pair profit.
//!
//! Architecture:
//!   MakerDeps → engine::run() → quoter, replacer, inventory, fee, rebate
//!   Shared state written back to DashboardState and InventoryTracker.

use crate::config::MakerConfig;
use crate::maker::{
    fee::FeeCache,
    inventory::InventoryTracker,
    quoter::{self, QuoteParams},
    rebate::RebateTracker,
    replacer::{self, ActiveQuote, ReplaceDecision},
};
use crate::markets::state::{MarketEvent, MarketState, MarketType};
use crate::risk::drawdown::DrawdownTracker;
use dashmap::DashMap;
use reqwest::Client;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::time::{interval, Duration};
use tracing::{error, info, warn};

// ── MakerDeps ─────────────────────────────────────────────────────────────────

pub struct MakerDeps {
    pub market_state: MarketState,
    pub event_rx: broadcast::Receiver<MarketEvent>,
    pub cfg: MakerConfig,
    pub clob_url: String,
    pub private_key: String,
    pub drawdown: DrawdownTracker,
    pub inventory: InventoryTracker,
    pub rebate: RebateTracker,
    pub fee_cache: FeeCache,
}

// ── Shared call context ───────────────────────────────────────────────────────

/// Bundles per-call network + auth context to keep helper arg counts ≤7.
struct MakerCtx<'a> {
    http: &'a Client,
    fee_cache: &'a FeeCache,
    clob_url: &'a str,
    private_key: &'a str,
}

// ── Main maker task ───────────────────────────────────────────────────────────

/// Entry point — spawn via `tokio::spawn(maker::engine::run(deps))`.
pub async fn run(mut deps: MakerDeps) {
    if !deps.cfg.enabled {
        info!("Maker engine disabled (config: maker.enabled = false)");
        return;
    }

    info!("Maker engine started — targeting 5-min/15-min Flash markets");

    let http = Client::new();
    // Active quotes: condition_id → ActiveQuote
    let active_quotes: Arc<DashMap<String, ActiveQuote>> = Arc::new(DashMap::new());

    // Replace loop timer: fires every `replace_loop_target_ms` ms.
    let replace_ms = deps.cfg.replace_loop_ms;
    let mut replace_timer = interval(Duration::from_millis(replace_ms));

    // Market scan timer: evaluate new markets every 5 seconds.
    let mut scan_timer = interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            // ── Replace loop ─────────────────────────────────────────────────
            _ = replace_timer.tick() => {
                let ctx = MakerCtx {
                    http: &http,
                    fee_cache: &deps.fee_cache,
                    clob_url: &deps.clob_url,
                    private_key: &deps.private_key,
                };
                run_replace_cycle(
                    &active_quotes,
                    &deps.market_state,
                    &deps.cfg,
                    &deps.inventory,
                    &ctx,
                ).await;
            }

            // ── Market scan ──────────────────────────────────────────────────
            _ = scan_timer.tick() => {
                let ctx = MakerCtx {
                    http: &http,
                    fee_cache: &deps.fee_cache,
                    clob_url: &deps.clob_url,
                    private_key: &deps.private_key,
                };
                enter_new_markets(
                    &deps.market_state,
                    &deps.cfg,
                    &active_quotes,
                    &deps.inventory,
                    &ctx,
                ).await;

                // Check inventory for mergeable pairs.
                run_merge_check(
                    &deps.inventory,
                    &active_quotes,
                    &deps.rebate,
                    &http,
                    &deps.clob_url,
                    &deps.private_key,
                ).await;

                deps.inventory.warn_excess();
            }

            // ── Event stream ─────────────────────────────────────────────────
            event = deps.event_rx.recv() => {
                match event {
                    Ok(MarketEvent::MarketResolved { condition_id, outcome }) => {
                        handle_resolution(
                            &condition_id,
                            &outcome,
                            &active_quotes,
                            &deps.inventory,
                            &deps.rebate,
                        ).await;
                    }
                    Ok(MarketEvent::BestBidAsk { condition_id, best_bid, best_ask }) => {
                        // Live orderbook update — update market snapshot in place.
                        if let Some(mut snap) = deps.market_state.markets.get_mut(&condition_id) {
                            snap.best_bid = Some(best_bid);
                            snap.best_ask = Some(best_ask);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("Maker engine: event channel lagged by {n} messages");
                    }
                    _ => {}
                }
            }
        }
    }
}

// ── Market entry ──────────────────────────────────────────────────────────────

async fn enter_new_markets(
    state: &MarketState,
    cfg: &MakerConfig,
    active_quotes: &Arc<DashMap<String, ActiveQuote>>,
    inventory: &InventoryTracker,
    ctx: &MakerCtx<'_>,
) {
    for entry in state.markets.iter() {
        let snap = entry.value();

        // Only Flash markets (5-min/15-min crypto).
        if snap.market_type != MarketType::Flash {
            continue;
        }

        // Already quoting this market.
        if active_quotes.contains_key(&snap.condition_id) {
            continue;
        }

        // Volume filter.
        if snap.volume < cfg.min_market_volume_usd {
            continue;
        }

        // Time window: enter 60–180s before resolution.
        let secs = snap.seconds_to_resolution();
        if !(60..=180).contains(&secs) {
            continue;
        }

        // Avoid high-fee zone: p between skip_probability_min and skip_probability_max.
        let p = snap.yes_price;
        if p >= cfg.skip_probability_min && p <= cfg.skip_probability_max {
            info!(
                condition_id = %snap.condition_id,
                yes_price = %p,
                "Maker: skipping high-fee zone market"
            );
            continue;
        }

        // Need valid best_bid / best_ask.
        let (best_bid, best_ask) = match (snap.best_bid, snap.best_ask) {
            (Some(b), Some(a)) => (b, a),
            _ => continue,
        };

        // Fetch fee before quoting.
        let fee_bps = match ctx
            .fee_cache
            .get_fee_bps(ctx.http, &snap.condition_id)
            .await
        {
            Ok(bps) => bps,
            Err(e) => {
                error!(
                    condition_id = %snap.condition_id,
                    "Maker: fee fetch failed — skipping market: {e}"
                );
                continue;
            }
        };

        let params = QuoteParams {
            best_bid,
            best_ask,
            volatility_pct: dec!(0.1), // initial conservative estimate
            seconds_to_resolution: secs,
            inventory_skew: inventory.quote_skew(&snap.condition_id),
        };

        let quote = match quoter::calculate(&params, cfg) {
            Some(q) => q,
            None => continue,
        };

        info!(
            condition_id = %snap.condition_id,
            bid = %quote.bid,
            ask = %quote.ask,
            fee_bps = fee_bps,
            secs = secs,
            "Maker: entering market — placing initial quotes"
        );

        // Place bid + ask orders (paper stub — same pattern as executor.rs).
        let bid_id = place_maker_order_stub(
            &snap.condition_id,
            "BID",
            quote.bid,
            cfg.max_per_side_usd,
            fee_bps,
            ctx.clob_url,
            ctx.private_key,
        )
        .await;

        let ask_id = place_maker_order_stub(
            &snap.condition_id,
            "ASK",
            quote.ask,
            cfg.max_per_side_usd,
            fee_bps,
            ctx.clob_url,
            ctx.private_key,
        )
        .await;

        active_quotes.insert(
            snap.condition_id.clone(),
            ActiveQuote {
                condition_id: snap.condition_id.clone(),
                bid_order_id: bid_id,
                ask_order_id: ask_id,
                bid_price: quote.bid,
                ask_price: quote.ask,
                size_per_side: cfg.max_per_side_usd,
            },
        );
    }
}

// ── Replace cycle ─────────────────────────────────────────────────────────────

async fn run_replace_cycle(
    active_quotes: &Arc<DashMap<String, ActiveQuote>>,
    state: &MarketState,
    cfg: &MakerConfig,
    inventory: &InventoryTracker,
    ctx: &MakerCtx<'_>,
) {
    // Collect condition_ids to avoid holding DashMap ref across await.
    let ids: Vec<String> = active_quotes.iter().map(|e| e.key().clone()).collect();

    for condition_id in ids {
        let snap = match state.markets.get(&condition_id) {
            Some(s) => s.clone(),
            None => {
                active_quotes.remove(&condition_id);
                continue;
            }
        };

        let (best_bid, best_ask) = match (snap.best_bid, snap.best_ask) {
            (Some(b), Some(a)) => (b, a),
            _ => continue,
        };

        let secs = snap.seconds_to_resolution();
        let active = match active_quotes.get(&condition_id) {
            Some(a) => a.clone(),
            None => continue,
        };

        let params = QuoteParams {
            best_bid,
            best_ask,
            volatility_pct: dec!(0.1),
            seconds_to_resolution: secs,
            inventory_skew: inventory.quote_skew(&condition_id),
        };

        match replacer::evaluate(&active, &params, cfg) {
            ReplaceDecision::Hold => {}

            ReplaceDecision::CancelAll => {
                cancel_maker_quotes(&active, ctx.clob_url, ctx.private_key).await;
                active_quotes.remove(&condition_id);
            }

            ReplaceDecision::Replace { new_quote } => {
                // Cancel existing.
                cancel_maker_quotes(&active, ctx.clob_url, ctx.private_key).await;

                // Fetch fresh fee.
                let fee_bps = match ctx.fee_cache.get_fee_bps(ctx.http, &condition_id).await {
                    Ok(bps) => bps,
                    Err(e) => {
                        error!(condition_id = %condition_id, "Maker replace: fee fetch failed: {e}");
                        active_quotes.remove(&condition_id);
                        continue;
                    }
                };

                // Place fresh quotes.
                let bid_id = place_maker_order_stub(
                    &condition_id,
                    "BID",
                    new_quote.bid,
                    active.size_per_side,
                    fee_bps,
                    ctx.clob_url,
                    ctx.private_key,
                )
                .await;
                let ask_id = place_maker_order_stub(
                    &condition_id,
                    "ASK",
                    new_quote.ask,
                    active.size_per_side,
                    fee_bps,
                    ctx.clob_url,
                    ctx.private_key,
                )
                .await;

                active_quotes.insert(
                    condition_id.clone(),
                    ActiveQuote {
                        condition_id: condition_id.clone(),
                        bid_order_id: bid_id,
                        ask_order_id: ask_id,
                        bid_price: new_quote.bid,
                        ask_price: new_quote.ask,
                        size_per_side: active.size_per_side,
                    },
                );
            }
        }
    }
}

// ── Merge check ───────────────────────────────────────────────────────────────

async fn run_merge_check(
    inventory: &InventoryTracker,
    active_quotes: &Arc<DashMap<String, ActiveQuote>>,
    rebate: &RebateTracker,
    http: &Client,
    clob_url: &str,
    private_key: &str,
) {
    let _ = (http, clob_url, private_key); // suppress unused until real SDK
    for condition_id in inventory.active_markets() {
        let snap = inventory.snapshot(&condition_id);
        let pairs = snap.mergeable_pairs();
        if pairs > Decimal::ZERO {
            let profit = inventory.record_merge(&condition_id, pairs);
            rebate.record_fill(pairs); // count merge volume toward rebate
            info!(
                condition_id = %condition_id,
                pairs = %pairs,
                profit_usd = %profit,
                "Maker: merged YES+NO pairs for risk-free profit"
            );
        }
    }
    let _ = active_quotes;
}

// ── Resolution handler ────────────────────────────────────────────────────────

async fn handle_resolution(
    condition_id: &str,
    outcome: &str,
    active_quotes: &Arc<DashMap<String, ActiveQuote>>,
    inventory: &InventoryTracker,
    rebate: &RebateTracker,
) {
    // If we still have active quotes, they should have been cancelled at T-10s.
    // If somehow we still have them, log a warning — this is a risk event.
    if let Some(active) = active_quotes.remove(condition_id) {
        warn!(
            condition_id = %condition_id,
            outcome = %outcome,
            bid = %active.1.bid_price,
            ask = %active.1.ask_price,
            "Maker: RESOLUTION with active quotes — T-10s cancel may have missed"
        );
    }

    let snap = inventory.snapshot(condition_id);
    let remaining_imbalance = snap.imbalance();
    if remaining_imbalance.abs() > Decimal::ZERO {
        warn!(
            condition_id = %condition_id,
            imbalance = %remaining_imbalance,
            outcome = %outcome,
            "Maker: resolved with non-zero inventory imbalance"
        );
    }

    // Log spread profit captured.
    let profit = snap.spread_profit_usd + snap.merge_profit_usd;
    info!(
        condition_id = %condition_id,
        outcome = %outcome,
        spread_profit = %snap.spread_profit_usd,
        merge_profit = %snap.merge_profit_usd,
        total_profit = %profit,
        "Maker: market resolved"
    );

    inventory.clear_market(condition_id);
    rebate.record_fill(snap.spread_profit_usd.abs());
}

// ── Paper stubs ───────────────────────────────────────────────────────────────

/// Paper trade stub for placing maker limit orders.
/// TODO(P6): replace with real CLOB SDK call (same pattern as executor.rs).
async fn place_maker_order_stub(
    condition_id: &str,
    side: &str,
    price: Decimal,
    size_usd: Decimal,
    fee_bps: u64,
    _clob_url: &str,
    _private_key: &str,
) -> Option<String> {
    // TODO(P6): actual SDK call:
    //   let signer = LocalSigner::from_str(private_key)?;
    //   let client = ClobClient::new(clob_url, signer).authenticate().await?;
    //   let order_args = OrderArgs {
    //       token_id,
    //       price: price.to_f64()?,
    //       size: size_usd.to_f64()?,
    //       side: if side == "BID" { Side::Buy } else { Side::Sell },
    //       fee_rate_bps: fee_bps,  // REQUIRED — order rejected without this
    //   };
    //   let signed = client.create_order(order_args, None).await?;
    //   let resp = client.post_order(signed, OrderType::Gtc).await?;
    //   return Some(resp.order_id);

    let _ = fee_bps;
    let order_id = format!(
        "MAKER-{side}-{}-{}",
        &condition_id[..8.min(condition_id.len())],
        chrono::Utc::now().timestamp_millis()
    );
    info!(
        order_id = %order_id,
        side = %side,
        price = %price,
        size_usd = %size_usd,
        fee_bps = fee_bps,
        "Maker: order placed (paper stub)"
    );
    Some(order_id)
}

/// Paper trade stub for cancelling a single maker order.
async fn cancel_maker_order_stub(order_id: &str, _clob_url: &str, _private_key: &str) {
    // TODO(P6): client.cancel(CancelOrderArgs { order_id: order_id.to_string() }).await?;
    info!(order_id = %order_id, "Maker: order cancelled (paper stub)");
}

async fn cancel_maker_quotes(active: &ActiveQuote, clob_url: &str, private_key: &str) {
    if let Some(ref id) = active.bid_order_id {
        cancel_maker_order_stub(id, clob_url, private_key).await;
    }
    if let Some(ref id) = active.ask_order_id {
        cancel_maker_order_stub(id, clob_url, private_key).await;
    }
}
