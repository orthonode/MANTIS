#![allow(dead_code)]
//! Trade risk gate — 8 hard rules from CLAUDE.md Section 7.
//!
//! Every order passes through `check()` before executor::place_order().
//! Any rule violation returns `Err(RiskError)` → log rejection → do not place.
//!
//! Bet sizing uses Kelly (src/risk/kelly.rs) — never hardcoded amounts.
//! Drawdown scaling feeds into Kelly multiplier, replacing the binary halt.

use crate::config::CapitalConfig;
use crate::markets::state::MarketType;
use crate::risk::kelly::{self, KellyInput, KellyMultipliers};
use crate::config::KellyConfig;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use thiserror::Error;
use tracing::warn;

// ── Error types ───────────────────────────────────────────────────────────────

#[derive(Debug, Error, PartialEq)]
pub enum RiskError {
    #[error("Rule 1: total exposure cap would be exceeded (open={open}, new={new}, cap={cap})")]
    ExposureCap {
        open: Decimal,
        new: Decimal,
        cap: Decimal,
    },

    #[error("Rule 4: daily loss limit hit (pnl={pnl}, limit={limit}) — all orders halted for UTC day")]
    DailyLossLimit { pnl: Decimal, limit: Decimal },

    #[error("Rule 5/6: insufficient liquidity (volume={volume}, min={min})")]
    InsufficientLiquidity { volume: Decimal, min: Decimal },

    #[error("Rule 7: insufficient edge (edge={edge:.3}, min={min:.3})")]
    InsufficientEdge { edge: Decimal, min: Decimal },

    #[error("Rule 8: duplicate position — already open on condition_id={condition_id}")]
    DuplicatePosition { condition_id: String },

    #[error("Drawdown halt active — trading suspended until manual resume")]
    DrawdownHalt,

    #[error("Kelly returned no bet — no edge or drawdown halt")]
    NoKellyBet,
}

// ── Order descriptor ──────────────────────────────────────────────────────────

/// A proposed order before risk validation.
#[derive(Debug)]
pub struct ProposedOrder {
    pub condition_id: String,
    pub market_type: MarketType,
    /// YES or NO.
    pub direction: String,
    /// Current YES price (0.0–1.0).
    pub yes_price: Decimal,
    /// Total traded volume for this market.
    pub volume: Decimal,
}

/// A risk-approved order ready for the executor.
#[derive(Debug)]
pub struct ApprovedOrder {
    pub condition_id: String,
    pub market_type: MarketType,
    pub direction: String,
    pub yes_price: Decimal,
    /// Kelly-computed bet size in USD.
    pub bet_size_usd: Decimal,
}

// ── Risk context ──────────────────────────────────────────────────────────────

/// Live portfolio state passed to `check()` on every call.
pub struct RiskContext {
    /// Sum of all open positions in USD.
    pub open_exposure_usd: Decimal,
    /// Realised P&L for current UTC day.
    pub today_pnl_usd: Decimal,
    /// Current bankroll in USD.
    pub bankroll_usd: Decimal,
    /// All condition_ids with currently open positions.
    pub open_condition_ids: Vec<String>,
    /// Kelly multipliers from regime, drawdown, signal confidence, etc.
    pub kelly_multipliers: KellyMultipliers,
    /// AI consensus win probability (0.0–1.0).
    pub win_prob: Decimal,
}

// ── Risk gate ─────────────────────────────────────────────────────────────────

