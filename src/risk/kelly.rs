#![allow(dead_code)]
//! Fractional Kelly bet sizing.
//!
//! Every order must call `kelly::size()` — hardcoded bet amounts are forbidden.
//!
//! Formula:
//!   f* = (p * b - q) / b
//!   where p = win probability, q = 1 - p, b = net odds
//!
//! Net odds for a binary prediction market:
//!   b = (1 - yes_price) / yes_price
//!   (paying yes_price to win 1.0, net profit = 1 - yes_price per dollar risked)
//!
//! The raw Kelly fraction is then multiplied by seven dynamic factors bundled
//! in `KellyMultipliers`. Each multiplier is clamped to its declared range so
//! a single bad reading cannot produce a catastrophic position size.
//!
//! Final bet = raw_kelly * product(multipliers) * bankroll
//! Hard cap:  min(final_bet, bankroll * max_fraction)   [from config.toml]
//! Hard floor: max(final_bet, min_bet_usd)              [from config.toml]

use crate::config::KellyConfig;
use rust_decimal::prelude::*;
use rust_decimal_macros::dec;

// ── Multipliers ──────────────────────────────────────────────────────────────

/// Seven dynamic scaling factors applied to the raw Kelly fraction.
/// Each field is a Decimal in the documented range — set by the caller based
/// on live market conditions, drawdown state, and regime.
///
/// Defaults (`KellyMultipliers::neutral()`) set every factor to 1.0 so that
/// the raw Kelly fraction is used unmodified — useful in tests and early phases
/// before all signals are wired.
#[derive(Debug, Clone)]
pub struct KellyMultipliers {
    /// How strongly Groq and Claude agree. Range: 0.5 – 1.0.
    /// Both at max certainty → 1.0. One score borderline → 0.5.
    pub confidence: Decimal,

    /// Drawdown-based scaling. Supplied by `drawdown::DrawdownTracker`.
    /// Range: 0.0 – 1.0. At >=20% drawdown this is 0.0 (full halt).
    pub drawdown: Decimal,

    /// Prefer markets resolving sooner. Range: 0.6 – 1.0.
    /// <30s to resolution → 1.0. Approaching 6h window → 0.6.
    pub timeline: Decimal,

    /// Shrinks in high-volatility regimes. Range: 0.4 – 1.0.
    /// VOLATILE/BREAKING regime → 0.4. QUIET/TRENDING → 1.0.
    pub volatility: Decimal,

    /// Regime trend bonus. Range: 0.5 – 1.2.
    /// TRENDING and signal aligns with trend → 1.2. Against trend → 0.5.
    pub regime: Decimal,

    /// Thin orderbook penalty. Range: 0.5 – 1.0.
    /// Wide spread or shallow book → 0.5. Deep liquid book → 1.0.
    pub liquidity: Decimal,

    /// Market category adjustment. Range: 0.7 – 1.1.
    /// Set from `KellyConfig`: flash=1.1, standard=0.9, political=0.7.
    pub category: Decimal,
}

impl KellyMultipliers {
    /// All multipliers at 1.0 — raw Kelly used unmodified.
    /// Used in early phases before all signals are connected.
    pub fn neutral() -> Self {
        Self {
            confidence: Decimal::ONE,
            drawdown: Decimal::ONE,
            timeline: Decimal::ONE,
            volatility: Decimal::ONE,
            regime: Decimal::ONE,
            liquidity: Decimal::ONE,
            category: Decimal::ONE,
        }
    }

    /// Product of all seven multipliers.
    pub fn product(&self) -> Decimal {
        self.confidence
            * self.drawdown
            * self.timeline
            * self.volatility
            * self.regime
            * self.liquidity
            * self.category
    }
}

// ── Kelly sizing ─────────────────────────────────────────────────────────────

/// Inputs required to compute a Kelly bet size.
pub struct KellyInput {
    /// AI consensus win probability. Range: 0.0 – 1.0.
    pub win_prob: Decimal,
    /// Current YES price in the market. Range: 0.0 – 1.0.
    pub yes_price: Decimal,
    /// Current bankroll in USD.
    pub bankroll: Decimal,
    /// Seven dynamic scaling factors.
    pub multipliers: KellyMultipliers,
}

/// Compute the recommended bet size in USD.
///
/// Returns `None` if:
///   - `yes_price` is at or near 0 or 1 (division by zero / infinite odds)
///   - Raw Kelly fraction is zero or negative (no edge)
///   - `drawdown` multiplier is 0.0 (system in full halt — do NOT trade)
pub fn size(input: &KellyInput, cfg: &KellyConfig) -> Option<Decimal> {
    // Full halt: drawdown multiplier is 0.0 means DRAWDOWN_HALT is active.
    if input.multipliers.drawdown == Decimal::ZERO {
        return None;
    }

    // Guard against degenerate yes_price values.
    if input.yes_price <= dec!(0.01) || input.yes_price >= dec!(0.99) {
        return None;
    }

    // Net odds: profit per $1 risked if YES wins.
    // b = (1 - yes_price) / yes_price
    let b = (Decimal::ONE - input.yes_price) / input.yes_price;

    let p = input.win_prob;
    let q = Decimal::ONE - p;

    // Raw Kelly: f* = (p*b - q) / b
    let raw = (p * b - q) / b;

    // No edge — do not trade.
    if raw <= Decimal::ZERO {
        return None;
    }

    // Apply all seven multipliers.
    let scaled = raw * input.multipliers.product();

    // Final bet = scaled fraction × bankroll.
    let bet = scaled * input.bankroll;

    // Apply hard cap and hard floor.
    let capped = bet.min(input.bankroll * cfg.max_fraction);
    let floored = capped.max(cfg.min_bet_usd);

    // Final sanity: never exceed bankroll itself.
    if floored > input.bankroll {
        return None;
    }

    Some(floored.round_dp(2))
}

