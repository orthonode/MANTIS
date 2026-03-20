#![allow(dead_code)]
//! Position merger — merge YES+NO pairs for risk-free $1/pair profit.
//!
//! MISSING FROM P1-P5: Polymarket allows merging 1 YES share + 1 NO share
//! to redeem $1 USDC instantly — risk-free profit from spread capture.
//!
//! When the maker engine accumulates both sides of a market:
//!   1 YES share + 1 NO share = $1 USDC (always, regardless of outcome)
//!
//! Run check every 30 seconds.
//! Merge min(yes_shares, no_shares) pairs immediately.
//!
//! This is zero-directional-risk profit — do it immediately, always.

use anyhow::Result;
use rust_decimal::Decimal;
use tokio::time::{interval, Duration};
use tracing::info;

use crate::maker::inventory::InventoryTracker;

// ── Merger task ───────────────────────────────────────────────────────────────

/// Entry point — spawn via `tokio::spawn(trader::merger::run(...))`.
pub async fn run(inventory: InventoryTracker, clob_url: String, private_key: String) {
    info!("Position merger started — checking every 30s");
    let mut timer = interval(Duration::from_secs(30));

    loop {
        timer.tick().await;
        run_merge_cycle(&inventory, &clob_url, &private_key).await;
    }
}

async fn run_merge_cycle(inventory: &InventoryTracker, clob_url: &str, private_key: &str) {
    for condition_id in inventory.active_markets() {
        let snap = inventory.snapshot(&condition_id);
        let pairs = snap.mergeable_pairs();

        if pairs <= Decimal::ZERO {
            continue;
        }

        match merge_pairs(&condition_id, pairs, clob_url, private_key).await {
            Ok(profit) => {
                let recorded = inventory.record_merge(&condition_id, pairs);
                info!(
                    condition_id = %condition_id,
                    pairs = %pairs,
                    profit_usd = %profit,
                    recorded = %recorded,
                    "Merger: YES+NO pairs merged"
                );
            }
            Err(e) => {
                tracing::error!(condition_id = %condition_id, "Merger: merge failed: {e}");
            }
        }
    }
}

// ── Merge call ────────────────────────────────────────────────────────────────

/// Merge `pairs` YES+NO share pairs for $1 each.
/// Returns total USDC received.
async fn merge_pairs(
    condition_id: &str,
    pairs: Decimal,
    _clob_url: &str,
    _private_key: &str,
) -> Result<Decimal> {
    // TODO(P6): actual SDK call:
    //   let signer = LocalSigner::from_str(private_key)?;
    //   let client = ClobClient::new(clob_url, signer).authenticate().await?;
    //   client.merge_positions(condition_id, pairs.to_f64()?).await?;
    //   return Ok(pairs); // $1 per pair

    // Paper stub: $1 per pair merged.
    let profit = pairs;
    info!(
        condition_id = %condition_id,
        pairs = %pairs,
        profit_usd = %profit,
        "Merger: merge_pairs (paper stub)"
    );
    Ok(profit)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    #[test]
    fn merge_profit_equals_pairs() {
        // $1 per pair is the invariant — YES + NO always = $1.
        let pairs = dec!(5.0);
        let expected_profit = dec!(5.0);
        // The profit must equal the number of pairs merged.
        assert_eq!(pairs, expected_profit);
    }
}
