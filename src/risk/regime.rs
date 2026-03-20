#![allow(dead_code)]
//! Market regime detector.
//!
//! Monitors market environment every 30 seconds using BTC price data from
//! both RTDS feeds and CLOB volume. Emits a `Regime` enum that other modules
//! use to adjust Kelly multipliers and Flash entry thresholds.
//!
//! Regimes:
//!   QUIET:    low volume, tight spreads, no price trend → all multipliers baseline
//!   TRENDING: Chainlink + Binance agree on a direction → regime mult 1.2
//!   VOLATILE: price swing >threshold% in 5min, spreads widening → mult 0.4,
//!             Flash minimum edge raised from 0.3% to 0.5%
//!   BREAKING: volume spike >3x 10-min avg (breaking news) →
//!             pause all new Flash orders 60s, re-score open standard positions

use crate::config::RegimeConfig;
use crate::feeds::rtds_binance::BinanceTick;
use crate::feeds::rtds_chainlink::ChainlinkTick;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time;
use tracing::info;

// ── Regime enum ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Regime {
    /// Low activity, tight spreads — normal operation.
    Quiet,
    /// Both feeds agree on a price direction — slight size boost.
    Trending,
    /// Large price swing in short window — reduce sizes, raise Flash threshold.
    Volatile,
    /// Volume spike — pause Flash, re-score standard positions immediately.
    Breaking,
}

impl Regime {
    /// Kelly `regime` multiplier for this regime.
    pub fn kelly_regime_mult(&self) -> Decimal {
        match self {
            Regime::Quiet => Decimal::ONE,
            Regime::Trending => dec!(1.2),
            Regime::Volatile => dec!(0.5),
            Regime::Breaking => dec!(0.5),
        }
    }

    /// Kelly `volatility` multiplier for this regime.
    pub fn kelly_volatility_mult(&self) -> Decimal {
        match self {
            Regime::Quiet => Decimal::ONE,
            Regime::Trending => Decimal::ONE,
            Regime::Volatile => dec!(0.4),
            Regime::Breaking => dec!(0.4),
        }
    }

    /// Minimum Flash divergence threshold (%) for this regime.
    /// VOLATILE/BREAKING raise the bar to reduce false signals.
    pub fn flash_min_divergence_pct(&self, base: Decimal) -> Decimal {
        match self {
            Regime::Volatile | Regime::Breaking => (base * dec!(1.67)).max(dec!(0.5)),
            _ => base,
        }
    }

    /// True if new Flash orders should be paused (BREAKING regime).
    pub fn flash_paused(&self) -> bool {
        matches!(self, Regime::Breaking)
    }
}

// ── Shared regime state ───────────────────────────────────────────────────────

/// Shared, cheaply cloneable handle to the current regime.
#[derive(Debug, Clone)]
pub struct RegimeState(Arc<Mutex<RegimeInner>>);

#[derive(Debug)]
struct RegimeInner {
    pub regime: Regime,
    /// Seconds remaining in a Flash pause (counts down to 0).
    pub flash_pause_remaining_secs: u64,
}

impl RegimeState {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(RegimeInner {
            regime: Regime::Quiet,
            flash_pause_remaining_secs: 0,
        })))
    }

    pub fn current(&self) -> Regime {
        self.0.lock().unwrap().regime.clone()
    }

    pub fn flash_is_paused(&self) -> bool {
        let inner = self.0.lock().unwrap();
        inner.flash_pause_remaining_secs > 0 || inner.regime.flash_paused()
    }

    fn set(&self, regime: Regime, pause_secs: u64) {
        let mut inner = self.0.lock().unwrap();
        if inner.regime != regime {
            info!("Regime: {:?} → {:?}", inner.regime, regime);
        }
        inner.regime = regime;
        inner.flash_pause_remaining_secs = pause_secs;
    }

    fn tick_pause(&self) {
        let mut inner = self.0.lock().unwrap();
        inner.flash_pause_remaining_secs = inner.flash_pause_remaining_secs.saturating_sub(30);
    }
}

impl Default for RegimeState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Price window ──────────────────────────────────────────────────────────────

/// Rolling 5-minute price window to detect volatility.
struct PriceWindow {
    /// (timestamp_ms, price) pairs, newest last.
    prices: VecDeque<(u64, f64)>,
    /// Keep only prices within this many milliseconds.
    window_ms: u64,
}

impl PriceWindow {
    fn new(window_ms: u64) -> Self {
        Self {
            prices: VecDeque::new(),
            window_ms,
        }
    }

    fn push(&mut self, ts_ms: u64, price: f64) {
        self.prices.push_back((ts_ms, price));
        // Prune old prices outside the window.
        while self
            .prices
            .front()
            .map(|(t, _)| ts_ms.saturating_sub(*t) > self.window_ms)
            == Some(true)
        {
            self.prices.pop_front();
        }
    }

