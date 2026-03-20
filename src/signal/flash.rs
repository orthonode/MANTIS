#![allow(dead_code)]
//! Flash market signal — Binance vs Chainlink divergence detector.
//!
//! The edge: Polymarket Flash markets resolve via Chainlink Data Streams.
//! The crowd uses Binance UI (slow). MANTIS reads both feeds directly.
//! When Chainlink lags Binance by >= divergence_threshold%, the direction
//! is predictable and we can place before the crowd catches up.
//!
//! DIRECTION rule: follow Chainlink (it is the settlement oracle).
//! If Binance is UP vs Chainlink: Chainlink will likely update UP → buy YES.
//! If Binance is DOWN vs Chainlink: Chainlink will likely update DOWN → buy NO.

use crate::feeds::rtds_binance::BinanceTick;
use crate::feeds::rtds_chainlink::ChainlinkTick;
use crate::markets::state::MarketSnapshot;
use crate::risk::regime::RegimeState;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::info;

// ── Flash signal output ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlashDirection {
    Yes, // Chainlink will update UP — buy YES
    No,  // Chainlink will update DOWN — buy NO
}

/// Signal emitted when a Flash market trade opportunity is detected.
#[derive(Debug, Clone)]
pub struct FlashSignal {
    pub condition_id: String,
    pub direction: FlashDirection,
    /// Divergence magnitude (abs %) between Binance and Chainlink.
    pub divergence_pct: Decimal,
    /// Binance price at signal time.
    pub binance_price: Decimal,
    /// Chainlink price at signal time.
    pub chainlink_price: Decimal,
    /// Seconds to resolution at signal time.
    pub seconds_to_resolution: u64,
}

// ── Divergence detector ───────────────────────────────────────────────────────

/// Entry condition parameters for Flash evaluation.
pub struct FlashParams {
    pub threshold_pct: Decimal,
    pub min_secs_to_resolve: u64,
    pub max_secs_to_resolve: u64,
    pub min_volume: Decimal,
}

/// Evaluate whether a Flash signal exists given current feed prices.
///
/// Returns `Some(FlashSignal)` when ALL entry conditions are met:
///   - Market is of type Flash
///   - Time to resolution is within [flash_min_secs, flash_max_secs]
///   - |divergence| >= threshold_pct (adjusted by regime if VOLATILE/BREAKING)
///   - Both feeds have recent prices (not zero)
///   - Flash orders are not paused by the regime detector
///   - Market volume >= minimum
///
/// Returns `None` if any condition fails (most common path — no edge).
pub fn evaluate(
    market: &MarketSnapshot,
    binance: &BinanceTick,
    chainlink: &ChainlinkTick,
    params: &FlashParams,
    regime: &RegimeState,
) -> Option<FlashSignal> {
    use crate::markets::state::MarketType;

    // Only evaluate Flash markets.
    if market.market_type != MarketType::Flash {
        return None;
    }

    // Skip if regime has paused Flash orders (BREAKING regime).
    if regime.flash_is_paused() {
        return None;
    }

    // Time window check.
    let secs = market.seconds_to_resolution();
    if secs < params.min_secs_to_resolve || secs > params.max_secs_to_resolve {
        return None;
    }

    // Volume check.
    if market.volume < params.min_volume {
        return None;
    }

    // Guard: prices must be valid.
    if binance.price <= 0.0 || chainlink.price <= 0.0 {
        return None;
    }

    let b_price = Decimal::try_from(binance.price).ok()?;
    let c_price = Decimal::try_from(chainlink.price).ok()?;

    // Divergence: (binance - chainlink) / chainlink * 100
    let divergence = (b_price - c_price) / c_price * dec!(100);
    let abs_div = divergence.abs();

    // Apply regime-adjusted threshold.
    let effective_threshold = regime.current().flash_min_divergence_pct(params.threshold_pct);

    if abs_div < effective_threshold {
        return None;
    }

    // Direction: follow Chainlink (settlement oracle).
    // Binance > Chainlink → Chainlink will update UP → YES.
    // Binance < Chainlink → Chainlink will update DOWN → NO.
    let direction = if divergence > Decimal::ZERO {
        FlashDirection::Yes
    } else {
        FlashDirection::No
    };

    info!(
        condition_id = %market.condition_id,
        direction = ?direction,
        divergence_pct = %abs_div,
        binance = %b_price,
        chainlink = %c_price,
        secs_to_res = secs,
        "FLASH SIGNAL DETECTED"
    );

    Some(FlashSignal {
        condition_id: market.condition_id.clone(),
        direction,
        divergence_pct: abs_div,
        binance_price: b_price,
        chainlink_price: c_price,
        seconds_to_resolution: secs,
    })
}

// ── Implied edge ──────────────────────────────────────────────────────────────

/// Compute the implied probability edge for a Flash signal.
///
/// YES price in market represents crowd's probability estimate.
/// If we expect YES to resolve (Chainlink updating up) but crowd has YES at 0.45,
/// our edge = 1.0 - 0.45 = 0.55 (55% expected profit per dollar if correct).
/// We require edge >= min_edge (config: 15%) before the risk module accepts.
pub fn implied_edge(market_yes_price: Decimal, direction: &FlashDirection) -> Decimal {
    match direction {
        FlashDirection::Yes => Decimal::ONE - market_yes_price,
        FlashDirection::No => market_yes_price, // buying NO at (1 - no_price), edge = no_price
    }
}