// ── Multiplier helpers ───────────────────────────────────────────────────────

/// Compute the `confidence` multiplier from two AI scores (0–100 each).
///
/// Both agree strongly (both >= 80): 1.0
/// One borderline (one < 60):        0.5
/// Linear interpolation between those extremes.
pub fn confidence_mult(groq_score: u8, claude_score: u8) -> Decimal {
    let min_score = groq_score.min(claude_score);
    if min_score >= 80 {
        return Decimal::ONE;
    }
    if min_score < 60 {
        return dec!(0.5);
    }
    // Linear: 60→0.5, 80→1.0
    let t = Decimal::from(min_score - 60) / dec!(20.0);
    dec!(0.5) + t * dec!(0.5)
}

/// Compute the `timeline` multiplier from seconds to resolution.
///
/// Under 60s: 1.0  (very imminent — maximum urgency / confidence)
/// At 21600s:  0.6  (6h out — lowest timeline multiplier allowed)
/// Linear interpolation between those extremes.
pub fn timeline_mult(seconds_to_resolution: u64) -> Decimal {
    if seconds_to_resolution <= 60 {
        return Decimal::ONE;
    }
    let secs = seconds_to_resolution.min(21600) as i64;
    // t goes 0.0 (at 60s) → 1.0 (at 21600s)
    let t = Decimal::from(secs - 60) / dec!(21540.0);
    (Decimal::ONE - t * dec!(0.4)).max(dec!(0.6))
}

/// Compute the `liquidity` multiplier from YES-token spread.
///
/// Spread <= 0.02 (2 cents): 1.0 (tight book)
/// Spread >= 0.20 (20 cents): 0.5 (wide / illiquid)
pub fn liquidity_mult(spread: Decimal) -> Decimal {
    if spread <= dec!(0.02) {
        return Decimal::ONE;
    }
    if spread >= dec!(0.20) {
        return dec!(0.5);
    }
    // Linear: 0.02→1.0, 0.20→0.5
    let t = (spread - dec!(0.02)) / dec!(0.18);
    (Decimal::ONE - t * dec!(0.5)).max(dec!(0.5))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn test_cfg() -> KellyConfig {
        KellyConfig {
            max_fraction: dec!(0.15),
            min_bet_usd: dec!(1.00),
            flash_category_mult: dec!(1.1),
            standard_category_mult: dec!(0.9),
            political_category_mult: dec!(0.7),
        }
    }

    #[test]
    fn test_positive_edge_returns_bet() {
        let input = KellyInput {
            win_prob: dec!(0.65),
            yes_price: dec!(0.35),
            bankroll: dec!(100.0),
            multipliers: KellyMultipliers::neutral(),
        };
        let bet = size(&input, &test_cfg()).unwrap();
        assert!(bet >= dec!(1.00), "bet should meet floor: {bet}");
        assert!(bet <= dec!(15.00), "bet should not exceed 15% cap: {bet}");
    }

    #[test]
    fn test_no_edge_returns_none() {
        // win_prob = 0.35, yes_price = 0.60 → b = 0.667, raw = (0.35*0.667 - 0.65)/0.667 < 0
        let input = KellyInput {
            win_prob: dec!(0.35),
            yes_price: dec!(0.60),
            bankroll: dec!(100.0),
            multipliers: KellyMultipliers::neutral(),
        };
        assert!(size(&input, &test_cfg()).is_none());
    }

    #[test]
    fn test_drawdown_halt_returns_none() {
        let mut mults = KellyMultipliers::neutral();
        mults.drawdown = Decimal::ZERO;
        let input = KellyInput {
            win_prob: dec!(0.70),
            yes_price: dec!(0.30),
            bankroll: dec!(100.0),
            multipliers: mults,
        };
        assert!(size(&input, &test_cfg()).is_none());
    }

    #[test]
    fn test_cap_at_max_fraction() {
        // Very strong edge — raw Kelly would exceed 15% cap.
        let input = KellyInput {
            win_prob: dec!(0.95),
            yes_price: dec!(0.10),
            bankroll: dec!(100.0),
            multipliers: KellyMultipliers::neutral(),
        };
        let bet = size(&input, &test_cfg()).unwrap();
        assert!(bet <= dec!(15.00), "bet should be capped at 15%: {bet}");
    }

    #[test]
    fn test_floor_applied() {
        // Tiny bankroll → bet would be < $1 without floor.
        let input = KellyInput {
            win_prob: dec!(0.52),
            yes_price: dec!(0.49),
            bankroll: dec!(5.0),
            multipliers: KellyMultipliers::neutral(),
        };
        if let Some(bet) = size(&input, &test_cfg()) {
            assert!(bet >= dec!(1.00), "floor should apply: {bet}");
        }
    }

    #[test]
    fn test_confidence_mult_strong() {
        assert_eq!(confidence_mult(85, 90), Decimal::ONE);
    }

    #[test]
    fn test_confidence_mult_weak() {
        assert_eq!(confidence_mult(55, 70), dec!(0.5));
    }

    #[test]
    fn test_timeline_mult_imminent() {
        assert_eq!(timeline_mult(30), Decimal::ONE);
    }

    #[test]
    fn test_timeline_mult_far() {
        let m = timeline_mult(21600);
        assert!(m >= dec!(0.6) && m <= dec!(0.61));
    }
}
