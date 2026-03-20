#![allow(dead_code)]
//! Per-position dynamic exit monitor.
//!
//! For every open position, a `tokio::spawn` runs this watcher every 5 seconds.
//! It implements four exit strategies from the upgrade context:
//!
//!   PROFIT LOCK (trailing stop):
//!     Once position is up 40% of potential profit, activate trailing stop.
//!     If price then reverses 15% from peak profit point: exit immediately.
//!
//!   SENTIMENT SHIFT:
//!     Re-score via Groq every 60s while position is open.
//!     If Groq direction flips from entry direction:
//!       - P&L positive → exit immediately, lock profit
//!       - P&L negative → hold unless shift score > 80 in wrong direction
//!
//!   TIME-BASED:
//!     <5min to resolution AND profitable: hold to resolution
//!     <5min to resolution AND breakeven: hold (fees not worth exit)
//!     <5min to resolution AND losing >20%: exit, take partial loss
//!
//!   LIQUIDITY EMERGENCY:
//!     If CLOB bid disappears (no bid): log LIQUIDITY_EMERGENCY, hold to resolution.

use crate::config::{AiConfig, ExitConfig};
use crate::markets::state::MarketState;
use crate::signal::groq::{self, Direction};
use crate::trader::executor::{cancel_order, OpenPosition};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::time::Duration;
use tokio::time;
use tracing::{info, warn, error};

// ── Exit reason ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum ExitReason {
    TrailingStop {
        peak_price: Decimal,
        current_price: Decimal,
    },
    SentimentShift {
        original_direction: String,
        new_direction: String,
        groq_score: u8,
    },
    TimeBased {
        seconds_remaining: u64,
        pnl_pct: Decimal,
    },
    LiquidityEmergency,
}

// ── Monitor task ──────────────────────────────────────────────────────────────

/// Infrastructure dependencies bundled to keep argument count within clippy limit.
pub struct MonitorDeps {
    pub state: MarketState,
    pub cfg: ExitConfig,
    pub ai_cfg: AiConfig,
    pub groq_api_key: String,
    pub clob_url: String,
    pub private_key: String,
    pub exit_tx: tokio::sync::mpsc::Sender<(String, ExitReason)>,
}

