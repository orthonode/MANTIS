#![allow(dead_code)]
//! Order executor — place and cancel orders via polymarket-client-sdk.
//!
//! Before first trade:
//!   Run `cargo run --example approvals` ONCE per wallet to set USDC allowance.
//!   See examples/approvals.rs in the polymarket-client-sdk source.
//!   Without this every order fails with "insufficient allowance".
//!
//! Token IDs are always fetched dynamically from MarketState — never hardcoded.
//! (Section 11 error: hardcoded IDs change when Polymarket refreshes markets.)

use crate::trader::risk::ApprovedOrder;
use crate::markets::state::MarketState;
use anyhow::{Context, Result};
use rust_decimal::prelude::ToPrimitive;
use tracing::{error, info, warn};

// ── Position record ────────────────────────────────────────────────────────────

/// A position that has been placed and is being tracked.
#[derive(Debug, Clone)]
pub struct OpenPosition {
    pub condition_id: String,
    pub direction: String,
    /// Price paid (yes_price at entry).
    pub entry_price: rust_decimal::Decimal,
    /// USD size of the bet.
    pub size_usd: rust_decimal::Decimal,
    /// Polymarket order ID returned by the CLOB.
    pub order_id: String,
    pub opened_at: chrono::DateTime<chrono::Utc>,
}

// ── Executor ──────────────────────────────────────────────────────────────────

/// Place an approved order via the Polymarket CLOB.
///
/// Looks up the token_id from `MarketState`, then submits a GTC limit order
/// at `yes_price`. The polymarket-client-sdk handles signing via LocalSigner.
///
/// Returns the order ID on success.
///
/// IMPORTANT: Call `run_approvals()` once per wallet before any trades.
pub async fn place_order(
    order: &ApprovedOrder,
    state: &MarketState,
    clob_url: &str,
    private_key: &str,
) -> Result<OpenPosition> {
    // Look up the token_id for the direction we want.
    let snap = state
        .markets
        .get(&order.condition_id)
        .with_context(|| format!("Market {} not in state", order.condition_id))?;

    let token_id = match order.direction.as_str() {
        "YES" => snap.token_id_yes.clone(),
        "NO" => snap.token_id_no.clone(),
        other => anyhow::bail!("Invalid direction: {other}"),
    };
    drop(snap); // release DashMap read guard

    let size_f64 = order
        .bet_size_usd
        .to_f64()
        .context("Failed to convert bet size to f64")?;
    let price_f64 = order
        .yes_price
        .to_f64()
        .context("Failed to convert yes_price to f64")?;

    info!(
        condition_id = %order.condition_id,
        direction = %order.direction,
        token_id = %token_id,
        size_usd = %order.bet_size_usd,
        price = %order.yes_price,
        clob_url = %clob_url,
        "Placing order"
    );

    // TODO(P4): replace with actual polymarket-client-sdk call once CLOB
    // authentication is wired. Pattern from examples/clob/authenticated.rs:
    //
    //   let signer = LocalSigner::from_str(private_key)?;
    //   let client = ClobClient::new(clob_url, signer).authenticate().await?;
    //   let order_args = OrderArgs { token_id, price: price_f64, size: size_f64, side: Side::Buy };
    //   let signed = client.create_order(order_args, None).await?;
    //   let resp = client.post_order(signed, OrderType::Gtc).await?;
    //   let order_id = resp.order_id;

    let _ = (size_f64, price_f64, private_key); // suppress unused until wired

    // Paper trade stub — generates a placeholder order_id for P4 testing.
    let order_id = format!("PAPER-{}-{}", &order.condition_id[..8.min(order.condition_id.len())], chrono::Utc::now().timestamp());

    info!(
        order_id = %order_id,
        "Order placed (paper trade)"
    );

    Ok(OpenPosition {
        condition_id: order.condition_id.clone(),
        direction: order.direction.clone(),
        entry_price: order.yes_price,
        size_usd: order.bet_size_usd,
        order_id,
        opened_at: chrono::Utc::now(),
    })
}

/// Cancel a single open order by order_id.
pub async fn cancel_order(
    order_id: &str,
    clob_url: &str,
    private_key: &str,
) -> Result<()> {
    info!(order_id = %order_id, "Cancelling order");

    // TODO(P4): actual SDK call:
    //   let client = ClobClient::new(clob_url, signer).authenticate().await?;
    //   client.cancel(CancelOrderArgs { order_id: order_id.to_string() }).await?;

    let _ = (clob_url, private_key);
    warn!(order_id = %order_id, "cancel_order: paper trade stub — no real cancellation");
    Ok(())
}

/// Cancel ALL open orders. Called on graceful shutdown (Ctrl-C).
/// Iterates positions and cancels each; logs errors but does not abort.
pub async fn cancel_all_orders(
    positions: &[OpenPosition],
    clob_url: &str,
    private_key: &str,
) {
    if positions.is_empty() {
        info!("cancel_all_orders: no open positions");
        return;
    }
    info!("cancel_all_orders: cancelling {} positions", positions.len());
    for pos in positions {
        if let Err(e) = cancel_order(&pos.order_id, clob_url, private_key).await {
            error!(order_id = %pos.order_id, "Failed to cancel: {e}");
        }
    }
    info!("cancel_all_orders: done");
}
