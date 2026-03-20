#![allow(dead_code)]
//! Claude deep-verify scorer — second stage of dual-AI signal verification.
//!
//! Only called if Groq score >= groq_min_score (config: 55).
//! Provides deeper reasoning and primary-source cross-referencing.
//!
//! Uses POST https://api.anthropic.com/v1/messages with claude-sonnet-4-20250514.
//! Max tokens: 256 (JSON only — keep cost minimal).
//! Cost: ~$0.015/day at 10 calls/day.
//!
//! On any parse failure: log raw response, return SKIP — never crash.

use crate::signal::groq::Direction;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{error, warn};

// ── Types ─────────────────────────────────────────────────────────────────────

/// Claude scorer output.
#[derive(Debug, Clone)]
pub struct ClaudeScore {
    /// Certainty 0–100.
    pub score: u8,
    pub direction: Direction,
    /// Up to 15-word reasoning.
    pub reasoning: String,
    /// True only if context contains an official gov/institution statement.
    pub primary_source_confirms: bool,
}

// ── Anthropic request/response shapes ────────────────────────────────────────

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u16,
    messages: Vec<AnthropicMessage>,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeJson {
    certainty: u8,
    direction: String,
    reasoning: String,
    primary_source_confirms: bool,
}

// ── Scorer ────────────────────────────────────────────────────────────────────

/// Deep-verify a market with Claude.
///
/// # Arguments
/// * `client`       — shared reqwest client
/// * `api_key`      — ANTHROPIC_API_KEY from .env
/// * `model`        — from config.ai.claude_model
/// * `question`     — market question text
/// * `yes_price`    — crowd-implied probability (0.0–1.0)
/// * `hours_to_res` — hours until resolution
/// * `context`      — context snippet (news headlines, primary source text)
///
/// Returns `Ok(None)` on any failure — caller treats as SKIP.
pub async fn score(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    question: &str,
    yes_price: f64,
    hours_to_res: f64,
    context: &str,
) -> Result<Option<ClaudeScore>> {
    let pct = (yes_price * 100.0).round() as u32;
    let prompt = format!(
        r#"You are a prediction market analyst. Return ONLY valid JSON, no other text.

Market: {question}
YES price: {yes_price:.2} (crowd implies {pct}% probability)
Resolves in: {hours_to_res:.1} hours
Context: {context}

Return exactly this JSON:
{{"certainty": <0-100>, "direction": "YES"|"NO"|"SKIP", "reasoning": "<max 15 words>", "primary_source_confirms": true|false}}

Guide: 0-49=skip | 50-69=borderline | 70-84=trade | 85-100=high conviction
primary_source_confirms=true ONLY if context has official gov/institution statement.
If uncertain: certainty 0, direction SKIP"#
    );

    let req = MessagesRequest {
        model,
        max_tokens: 256,
        messages: vec![AnthropicMessage {
            role: "user".to_string(),
            content: prompt,
        }],
    };

    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&req)
        .send()
        .await
        .context("Claude API request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        error!("Claude API error {status}: {body}");
        return Ok(None);
    }

    let msg: MessagesResponse = resp.json().await.context("Failed to parse Claude response")?;
    let raw = msg
        .content
        .iter()
        .find(|b| b.block_type == "text")
        .and_then(|b| b.text.as_deref())
        .unwrap_or("");

    if raw.is_empty() {
        warn!("Claude: empty text block in response");
        return Ok(None);
    }

    Ok(parse_json(raw))
}

/// Extract JSON from Claude's response and parse it.
/// Claude occasionally wraps JSON in prose — extract between `{` and `}`.
fn parse_json(raw: &str) -> Option<ClaudeScore> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if start >= end {
        return None;
    }
    let json_str = &raw[start..=end];
    let parsed: ClaudeJson = serde_json::from_str(json_str)
        .map_err(|e| warn!("Claude: JSON parse error: {e} — raw: {json_str}"))
        .ok()?;

    Some(ClaudeScore {
        score: parsed.certainty,
        direction: Direction::from_str(&parsed.direction),
        reasoning: parsed.reasoning,
        primary_source_confirms: parsed.primary_source_confirms,
    })
}
