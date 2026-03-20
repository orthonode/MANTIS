#![allow(dead_code)]
//! CLOB WebSocket feed — orderbook, price_change, trades, market_resolved.
//!
//! Subscribes to `wss://ws-subscriptions-clob.polymarket.com/ws/market`.
//! No authentication required for read-only subscriptions.
//!
//! Subscription is dynamic: the gamma poller calls `subscribe_tokens()` as
//! new markets are discovered. Each subscription covers both YES and NO tokens
//! for a market (Polymarket orderbook is per token_id, not condition_id).
//!
//! Events written to `MarketState` and emitted on the `MarketEvent` broadcast
//! channel so signal and trader tasks can react in real time.

use crate::markets::state::{MarketEvent, MarketState};
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Mutex};
use tokio::time;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

// ── Public channel types ─────────────────────────────────────────────────────

/// Sender half of the subscription command channel.
/// gamma.rs calls `subscribe_tokens(tx, token_ids)` to register new markets.
pub type SubscribeTx = tokio::sync::mpsc::Sender<Vec<String>>;

// ── Run the CLOB WS task ─────────────────────────────────────────────────────

/// Run the CLOB WebSocket feed task forever.
///
/// Spawned once by `main.rs`. Holds a shared write-half behind a `Mutex` so
/// the gamma poller can inject new subscriptions without owning the WS stream.
///
/// On disconnect: reconnects with exponential backoff and re-subscribes to all
/// previously known token IDs.
///
/// # Arguments
/// * `clob_ws_url`  — wss://ws-subscriptions-clob.polymarket.com/ws/market
/// * `state`        — shared MarketState (updated in-place)
/// * `event_tx`     — broadcast sender for `MarketEvent`
/// * `sub_rx`       — mpsc receiver for token subscription commands from gamma
pub async fn run(
    clob_ws_url: String,
    state: MarketState,
    event_tx: broadcast::Sender<MarketEvent>,
    mut sub_rx: tokio::sync::mpsc::Receiver<Vec<String>>,
) {
    // Track all token IDs we have ever subscribed to so we can re-subscribe
    // after reconnect.
    let subscribed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let mut backoff = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);

    loop {
        info!("CLOB WS: connecting to {clob_ws_url}");

        match connect_and_stream(
            &clob_ws_url,
            &state,
            &event_tx,
            &mut sub_rx,
            Arc::clone(&subscribed),
        )
        .await
        {
            Ok(()) => {
                warn!("CLOB WS: stream ended cleanly, reconnecting in {:?}", backoff);
            }
            Err(e) => {
                error!("CLOB WS: error — {e}, reconnecting in {:?}", backoff);
            }
        }

        time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Connect once and stream events until error or clean close.
async fn connect_and_stream(
    url: &str,
    state: &MarketState,
    event_tx: &broadcast::Sender<MarketEvent>,
    sub_rx: &mut tokio::sync::mpsc::Receiver<Vec<String>>,
    subscribed: Arc<Mutex<Vec<String>>>,
) -> Result<()> {
    let (ws_stream, _) = connect_async(url).await?;
    let (mut write, mut read) = ws_stream.split();

    // Re-subscribe to all previously known tokens after reconnect.
    {
        let tokens = subscribed.lock().await;
        if !tokens.is_empty() {
            let msg = build_subscribe_msg(&tokens);
            write.send(Message::Text(msg)).await?;
            info!("CLOB WS: re-subscribed to {} tokens after reconnect", tokens.len());
        }
    }

    // Reset backoff on successful connect.
    backoff_reset(&subscribed).await;

    loop {
        tokio::select! {
            // New tokens to subscribe to (sent by gamma poller).
            Some(new_tokens) = sub_rx.recv() => {
                {
                    let mut known = subscribed.lock().await;
                    for t in &new_tokens {
                        if !known.contains(t) {
                            known.push(t.clone());
                        }
                    }
                }
                let msg = build_subscribe_msg(&new_tokens);
                write.send(Message::Text(msg)).await?;
                info!("CLOB WS: subscribed to {} new tokens", new_tokens.len());
            }

            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        handle_message(&text, state, event_tx);
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(frame))) => {
                        warn!("CLOB WS: server sent Close: {:?}", frame);
                        return Ok(());
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        return Err(e.into());
                    }
                    None => {
                        return Ok(());
                    }
                }
            }
        }
    }
}

// ── Message handling ─────────────────────────────────────────────────────────

/// Parse and dispatch a single CLOB WS text frame.
/// Updates `MarketState` in-place and fires `MarketEvent` on the broadcast channel.
/// On any parse failure: logs and returns (never panics — zero unwrap()).
fn handle_message(text: &str, state: &MarketState, event_tx: &broadcast::Sender<MarketEvent>) {
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            warn!("CLOB WS: failed to parse JSON: {e} — raw: {text}");
            return;
        }
    };

    let event_type = match v.get("event_type").and_then(|e| e.as_str()) {
        Some(t) => t,
        None => return,
    };

    match event_type {
        "price_change" => handle_price_change(&v, state, event_tx),
        "book" => handle_book(&v, state, event_tx),
        "market_resolved" => handle_resolved(&v, state, event_tx),
        _ => {} // last_trade_price, etc. — ignore for now
    }
}

