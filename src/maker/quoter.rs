#![allow(dead_code)]
//! Quote calculator — optimal bid/ask prices for the maker engine.
//!
//! Computes bid/ask around mid-price with dynamic spread widening:
//!   - High volatility (>0.5% move): spread × 1.5
//!   - Low time to resolution (<30s): spread × 2.0
//!   - Inventory imbalance: skew quotes to reduce imbalance faster
//!
//! All prices are rounded to 2 decimal places (Polymarket tick size).

use crate::config::MakerConfig;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

// ── Quote output ─────────────────────────────────────────────────────────────

/// A pair of bid/ask prices for placing maker quotes.
#[derive(Debug, Clone)]
pub struct Quote {
    /// Our limit buy price for YES token.
    pub bid: Decimal,
    /// Our limit sell price for YES token.
    pub ask: Decimal,
    /// Effective spread used (after dynamic adjustments).
    pub effective_spread: Decimal,
    /// True if spread was widened due to high volatility or low time.
    pub widened: bool,
}

impl Quote {
    /// Spread actually captured if both sides fill.
    pub fn capture_pct(&self) -> Decimal {
        self.ask - self.bid
    }
}

// ── Quote params ─────────────────────────────────────────────────────────────

/// Runtime inputs to the quote calculator.
pub struct QuoteParams {
    pub best_bid: Decimal,
    pub best_ask: Decimal,
    /// Recent price volatility as % of mid (0-100 scale, e.g. 0.5 = 0.5%).
    pub volatility_pct: Decimal,
    /// Seconds until market resolution.
    pub seconds_to_resolution: u64,
    /// Inventory imbalance skew from `InventoryTracker::quote_skew()`.
    pub inventory_skew: Decimal,
}

// ── Quoter ────────────────────────────────────────────────────────────────────

/// Calculate optimal bid/ask given current market state.
///
/// Returns None if best_bid >= best_ask (crossed book — do not quote).
pub fn calculate(params: &QuoteParams, cfg: &MakerConfig) -> Option<Quote> {
    // Sanity: don't quote into a crossed book.
    if params.best_bid >= params.best_ask {
        return None;
    }

    let mid = (params.best_bid + params.best_ask) / dec!(2);
    let mut spread = cfg.target_spread_pct;
    let mut widened = false;

    // Widen for high volatility (>0.5%).
    if params.volatility_pct > dec!(0.5) {
        spread *= cfg.volatility_spread_mult;
        widened = true;
    }

    // Widen for time < 60s.
    if params.seconds_to_resolution < 60 {
        spread *= cfg.low_time_spread_mult_60s;
        widened = true;
    }

    // Widen further for time < 30s.
    if params.seconds_to_resolution < 30 {
        spread *= cfg.low_time_spread_mult_30s;
        widened = true;
    }

    let half = spread / dec!(2);

    // Base quotes.
    let mut bid = mid - half;
    let mut ask = mid + half;

    // Apply inventory skew: positive skew = long YES → lower ask (sell faster).
    // Negative skew = long NO → lower bid (buy less YES).
    if params.inventory_skew != Decimal::ZERO {
        bid -= params.inventory_skew;
        ask -= params.inventory_skew;
    }

    // Clamp to valid Polymarket price range [0.01, 0.99].
    bid = bid.max(dec!(0.01)).min(dec!(0.98));
    ask = ask.max(dec!(0.02)).min(dec!(0.99));

    // After clamping, ensure bid < ask.
    if bid >= ask {
        return None;
    }

    // Round to 2 decimal places (Polymarket tick).
    let round = |d: Decimal| d.round_dp(2);

    Some(Quote {
        bid: round(bid),
        ask: round(ask),
        effective_spread: spread,
        widened,
    })
}

/// Returns true if our existing quotes have drifted more than 1 tick (0.01)
/// from the freshly calculated optimal. Triggers a cancel/replace cycle.
pub fn quotes_need_replace(existing_bid: Decimal, existing_ask: Decimal, fresh: &Quote) -> bool {
    let tick = dec!(0.01);
    (existing_bid - fresh.bid).abs() > tick || (existing_ask - fresh.ask).abs() > tick
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MakerConfig;

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

    #[test]
    fn basic_quote_around_mid() {
        let params = QuoteParams {
            best_bid: dec!(0.44),
            best_ask: dec!(0.46),
            volatility_pct: dec!(0.1),
            seconds_to_resolution: 120,
            inventory_skew: Decimal::ZERO,
        };
        let q = calculate(&params, &test_cfg()).expect("quote");
        assert!(q.bid < q.ask);
        assert_eq!(q.effective_spread, dec!(0.025));
        assert!(!q.widened);
    }

    #[test]
    fn widen_on_high_volatility() {
        let params = QuoteParams {
            best_bid: dec!(0.44),
            best_ask: dec!(0.46),
            volatility_pct: dec!(0.8), // above 0.5% threshold
            seconds_to_resolution: 120,
            inventory_skew: Decimal::ZERO,
        };
        let q = calculate(&params, &test_cfg()).expect("quote");
        assert!(q.widened);
        assert_eq!(q.effective_spread, dec!(0.025) * dec!(1.8));
    }

    #[test]
    fn widen_on_low_time_30s() {
        let params = QuoteParams {
            best_bid: dec!(0.44),
            best_ask: dec!(0.46),
            volatility_pct: dec!(0.1),
            seconds_to_resolution: 15, // below 30s threshold → both 60s and 30s wideners
            inventory_skew: Decimal::ZERO,
        };
        let q = calculate(&params, &test_cfg()).expect("quote");
        assert!(q.widened);
        // Applied: 60s mult then 30s mult
        assert_eq!(q.effective_spread, dec!(0.025) * dec!(1.5) * dec!(2.5));
    }

    #[test]
    fn crossed_book_returns_none() {
        let params = QuoteParams {
            best_bid: dec!(0.50),
            best_ask: dec!(0.49), // crossed
            volatility_pct: dec!(0.1),
            seconds_to_resolution: 60,
            inventory_skew: Decimal::ZERO,
        };
        assert!(calculate(&params, &test_cfg()).is_none());
    }

    #[test]
    fn drift_detection() {
        let q = Quote {
            bid: dec!(0.44),
            ask: dec!(0.47),
            effective_spread: dec!(0.03),
            widened: false,
        };
        // No drift — same values.
        assert!(!quotes_need_replace(dec!(0.44), dec!(0.47), &q));
        // More than 1 tick drift on bid.
        assert!(quotes_need_replace(dec!(0.42), dec!(0.47), &q));
    }
}
