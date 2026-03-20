//! Main trading task — signal detection → risk gate → order execution.
//!
//! This is the core loop that connects all pipeline stages:
//!
//!   FLASH path (triggered on every PriceUpdate event):
//!     MarketEvent::PriceUpdate → flash::evaluate() → risk::check() →
//!     executor::place_order() → monitor::run() spawned per position
//!
//!   STANDARD path (timer-driven, every 5 minutes):
//!     All Standard markets → consensus::evaluate() (Groq + Claude) →
//!     risk::check() → executor::place_order() → monitor::run() spawned
//!
//!   EXIT path:
//!     monitor::ExitReason received → record P&L → drawdown::update() →
//!     dashboard state updated → position removed from tracking list
//!
//!   RESOLUTION path:
//!     MarketEvent::MarketResolved → close position if open → record settlement P&L
//!
//! Key invariants:
//!   One position per market (enforced by risk::check Rule 8).
//!   Zero unwrap() — all errors logged, task continues.
//!   P&L computed as: profit = size_usd * (1/entry_price - 1) if win, -size_usd if loss.

use crate::config::{AiConfig, CapitalConfig, ExitConfig, FiltersConfig, KellyConfig, SignalConfig};
use crate::dashboard::tui::{LogEntry, SharedDashState, SignalLogEntry};
use crate::feeds::rtds_binance::BinanceTick;
use crate::feeds::rtds_chainlink::ChainlinkTick;
use crate::markets::state::{MarketEvent, MarketState, MarketType};
use crate::risk::drawdown::DrawdownTracker;
use crate::risk::kelly::{self, KellyMultipliers};
use crate::risk::regime::RegimeState;
use crate::signal::consensus::{self, MarketContext};
use crate::signal::flash::{self, FlashDirection, FlashParams};
use crate::trader::executor::{cancel_all_orders, place_order, OpenPosition};
use crate::trader::monitor::{run as monitor_run, ExitReason, MonitorDeps};
use crate::trader::risk::{self as risk_gate, ProposedOrder, RiskContext};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio::time;
use tracing::{error, info, warn};

// ── Task dependencies ─────────────────────────────────────────────────────────

/// All dependencies for the trading task. Bundled to keep argument count low.
pub struct TaskDeps {
    pub market_state: MarketState,
    pub event_rx: broadcast::Receiver<MarketEvent>,
    pub binance_rx: broadcast::Receiver<BinanceTick>,
    pub chainlink_rx: broadcast::Receiver<ChainlinkTick>,
    pub signal: SignalConfig,
    pub filters: FiltersConfig,
    pub capital: CapitalConfig,
    pub kelly: KellyConfig,
    pub exit: ExitConfig,
    pub ai: AiConfig,
    pub drawdown: DrawdownTracker,
    pub regime: RegimeState,
    pub dash_state: SharedDashState,
    pub groq_api_key: String,
    pub anthropic_api_key: String,
    pub private_key: String,
    pub clob_url: String,
}

// ── Main loop ─────────────────────────────────────────────────────────────────

