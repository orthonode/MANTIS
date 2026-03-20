#![allow(dead_code)]
//! Gamma REST API poller — market discovery.
//!
//! Polls `https://gamma-api.polymarket.com/markets` every `poll_interval_secs`
//! seconds. Filters markets to those resolving within the configured window
//! with sufficient volume. Writes new markets to `MarketState` and sends
//! subscription commands to the CLOB WS task.
//!
//! Token IDs fetched here are ALWAYS from this live API response — never
//! hardcoded. (Section 11 error: hardcoded IDs change when Polymarket refreshes
//! markets; bot silently trades wrong market.)

use crate::feeds::clob_ws::{subscribe_tokens, SubscribeTx};
use crate::markets::state::{MarketEvent, MarketSnapshot, MarketState, MarketType};
use anyhow::Result;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time;
use tracing::{error, info, warn};

// ── Gamma API response types ─────────────────────────────────────────────────

/// Partial shape of a Gamma market object. Only fields we care about.
#[derive(Debug, serde::Deserialize)]
struct GammaMarket {
    /// Hex condition ID — used as primary key in MarketState.
    #[serde(rename = "conditionId")]
    condition_id: Option<String>,

    /// Market question text.
    question: Option<String>,

    /// Optional longer description.
    description: Option<String>,

    /// JSON array of outcome token objects: [{"token_id": "...", "outcome": "Yes"}, ...]
    tokens: Option<Vec<GammaToken>>,

    /// Total USD volume traded.
    #[serde(rename = "volume")]
    volume: Option<serde_json::Value>,

    /// Unix timestamp (seconds) of resolution.
    #[serde(rename = "endDateIso")]
    end_date_iso: Option<String>,

    /// Whether market is still active.
    active: Option<bool>,

    /// Whether market is closed.
    closed: Option<bool>,
}

#[derive(Debug, serde::Deserialize)]
struct GammaToken {
    token_id: Option<String>,
    outcome: Option<String>,
}

// ── Config struct ─────────────────────────────────────────────────────────────

/// Market filter parameters for the Gamma poller.
pub struct GammaFilters {
    /// Maximum hours to resolution to include a market.
    pub max_hours_to_resolution: f64,
    /// Minimum USD volume — used for all market types.
    pub min_volume: Decimal,
}

// ── Run the gamma poll task ───────────────────────────────────────────────────

/// Run the Gamma market discovery task forever.
///
/// Every `poll_interval_secs`:
///  1. Fetch open markets from Gamma API.
///  2. Filter: active, not closed, volume >= threshold, resolves within window.
///  3. Classify: Flash (BTC/ETH 5-min) or Standard.
///  4. Upsert new snapshots into `MarketState`.
///  5. Send token IDs to CLOB WS via `clob_sub_tx`.
///  6. Emit `MarketEvent::NewMarket` on broadcast channel.
///
/// Errors are logged and retried — this task never exits.
pub async fn run(
    gamma_url: String,
    poll_interval_secs: u64,
    state: MarketState,
    event_tx: broadcast::Sender<MarketEvent>,
    clob_sub_tx: SubscribeTx,
    filters: GammaFilters,
) {
    let client = reqwest::Client::new();
    let mut interval = time::interval(Duration::from_secs(poll_interval_secs));

    loop {
        interval.tick().await;

        match poll_once(
            &client,
            &gamma_url,
            &state,
            &event_tx,
            &clob_sub_tx,
            &filters,
        )
        .await
        {
            Ok(new_count) => {
                if new_count > 0 {
                    info!(
                        "Gamma: discovered {} new markets (total open: {})",
                        new_count,
                        state.open_market_count()
                    );
                }
            }
            Err(e) => {
                error!("Gamma: poll failed — {e}");
            }
        }
    }
}

/// Single poll cycle. Returns the number of newly added markets.
async fn poll_once(
    client: &reqwest::Client,
    gamma_url: &str,
    state: &MarketState,
    event_tx: &broadcast::Sender<MarketEvent>,
    clob_sub_tx: &SubscribeTx,
    filters: &GammaFilters,
) -> Result<usize> {
    let mut new_count = 0usize;
    let mut offset = 0usize;
    let limit = 100usize;

    loop {
        let url = format!("{gamma_url}?closed=false&limit={limit}&offset={offset}");
        let resp = client
            .get(&url)
            .header("User-Agent", "mantis-trading-agent/0.1.0")
            .send()
            .await?;

        let markets: Vec<GammaMarket> = resp.json().await?;
        let page_len = markets.len();

        for gm in markets {
            match process_market(
                gm,
                state,
                event_tx,
                clob_sub_tx,
                filters,
            )
            .await
            {
                Some(true) => new_count += 1,
                Some(false) => {} // updated existing
                None => {}        // filtered out
            }
        }

        if page_len < limit {
            break; // last page
        }
        offset += limit;
    }

    // Prune closed/resolved markets from state.
    state.prune_closed();

    Ok(new_count)
}

