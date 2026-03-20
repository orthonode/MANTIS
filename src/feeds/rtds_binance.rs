#![allow(dead_code)]
//! RTDS WebSocket feed — Binance BTC/USD price tick.
//!
//! Connects to Polymarket's RTDS endpoint and subscribes to the Binance
//! BTC/USDT crypto price topic. Publishes ticks to a `broadcast::Sender<BinanceTick>`.
//!
//! CRITICAL (Section 11 error):
//!   ONE symbol per RTDS connection. This task handles ONLY Binance BTC/USDT.
//!   Chainlink runs in a SEPARATE tokio::spawn with its own WS connection.
//!
//! CRITICAL (Section 11 error):
//!   Server closes connection after ~30s of silence.
//!   This task sends a PING frame every `ping_interval_secs` seconds.
//!   On disconnect: reconnects with exponential backoff (1s → 2s → 4s → max 30s).

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

/// A single price tick received from the Binance RTDS feed.
#[derive(Debug, Clone)]
pub struct BinanceTick {
    /// BTC/USDT price in USD. Uses f64 here only for raw feed parsing —
    /// converted to rust_decimal::Decimal before any arithmetic.
    pub price: f64,
    /// Server-side timestamp as Unix millis.
    pub timestamp_ms: u64,
}

/// Subscribe message sent to the RTDS WebSocket for Binance BTC.
const SUBSCRIBE_MSG: &str = r#"{"action":"subscribe","subscriptions":[{"topic":"crypto_prices","type":"update","filters":"btcusdt"}]}"#;

/// Run the Binance RTDS feed task forever.
///
/// Spawned once by `main.rs`. Never returns unless the process exits.
/// On any error (WS close, parse failure, network drop): logs, waits, reconnects.
///
/// # Arguments
/// * `rtds_url`           — wss://ws-live-data.polymarket.com
/// * `ping_interval_secs` — seconds between PING frames (config: 5)
/// * `tx`                 — broadcast sender; receivers live in signal task
pub async fn run(
    rtds_url: String,
    ping_interval_secs: u64,
    tx: broadcast::Sender<BinanceTick>,
) {
    let mut backoff = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);

    loop {
        info!("RTDS Binance: connecting to {rtds_url}");

        match connect_and_stream(&rtds_url, ping_interval_secs, &tx).await {
            Ok(()) => {
                // Clean disconnect — treat as transient, reconnect immediately.
                warn!("RTDS Binance: stream ended cleanly, reconnecting in {:?}", backoff);
            }
            Err(e) => {
                error!("RTDS Binance: error — {e}, reconnecting in {:?}", backoff);
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
    tx: &broadcast::Sender<BinanceTick>,
) -> Result<()> {
    let (ws_stream, _) = connect_async(url).await?;
    let (mut write, mut read) = ws_stream.split();

    // Send subscribe message immediately after connect.
    write.send(Message::Text(SUBSCRIBE_MSG.to_string())).await?;
    info!("RTDS Binance: subscribed to btcusdt");

    // Reset backoff on successful connect.
    let mut ping_interval = time::interval(Duration::from_secs(ping_interval_secs));

    loop {
        tokio::select! {
            // PING every 5s — server drops connection without it (Section 11).
            _ = ping_interval.tick() => {
                write.send(Message::Ping(vec![])).await?;
            }

            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(tick) = parse_tick(&text) {
                            // Ignore send errors — receivers may lag or be absent.
                            let _ = tx.send(tick);
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {
                        // Server pong — connection alive.
                    }
                    Some(Ok(Message::Close(frame))) => {
                        warn!("RTDS Binance: server sent Close: {:?}", frame);
                        return Ok(());
                    }
                    Some(Ok(_)) => {
                        // Binary, Ping, etc. — ignore.
                    }
                    Some(Err(e)) => {
                        return Err(e.into());
                    }
                    None => {
                        // Stream exhausted.
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Parse a raw RTDS text frame into a `BinanceTick`.
///
/// RTDS Binance price frame example:
/// ```json
/// {"event":"update","topic":"crypto_prices","data":{"asset":"btcusdt","price":"67423.50","t":1710000000000}}
/// ```
/// Returns `None` for non-price frames (ack, subscription confirm, etc.).
fn parse_tick(text: &str) -> Option<BinanceTick> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;

    // Only process update events with the crypto_prices topic.
    if v.get("event")?.as_str()? != "update" {
        return None;
    }
    if v.get("topic")?.as_str()? != "crypto_prices" {
        return None;
    }

    let data = v.get("data")?;
    let price_str = data.get("price")?.as_str()?;
    let price: f64 = price_str.parse().ok()?;
    let timestamp_ms = data.get("t")?.as_u64().unwrap_or(0);

    Some(BinanceTick { price, timestamp_ms })
}
