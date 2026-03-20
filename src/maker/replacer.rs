#![allow(dead_code)]
//! Cancel/replace loop for the market-maker engine.
//!
//! Target: complete a cancel + replace cycle in <100ms.
//!
//! CRITICAL RULES (P6):
//!   - Use CLOB WebSocket (not REST) for best_bid/ask — REST is too slow.
//!   - Cancel ALL maker orders at T-10s before resolution. Non-negotiable.
//!   - Never hold maker inventory through resolution.
//!
//! The replacer runs every `replace_loop_target_ms` milliseconds.
//! It compares current quotes against fresh calculations and only
//! cancels/replaces if drift exceeds 1 tick (0.01).

use crate::maker::quoter::{self, Quote, QuoteParams};
use crate::config::MakerConfig;
use rust_decimal::Decimal;
use tracing::{debug, info, warn};

// ── Active maker quote ────────────────────────────────────────────────────────

/// A live maker quote pair for one market.
#[derive(Debug, Clone)]
pub struct ActiveQuote {
    pub condition_id: String,
    /// Order ID for the BID order (buy YES at bid price).
    pub bid_order_id: Option<String>,
    /// Order ID for the ASK order (sell YES at ask price).
    pub ask_order_id: Option<String>,
    pub bid_price: Decimal,
    pub ask_price: Decimal,
    pub size_per_side: Decimal,
}

// ── Replace decision ──────────────────────────────────────────────────────────

/// What the replacer decided to do.
#[derive(Debug)]
pub enum ReplaceDecision {
    /// Quotes are still valid — no action needed.
    Hold,
    /// Quotes drifted or market conditions changed — cancel and replace.
    Replace { new_quote: Quote },
    /// T-10s before resolution — cancel everything immediately.
    CancelAll,
}

/// Evaluate whether the active quote needs to be replaced or cancelled.
///
/// Returns `ReplaceDecision` — caller is responsible for executing API calls.
pub fn evaluate(
    active: &ActiveQuote,
    params: &QuoteParams,
    cfg: &MakerConfig,
) -> ReplaceDecision {
    // Hard rule: cancel all at T-10s before resolution.
    if params.seconds_to_resolution <= cfg.cancel_before_resolution_secs {
        info!(
            condition_id = %active.condition_id,
            secs_left = params.seconds_to_resolution,
            "REPLACER: T-10s cancel — removing all maker orders before resolution"
        );
        return ReplaceDecision::CancelAll;
    }

    // Calculate fresh optimal quotes.
    let fresh = match quoter::calculate(params, cfg) {
        Some(q) => q,
        None => {
            warn!(
                condition_id = %active.condition_id,
                "REPLACER: crossed book — cancelling quotes"
            );
            return ReplaceDecision::CancelAll;
        }
    };

    // Check if existing quotes have drifted more than 1 tick.
    if quoter::quotes_need_replace(active.bid_price, active.ask_price, &fresh) {
        debug!(
            condition_id = %active.condition_id,
            old_bid = %active.bid_price,
            old_ask = %active.ask_price,
            new_bid = %fresh.bid,
            new_ask = %fresh.ask,
            "REPLACER: drift detected — replacing quotes"
        );
        return ReplaceDecision::Replace { new_quote: fresh };
    }

    ReplaceDecision::Hold
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MakerConfig;
    use rust_decimal_macros::dec;

    fn test_cfg() -> MakerConfig {
        MakerConfig {
            enabled: true,
            target_spread_pct: dec!(0.025),
            max_imbalance_shares: dec!(15),
            cancel_before_resolution_secs: 10,
            replace_loop_ms: 80,
            min_market_volume_usd: dec!(10000),
            max_per_side_usd: dec!(5),
            skip_probability_min: dec!(0.35),
            skip_probability_max: dec!(0.65),
            volatility_spread_mult: dec!(1.8),
            low_time_spread_mult_60s: dec!(1.5),
            low_time_spread_mult_30s: dec!(2.5),
        }
    }

    fn active(bid: Decimal, ask: Decimal) -> ActiveQuote {
        ActiveQuote {
            condition_id: "test".to_string(),
            bid_order_id: Some("bid-1".to_string()),
            ask_order_id: Some("ask-1".to_string()),
            bid_price: bid,
            ask_price: ask,
            size_per_side: dec!(5),
        }
    }

    #[test]
    fn cancel_at_t10() {
        let active = active(dec!(0.44), dec!(0.47));
        let params = QuoteParams {
            best_bid: dec!(0.44),
            best_ask: dec!(0.46),
            volatility_pct: dec!(0.1),
            seconds_to_resolution: 8, // ≤ 10
            inventory_skew: Decimal::ZERO,
        };
        matches!(evaluate(&active, &params, &test_cfg()), ReplaceDecision::CancelAll);
    }

    #[test]
    fn hold_when_no_drift() {
        let active = active(dec!(0.435), dec!(0.465));
        let params = QuoteParams {
            best_bid: dec!(0.44),
            best_ask: dec!(0.46),
            volatility_pct: dec!(0.1),
            seconds_to_resolution: 90,
            inventory_skew: Decimal::ZERO,
        };
        matches!(evaluate(&active, &params, &test_cfg()), ReplaceDecision::Hold);
    }
}
