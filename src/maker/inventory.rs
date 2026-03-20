#![allow(dead_code)]
//! Inventory tracker for the market-maker engine.
//!
//! Tracks YES/NO share positions per market. Signals quote skewing when
//! imbalance exceeds threshold. Triggers aggressive flattening at T-30s.
//!
//! CRITICAL: All maker orders MUST be cancelled at T-10s before resolution.
//! Holding inventory through resolution with imbalance = directional gamble.

use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use tracing::warn;

// ── Inventory state per market ────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct MarketInventory {
    /// YES shares currently held (filled maker buy orders on YES token).
    pub yes_shares: Decimal,
    /// NO shares currently held (filled maker buy orders on NO token).
    pub no_shares: Decimal,
    /// Running P&L from merged YES+NO pairs ($1 per pair).
    pub merge_profit_usd: Decimal,
    /// Running P&L from spread capture (ask_fill - bid_fill).
    pub spread_profit_usd: Decimal,
}

impl MarketInventory {
    /// Net imbalance: positive = long YES, negative = long NO.
    pub fn imbalance(&self) -> Decimal {
        self.yes_shares - self.no_shares
    }

    /// True if inventory is completely flat.
    pub fn is_flat(&self) -> bool {
        self.yes_shares == Decimal::ZERO && self.no_shares == Decimal::ZERO
    }

    /// Number of pairs that can be merged for $1 each.
    pub fn mergeable_pairs(&self) -> Decimal {
        self.yes_shares.min(self.no_shares)
    }
}

// ── InventoryTracker ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct InventoryTracker {
    markets: Arc<DashMap<String, MarketInventory>>,
    max_imbalance: Decimal,
}

impl InventoryTracker {
    pub fn new(max_imbalance: Decimal) -> Self {
        Self {
            markets: Arc::new(DashMap::new()),
            max_imbalance,
        }
    }

