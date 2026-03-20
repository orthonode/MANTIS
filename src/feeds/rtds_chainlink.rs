#![allow(dead_code)]
//! RTDS WebSocket feed — Chainlink BTC/USD oracle price.
//!
//! CRITICAL: This is a SEPARATE WS connection from rtds_binance.rs.
//! A second subscribe on the same connection would silently replace the first
//! (Section 11 error). This task is spawned independently in main.rs.
//!
//! Subscribes to the `crypto_prices_chainlink` topic for `btc/usd`.
//! Polymarket's CLOB resolves Flash markets against this feed — it is the
//! settlement oracle and the direction signal for Flash trades.
//!
//! PING every `ping_interval_secs` seconds — same requirement as Binance.
//! Auto-reconnect with exponential backoff on any failure.

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

/// A single price tick from the Chainlink BTC/USD Data Streams feed.
#[derive(Debug, Clone)]
pub struct ChainlinkTick {
    /// BTC/USD price in USD from Chainlink oracle.
    /// Parsed as f64 from raw feed — convert to Decimal before arithmetic.
    pub price: f64,
    /// Server-side timestamp as Unix millis.
    pub timestamp_ms: u64,
}

/// Subscribe message for the Chainlink BTC/USD feed.
/// NOTE: topic is "crypto_prices_chainlink", filter is "btc/usd" (not "btcusdt").
const SUBSCRIBE_MSG: &str = r#"{"action":"subscribe","subscriptions":[{"topic":"crypto_prices_chainlink","type":"update","filters":"btc/usd"}]}"#;

/// Run the Chainlink RTDS feed task forever.
///
/// Spawned independently from `rtds_binance::run` — NEVER on the same WS handle.
/// On any error: logs, waits with exponential backoff, reconnects.
///
/// # Arguments
/// * `rtds_url`           — wss://ws-live-data.polymarket.com (same host, different subscription)
/// * `ping_interval_secs` — seconds between PING frames (config: 5)
/// * `tx`                 — broadcast sender; signal/flash task is the receiver
pub async fn run(
    rtds_url: String,
    ping_interval_secs: u64,
    tx: broadcast::Sender<ChainlinkTick>,
) {
    let mut backoff = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);

    loop {
        info!("RTDS Chainlink: connecting to {rtds_url}");

        match connect_and_stream(&rtds_url, ping_interval_secs, &tx).await {
            Ok(()) => {
                warn!("RTDS Chainlink: stream ended cleanly, reconnecting in {:?}", backoff);
            }
            Err(e) => {
                error!("RTDS Chainlink: error — {e}, reconnecting in {:?}", backoff);
            }
        }

        time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Connect once, subscribe, stream ticks until error or clean close.
async fn connect_and_stream(
    url: &str,
    ping_interval_secs: u64,
    tx: &broadcast::Sender<ChainlinkTick>,
) -> Result<()> {
    let (ws_stream, _) = connect_async(url).await?;
    let (mut write, mut read) = ws_stream.split();

    write.send(Message::Text(SUBSCRIBE_MSG.to_string())).await?;
    info!("RTDS Chainlink: subscribed to btc/usd");

    let mut ping_interval = time::interval(Duration::from_secs(ping_interval_secs));

    loop {
        tokio::select! {
            _ = ping_interval.tick() => {
                write.send(Message::Ping(vec![])).await?;
            }

            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(tick) = parse_tick(&text) {
                            let _ = tx.send(tick);
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(frame))) => {
                        warn!("RTDS Chainlink: server sent Close: {:?}", frame);
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

/// Parse a raw RTDS text frame into a `ChainlinkTick`.
///
/// Chainlink price frame example:
/// ```json
/// {"event":"update","topic":"crypto_prices_chainlink","data":{"asset":"btc/usd","price":"67410.00","t":1710000000000}}
/// ```
fn parse_tick(text: &str) -> Option<ChainlinkTick> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;

    if v.get("event")?.as_str()? != "update" {
        return None;
    }
    if v.get("topic")?.as_str()? != "crypto_prices_chainlink" {
        return None;
    }

    let data = v.get("data")?;
    let price_str = data.get("price")?.as_str()?;
    let price: f64 = price_str.parse().ok()?;
    let timestamp_ms = data.get("t")?.as_u64().unwrap_or(0);

    Some(ChainlinkTick { price, timestamp_ms })
}
