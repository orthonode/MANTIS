#![allow(dead_code)]
//! Groq fast-pass AI scorer — first stage of dual-AI signal verification.
//!
//! Uses llama-4-scout-17b-16e-instruct on the Groq free tier (~150ms latency).
//! Called BEFORE Claude to gate expensive deep-verify calls.
//! If Groq score < `groq_min_score` (config: 55): return SKIP immediately,
//! do NOT call Claude (saves cost and latency).
//!
//! Groq endpoint: https://api.groq.com/openai/v1/chat/completions
//! Auth: `Authorization: Bearer {GROQ_API_KEY}`
//! Free tier: 14,400 requests/day — ample for 1–2 signals/hour.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{error, warn};

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Direction {
    Yes,
    No,
    Skip,
}

impl Direction {
    pub fn from_str(s: &str) -> Self {
        match s.to_uppercase().trim() {
            "YES" => Direction::Yes,
            "NO" => Direction::No,
            _ => Direction::Skip,
        }
    }
}

/// Groq scorer output.
#[derive(Debug, Clone)]
pub struct GroqScore {
    /// Certainty 0–100.
    pub score: u8,
    pub direction: Direction,
    /// Up to 10-word reasoning.
    pub reasoning: String,
}

// ── OpenAI-compatible request/response shapes ─────────────────────────────────

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    max_tokens: u16,
    temperature: f32,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Deserialize)]
struct AssistantMessage {
    content: String,
}

#[derive(Deserialize)]
struct GroqJson {
    score: u8,
    direction: String,
    reasoning: String,
}

// ── Scorer ────────────────────────────────────────────────────────────────────

/// Score a single market with Groq.
///
/// # Arguments
/// * `client`      — shared reqwest client
/// * `api_key`     — GROQ_API_KEY from .env
/// * `model`       — from config.ai.groq_model
/// * `question`    — market question text
/// * `yes_price`   — crowd-implied probability (0.0–1.0)
/// * `hours_to_res` — hours until resolution
/// * `context`     — optional context snippet (news, primary source summary)
///
/// Returns `Ok(None)` on parse failure — caller should treat as SKIP.
/// Never panics — all errors are logged and return Ok(None) or Err.
pub async fn score(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    question: &str,
    yes_price: f64,
    hours_to_res: f64,
    context: &str,
) -> Result<Option<GroqScore>> {
    let pct = (yes_price * 100.0).round() as u32;
    let prompt = format!(
        r#"You are a prediction market analyst. Return ONLY valid JSON, no other text.

Market: {question}
YES price: {yes_price:.2} (crowd implies {pct}% probability)
Resolves in: {hours_to_res:.1} hours
Context: {context}

Return exactly this JSON:
{{"score": <0-100>, "direction": "YES"|"NO"|"SKIP", "reasoning": "<max 10 words>"}}

Guide: 0-54=skip | 55-69=borderline | 70-84=trade | 85-100=high conviction
If uncertain: score 0, direction SKIP"#
    );

    let req = ChatRequest {
        model,
        messages: vec![Message {
            role: "user",
            content: prompt,
        }],
        max_tokens: 128,
        temperature: 0.1,
    };

    let resp = client
        .post("https://api.groq.com/openai/v1/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&req)
        .send()
        .await
        .context("Groq API request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        error!("Groq API error {status}: {body}");
        return Ok(None);
    }

    let chat: ChatResponse = resp.json().await.context("Failed to parse Groq response")?;
    let raw = match chat.choices.first() {
        Some(c) => c.message.content.clone(),
        None => {
            warn!("Groq: empty choices array");
            return Ok(None);
        }
    };

    Ok(parse_json(&raw))
}

/// Extract JSON from the response text and parse it.
/// Groq may occasionally prepend text — we extract between first `{` and last `}`.
fn parse_json(raw: &str) -> Option<GroqScore> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if start >= end {
        return None;
    }
    let json_str = &raw[start..=end];
    let parsed: GroqJson = serde_json::from_str(json_str)
        .map_err(|e| warn!("Groq: JSON parse error: {e} — raw: {json_str}"))
        .ok()?;

    Some(GroqScore {
        score: parsed.score,
        direction: Direction::from_str(&parsed.direction),
        reasoning: parsed.reasoning,
    })
}