    fn entry(&self, condition_id: &str) -> dashmap::mapref::one::RefMut<'_, String, MarketInventory> {
        self.markets
            .entry(condition_id.to_string())
            .or_default()
    }

    /// Record a YES fill (maker buy order filled on YES token).
    pub fn fill_yes(&self, condition_id: &str, shares: Decimal, fill_price: Decimal) {
        let mut inv = self.entry(condition_id);
        inv.yes_shares += shares;
        // Record cost basis contribution (fill_price per share).
        let _ = fill_price; // used by higher-level P&L accounting
    }

    /// Record a NO fill (maker buy order filled on NO token).
    pub fn fill_no(&self, condition_id: &str, shares: Decimal, fill_price: Decimal) {
        let mut inv = self.entry(condition_id);
        inv.no_shares += shares;
        let _ = fill_price;
    }

    /// Record a YES ask fill (someone bought our YES offer — we sold YES).
    pub fn sell_yes(&self, condition_id: &str, shares: Decimal, spread_captured: Decimal) {
        let mut inv = self.entry(condition_id);
        inv.yes_shares = (inv.yes_shares - shares).max(Decimal::ZERO);
        inv.spread_profit_usd += spread_captured;
    }

    /// Record a NO ask fill (someone bought our NO offer — we sold NO).
    pub fn sell_no(&self, condition_id: &str, shares: Decimal, spread_captured: Decimal) {
        let mut inv = self.entry(condition_id);
        inv.no_shares = (inv.no_shares - shares).max(Decimal::ZERO);
        inv.spread_profit_usd += spread_captured;
    }

    /// Record a merge event ($1 per pair). Returns profit added.
    pub fn record_merge(&self, condition_id: &str, pairs: Decimal) -> Decimal {
        let profit = pairs;
        let mut inv = self.entry(condition_id);
        inv.yes_shares = (inv.yes_shares - pairs).max(Decimal::ZERO);
        inv.no_shares = (inv.no_shares - pairs).max(Decimal::ZERO);
        inv.merge_profit_usd += profit;
        profit
    }

    /// Clear all inventory for a resolved/closed market.
    pub fn clear_market(&self, condition_id: &str) {
        self.markets.remove(condition_id);
    }

    /// Returns the imbalance for a market. Positive = long YES, negative = long NO.
    pub fn imbalance(&self, condition_id: &str) -> Decimal {
        self.markets
            .get(condition_id)
            .map(|inv| inv.imbalance())
            .unwrap_or(Decimal::ZERO)
    }

    /// Returns quote skew direction. Positive skew = lower our ask (sell YES faster).
    /// Negative skew = reduce bid (buy less YES, reduce long).
    pub fn quote_skew(&self, condition_id: &str) -> Decimal {
        let imbalance = self.imbalance(condition_id);
        if imbalance.abs() < self.max_imbalance * dec!(0.3) {
            // Within 30% of limit — no skew needed.
            return Decimal::ZERO;
        }
        // Skew proportional to how far we are from the limit.
        let ratio = imbalance / self.max_imbalance;
        ratio.clamp(dec!(-1.0), dec!(1.0)) * dec!(0.02) // max 2-cent skew
    }

    /// Returns true if imbalance exceeds the configured limit.
    /// Triggers aggressive inventory flattening mode.
    pub fn needs_flattening(&self, condition_id: &str) -> bool {
        self.imbalance(condition_id).abs() >= self.max_imbalance
    }

    /// Returns a snapshot for the dashboard.
    pub fn snapshot(&self, condition_id: &str) -> MarketInventory {
        self.markets
            .get(condition_id)
            .map(|inv| inv.clone())
            .unwrap_or_default()
    }

    /// Total spread profit across all markets (for dashboard).
    pub fn total_spread_profit_usd(&self) -> Decimal {
        self.markets
            .iter()
            .map(|e| e.value().spread_profit_usd + e.value().merge_profit_usd)
            .sum()
    }

    /// All condition_ids with non-flat inventory.
    pub fn active_markets(&self) -> Vec<String> {
        self.markets
            .iter()
            .filter(|e| !e.value().is_flat())
            .map(|e| e.key().clone())
            .collect()
    }

    /// Warn if any market has been flagged at-limit for too long.
    pub fn warn_excess(&self) {
        for entry in self.markets.iter() {
            if self.needs_flattening(entry.key()) {
                warn!(
                    condition_id = %entry.key(),
                    imbalance = %entry.value().imbalance(),
                    "INVENTORY: imbalance exceeds limit — aggressive flatten mode"
                );
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn fill_and_imbalance() {
        let tracker = InventoryTracker::new(dec!(20));
        tracker.fill_yes("abc", dec!(10), dec!(0.45));
        assert_eq!(tracker.imbalance("abc"), dec!(10));
        tracker.fill_no("abc", dec!(8), dec!(0.55));
        assert_eq!(tracker.imbalance("abc"), dec!(2));
    }

    #[test]
    fn mergeable_pairs() {
        let tracker = InventoryTracker::new(dec!(20));
        tracker.fill_yes("abc", dec!(5), dec!(0.45));
        tracker.fill_no("abc", dec!(3), dec!(0.55));
        let inv = tracker.snapshot("abc");
        assert_eq!(inv.mergeable_pairs(), dec!(3));
    }

    #[test]
    fn skew_zero_within_threshold() {
        let tracker = InventoryTracker::new(dec!(20));
        tracker.fill_yes("abc", dec!(5), dec!(0.45)); // 5/20 = 25% — under 30%
        assert_eq!(tracker.quote_skew("abc"), Decimal::ZERO);
    }

    #[test]
    fn needs_flattening_at_limit() {
        let tracker = InventoryTracker::new(dec!(20));
        tracker.fill_yes("abc", dec!(20), dec!(0.45));
        assert!(tracker.needs_flattening("abc"));
    }
}