/// Spawn a position monitor for a single open position.
///
/// Polls every 5 seconds and calls `cancel_order` when an exit condition is met.
/// The caller removes the position from its tracking list when this task returns.
pub async fn run(position: OpenPosition, deps: MonitorDeps) {
    let MonitorDeps {
        state,
        cfg,
        ai_cfg,
        groq_api_key,
        clob_url,
        private_key,
        exit_tx,
    } = deps;
    let client = reqwest::Client::new();
    let mut poll = time::interval(Duration::from_secs(5));
    let mut groq_timer = time::interval(Duration::from_secs(cfg.groq_rescore_interval_secs));
    let mut peak_price = position.entry_price;
    let mut trailing_stop_active = false;

    loop {
        tokio::select! {
            _ = poll.tick() => {
                // Get current market snapshot.
                let snap = match state.markets.get(&position.condition_id) {
                    Some(s) => s.clone(),
                    None => {
                        // Market removed (resolved/pruned) — position closed externally.
                        info!(
                            condition_id = %position.condition_id,
                            "Monitor: market removed from state, assuming resolved"
                        );
                        return;
                    }
                };

                let current_price = match position.direction.as_str() {
                    "YES" => snap.yes_price,
                    _ => dec!(1) - snap.yes_price,
                };

                let secs_remaining = snap.seconds_to_resolution();

                // Compute unrealised P&L as fraction of entry.
                let pnl_pct = if position.entry_price > Decimal::ZERO {
                    (current_price - position.entry_price) / position.entry_price
                } else {
                    Decimal::ZERO
                };

                // ── PROFIT LOCK: trailing stop ───────────────────────────────
                // Max potential profit per dollar = 1 - entry_price.
                let max_profit_pct = if position.entry_price > Decimal::ZERO {
                    (dec!(1) - position.entry_price) / position.entry_price
                } else {
                    Decimal::ONE
                };
                let profit_lock_trigger = max_profit_pct * cfg.profit_lock_threshold;

                if pnl_pct >= profit_lock_trigger && !trailing_stop_active {
                    trailing_stop_active = true;
                    info!(
                        condition_id = %position.condition_id,
                        pnl_pct = %pnl_pct,
                        "Monitor: trailing stop ACTIVATED"
                    );
                }

                if current_price > peak_price {
                    peak_price = current_price;
                }

                if trailing_stop_active {
                    // Exit if price reverses trailing_stop_reversal from peak.
                    let stop_price = peak_price * (dec!(1) - cfg.trailing_stop_reversal);
                    if current_price <= stop_price {
                        let reason = ExitReason::TrailingStop {
                            peak_price,
                            current_price,
                        };
                        info!(
                            condition_id = %position.condition_id,
                            peak = %peak_price,
                            current = %current_price,
                            stop = %stop_price,
                            "Monitor: TRAILING STOP HIT — exiting"
                        );
                        do_exit(&position, &reason, &clob_url, &private_key, &exit_tx).await;
                        return;
                    }
                }

                // ── TIME-BASED exit ──────────────────────────────────────────
                if secs_remaining < 300 {
                    if pnl_pct > Decimal::ZERO {
                        // Profitable and <5min: hold to resolution.
                        info!(
                            condition_id = %position.condition_id,
                            secs = secs_remaining,
                            pnl_pct = %pnl_pct,
                            "Monitor: <5min, profitable — holding to resolution"
                        );
                    } else if pnl_pct >= Decimal::ZERO {
                        // Breakeven: hold, fees not worth exit.
                    } else if pnl_pct < -cfg.losing_exit_threshold {
                        // Losing >threshold and <5min: exit.
                        let reason = ExitReason::TimeBased {
                            seconds_remaining: secs_remaining,
                            pnl_pct,
                        };
                        warn!(
                            condition_id = %position.condition_id,
                            secs = secs_remaining,
                            pnl_pct = %pnl_pct,
                            "Monitor: <5min, losing >{:.0}% — exiting",
                            cfg.losing_exit_threshold * dec!(100)
                        );
                        do_exit(&position, &reason, &clob_url, &private_key, &exit_tx).await;
                        return;
                    }
                }

                // ── LIQUIDITY EMERGENCY ──────────────────────────────────────
                if snap.best_bid.is_none() && secs_remaining > 60 {
                    error!(
                        condition_id = %position.condition_id,
                        "Monitor: LIQUIDITY_EMERGENCY — no bid in book, holding to resolution"
                    );
                    // Do NOT exit — no liquidity means we'd get zero. Hold.
                }
            }

            // ── GROQ SENTIMENT RE-SCORE ──────────────────────────────────────
            _ = groq_timer.tick() => {
                let snap = match state.markets.get(&position.condition_id) {
                    Some(s) => s.clone(),
                    None => return,
                };

                let yes_price_f64 = snap.yes_price.to_string().parse::<f64>().unwrap_or(0.5);
                let hours_to_res = snap.seconds_to_resolution() as f64 / 3600.0;

                match groq::score(
                    &client,
                    &groq_api_key,
                    &ai_cfg.groq_model,
                    &snap.question,
                    yes_price_f64,
                    hours_to_res,
                    "",
                ).await {
                    Ok(Some(groq_result)) => {
                        let original_dir = Direction::from_str(&position.direction);
                        let flipped = groq_result.direction != original_dir
                            && groq_result.direction != Direction::Skip;

                        if !flipped {
                            continue;
                        }

                        let current_price = match position.direction.as_str() {
                            "YES" => snap.yes_price,
                            _ => dec!(1) - snap.yes_price,
                        };
                        let pnl_pct = if position.entry_price > Decimal::ZERO {
                            (current_price - position.entry_price) / position.entry_price
                        } else {
                            Decimal::ZERO
                        };

                        // Direction flipped — exit logic.
                        let should_exit = if pnl_pct > Decimal::ZERO {
                            // P&L positive: exit and lock profit.
                            true
                        } else {
                            // P&L negative: only exit if Groq is very confident wrong way.
                            groq_result.score > 80
                        };

                        if should_exit {
                            let reason = ExitReason::SentimentShift {
                                original_direction: position.direction.clone(),
                                new_direction: format!("{:?}", groq_result.direction),
                                groq_score: groq_result.score,
                            };
                            warn!(
                                condition_id = %position.condition_id,
                                groq_score = groq_result.score,
                                pnl_pct = %pnl_pct,
                                "Monitor: SENTIMENT SHIFT — exiting"
                            );
                            do_exit(&position, &reason, &clob_url, &private_key, &exit_tx).await;
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => warn!(condition_id = %position.condition_id, "Monitor: Groq rescore error: {e}"),
                }
            }
        }
    }
}

/// Execute the exit: cancel order and notify trader task.
async fn do_exit(
    position: &OpenPosition,
    reason: &ExitReason,
    clob_url: &str,
    private_key: &str,
    exit_tx: &tokio::sync::mpsc::Sender<(String, ExitReason)>,
) {
    if let Err(e) = cancel_order(&position.order_id, clob_url, private_key).await {
        error!(order_id = %position.order_id, "Monitor: cancel_order failed: {e}");
    }
    let _ = exit_tx.send((position.condition_id.clone(), reason.clone())).await;
}
