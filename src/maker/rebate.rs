#![allow(dead_code)]
//! Rebate tracker — estimate and record daily USDC maker rebates.
//!
//! Polymarket pays maker rebates daily at midnight UTC, proportional to
//! filled maker volume. The rebate rate is market-dependent and not
//! published as a fixed value — we estimate from filled volume.
//!
//! Actual rebates are fetched from data-api.polymarket.com daily.
//! This module tracks the running estimate for dashboard display.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::{Arc, Mutex};
use tracing::info;

// ── Rebate record ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RebateRecord {
    /// UTC date this rebate was paid (midnight UTC).
    pub date: DateTime<Utc>,
    /// USDC rebate amount paid by Polymarket.
    pub amount_usd: Decimal,
}

// ── RebateTracker ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RebateTracker {
    inner: Arc<Mutex<RebateInner>>,
}

#[derive(Debug)]
struct RebateInner {
    /// Filled maker volume today in USD (resets at midnight UTC).
    today_maker_volume_usd: Decimal,
    /// Estimated rebate for today (volume × estimated rebate rate).
    today_estimated_rebate_usd: Decimal,
    /// Confirmed rebate records (from data-api.polymarket.com daily fetch).
    confirmed_rebates: Vec<RebateRecord>,
    /// Total confirmed rebates all time.
    total_confirmed_usd: Decimal,
    /// Approximate rebate rate (updated when actual rebates are confirmed).
    rebate_rate: Decimal,
    /// Timestamp of last reset.
    last_reset: DateTime<Utc>,
}

impl RebateTracker {
    /// Estimated rebate rate: 0.15% of maker volume (conservative estimate).
    /// Actual rate varies by market and Polymarket's daily distribution.
    const DEFAULT_REBATE_RATE: Decimal = dec!(0.0015);

    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RebateInner {
                today_maker_volume_usd: Decimal::ZERO,
                today_estimated_rebate_usd: Decimal::ZERO,
                confirmed_rebates: Vec::new(),
                total_confirmed_usd: Decimal::ZERO,
                rebate_rate: Self::DEFAULT_REBATE_RATE,
                last_reset: Utc::now(),
            })),
        }
    }

    /// Record filled maker volume (call each time a maker order is filled).
    pub fn record_fill(&self, volume_usd: Decimal) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.today_maker_volume_usd += volume_usd;
            inner.today_estimated_rebate_usd =
                inner.today_maker_volume_usd * inner.rebate_rate;
        }
    }

    /// Record an actual confirmed rebate payment from Polymarket.
    /// Updates the rebate rate estimate from actual data.
    pub fn record_confirmed(&self, amount_usd: Decimal, date: DateTime<Utc>) {
        if let Ok(mut inner) = self.inner.lock() {
            // Refine rate estimate if we have volume to compare against.
            if inner.today_maker_volume_usd > Decimal::ZERO {
                inner.rebate_rate = amount_usd / inner.today_maker_volume_usd;
            }
            inner.total_confirmed_usd += amount_usd;
            inner.confirmed_rebates.push(RebateRecord { date, amount_usd });
            info!(
                amount_usd = %amount_usd,
                total_usd = %inner.total_confirmed_usd,
                "Rebate confirmed"
            );
        }
    }

    /// Reset daily counters at midnight UTC.
    pub fn reset_daily(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.today_maker_volume_usd = Decimal::ZERO;
            inner.today_estimated_rebate_usd = Decimal::ZERO;
            inner.last_reset = Utc::now();
        }
    }

    /// Today's filled maker volume in USD.
    pub fn today_volume_usd(&self) -> Decimal {
        self.inner
            .lock()
            .map(|i| i.today_maker_volume_usd)
            .unwrap_or(Decimal::ZERO)
    }

    /// Today's estimated rebate in USD (not yet confirmed).
    pub fn today_estimated_usd(&self) -> Decimal {
        self.inner
            .lock()
            .map(|i| i.today_estimated_rebate_usd)
            .unwrap_or(Decimal::ZERO)
    }

    /// Total confirmed rebates all time.
    pub fn total_confirmed_usd(&self) -> Decimal {
        self.inner
            .lock()
            .map(|i| i.total_confirmed_usd)
            .unwrap_or(Decimal::ZERO)
    }

    /// Airdrop qualification score estimate.
    ///
    /// $POLY airdrop likely based on trade volume + maker activity.
    /// Score = (total maker volume / $100 target) * 100 — capped at 100.
    pub fn airdrop_score(&self, total_maker_volume_usd: Decimal) -> u8 {
        let target = dec!(100); // $100 total maker volume as baseline
        let raw = (total_maker_volume_usd / target * dec!(100))
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0);
        raw.min(100.0) as u8
    }
}

impl Default for RebateTracker {
    fn default() -> Self {
        Self::new()
    }
}