/// Run the trading task forever.
///
/// Exits cleanly only when the tokio runtime shuts down (Ctrl-C).
/// Never panics — all errors are logged and the loop continues.
pub async fn run(mut deps: TaskDeps) {
    let client = reqwest::Client::new();
    let mut open_positions: Vec<OpenPosition> = vec![];

    // Channel: monitor tasks → this task (exit notifications).
    let (exit_tx, mut exit_rx) = mpsc::channel::<(String, ExitReason)>(32);

    // Cache latest RTDS ticks for flash evaluation.
    let mut latest_binance: Option<BinanceTick> = None;
    let mut latest_chainlink: Option<ChainlinkTick> = None;

    // Standard market rescan timer (every 5 minutes).
    let mut std_timer = time::interval(Duration::from_secs(300));
    std_timer.tick().await; // discard the immediate first tick

    info!("Trader task started");

    loop {
        tokio::select! {
            // ── Update RTDS price cache ──────────────────────────────────────
            Ok(tick) = deps.binance_rx.recv() => {
                if let Ok(mut d) = deps.dash_state.lock() {
                    let cl_price = latest_chainlink.as_ref().map(|t| t.price).unwrap_or(0.0);
                    d.binance_price = tick.price;
                    if cl_price > 0.0 {
                        d.push_btc(tick.price, cl_price);
                    }
                    d.conn_binance = true;
                }
                latest_binance = Some(tick);
            }

            Ok(tick) = deps.chainlink_rx.recv() => {
                if let Ok(mut dash) = deps.dash_state.lock() {
                    let bn_price = latest_binance.as_ref().map(|t| t.price).unwrap_or(0.0);
                    dash.chainlink_price = tick.price;
                    if bn_price > 0.0 {
                        dash.push_btc(bn_price, tick.price);
                    }
                    dash.conn_chainlink = true;
                }
                latest_chainlink = Some(tick);
            }

            // ── Market events ────────────────────────────────────────────────
            result = deps.event_rx.recv() => {
                match result {
                    Ok(MarketEvent::PriceUpdate { condition_id, .. }) => {
                        // Flash path: only if we have both RTDS prices.
                        if let (Some(bn), Some(cl)) = (&latest_binance, &latest_chainlink) {
                            try_flash_trade(
                                &condition_id,
                                bn,
                                cl,
                                &deps,
                                &client,
                                &mut open_positions,
                                exit_tx.clone(),
                            )
                            .await;
                        }
                    }

                    Ok(MarketEvent::MarketResolved { condition_id, outcome }) => {
                        settle_resolution(
                            &condition_id,
                            &outcome,
                            &mut open_positions,
                            &deps,
                        );
                    }

                    Ok(_) => {} // BestBidAsk, NewMarket — no action needed here

                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("Trader task: event_rx lagged — skipped {n} events");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        error!("Trader task: event_rx closed — stopping");
                        break;
                    }
                }
            }

            // ── Position exit notification from monitor ───────────────────────
            Some((condition_id, reason)) = exit_rx.recv() => {
                handle_exit(&condition_id, &reason, &mut open_positions, &deps);
            }

            // ── Standard market scan (every 5 minutes) ───────────────────────
            _ = std_timer.tick() => {
                try_standard_trades(&deps, &client, &mut open_positions, exit_tx.clone()).await;
            }
        }
    }

    // Reached only on shutdown — cancel all open orders.
    info!("Trader task shutting down — cancelling {} open positions", open_positions.len());
    cancel_all_orders(&open_positions, &deps.clob_url, &deps.private_key).await;
}

// ── Flash path ────────────────────────────────────────────────────────────────

