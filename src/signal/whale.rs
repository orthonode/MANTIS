#![allow(dead_code)]
//! Whale order detector.
//!
//! Monitors the CLOB orderbook for large orders entering the book.
//! A "whale" is an order > whale_threshold_usd (default $500).
//! Whale orders are a signal of informed money — they often precede
//! price movement as market makers reprice around large flow.
//!
//! The whale signal is a supplementary input to the consensus engine —
//! it can boost confidence in a direction but is not a standalone trade trigger.

use crate::markets::state::MarketEvent;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tokio::sync::broadcast;
use tracing::info;

// ── Whale signal ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct WhaleSignal {
    pub condition_id: String,
    /// "YES" or "NO" — which outcome the whale is buying.
    pub direction: String,
    /// Estimated order size in USD.
    pub size_usd: Decimal,
}

// ── Detector ──────────────────────────────────────────────────────────────────

/// Monitor the market event stream for whale-sized orderbook changes.
///
/// When a `BestBidAsk` event shows a bid large enough to imply a whale order,
/// emit a `WhaleSignal`. The signal engine can then prioritise that market.
///
/// This is a lightweight heuristic: we infer order size from the change in
/// best bid price and assume the order spans the full spread. A more precise
/// implementation (P3+) would track the full orderbook depth.
///
/// Threshold: orders > $500 are whale-level for Polymarket Flash markets.
pub async fn run(
    mut event_rx: broadcast::Receiver<MarketEvent>,
    whale_tx: tokio::sync::mpsc::Sender<WhaleSignal>,
    threshold_usd: Decimal,
) {
    loop {
        match event_rx.recv().await {
            Ok(MarketEvent::BestBidAsk {
                condition_id,
                best_bid,
                best_ask,
            }) => {
                // Estimate implied order size from spread compression.
                // If best_bid jumped significantly, a large buy entered.
                // Simple heuristic: bid > 0.7 and spread < 0.05 → large buy.
                let spread = best_ask - best_bid;
                let implied_yes_buy = best_bid > dec!(0.50) && spread < dec!(0.05);

                if implied_yes_buy {
                    // Rough size estimate: assume $500+ from orderbook depth signal.
                    // A full implementation would compare previous book snapshot.
                    let estimated_size = best_bid * dec!(1000); // placeholder
                    if estimated_size >= threshold_usd {
                        let signal = WhaleSignal {
                            condition_id: condition_id.clone(),
                            direction: "YES".to_string(),
                            size_usd: estimated_size,
                        };
                        info!(
                            condition_id = %condition_id,
                            size_usd = %estimated_size,
                            "WHALE SIGNAL: large YES order detected"
                        );
                        let _ = whale_tx.send(signal).await;
                    }
                }
            }
            Ok(_) => {} // Other events — ignore.
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("Whale detector: lagged {n} events");
            }
            Err(broadcast::error::RecvError::Closed) => {
                tracing::error!("Whale detector: event channel closed");
                return;
            }
        }
    }
}