/// Process a single Gamma market. Returns:
///   Some(true)  — new market added
///   Some(false) — existing market updated
///   None        — filtered out (closed, wrong volume, too far out, etc.)
async fn process_market(
    gm: GammaMarket,
    state: &MarketState,
    event_tx: &broadcast::Sender<MarketEvent>,
    clob_sub_tx: &SubscribeTx,
    filters: &GammaFilters,
) -> Option<bool> {
    // Skip inactive or closed markets.
    if gm.active == Some(false) || gm.closed == Some(true) {
        return None;
    }

    let condition_id = gm.condition_id?;
    let question = gm.question.unwrap_or_default();

    // Parse resolution time.
    let resolution_time: DateTime<Utc> = gm
        .end_date_iso
        .as_deref()
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())?;

    // Filter by time to resolution.
    let hours_to_resolution = (resolution_time - Utc::now()).num_minutes() as f64 / 60.0;
    if hours_to_resolution <= 0.0 || hours_to_resolution > filters.max_hours_to_resolution {
        return None;
    }

    // Parse volume.
    let volume = parse_decimal_field(&gm.volume)?;

    // Classify market type.
    let market_type = classify_market(&question);

    // Apply volume filter.
    if volume < filters.min_volume {
        return None;
    }

    // Extract YES and NO token IDs. Gamma returns tokens as an array of
    // {token_id, outcome} objects — outcome "Yes" is the YES token.
    let tokens = gm.tokens.unwrap_or_default();
    let token_id_yes = tokens
        .iter()
        .find(|t| t.outcome.as_deref().map(|o| o.eq_ignore_ascii_case("yes")) == Some(true))
        .and_then(|t| t.token_id.clone())?;
    let token_id_no = tokens
        .iter()
        .find(|t| t.outcome.as_deref().map(|o| o.eq_ignore_ascii_case("no")) == Some(true))
        .and_then(|t| t.token_id.clone())?;

    let is_new = !state.markets.contains_key(&condition_id);

    let snapshot = MarketSnapshot {
        condition_id: condition_id.clone(),
        question,
        description: gm.description,
        token_id_yes: token_id_yes.clone(),
        token_id_no: token_id_no.clone(),
        yes_price: Decimal::ZERO,  // updated by CLOB WS price_change events
        no_price: Decimal::ONE,
        volume,
        best_bid: None,
        best_ask: None,
        resolution_time,
        market_type,
        last_updated: Utc::now(),
        is_closed: false,
    };

    state.upsert(snapshot);

    // Subscribe CLOB WS to YES and NO tokens for this market.
    subscribe_tokens(clob_sub_tx, vec![token_id_yes, token_id_no]).await;

    if is_new {
        let _ = event_tx.send(MarketEvent::NewMarket {
            condition_id: condition_id.clone(),
        });
        return Some(true);
    }

    Some(false)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Classify a market as Flash or Standard based on the question text.
///
/// Flash markets follow the pattern: "Will BTC be above $X at HH:MM?"
/// or "BTC UP" / "ETH UP" style Polymarket 5-minute markets.
fn classify_market(question: &str) -> MarketType {
    let q = question.to_lowercase();
    // Polymarket Flash market patterns: "btc up", "eth up", "btc down", "eth down"
    // or time-boxed BTC/ETH price questions.
    let is_flash = (q.contains("btc") || q.contains("bitcoin") || q.contains("eth") || q.contains("ethereum"))
        && (q.contains(" up") || q.contains(" down") || q.contains("above") || q.contains("below"))
        && (q.contains("5-min") || q.contains("5 min") || q.contains(":") || q.contains("at "));

    if is_flash {
        MarketType::Flash
    } else {
        MarketType::Standard
    }
}

/// Parse a Decimal from a serde_json::Value that may be a string or number.
fn parse_decimal_field(v: &Option<serde_json::Value>) -> Option<Decimal> {
    match v {
        Some(serde_json::Value::String(s)) => Decimal::from_str(s).ok(),
        Some(serde_json::Value::Number(n)) => {
            Decimal::from_str(&n.to_string()).ok()
        }
        _ => {
            warn!("Gamma: could not parse decimal from {:?}", v);
            None
        }
    }
}
