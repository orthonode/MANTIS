#![allow(dead_code)]
//! Kalshi API client — fetch markets and orderbooks for cross-platform arb.
//!
//! Kalshi is a US-regulated prediction market. Same events appear on both
//! Polymarket and Kalshi with different prices — buying the cheaper side
//! on each platform locks in profit regardless of outcome.
//!
//! API: https://trading.kalshi.com/trade-api/v2
//! Auth: optional for market reads; required for order placement.
//!
//! IMPORTANT: Always start in alert-only mode (config.arb.enabled = false).
//! Validate 10 real opportunities manually before enabling auto-execution.

use anyhow::{Context, Result};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use tracing::warn;

// ── Kalshi market types ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct KalshiMarket {
    pub ticker: String,
    pub title: String,
    pub yes_price: Decimal,
    pub no_price: Decimal,
    pub volume: Decimal,
    pub close_time: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
struct KalshiMarketsResponse {
    markets: Vec<KalshiMarketRaw>,
}

#[derive(Debug, Deserialize)]
struct KalshiMarketRaw {
    ticker: Option<String>,
    title: Option<String>,
    yes_bid: Option<f64>,
    yes_ask: Option<f64>,
    volume: Option<f64>,
    close_time: Option<String>,
}

// ── Kalshi client ─────────────────────────────────────────────────────────────

const KALSHI_BASE: &str = "https://trading.kalshi.com/trade-api/v2";

/// Fetch active Kalshi markets matching a keyword.
///
/// Returns empty vec on any error (arb scanner treats this as no opportunities).
pub async fn fetch_markets(http: &Client, keyword: &str) -> Vec<KalshiMarket> {
    let url = format!(
        "{}/markets?status=active&limit=50&keyword={}",
        KALSHI_BASE, keyword
    );

    let raw = match http
        .get(&url)
        .header("User-Agent", "mantis-trading-agent/0.1.0")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("Kalshi: fetch_markets failed: {e}");
            return vec![];
        }
    };

    let body: KalshiMarketsResponse = match raw.json().await {
        Ok(b) => b,
        Err(e) => {
            warn!("Kalshi: parse error: {e}");
            return vec![];
        }
    };

    body.markets
        .into_iter()
        .filter_map(|m| {
            let ticker = m.ticker?;
            let title = m.title?;
            let yes_bid = m.yes_bid?;
            let yes_ask = m.yes_ask?;
            // Midpoint price.
            let yes_price = Decimal::try_from((yes_bid + yes_ask) / 2.0).ok()?;
            let no_price = Decimal::ONE - yes_price;
            let volume = Decimal::try_from(m.volume.unwrap_or(0.0)).ok()?;
            let close_time = m
                .close_time
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or_else(chrono::Utc::now);

            Some(KalshiMarket {
                ticker,
                title,
                yes_price,
                no_price,
                volume,
                close_time,
            })
        })
        .collect()
}

/// Session token for authenticated Kalshi requests (order placement).
pub struct KalshiSession {
    pub token: String,
}

/// Authenticate with Kalshi and return a session token.
/// Required for order placement. Not needed for market reads.
pub async fn authenticate(http: &Client, email: &str, password: &str) -> Result<KalshiSession> {
    if email.is_empty() || password.is_empty() {
        anyhow::bail!("Kalshi credentials not set — arb auto-execution unavailable");
    }

    let body = serde_json::json!({
        "login": { "email": email, "password": password }
    });

    let resp: serde_json::Value = http
        .post(format!("{}/login", KALSHI_BASE))
        .json(&body)
        .send()
        .await
        .context("Kalshi auth request failed")?
        .json()
        .await
        .context("Kalshi auth response parse failed")?;

    let token = resp["token"]
        .as_str()
        .context("Kalshi auth: no token in response")?
        .to_string();

    Ok(KalshiSession { token })
}