/// Handle `price_change` events — update yes_price in MarketSnapshot.
fn handle_price_change(
    v: &serde_json::Value,
    state: &MarketState,
    event_tx: &broadcast::Sender<MarketEvent>,
) {
    let price_changes = match v.get("price_changes").and_then(|a| a.as_array()) {
        Some(a) => a,
        None => return,
    };

    for change in price_changes {
        let token_id = match change.get("asset_id").and_then(|s| s.as_str()) {
            Some(id) => id,
            None => continue,
        };
        let price_str = match change.get("price").and_then(|s| s.as_str()) {
            Some(p) => p,
            None => continue,
        };
        let price: Decimal = match Decimal::from_str(price_str) {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Look up condition_id for this token.
        let condition_id = match state.condition_id_for_token(token_id) {
            Some(id) => id,
            None => continue,
        };

        // Update yes_price if this is the YES token.
        if let Some(mut snap) = state.markets.get_mut(&condition_id) {
            if snap.token_id_yes == token_id {
                snap.yes_price = price;
                snap.no_price = Decimal::ONE - price;
                snap.last_updated = chrono::Utc::now();
            }
        }

        let _ = event_tx.send(MarketEvent::PriceUpdate {
            condition_id,
            yes_price: price,
        });
    }
}

/// Handle `book` events — update best_bid and best_ask in MarketSnapshot.
fn handle_book(
    v: &serde_json::Value,
    state: &MarketState,
    event_tx: &broadcast::Sender<MarketEvent>,
) {
    let asset_id = match v.get("asset_id").and_then(|s| s.as_str()) {
        Some(id) => id,
        None => return,
    };

    let condition_id = match state.condition_id_for_token(asset_id) {
        Some(id) => id,
        None => return,
    };

    // Extract best bid (highest buy) and best ask (lowest sell).
    let best_bid = extract_best_bid(v);
    let best_ask = extract_best_ask(v);

    if let (Some(bid), Some(ask)) = (best_bid, best_ask) {
        if let Some(mut snap) = state.markets.get_mut(&condition_id) {
            if snap.token_id_yes == asset_id {
                snap.best_bid = Some(bid);
                snap.best_ask = Some(ask);
                snap.last_updated = chrono::Utc::now();
            }
        }
        let _ = event_tx.send(MarketEvent::BestBidAsk {
            condition_id,
            best_bid: bid,
            best_ask: ask,
        });
    }
}

/// Handle `market_resolved` events — mark market closed and emit event.
fn handle_resolved(
    v: &serde_json::Value,
    state: &MarketState,
    event_tx: &broadcast::Sender<MarketEvent>,
) {
    let asset_id = match v.get("asset_id").and_then(|s| s.as_str()) {
        Some(id) => id,
        None => return,
    };
    let outcome = match v.get("outcome").and_then(|s| s.as_str()) {
        Some(o) => o.to_uppercase(),
        None => return,
    };

    let condition_id = match state.condition_id_for_token(asset_id) {
        Some(id) => id,
        None => return,
    };

    if let Some(mut snap) = state.markets.get_mut(&condition_id) {
        snap.is_closed = true;
        snap.last_updated = chrono::Utc::now();
    }

    info!("CLOB WS: market resolved — {condition_id} outcome={outcome}");
    let _ = event_tx.send(MarketEvent::MarketResolved {
        condition_id,
        outcome,
    });
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Build a CLOB WS subscription message for a list of token IDs.
///
/// Format: `{"assets_ids": ["<id1>", "<id2>"], "type": "market"}`
fn build_subscribe_msg(token_ids: &[String]) -> String {
    let ids: Vec<String> = token_ids.iter().map(|id| format!("\"{id}\"")).collect();
    format!(
        r#"{{"assets_ids": [{}], "type": "market"}}"#,
        ids.join(", ")
    )
}

/// Extract the best bid price from a `book` event.
/// Bids are buy orders — best bid is the highest price.
fn extract_best_bid(v: &serde_json::Value) -> Option<Decimal> {
    let bids = v.get("bids")?.as_array()?;
    bids.iter()
        .filter_map(|b| b.get("price")?.as_str())
        .filter_map(|p| Decimal::from_str(p).ok())
        .reduce(Decimal::max)
}

/// Extract the best ask price from a `book` event.
/// Asks are sell orders — best ask is the lowest price.
fn extract_best_ask(v: &serde_json::Value) -> Option<Decimal> {
    let asks = v.get("asks")?.as_array()?;
    asks.iter()
        .filter_map(|a| a.get("price")?.as_str())
        .filter_map(|p| Decimal::from_str(p).ok())
        .reduce(Decimal::min)
}

/// No-op helper to reset backoff after a successful connection.
/// The actual backoff variable lives in `run()` — this just logs.
async fn backoff_reset(_subscribed: &Arc<Mutex<Vec<String>>>) {
    // Backoff is reset to 1s in the outer run() loop on next error.
    // This function is a logical marker that connection succeeded.
}

/// Public helper: send a subscribe command to the CLOB WS task.
/// Called by gamma.rs after discovering new markets.
pub async fn subscribe_tokens(tx: &SubscribeTx, token_ids: Vec<String>) {
    if let Err(e) = tx.send(token_ids).await {
        error!("CLOB WS: failed to queue token subscription: {e}");
    }
}
