#![allow(dead_code)]
//! Auto-redeemer — redeem winning positions within 60s of market resolution.
//!
//! MISSING FROM P1-P5: Winning shares do NOT auto-convert to USDC.
//! Without redeeming, capital sits locked in resolved-but-unredeemed shares.
//!
//! On every `MarketResolved` event:
//!   1. Check if we hold a position in this market.
//!   2. If outcome matches our direction: redeem shares for USDC.
//!   3. Log the redemption amount and update P&L.
//!
//! Redemption window: up to 60 seconds after resolution event.
//! After that, shares can still be redeemed manually but auto-redeem gives up.

use crate::markets::state::{MarketEvent, MarketState};
use crate::trader::executor::OpenPosition;
use crate::dashboard::tui::SharedDashState;
use anyhow::Result;
use rust_decimal::Decimal;
use tokio::sync::broadcast;
use tokio::time::{Duration, timeout};
use tracing::{error, info, warn};

// ── Redeemer task ─────────────────────────────────────────────────────────────

pub struct RedeemerDeps {
    pub market_state: MarketState,
    pub event_rx: broadcast::Receiver<MarketEvent>,
    pub clob_url: String,
    pub private_key: String,
    pub dash_state: SharedDashState,
}

/// Entry point — spawn via `tokio::spawn(trader::redeemer::run(deps))`.
///
/// Listens for `MarketResolved` events and auto-redeems winning shares.
/// Positions are passed in as a shared structure by the trader task.
/// This simplified version operates on positions provided via the event stream.
pub async fn run(
    mut event_rx: broadcast::Receiver<MarketEvent>,
    positions: std::sync::Arc<dashmap::DashMap<String, OpenPosition>>,
    clob_url: String,
    private_key: String,
    dash_state: SharedDashState,
) {
    info!("Auto-redeemer started");

    loop {
        match event_rx.recv().await {
            Ok(MarketEvent::MarketResolved { condition_id, outcome }) => {
                if let Some((_id, pos)) = positions.remove(&condition_id) {
                    let won = pos.direction == outcome;
                    if won {
                        // Redeem with 60s timeout.
                        match timeout(
                            Duration::from_secs(60),
                            redeem_position(&pos, &clob_url, &private_key),
                        )
                        .await
                        {
                            Ok(Ok(redeemed_usd)) => {
                                info!(
                                    condition_id = %condition_id,
                                    redeemed_usd = %redeemed_usd,
                                    "Auto-redeemer: position redeemed"
                                );
                                if let Ok(mut dash) = dash_state.lock() {
                                    dash.total_pnl += redeemed_usd - pos.size_usd;
                                }
                            }
                            Ok(Err(e)) => {
                                error!(
                                    condition_id = %condition_id,
                                    "Auto-redeemer: redemption failed: {e}"
                                );
                            }
                            Err(_) => {
                                warn!(
                                    condition_id = %condition_id,
                                    "Auto-redeemer: redemption timed out after 60s"
                                );
                            }
                        }
                    } else {
                        info!(
                            condition_id = %condition_id,
                            direction = %pos.direction,
                            outcome = %outcome,
                            "Auto-redeemer: position lost — no redemption"
                        );
                        if let Ok(mut dash) = dash_state.lock() {
                            dash.total_pnl -= pos.size_usd;
                        }
                    }
                }
            }

            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("Auto-redeemer: event channel lagged by {n} messages");
            }

            Err(broadcast::error::RecvError::Closed) => {
                info!("Auto-redeemer: event channel closed — stopping");
                return;
            }

            Ok(_) => {} // Other events — ignore.
        }
    }
}

// ── Redemption logic ──────────────────────────────────────────────────────────

/// Redeem winning shares for USDC. Returns the USDC amount received.
///
/// Paper stub — real implementation requires CLOB SDK call.
async fn redeem_position(pos: &OpenPosition, _clob_url: &str, _private_key: &str) -> Result<Decimal> {
    // TODO(P6): actual SDK redemption:
    //   let signer = LocalSigner::from_str(private_key)?;
    //   let client = ClobClient::new(clob_url, signer).authenticate().await?;
    //   let shares = pos.size_usd / pos.entry_price;  // shares held
    //   client.redeem_position(pos.condition_id.clone(), shares).await?;
    //   return Ok(shares); // $1 per winning share = shares USDC

    // Paper: payout = shares * $1 per winning share.
    // shares = size_usd / entry_price (we paid entry_price per share).
    let shares = pos.size_usd / pos.entry_price;
    let payout = shares; // $1 per winning share

    info!(
        condition_id = %pos.condition_id,
        direction = %pos.direction,
        shares = %shares,
        payout_usd = %payout,
        "Auto-redeemer: redeem_position (paper stub)"
    );

    Ok(payout)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    #[test]
    fn payout_calculation() {
        // $5 bet at 0.45 YES price → 11.11 shares → $11.11 payout.
        let size = dec!(5.0);
        let entry = dec!(0.45);
        let shares = size / entry;
        let payout = shares;
        // sanity: payout > size (we win because YES resolved)
        assert!(payout > size);
    }
}