async fn try_flash_trade(
    condition_id: &str,
    binance: &BinanceTick,
    chainlink: &ChainlinkTick,
    deps: &TaskDeps,
    client: &reqwest::Client,
    open_positions: &mut Vec<OpenPosition>,
    exit_tx: mpsc::Sender<(String, ExitReason)>,
) {
    let snap = match deps.market_state.markets.get(condition_id) {
        Some(s) => s.clone(),
        None => return,
    };

    // Only Flash markets on this path.
    if snap.market_type != MarketType::Flash {
        return;
    }

    let params = FlashParams {
        threshold_pct: deps.signal.flash_divergence_threshold_pct,
        min_secs_to_resolve: deps.signal.flash_min_time_to_resolve_secs,
        max_secs_to_resolve: deps.signal.flash_max_time_to_resolve_secs,
        min_volume: deps.filters.flash_min_volume_usd,
    };

    let signal = match flash::evaluate(&snap, binance, chainlink, &params, &deps.regime) {
        Some(s) => s,
        None => return,
    };

    let direction = match signal.direction {
        FlashDirection::Yes => "YES",
        FlashDirection::No => "NO",
    };

    let implied_edge = flash::implied_edge(snap.yes_price, &signal.direction);
    let win_prob = Decimal::ONE - snap.yes_price + implied_edge / dec!(2);

    let open_exposure: Decimal = open_positions.iter().map(|p| p.size_usd).sum();
    let (today_pnl, bankroll) = {
        let d = deps.dash_state.lock().unwrap();
        (d.today_pnl, d.bankroll)
    };

    let spread = snap.spread().unwrap_or(dec!(0.05));
    let dd_mult = deps.drawdown.multiplier();
    let multipliers = KellyMultipliers {
        confidence: kelly::confidence_mult(70, 0), // flash: no AI score, use baseline
        drawdown: dd_mult,
        timeline: kelly::timeline_mult(signal.seconds_to_resolution),
        volatility: deps.regime.current().kelly_volatility_mult(),
        regime: deps.regime.current().kelly_regime_mult(),
        liquidity: kelly::liquidity_mult(spread),
        category: deps.kelly.flash_category_mult,
    };

    let proposed = ProposedOrder {
        condition_id: condition_id.to_string(),
        market_type: MarketType::Flash,
        direction: direction.to_string(),
        yes_price: snap.yes_price,
        volume: snap.volume,
    };

    let ctx = RiskContext {
        open_exposure_usd: open_exposure,
        today_pnl_usd: today_pnl,
        bankroll_usd: bankroll,
        open_condition_ids: open_positions.iter().map(|p| p.condition_id.clone()).collect(),
        kelly_multipliers: multipliers,
        win_prob,
    };

    match risk_gate::check(&proposed, &ctx, &deps.capital, &deps.kelly) {
        Ok(approved) => {
            execute_and_monitor(
                approved.condition_id.clone(),
                approved.direction.clone(),
                approved.yes_price,
                approved.bet_size_usd,
                open_positions,
                deps,
                client,
                exit_tx,
                Some(70),
                None,
                approved.bet_size_usd,
                "FLASH".to_string(),
            )
            .await;
        }
        Err(e) => {
            log_signal(
                deps,
                condition_id,
                Some(70),
                None,
                None,
                None,
                "REJECTED",
            );
            info!(condition_id, reason = %e, "Flash trade rejected by risk gate");
        }
    }
}

// ── Standard path ─────────────────────────────────────────────────────────────

