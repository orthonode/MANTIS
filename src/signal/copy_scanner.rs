#![allow(dead_code)]
//! Copy scanner — track top wallets on Polymarket and alert on their entries.
//!
//! Polls data-api.polymarket.com/profiles/leaderboard every 5 minutes.
//! When a top wallet enters a new position:
//!   1. Groq fast-pass scores the market (if score >= 60).
//!   2. If Groq score >= 60 AND market liquidity > $5,000: alert dashboard.
//!   3. Dashboard shows the alert. Human presses 'C' to approve copy.
//!
//! Never auto-copies. Human approval required.
//! This is a third signal source on top of Flash (oracle lag) and AI scoring.

use crate::dashboard::tui::SharedDashState;
use crate::markets::state::MarketState;
use anyhow::Result;
use reqwest::Client;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::time::{Duration, interval};
use tracing::{info, warn};

// ── API types ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct LeaderboardEntry {
    #[serde(rename = "proxyWallet")]
    proxy_wallet: String,
    #[serde(rename = "pnl")]
    pnl: Option<f64>,
    #[serde(rename = "volume")]
    volume: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct LeaderboardResponse {
    data: Vec<LeaderboardEntry>,
}

#[derive(Debug, Deserialize)]
struct WalletPosition {
    #[serde(rename = "conditionId")]
    condition_id: String,
    #[serde(rename = "size")]
    size: Option<f64>,
    #[serde(rename = "side")]
    side: Option<String>,
}

// ── Copy alert ────────────────────────────────────────────────────────────────

/// A pending copy-trade alert shown on the dashboard.
#[derive(Debug, Clone)]
pub struct CopyAlert {
    pub condition_id: String,
    pub market_question: String,
    pub wallet: String,
    pub direction: String,
    pub groq_score: u8,
    pub liquidity_usd: Decimal,
}

// ── Scanner task ─────────────────────────────────────────────────────────────

pub struct CopyScannerDeps {
    pub market_state: MarketState,
    pub data_api_url: String,
    pub groq_api_key: String,
    pub dash_state: SharedDashState,
}

/// Entry point — spawn via `tokio::spawn(signal::copy_scanner::run(deps))`.
pub async fn run(deps: CopyScannerDeps) {
    info!("Copy scanner started — polling leaderboard every 5 min");
    let http = Client::new();
    let mut poll_timer = interval(Duration::from_secs(300)); // 5 min
    let mut known_positions: HashSet<(String, String)> = HashSet::new(); // (wallet, condition_id)

    loop {
        poll_timer.tick().await;

        let wallets = match fetch_top_wallets(&http, &deps.data_api_url).await {
            Ok(w) => w,
            Err(e) => {
                warn!("Copy scanner: leaderboard fetch failed: {e}");
                continue;
            }
        };

        for wallet in &wallets {
            let positions =
                match fetch_wallet_positions(&http, &deps.data_api_url, wallet).await {
                    Ok(p) => p,
                    Err(_) => continue,
                };

            for pos in positions {
                let key = (wallet.clone(), pos.condition_id.clone());
                if known_positions.contains(&key) {
                    continue; // Already seen this position.
                }
                known_positions.insert(key);

                // Look up market in state.
                let snap = match deps.market_state.markets.get(&pos.condition_id) {
                    Some(s) => s.clone(),
                    None => continue,
                };

                // Minimum liquidity filter.
                if snap.volume < dec!(5000) {
                    continue;
                }

                let direction = pos.side.unwrap_or_else(|| "YES".to_string());
                let groq_score =
                    quick_groq_score(&http, &snap.question, &deps.groq_api_key).await;

                if groq_score < 60 {
                    continue;
                }

                let alert = CopyAlert {
                    condition_id: pos.condition_id.clone(),
                    market_question: snap.question.clone(),
                    wallet: wallet.clone(),
                    direction: direction.clone(),
                    groq_score,
                    liquidity_usd: snap.volume,
                };

                info!(
                    condition_id = %alert.condition_id,
                    wallet = %wallet,
                    direction = %direction,
                    groq_score = groq_score,
                    liquidity_usd = %snap.volume,
                    "COPY ALERT — press [C] on dashboard to approve"
                );

                // Push alert to dashboard log.
                if let Ok(mut dash) = deps.dash_state.lock() {
                    dash.push_log(crate::dashboard::tui::LogEntry::info(format!(
                        "COPY ALERT: {:.8} {direction} groq:{groq_score} liq:${:.0}",
                        alert.condition_id, alert.liquidity_usd
                    )));
                }
            }
        }
    }
}

// ── API helpers ───────────────────────────────────────────────────────────────

/// Fetch top 5 wallet addresses from the Polymarket leaderboard.
async fn fetch_top_wallets(http: &Client, data_api_url: &str) -> Result<Vec<String>> {
    let url = format!("{}/profiles/leaderboard?limit=5&sort=pnl", data_api_url);
    let resp: serde_json::Value = http.get(&url).send().await?.json().await?;

    let wallets = resp["data"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|entry| entry["proxyWallet"].as_str().map(|s| s.to_string()))
        .collect();

    Ok(wallets)
}

/// Fetch open positions for a specific wallet.
async fn fetch_wallet_positions(
    http: &Client,
    data_api_url: &str,
    wallet: &str,
) -> Result<Vec<WalletPosition>> {
    let url = format!("{}/positions?user={}&sizeThreshold=10&limit=20", data_api_url, wallet);
    let resp: serde_json::Value = http.get(&url).send().await?.json().await?;

    let positions = resp["data"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|p| serde_json::from_value::<WalletPosition>(p.clone()).ok())
        .collect();

    Ok(positions)
}

/// Quick Groq score for a market question (fire-and-forget scoring).
/// Returns 0 on any error — callers filter by score >= 60.
async fn quick_groq_score(http: &Client, question: &str, groq_api_key: &str) -> u8 {
    use serde_json::json;

    if groq_api_key.is_empty() {
        return 0;
    }

    let body = json!({
        "model": "llama-4-scout-17b-16e-instruct",
        "messages": [{
            "role": "user",
            "content": format!(
                "Rate this prediction market 0-100 for certainty. Return ONLY the number.\nMarket: {}",
                question
            )
        }],
        "max_tokens": 10,
        "temperature": 0.1
    });

    let resp = match http
        .post("https://api.groq.com/openai/v1/chat/completions")
        .bearer_auth(groq_api_key)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return 0,
    };

    let data: serde_json::Value = match resp.json().await {
        Ok(d) => d,
        Err(_) => return 0,
    };

    data["choices"][0]["message"]["content"]
        .as_str()
        .and_then(|s| s.trim().parse::<u8>().ok())
        .unwrap_or(0)
}