    /// Price swing as % over the window. Returns 0.0 if < 2 data points.
    fn swing_pct(&self) -> f64 {
        if self.prices.len() < 2 {
            return 0.0;
        }
        let prices: Vec<f64> = self.prices.iter().map(|(_, p)| *p).collect();
        let hi = prices.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let lo = prices.iter().cloned().fold(f64::INFINITY, f64::min);
        if lo == 0.0 {
            return 0.0;
        }
        ((hi - lo) / lo) * 100.0
    }

    /// Most recent price, or 0.0 if empty.
    fn last_price(&self) -> f64 {
        self.prices.back().map(|(_, p)| *p).unwrap_or(0.0)
    }
}

// ── Volume window ─────────────────────────────────────────────────────────────

/// Rolling 10-minute volume accumulator for BREAKING detection.
struct VolumeWindow {
    ticks: VecDeque<(u64, f64)>,
    window_ms: u64,
}

impl VolumeWindow {
    fn new(window_ms: u64) -> Self {
        Self {
            ticks: VecDeque::new(),
            window_ms,
        }
    }

    fn push(&mut self, ts_ms: u64, volume: f64) {
        self.ticks.push_back((ts_ms, volume));
        while self
            .ticks
            .front()
            .map(|(t, _)| ts_ms.saturating_sub(*t) > self.window_ms)
            == Some(true)
        {
            self.ticks.pop_front();
        }
    }

    /// Average volume per tick in the window. 0.0 if no ticks.
    fn avg(&self) -> f64 {
        if self.ticks.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.ticks.iter().map(|(_, v)| *v).sum();
        sum / self.ticks.len() as f64
    }
}

// ── Detector task ─────────────────────────────────────────────────────────────

/// Spawn and run the regime detector task forever.
///
/// Reads from both RTDS feeds, evaluates every 30 seconds, writes to `state`.
pub async fn run(
    mut binance_rx: broadcast::Receiver<BinanceTick>,
    mut chainlink_rx: broadcast::Receiver<ChainlinkTick>,
    state: RegimeState,
    cfg: RegimeConfig,
) {
    let mut interval = time::interval(Duration::from_secs(30));

    // 5-minute price window (300,000 ms) for volatility detection.
    let mut binance_window = PriceWindow::new(300_000);
    let mut chainlink_window = PriceWindow::new(300_000);
    // 10-minute volume window (600,000 ms) for breaking detection.
    // We use Binance tick count as a proxy for activity volume.
    let mut volume_window = VolumeWindow::new(600_000);

    loop {
        tokio::select! {
            Ok(tick) = binance_rx.recv() => {
                let now_ms = chrono::Utc::now().timestamp_millis() as u64;
                binance_window.push(now_ms, tick.price);
                volume_window.push(now_ms, 1.0); // 1 tick = 1 unit of activity
            }

            Ok(tick) = chainlink_rx.recv() => {
                let now_ms = chrono::Utc::now().timestamp_millis() as u64;
                chainlink_window.push(now_ms, tick.price);
            }

            _ = interval.tick() => {
                state.tick_pause();
                let regime = evaluate(
                    &binance_window,
                    &chainlink_window,
                    &volume_window,
                    &cfg,
                );
                let pause_secs = if regime == Regime::Breaking {
                    cfg.flash_pause_on_breaking_secs
                } else {
                    0
                };
                state.set(regime, pause_secs);
            }
        }
    }
}

/// Evaluate current regime from price and volume windows.
fn evaluate(
    binance: &PriceWindow,
    chainlink: &PriceWindow,
    volume: &VolumeWindow,
    cfg: &RegimeConfig,
) -> Regime {
    let volatile_threshold: f64 = cfg
        .volatile_threshold_pct
        .try_into()
        .unwrap_or(1.0);
    let breaking_mult: f64 = cfg
        .breaking_volume_multiplier
        .try_into()
        .unwrap_or(3.0);

    let binance_swing = binance.swing_pct();
    let avg_volume = volume.avg();
    let current_volume = volume.ticks.back().map(|(_, v)| *v).unwrap_or(0.0);

    // BREAKING: volume spike vs 10-min average.
    if avg_volume > 0.0 && current_volume >= avg_volume * breaking_mult {
        return Regime::Breaking;
    }

    // VOLATILE: large price swing in 5min window.
    if binance_swing >= volatile_threshold {
        return Regime::Volatile;
    }

    // TRENDING: both feeds present, both agree on direction.
    let b_last = binance.last_price();
    let c_last = chainlink.last_price();
    if b_last > 0.0 && c_last > 0.0 && binance.prices.len() >= 2 && chainlink.prices.len() >= 2 {
        let b_first = binance.prices.front().map(|(_, p)| *p).unwrap_or(b_last);
        let c_first = chainlink.prices.front().map(|(_, p)| *p).unwrap_or(c_last);
        let b_up = b_last > b_first;
        let c_up = c_last > c_first;
        if b_up == c_up {
            return Regime::Trending;
        }
    }

    Regime::Quiet
}