async fn try_standard_trades(
    deps: &TaskDeps,
    client: &reqwest::Client,
    open_positions: &mut Vec<OpenPosition>,
    exit_tx: mpsc::Sender<(String, ExitReason)>,
) {
    // Collect candidate markets once (release DashMap lock before async calls).
    let candidates: Vec<_> = deps
        .market_state
        .markets
        .iter()
        .filter_map(|entry| {
            let snap = entry.value();

            // Standard markets only.
            if snap.market_type != MarketType::Standard {
                return None;
            }
            // Skip closed or already open.
            if snap.is_closed {
                return None;
            }
            if open_positions.iter().any(|p| p.condition_id == snap.condition_id) {
                return None;
            }
            // Volume filter.
            if snap.volume < deps.filters.standard_min_volume_usd {
                return None;
            }
            // YES price filter (avoid heavy favourites).
            if snap.yes_price < deps.filters.standard_min_yes_price
                || snap.yes_price > deps.filters.standard_max_yes_price
            {
                return None;
            }
            // Time window filter.
            let hours = snap.seconds_to_resolution() as f64 / 3600.0;
            let max_hours = deps.signal.standard_max_hours_to_resolve
                .to_string()
                .parse::<f64>()
                .unwrap_or(6.0);
            if hours <= 0.0 || hours > max_hours {
                return None;
            }

            Some(snap.clone())
        })
        .collect();

    if candidates.is_empty() {
        return;
    }

    info!("Standard scan: {} candidate markets", candidates.len());

    for snap in candidates {
        // Re-check position wasn't opened mid-scan.
        if open_positions.iter().any(|p| p.condition_id == snap.condition_id) {
            continue;
        }

        let hours = snap.seconds_to_resolution() as f64 / 3600.0;
        let market = MarketContext {
            question: &snap.question,
            yes_price: snap.yes_price.to_string().parse::<f64>().unwrap_or(0.5),
            hours_to_res: hours,
            context: snap.description.as_deref().unwrap_or(""),
        };

        let result = consensus::evaluate(
            client,
            &deps.ai,
            &deps.groq_api_key,
            &deps.anthropic_api_key,
            &market,
        )
        .await;

        // Update AI panels in dashboard.
        if let Ok(mut dash) = deps.dash_state.lock() {
            dash.groq_commentary = result.groq_reasoning.clone();
            if let Some(cr) = &result.claude_reasoning {
                dash.claude_reasoning = cr.clone();
            }
            dash.conn_groq = true;
            if result.claude_called {
                dash.conn_claude = true;
            }
        }

        if !result.is_trade() {
            log_signal(
                deps,
                &snap.condition_id,
                Some(result.groq_score),
                result.claude_score,
                Some(result.score),
                None,
                "SKIPPED",
            );
            continue;
        }

        let direction = match result.direction {
            crate::signal::groq::Direction::Yes => "YES",
            crate::signal::groq::Direction::No => "NO",
            crate::signal::groq::Direction::Skip => continue,
        };

        let open_exposure: Decimal = open_positions.iter().map(|p| p.size_usd).sum();
        let (today_pnl, bankroll) = {
            let d = deps.dash_state.lock().unwrap();
            (d.today_pnl, d.bankroll)
        };

        let spread = snap.spread().unwrap_or(dec!(0.05));
        let confidence_mult =
            kelly::confidence_mult(result.groq_score, result.claude_score.unwrap_or(0));
        let dd_mult = deps.drawdown.multiplier();
        let multipliers = KellyMultipliers {
            confidence: confidence_mult,
            drawdown: dd_mult,
            timeline: kelly::timeline_mult(snap.seconds_to_resolution()),
            volatility: deps.regime.current().kelly_volatility_mult(),
            regime: deps.regime.current().kelly_regime_mult(),
            liquidity: kelly::liquidity_mult(spread),
            category: deps.kelly.standard_category_mult,
        };

        // win_prob: consensus score as probability.
        let win_prob = Decimal::from(result.score) / dec!(100);

        let proposed = ProposedOrder {
            condition_id: snap.condition_id.clone(),
            market_type: MarketType::Standard,
            direction: direction.to_string(),
            yes_price: snap.yes_price,
            volume: snap.volume,
        };
        let ctx = RiskContext {
            open_exposure_usd: open_exposure,
            today_pnl_usd: today_pnl,
            bankroll_usd: bankroll,
            open_condition_ids: open_positions.iter().map(|p| p.condition_id.clone()).collect(),
            kelly_multipliers: multipliers,
            win_prob,
        };

        match risk_gate::check(&proposed, &ctx, &deps.capital, &deps.kelly) {
            Ok(approved) => {
                execute_and_monitor(
                    approved.condition_id.clone(),
                    approved.direction.clone(),
                    approved.yes_price,
                    approved.bet_size_usd,
                    open_positions,
                    deps,
                    client,
                    exit_tx.clone(),
                    Some(result.groq_score),
                    result.claude_score,
                    approved.bet_size_usd,
                    "STANDARD".to_string(),
                )
                .await;
                // Update pipeline counter.
                if let Ok(mut dash) = deps.dash_state.lock() {
                    dash.pipeline_placed += 1;
                }
            }
            Err(e) => {
                log_signal(
                    deps,
                    &snap.condition_id,
                    Some(result.groq_score),
                    result.claude_score,
                    Some(result.score),
                    None,
                    "REJECTED",
                );
                info!(condition_id = %snap.condition_id, reason = %e, "Standard trade rejected by risk gate");
            }
        }
    }
}

