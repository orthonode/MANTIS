#![allow(dead_code)]
//! Continuous drawdown scaling.
//!
//! Replaces the binary "halt at -$15" rule with a smooth multiplier that
//! shrinks bet sizes proportionally as the portfolio falls from its peak.
//!
//! Drawdown is measured from the all-time high-water mark (peak bankroll),
//! not from today's open. This is a stricter and more accurate measure.
//!
//! Multiplier schedule:
//!   drawdown < 5%:  1.00 — no change, business as usual
//!   drawdown < 10%: 0.75 — quietly shrink
//!   drawdown < 15%: 0.50 — survival mode
//!   drawdown < 20%: 0.25 — minimal trades only
//!   drawdown >= 20%: 0.00 — DRAWDOWN_HALT: log + refuse all orders until
//!                           manually resumed (set resume flag via monitor)
//!
//! The multiplier is fed directly into `KellyMultipliers.drawdown` so that
//! every order automatically reflects the current capital health.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

// ── DrawdownTracker ───────────────────────────────────────────────────────────

/// Shared drawdown state. Wrapped in `Arc<Mutex>` so the trader task can
/// update it after each trade settlement while the Kelly sizer reads it.
///
/// Clone the `Arc` to share across tasks — do not clone the inner struct.
#[derive(Debug, Clone)]
pub struct DrawdownTracker(Arc<Mutex<DrawdownState>>);

#[derive(Debug)]
struct DrawdownState {
    /// All-time high-water mark for bankroll.
    peak_bankroll: Decimal,
    /// Current bankroll.
    current_bankroll: Decimal,
    /// True when drawdown has hit >= 20% and trading is halted.
    /// Cleared only by manual resume (operator action).
    halt_active: bool,
}

impl DrawdownTracker {
    pub fn new(initial_bankroll: Decimal) -> Self {
        Self(Arc::new(Mutex::new(DrawdownState {
            peak_bankroll: initial_bankroll,
            current_bankroll: initial_bankroll,
            halt_active: false,
        })))
    }

    /// Update current bankroll after a trade settles.
    /// Automatically updates the peak watermark if bankroll has grown.
    pub fn update(&self, current_bankroll: Decimal) {
        let mut state = self.0.lock().unwrap();
        state.current_bankroll = current_bankroll;
        if current_bankroll > state.peak_bankroll {
            state.peak_bankroll = current_bankroll;
            info!(
                peak = %current_bankroll,
                "Drawdown: new all-time high watermark"
            );
        }
        let dd = drawdown_pct(state.peak_bankroll, state.current_bankroll);
        if dd >= dec!(0.20) && !state.halt_active {
            state.halt_active = true;
            warn!(
                drawdown_pct = %dd,
                peak = %state.peak_bankroll,
                current = %state.current_bankroll,
                "DRAWDOWN_HALT: drawdown >= 20%, all trading suspended. Manual resume required."
            );
        }
    }

    /// Return the Kelly drawdown multiplier for the current state.
    /// Returns `Decimal::ZERO` when halt is active — Kelly sizer returns `None`.
    pub fn multiplier(&self) -> Decimal {
        let state = self.0.lock().unwrap();
        if state.halt_active {
            return Decimal::ZERO;
        }
        let dd = drawdown_pct(state.peak_bankroll, state.current_bankroll);
        schedule(dd)
    }

    /// Current drawdown as a fraction (0.0 = no loss, 0.20 = 20% down).
    pub fn current_drawdown(&self) -> Decimal {
        let state = self.0.lock().unwrap();
        drawdown_pct(state.peak_bankroll, state.current_bankroll)
    }

    /// Whether a halt is currently active.
    pub fn is_halted(&self) -> bool {
        self.0.lock().unwrap().halt_active
    }

    /// Resume trading after operator review.
    /// Only call this when explicitly instructed — do NOT auto-resume.
    pub fn manual_resume(&self) {
        let mut state = self.0.lock().unwrap();
        if state.halt_active {
            state.halt_active = false;
            info!(
                drawdown_pct = %drawdown_pct(state.peak_bankroll, state.current_bankroll),
                "Drawdown: HALT lifted by manual resume"
            );
        }
    }

    /// Snapshot for the dashboard: (peak, current, drawdown_pct, halted).
    pub fn snapshot(&self) -> (Decimal, Decimal, Decimal, bool) {
        let state = self.0.lock().unwrap();
        let dd = drawdown_pct(state.peak_bankroll, state.current_bankroll);
        (state.peak_bankroll, state.current_bankroll, dd, state.halt_active)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Drawdown as a fraction: (peak - current) / peak.
/// Returns 0.0 if peak is zero (avoid division by zero on fresh init).
fn drawdown_pct(peak: Decimal, current: Decimal) -> Decimal {
    if peak == Decimal::ZERO {
        return Decimal::ZERO;
    }
    ((peak - current) / peak).max(Decimal::ZERO)
}

/// Map a drawdown fraction to the Kelly multiplier per the schedule above.
fn schedule(dd: Decimal) -> Decimal {
    if dd < dec!(0.05) {
        dec!(1.00)
    } else if dd < dec!(0.10) {
        dec!(0.75)
    } else if dd < dec!(0.15) {
        dec!(0.50)
    } else if dd < dec!(0.20) {
        dec!(0.25)
    } else {
        Decimal::ZERO // >= 20%: full halt (handled by halt_active flag above)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_no_drawdown_is_one() {
        let tracker = DrawdownTracker::new(dec!(100.0));
        assert_eq!(tracker.multiplier(), dec!(1.00));
    }

    #[test]
    fn test_small_drawdown_unchanged() {
        let tracker = DrawdownTracker::new(dec!(100.0));
        tracker.update(dec!(97.0)); // 3% drawdown
        assert_eq!(tracker.multiplier(), dec!(1.00));
    }

    #[test]
    fn test_7pct_drawdown_is_075() {
        let tracker = DrawdownTracker::new(dec!(100.0));
        tracker.update(dec!(93.0)); // 7% drawdown
        assert_eq!(tracker.multiplier(), dec!(0.75));
    }

    #[test]
    fn test_12pct_drawdown_is_050() {
        let tracker = DrawdownTracker::new(dec!(100.0));
        tracker.update(dec!(88.0)); // 12% drawdown
        assert_eq!(tracker.multiplier(), dec!(0.50));
    }

    #[test]
    fn test_17pct_drawdown_is_025() {
        let tracker = DrawdownTracker::new(dec!(100.0));
        tracker.update(dec!(83.0)); // 17% drawdown
        assert_eq!(tracker.multiplier(), dec!(0.25));
    }

    #[test]
    fn test_20pct_drawdown_halts() {
        let tracker = DrawdownTracker::new(dec!(100.0));
        tracker.update(dec!(80.0)); // exactly 20%
        assert!(tracker.is_halted());
        assert_eq!(tracker.multiplier(), Decimal::ZERO);
    }

    #[test]
    fn test_peak_watermark_updated_on_growth() {
        let tracker = DrawdownTracker::new(dec!(100.0));
        tracker.update(dec!(110.0)); // new high: peak = 110
        // (110-99)/110 = 10.0% exactly → hits the 10%-15% tier → 0.50
        tracker.update(dec!(99.0));
        assert_eq!(tracker.multiplier(), dec!(0.50));
    }

    #[test]
    fn test_manual_resume_clears_halt() {
        let tracker = DrawdownTracker::new(dec!(100.0));
        tracker.update(dec!(79.0)); // >20% drawdown
        assert!(tracker.is_halted());
        tracker.manual_resume();
        assert!(!tracker.is_halted());
    }
}