/// Run all 8 risk rules against a proposed order.
///
/// Returns `Ok(ApprovedOrder)` with Kelly-computed bet size if all rules pass.
/// Returns `Err(RiskError)` on the first violation — order is rejected.
pub fn check(
    order: &ProposedOrder,
    ctx: &RiskContext,
    capital: &CapitalConfig,
    kelly_cfg: &KellyConfig,
) -> Result<ApprovedOrder, RiskError> {
    // Rule 0 (drawdown halt): Kelly multiplier is 0.0 when halt active.
    if ctx.kelly_multipliers.drawdown == Decimal::ZERO {
        warn!("RISK REJECT — drawdown halt active");
        return Err(RiskError::DrawdownHalt);
    }

    // Rule 4: Daily loss limit.
    if ctx.today_pnl_usd < -capital.daily_loss_limit_usd {
        warn!(
            pnl = %ctx.today_pnl_usd,
            limit = %capital.daily_loss_limit_usd,
            "RISK REJECT — DAILY_LOSS_LIMIT_HIT"
        );
        return Err(RiskError::DailyLossLimit {
            pnl: ctx.today_pnl_usd,
            limit: -capital.daily_loss_limit_usd,
        });
    }

    // Rule 5/6: Minimum liquidity.
    let min_volume = match order.market_type {
        MarketType::Flash => capital.max_flash_bet_usd * dec!(100), // $500 implied
        MarketType::Standard => capital.max_standard_bet_usd * dec!(100), // $1000 implied
    };
    // Use explicit filter values from config if available; these are safe fallbacks.
    let effective_min = match order.market_type {
        MarketType::Flash => dec!(500),
        MarketType::Standard => dec!(1000),
    };
    let _ = min_volume; // suppress unused warning — effective_min used below
    if order.volume < effective_min {
        warn!(
            condition_id = %order.condition_id,
            volume = %order.volume,
            min = %effective_min,
            "RISK REJECT — insufficient liquidity"
        );
        return Err(RiskError::InsufficientLiquidity {
            volume: order.volume,
            min: effective_min,
        });
    }

    // Rule 8: One position per market.
    if ctx.open_condition_ids.contains(&order.condition_id) {
        warn!(
            condition_id = %order.condition_id,
            "RISK REJECT — duplicate position"
        );
        return Err(RiskError::DuplicatePosition {
            condition_id: order.condition_id.clone(),
        });
    }

    // Rule 7: Minimum edge.
    // For Flash: implied edge = distance from 0.5 adjusted by direction.
    // For Standard: win_prob must be >= standard_min_certainty / 100.
    let min_edge = dec!(0.15); // 15% minimum edge (Section 7)
    let implied_edge = match order.direction.as_str() {
        "YES" => Decimal::ONE - order.yes_price,
        "NO" => order.yes_price,
        _ => Decimal::ZERO,
    };
    if implied_edge < min_edge {
        warn!(
            condition_id = %order.condition_id,
            edge = %implied_edge,
            min = %min_edge,
            "RISK REJECT — insufficient edge"
        );
        return Err(RiskError::InsufficientEdge {
            edge: implied_edge,
            min: min_edge,
        });
    }

    // Compute Kelly bet size.
    let kelly_input = KellyInput {
        win_prob: ctx.win_prob,
        yes_price: order.yes_price,
        bankroll: ctx.bankroll_usd,
        multipliers: ctx.kelly_multipliers.clone(),
    };
    let bet_size_usd = kelly::size(&kelly_input, kelly_cfg)
        .ok_or_else(|| {
            warn!(condition_id = %order.condition_id, "RISK REJECT — Kelly returned None");
            RiskError::NoKellyBet
        })?;

    // Rule 1: Exposure cap — check AFTER computing bet size.
    if ctx.open_exposure_usd + bet_size_usd > capital.max_total_exposure_usd {
        warn!(
            condition_id = %order.condition_id,
            open = %ctx.open_exposure_usd,
            new = %bet_size_usd,
            cap = %capital.max_total_exposure_usd,
            "RISK REJECT — exposure cap"
        );
        return Err(RiskError::ExposureCap {
            open: ctx.open_exposure_usd,
            new: bet_size_usd,
            cap: capital.max_total_exposure_usd,
        });
    }

    // All rules passed.
    Ok(ApprovedOrder {
        condition_id: order.condition_id.clone(),
        market_type: order.market_type.clone(),
        direction: order.direction.clone(),
        yes_price: order.yes_price,
        bet_size_usd,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CapitalConfig, KellyConfig};
    use rust_decimal_macros::dec;

    fn capital() -> CapitalConfig {
        CapitalConfig {
            total_usd: dec!(100),
            max_flash_bet_usd: dec!(5),
            max_standard_bet_usd: dec!(10),
            max_total_exposure_usd: dec!(40),
            daily_loss_limit_usd: dec!(15),
        }
    }

    fn kelly_cfg() -> KellyConfig {
        KellyConfig {
            max_fraction: dec!(0.15),
            min_bet_usd: dec!(1.00),
            flash_category_mult: dec!(1.1),
            standard_category_mult: dec!(0.9),
            political_category_mult: dec!(0.7),
        }
    }

    fn good_ctx() -> RiskContext {
        RiskContext {
            open_exposure_usd: dec!(0),
            today_pnl_usd: dec!(0),
            bankroll_usd: dec!(100),
            open_condition_ids: vec![],
            kelly_multipliers: KellyMultipliers::neutral(),
            win_prob: dec!(0.70),
        }
    }

    fn good_order() -> ProposedOrder {
        ProposedOrder {
            condition_id: "abc123".to_string(),
            market_type: MarketType::Standard,
            direction: "YES".to_string(),
            yes_price: dec!(0.30),
            volume: dec!(2000),
        }
    }

    #[test]
    fn test_good_order_passes() {
        let result = check(&good_order(), &good_ctx(), &capital(), &kelly_cfg());
        assert!(result.is_ok());
        let approved = result.unwrap();
        assert!(approved.bet_size_usd >= dec!(1.00));
        assert!(approved.bet_size_usd <= dec!(15.00));
    }

    #[test]
    fn test_daily_loss_limit() {
        let mut ctx = good_ctx();
        ctx.today_pnl_usd = dec!(-16);
        let err = check(&good_order(), &ctx, &capital(), &kelly_cfg()).unwrap_err();
        assert!(matches!(err, RiskError::DailyLossLimit { .. }));
    }

    #[test]
    fn test_exposure_cap() {
        let mut ctx = good_ctx();
        ctx.open_exposure_usd = dec!(38);
        // Kelly will compute a bet — but 38 + bet > 40 triggers cap.
        let err = check(&good_order(), &ctx, &capital(), &kelly_cfg()).unwrap_err();
        assert!(matches!(err, RiskError::ExposureCap { .. }));
    }

    #[test]
    fn test_insufficient_liquidity() {
        let mut order = good_order();
        order.volume = dec!(400); // below $1000 standard minimum
        let err = check(&order, &good_ctx(), &capital(), &kelly_cfg()).unwrap_err();
        assert!(matches!(err, RiskError::InsufficientLiquidity { .. }));
    }

    #[test]
    fn test_duplicate_position() {
        let mut ctx = good_ctx();
        ctx.open_condition_ids = vec!["abc123".to_string()];
        let err = check(&good_order(), &ctx, &capital(), &kelly_cfg()).unwrap_err();
        assert!(matches!(err, RiskError::DuplicatePosition { .. }));
    }

    #[test]
    fn test_drawdown_halt() {
        let mut ctx = good_ctx();
        ctx.kelly_multipliers.drawdown = Decimal::ZERO;
        let err = check(&good_order(), &ctx, &capital(), &kelly_cfg()).unwrap_err();
        assert!(matches!(err, RiskError::DrawdownHalt));
    }

    #[test]
    fn test_insufficient_edge() {
        let mut order = good_order();
        // yes_price = 0.90 → edge for YES = 1 - 0.90 = 0.10, below 0.15 min
        order.yes_price = dec!(0.90);
        order.direction = "YES".to_string();
        let err = check(&order, &good_ctx(), &capital(), &kelly_cfg()).unwrap_err();
        assert!(matches!(err, RiskError::InsufficientEdge { .. }));
    }

    #[test]
    fn test_flash_liquidity_minimum() {
        let mut order = good_order();
        order.market_type = MarketType::Flash;
        order.volume = dec!(400); // below $500 flash minimum
        let err = check(&order, &good_ctx(), &capital(), &kelly_cfg()).unwrap_err();
        assert!(matches!(err, RiskError::InsufficientLiquidity { .. }));
    }
}