// ── Execute + spawn monitor ───────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn execute_and_monitor(
    condition_id: String,
    direction: String,
    yes_price: Decimal,
    bet_size: Decimal,
    open_positions: &mut Vec<OpenPosition>,
    deps: &TaskDeps,
    _client: &reqwest::Client,
    exit_tx: mpsc::Sender<(String, ExitReason)>,
    groq_score: Option<u8>,
    claude_score: Option<u8>,
    kelly_size: Decimal,
    market_kind: String,
) {
    let snap = match deps.market_state.markets.get(&condition_id) {
        Some(s) => s.clone(),
        None => return,
    };

    // Build ApprovedOrder for executor.
    let approved = crate::trader::risk::ApprovedOrder {
        condition_id: condition_id.clone(),
        market_type: snap.market_type.clone(),
        direction: direction.clone(),
        yes_price,
        bet_size_usd: bet_size,
    };

    let position = match place_order(&approved, &deps.market_state, &deps.clob_url, &deps.private_key).await {
        Ok(p) => p,
        Err(e) => {
            error!(condition_id, "place_order failed: {e}");
            return;
        }
    };

    info!(
        condition_id,
        direction,
        kelly_size = %kelly_size,
        market_kind,
        "Trade placed"
    );

    // Update dashboard.
    log_signal(
        deps,
        &condition_id,
        groq_score,
        claude_score,
        groq_score.map(|g| {
            let c = claude_score.unwrap_or(0);
            (g as f32 * 0.4 + c as f32 * 0.6) as u8
        }),
        Some(kelly_size),
        "TRADED",
    );

    if let Ok(mut dash) = deps.dash_state.lock() {
        dash.positions.push(position.clone());
        dash.record_kelly(kelly_size);
        let bk = dash.bankroll;
        dash.record_equity(bk);
        dash.pipeline_placed = dash.pipeline_placed.saturating_add(1);
        // Update category counters (will be confirmed at resolution).
        if market_kind == "FLASH" {
            dash.flash_total += 1;
        } else {
            dash.standard_total += 1;
        }
        dash.push_log(LogEntry::info(format!(
            "Trade placed: {condition_id:.8} {direction} ${kelly_size:.2} [{market_kind}]"
        )));
    }

    // Spawn per-position monitor.
    let monitor_deps = MonitorDeps {
        state: deps.market_state.clone(),
        cfg: deps.exit.clone(),
        ai_cfg: deps.ai.clone(),
        groq_api_key: deps.groq_api_key.clone(),
        clob_url: deps.clob_url.clone(),
        private_key: deps.private_key.clone(),
        exit_tx,
    };
    // Need a new client for the monitor task.
    let monitor_client_deps = MonitorDeps {
        state: monitor_deps.state,
        cfg: monitor_deps.cfg,
        ai_cfg: monitor_deps.ai_cfg,
        groq_api_key: monitor_deps.groq_api_key,
        clob_url: monitor_deps.clob_url,
        private_key: monitor_deps.private_key,
        exit_tx: monitor_deps.exit_tx,
    };
    open_positions.push(position.clone());
    tokio::spawn(monitor_run(position, monitor_client_deps));
}

// ── Resolution settlement ─────────────────────────────────────────────────────

fn settle_resolution(
    condition_id: &str,
    outcome: &str,
    open_positions: &mut Vec<OpenPosition>,
    deps: &TaskDeps,
) {
    let pos = match open_positions.iter().find(|p| p.condition_id == condition_id) {
        Some(p) => p.clone(),
        None => return, // no position in this market
    };

    let won = pos.direction == outcome;
    let pnl = if won {
        // profit = size_usd * (1/entry_price - 1)
        if pos.entry_price > Decimal::ZERO {
            pos.size_usd * (Decimal::ONE / pos.entry_price - Decimal::ONE)
        } else {
            Decimal::ZERO
        }
    } else {
        -pos.size_usd
    };

    info!(
        condition_id,
        outcome,
        won,
        pnl = %pnl,
        "Position settled at resolution"
    );

    if let Ok(mut dash) = deps.dash_state.lock() {
        dash.total_pnl += pnl;
        dash.today_pnl += pnl;
        dash.bankroll += pnl;
        dash.total_trades += 1;

        // Determine market type from position for category tracking.
        // We track wins here at settlement; totals were incremented at placement.
        let is_flash = open_positions
            .iter()
            .find(|p| p.condition_id == condition_id)
            .map(|_| {
                deps.market_state
                    .markets
                    .get(condition_id)
                    .map(|s| s.market_type == MarketType::Flash)
                    .unwrap_or(false)
            })
            .unwrap_or(false);

        if won {
            dash.wins += 1;
            dash.gross_wins += pnl;
            if is_flash {
                dash.flash_wins += 1;
            } else {
                dash.standard_wins += 1;
            }
        } else {
            dash.gross_losses += pnl.abs();
        }

        let bk = dash.bankroll;
        dash.record_equity(bk);
        dash.positions.retain(|p| p.condition_id != condition_id);
        dash.push_log(LogEntry::info(format!(
            "Settled: {:.8} {} {}",
            condition_id,
            if won { "WON" } else { "LOST" },
            pnl_str(pnl),
        )));
    }

    deps.drawdown.update(
        deps.dash_state
            .lock()
            .map(|d| d.bankroll)
            .unwrap_or(pos.size_usd),
    );

    open_positions.retain(|p| p.condition_id != condition_id);
}

