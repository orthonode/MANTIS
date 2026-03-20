#![allow(dead_code)]
//! Kalshi cross-platform arb scanner.
//!
//! Polls Kalshi every 60s. Matches markets against Polymarket by title similarity.
//! When same event has different prices on both platforms:
//!   Polymarket YES = 0.35, Kalshi YES = 0.50 → buy YES on Poly + NO on Kalshi
//!   Combined cost: $0.35 + $0.50 = $0.85 → payout $1.00 → profit $0.15 (locked)
//!
//! ALERT-ONLY by default (config.arb.enabled = false).
//! After validating 10 real opportunities manually, set enabled = true.
//! 'A' keypress on dashboard toggles auto-execution (when enabled=true in config).
//!
//! Market matching heuristic: title similarity + resolution date proximity.
//! Only fire when combined_cost < (1.0 - config.arb.min_locked_profit).

use crate::arb::kalshi;
use crate::config::ArbConfig;
use crate::dashboard::tui::SharedDashState;
use crate::markets::state::MarketState;
use reqwest::Client;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::time::Duration;
use tokio::time::interval;
use tracing::info;

// ── Arb opportunity ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ArbOpportunity {
    pub poly_condition_id: String,
    pub poly_yes_price: Decimal,
    pub kalshi_ticker: String,
    pub kalshi_yes_price: Decimal,
    /// Combined cost of both legs.
    pub combined_cost: Decimal,
    /// Locked profit per $1 deployed.
    pub locked_profit: Decimal,
    pub description: String,
}

impl ArbOpportunity {
    /// Buy YES on Poly, NO on Kalshi (Poly is cheaper YES).
    pub fn buy_yes_poly(
        poly_condition_id: String,
        poly_yes_price: Decimal,
        kalshi_ticker: String,
        kalshi_yes_price: Decimal,
    ) -> Self {
        // Buy YES on Poly at poly_yes_price, buy NO on Kalshi at (1 - kalshi_yes_price).
        let kalshi_no_price = Decimal::ONE - kalshi_yes_price;
        let combined_cost = poly_yes_price + kalshi_no_price;
        let locked_profit = Decimal::ONE - combined_cost;
        ArbOpportunity {
            poly_condition_id,
            poly_yes_price,
            kalshi_ticker,
            kalshi_yes_price,
            combined_cost,
            locked_profit,
            description: format!(
                "BUY YES on Poly@{poly_yes_price:.2} + NO on Kalshi@{kalshi_no_price:.2}"
            ),
        }
    }

    /// Buy NO on Poly, YES on Kalshi (Kalshi is cheaper YES).
    pub fn buy_no_poly(
        poly_condition_id: String,
        poly_yes_price: Decimal,
        kalshi_ticker: String,
        kalshi_yes_price: Decimal,
    ) -> Self {
        let poly_no_price = Decimal::ONE - poly_yes_price;
        let combined_cost = kalshi_yes_price + poly_no_price;
        let locked_profit = Decimal::ONE - combined_cost;
        ArbOpportunity {
            poly_condition_id,
            poly_yes_price,
            kalshi_ticker,
            kalshi_yes_price,
            combined_cost,
            locked_profit,
            description: format!(
                "BUY NO on Poly@{poly_no_price:.2} + YES on Kalshi@{kalshi_yes_price:.2}"
            ),
        }
    }
}

// ── Scanner deps ──────────────────────────────────────────────────────────────

pub struct ArbScannerDeps {
    pub market_state: MarketState,
    pub arb_cfg: ArbConfig,
    pub clob_url: String,
    pub kalshi_email: String,
    pub kalshi_password: String,
    pub private_key: String,
    pub dash_state: SharedDashState,
}

// ── Scanner task ──────────────────────────────────────────────────────────────

pub async fn run(deps: ArbScannerDeps) {
    info!(
        enabled = deps.arb_cfg.enabled,
        "Arb scanner started — Kalshi cross-platform (alert-only until config.arb.enabled=true)"
    );

    let http = Client::new();
    let mut poll_timer = interval(Duration::from_secs(deps.arb_cfg.kalshi_poll_interval_secs));

    loop {
        poll_timer.tick().await;

        // Collect current Polymarket standard markets.
        let poly_markets: Vec<_> = deps
            .market_state
            .markets
            .iter()
            .filter(|e| !e.value().is_closed)
            .map(|e| {
                (
                    e.key().clone(),
                    e.value().question.clone(),
                    e.value().yes_price,
                )
            })
            .collect();

        // Build keyword list from market questions (top 5 words from each).
        // Use "btc" as a proxy to find crypto markets — these are the highest volume.
        let keywords = ["bitcoin", "ethereum", "fed", "election", "rate"];

        for keyword in keywords {
            let kalshi_markets = kalshi::fetch_markets(&http, keyword).await;

            for km in &kalshi_markets {
                // Volume filter on Kalshi side.
                if km.volume < deps.arb_cfg.min_market_volume {
                    continue;
                }

                // Find matching Polymarket market by title similarity.
                for (poly_id, poly_question, poly_yes) in &poly_markets {
                    if !titles_match(&km.title, poly_question) {
                        continue;
                    }

                    // Check for arb opportunity.
                    let opp = find_opportunity(
                        poly_id,
                        *poly_yes,
                        &km.ticker,
                        km.yes_price,
                        &deps.arb_cfg,
                    );

                    if let Some(opp) = opp {
                        handle_opportunity(opp, &deps, &http).await;
                    }
                }
            }
        }
    }
}

