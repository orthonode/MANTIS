#![allow(dead_code)]
//! Dual-AI consensus engine.
//!
//! Combines Groq (fast-pass) and Claude (deep-verify) scores into a single
//! trade decision. A trade fires only when:
//!   1. Both scorers agree on direction (both YES or both NO)
//!   2. Weighted consensus score >= consensus_min_score (config: 68)
//!
//! If Groq score < groq_min_score (config: 55): return SKIP without calling Claude.
//! Weighted score: groq * groq_weight + claude * claude_weight  (0.4 / 0.6)

use crate::config::AiConfig;
use crate::signal::claude;
use crate::signal::groq::{self, Direction, GroqScore};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::{info, warn};

// ── Consensus output ──────────────────────────────────────────────────────────

/// Final trade decision from the consensus engine.
#[derive(Debug, Clone)]
pub struct ConsensusResult {
    pub direction: Direction,
    /// Weighted consensus score 0–100.
    pub score: u8,
    pub groq_score: u8,
    pub claude_score: Option<u8>,
    pub groq_reasoning: String,
    pub claude_reasoning: Option<String>,
    /// True if Claude confirmed a primary source.
    pub primary_source_confirms: bool,
    /// True if Claude was called (score >= groq_min_score).
    pub claude_called: bool,
}

impl ConsensusResult {
    /// True if this result represents a tradeable signal.
    pub fn is_trade(&self) -> bool {
        self.direction != Direction::Skip
    }

    /// Confidence as a Decimal (0.0–1.0) for Kelly confidence multiplier.
    pub fn confidence_decimal(&self) -> Decimal {
        Decimal::from(self.score) / dec!(100.0)
    }
}

// ── Market context for scoring ────────────────────────────────────────────────

/// Market data passed to both AI scorers.
pub struct MarketContext<'a> {
    pub question: &'a str,
    pub yes_price: f64,
    pub hours_to_res: f64,
    pub context: &'a str,
}

// ── Consensus scorer ──────────────────────────────────────────────────────────

/// Run the full dual-AI scoring pipeline for a standard market.
///
/// Step 1: Groq fast-pass. If score < groq_min_score → SKIP (no Claude call).
/// Step 2: Claude deep-verify. If directions differ → SKIP.
/// Step 3: Weighted consensus. If score < consensus_min_score → SKIP.
///
/// Returns a `ConsensusResult` with `direction == Skip` when no trade should fire.
pub async fn evaluate(
    client: &reqwest::Client,
    cfg: &AiConfig,
    groq_api_key: &str,
    anthropic_api_key: &str,
    market: &MarketContext<'_>,
) -> ConsensusResult {
    // ── Step 1: Groq fast-pass ──────────────────────────────────────────────
    let groq_result: Option<GroqScore> = match groq::score(
        client,
        groq_api_key,
        &cfg.groq_model,
        market.question,
        market.yes_price,
        market.hours_to_res,
        market.context,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("Consensus: Groq call failed — {e}, returning SKIP");
            return skip_result(0, None);
        }
    };

    let groq = match groq_result {
        Some(g) => g,
        None => {
            warn!("Consensus: Groq returned no parseable result, returning SKIP");
            return skip_result(0, None);
        }
    };

    // Gate: if Groq score too low, skip without calling Claude.
    if groq.score < cfg.groq_min_score {
        info!(
            groq_score = groq.score,
            threshold = cfg.groq_min_score,
            "Consensus: Groq score below threshold, skipping Claude call"
        );
        return ConsensusResult {
            direction: Direction::Skip,
            score: groq.score,
            groq_score: groq.score,
            claude_score: None,
            groq_reasoning: groq.reasoning,
            claude_reasoning: None,
            primary_source_confirms: false,
            claude_called: false,
        };
    }

    // ── Step 2: Claude deep-verify ──────────────────────────────────────────
    let claude_result = match claude::score(
        client,
        anthropic_api_key,
        &cfg.claude_model,
        market.question,
        market.yes_price,
        market.hours_to_res,
        market.context,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("Consensus: Claude call failed — {e}, returning SKIP");
            return skip_result(groq.score, Some(groq.reasoning));
        }
    };

    let claude = match claude_result {
        Some(c) => c,
        None => {
            warn!("Consensus: Claude returned no parseable result, returning SKIP");
            return skip_result(groq.score, Some(groq.reasoning));
        }
    };

    // ── Step 3: Direction agreement check ──────────────────────────────────
    if groq.direction != claude.direction
        || groq.direction == Direction::Skip
        || claude.direction == Direction::Skip
    {
        info!(
            groq_dir = ?groq.direction,
            claude_dir = ?claude.direction,
            "Consensus: directions disagree or SKIP — no trade"
        );
        return ConsensusResult {
            direction: Direction::Skip,
            score: groq.score,
            groq_score: groq.score,
            claude_score: Some(claude.score),
            groq_reasoning: groq.reasoning,
            claude_reasoning: Some(claude.reasoning),
            primary_source_confirms: claude.primary_source_confirms,
            claude_called: true,
        };
    }

    // ── Step 4: Weighted consensus score ────────────────────────────────────
    let weighted = (Decimal::from(groq.score) * cfg.consensus_groq_weight
        + Decimal::from(claude.score) * cfg.consensus_claude_weight)
        .round()
        .to_u8()
        .unwrap_or(0);

    if weighted < cfg.consensus_min_score {
        info!(
            weighted_score = weighted,
            threshold = cfg.consensus_min_score,
            "Consensus: weighted score below threshold — no trade"
        );
        return ConsensusResult {
            direction: Direction::Skip,
            score: weighted,
            groq_score: groq.score,
            claude_score: Some(claude.score),
            groq_reasoning: groq.reasoning,
            claude_reasoning: Some(claude.reasoning),
            primary_source_confirms: claude.primary_source_confirms,
            claude_called: true,
        };
    }

    // ── Trade signal ─────────────────────────────────────────────────────────
    info!(
        direction = ?groq.direction,
        weighted_score = weighted,
        groq = groq.score,
        claude = claude.score,
        primary_source = claude.primary_source_confirms,
        "Consensus: TRADE SIGNAL"
    );

    ConsensusResult {
        direction: groq.direction, // both agree, so use either
        score: weighted,
        groq_score: groq.score,
        claude_score: Some(claude.score),
        groq_reasoning: groq.reasoning,
        claude_reasoning: Some(claude.reasoning),
        primary_source_confirms: claude.primary_source_confirms,
        claude_called: true,
    }
}

fn skip_result(groq_score: u8, groq_reasoning: Option<String>) -> ConsensusResult {
    ConsensusResult {
        direction: Direction::Skip,
        score: groq_score,
        groq_score,
        claude_score: None,
        groq_reasoning: groq_reasoning.unwrap_or_default(),
        claude_reasoning: None,
        primary_source_confirms: false,
        claude_called: false,
    }
}