// ── Exit handling (from monitor) ──────────────────────────────────────────────

fn handle_exit(
    condition_id: &str,
    reason: &ExitReason,
    open_positions: &mut Vec<OpenPosition>,
    deps: &TaskDeps,
) {
    // Find position — monitor already called cancel_order.
    let pos = match open_positions.iter().find(|p| p.condition_id == condition_id) {
        Some(p) => p.clone(),
        None => return,
    };

    // Estimate P&L from current market price (monitor exited at best available).
    let current_price = deps
        .market_state
        .markets
        .get(condition_id)
        .map(|s| {
            if pos.direction == "YES" {
                s.yes_price
            } else {
                dec!(1) - s.yes_price
            }
        })
        .unwrap_or(pos.entry_price);

    let pnl = if pos.entry_price > Decimal::ZERO {
        (current_price - pos.entry_price) / pos.entry_price * pos.size_usd
    } else {
        Decimal::ZERO
    };

    info!(
        condition_id,
        reason = ?reason,
        pnl = %pnl,
        "Position exited by monitor"
    );

    if let Ok(mut dash) = deps.dash_state.lock() {
        dash.total_pnl += pnl;
        dash.today_pnl += pnl;
        dash.bankroll += pnl;
        dash.total_trades += 1;
        if pnl > Decimal::ZERO {
            dash.wins += 1;
            dash.gross_wins += pnl;
        } else {
            dash.gross_losses += pnl.abs();
        }
        let bk = dash.bankroll;
        dash.record_equity(bk);
        dash.positions.retain(|p| p.condition_id != condition_id);
        dash.push_log(LogEntry::warn(format!(
            "Exit: {:.8} {} {}",
            condition_id,
            format!("{:?}", reason).chars().take(20).collect::<String>(),
            pnl_str(pnl),
        )));
    }

    deps.drawdown.update(
        deps.dash_state
            .lock()
            .map(|d| d.bankroll)
            .unwrap_or(pos.size_usd),
    );

    open_positions.retain(|p| p.condition_id != condition_id);
}

// ── Dashboard helpers ─────────────────────────────────────────────────────────

fn log_signal(
    deps: &TaskDeps,
    condition_id: &str,
    groq: Option<u8>,
    claude: Option<u8>,
    consensus: Option<u8>,
    kelly_size: Option<Decimal>,
    action: &str,
) {
    let ts = chrono::Utc::now().format("%H:%M:%S").to_string();
    let entry = SignalLogEntry {
        time: ts,
        market_slug: condition_id
            .chars()
            .take(18)
            .collect::<String>(),
        groq_score: groq,
        claude_score: claude,
        consensus_score: consensus,
        kelly_size,
        action: action.to_string(),
    };
    if let Ok(mut dash) = deps.dash_state.lock() {
        dash.log_signal(entry);
        dash.pipeline_signals += 1;
        if action == "TRADED" {
            dash.pipeline_risk_passed += 1;
        }
    }
}

fn pnl_str(v: Decimal) -> String {
    if v >= Decimal::ZERO {
        format!("+${:.2}", v)
    } else {
        format!("-${:.2}", v.abs())
    }
}