fn find_opportunity(
    poly_id: &str,
    poly_yes: Decimal,
    kalshi_ticker: &str,
    kalshi_yes: Decimal,
    cfg: &ArbConfig,
) -> Option<ArbOpportunity> {
    // Case 1: Poly YES cheaper → buy YES on Poly, NO on Kalshi.
    if poly_yes < kalshi_yes {
        let opp = ArbOpportunity::buy_yes_poly(
            poly_id.to_string(),
            poly_yes,
            kalshi_ticker.to_string(),
            kalshi_yes,
        );
        if opp.locked_profit >= cfg.min_locked_profit {
            return Some(opp);
        }
    }

    // Case 2: Kalshi YES cheaper → buy YES on Kalshi, NO on Poly.
    if kalshi_yes < poly_yes {
        let opp = ArbOpportunity::buy_no_poly(
            poly_id.to_string(),
            poly_yes,
            kalshi_ticker.to_string(),
            kalshi_yes,
        );
        if opp.locked_profit >= cfg.min_locked_profit {
            return Some(opp);
        }
    }

    None
}

async fn handle_opportunity(opp: ArbOpportunity, deps: &ArbScannerDeps, http: &Client) {
    let alert_msg = format!(
        "ARB: {} | {} | locked {:.1}¢/$ | poly:{:.2} kalshi:{:.2}",
        &opp.poly_condition_id[..8.min(opp.poly_condition_id.len())],
        opp.description,
        opp.locked_profit * dec!(100),
        opp.poly_yes_price,
        opp.kalshi_yes_price,
    );

    info!("{}", alert_msg);

    if let Ok(mut dash) = deps.dash_state.lock() {
        dash.push_log(crate::dashboard::tui::LogEntry::info(alert_msg));
        dash.arb_opportunities_found += 1;
    }

    // Auto-execute only if explicitly enabled in config.
    if deps.arb_cfg.enabled {
        if let Err(e) = execute_arb(&opp, deps, http).await {
            tracing::error!("Arb execution failed: {e}");
        }
    }
}

/// Execute both legs of an arb trade simultaneously.
/// Paper stub — requires real SDK auth + Kalshi order API.
async fn execute_arb(
    opp: &ArbOpportunity,
    _deps: &ArbScannerDeps,
    _http: &Client,
) -> anyhow::Result<()> {
    // TODO(ARB): actual execution:
    //   1. Place limit BUY on Polymarket at opp.poly_yes_price or opp.poly_no_price
    //   2. Simultaneously place BUY on Kalshi at the other side
    //   3. Both orders must fill for the arb to be valid — cancel both if either fails
    //   4. Max position: deps.arb_cfg.max_position_usd per pair

    info!(
        poly_id = %opp.poly_condition_id,
        kalshi_ticker = %opp.kalshi_ticker,
        locked_profit = %opp.locked_profit,
        "Arb: would execute (paper stub — auto-execution not yet implemented)"
    );
    Ok(())
}

/// Simple title similarity — true if both contain all words from a 3-word overlap.
fn titles_match(kalshi_title: &str, poly_question: &str) -> bool {
    let k_lower = kalshi_title.to_lowercase();
    let p_lower = poly_question.to_lowercase();

    // Extract words > 4 chars from Kalshi title.
    let k_words: Vec<&str> = k_lower.split_whitespace().filter(|w| w.len() > 4).collect();

    if k_words.is_empty() {
        return false;
    }

    // Count how many Kalshi words appear in Polymarket question.
    let matches = k_words.iter().filter(|w| p_lower.contains(**w)).count();

    // Require at least 2 meaningful word matches.
    matches >= 2
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn arb_locked_profit_calculation() {
        // Poly YES = 0.35, Kalshi YES = 0.50
        // Buy YES on Poly: cost $0.35
        // Buy NO on Kalshi: cost $0.50 (= 1 - yes = 0.50)
        // Combined: $0.85, payout $1.00, profit $0.15
        let opp = ArbOpportunity::buy_yes_poly(
            "poly123".to_string(),
            dec!(0.35),
            "KAL456".to_string(),
            dec!(0.50),
        );
        assert_eq!(opp.combined_cost, dec!(0.85));
        assert_eq!(opp.locked_profit, dec!(0.15));
    }

    #[test]
    fn no_arb_when_spread_too_small() {
        let cfg = ArbConfig {
            enabled: false,
            min_locked_profit: dec!(0.08),
            min_market_volume: dec!(5000),
            max_position_usd: dec!(20),
            kalshi_poll_interval_secs: 60,
        };
        // Poly=0.48, Kalshi=0.50 → profit = 1 - (0.48 + 0.50) = 0.02 < 0.08
        let result = find_opportunity("poly", dec!(0.48), "kal", dec!(0.50), &cfg);
        assert!(result.is_none());
    }

    #[test]
    fn title_match_works() {
        assert!(titles_match(
            "Federal Reserve raises interest rates",
            "Will the Federal Reserve raise interest rates?"
        ));
        assert!(!titles_match("Bitcoin price", "Ethereum network upgrade"));
    }
}
