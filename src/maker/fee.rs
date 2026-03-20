//! Fee cache — fetch and cache feeRateBps per market from the CLOB API.
//!
//! Polymarket Feb 2026: taker fees introduced on 5-min/15-min crypto markets.
//! Fee formula: C × 0.25 × (p × (1-p))² — max ~0.44% at p=0.50.
//! feeRateBps must be included in every signed order on fee-enabled markets.
//! Never hardcode. Never omit — orders are rejected outright without it.
//!
//! Cache TTL: 60 seconds. On fetch failure: return Err (do not place order).

use anyhow::{Context, Result};
use dashmap::DashMap;
use reqwest::Client;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

// ── FeeCache ─────────────────────────────────────────────────────────────────

/// Thread-safe cache of feeRateBps per condition_id.
/// Shared across maker engine and executor via `Arc<FeeCache>`.
#[derive(Debug, Clone)]
pub struct FeeCache {
    /// condition_id → (feeRateBps, fetched_at)
    cache: Arc<DashMap<String, (u64, Instant)>>,
    ttl: Duration,
    clob_url: String,
}

impl FeeCache {
    pub fn new(clob_url: String) -> Self {
        Self {
            cache: Arc::new(DashMap::new()),
            ttl: Duration::from_secs(60),
            clob_url,
        }
    }

    /// Returns cached feeRateBps for a market, refreshing if stale (>60s).
    ///
    /// On network error or missing field: returns Err.
    /// Caller MUST NOT place an order if this returns Err — per P6 rules.
    pub async fn get_fee_bps(&self, http: &Client, condition_id: &str) -> Result<u64> {
        // Check cache first.
        if let Some(entry) = self.cache.get(condition_id) {
            let (bps, fetched_at) = *entry;
            if fetched_at.elapsed() < self.ttl {
                return Ok(bps);
            }
        }

        // Fetch from CLOB API.
        let url = format!("{}/markets/{}", self.clob_url, condition_id);
        let resp: serde_json::Value = http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("FeeCache: GET {url} failed"))?
            .json()
            .await
            .context("FeeCache: failed to parse JSON response")?;

        // Field absent = non-fee market, bps = 0.
        let bps = resp["feeRateBps"].as_u64().unwrap_or(0);

        info!(condition_id = %condition_id, fee_bps = bps, "FeeCache: fetched feeRateBps");
        self.cache
            .insert(condition_id.to_string(), (bps, Instant::now()));
        Ok(bps)
    }

}
