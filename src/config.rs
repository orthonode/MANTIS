use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::Deserialize;

// ── Existing config sections ─────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct CapitalConfig {
    pub total_usd: Decimal,
    pub max_flash_bet_usd: Decimal,
    pub max_standard_bet_usd: Decimal,
    pub max_total_exposure_usd: Decimal,
    pub daily_loss_limit_usd: Decimal,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct SignalConfig {
    pub flash_divergence_threshold_pct: Decimal,
    pub flash_min_time_to_resolve_secs: u64,
    pub flash_max_time_to_resolve_secs: u64,
    pub standard_min_certainty: u8,
    pub standard_high_certainty: u8,
    pub standard_max_hours_to_resolve: Decimal,
    pub standard_high_certainty_max_hours: Decimal,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct FiltersConfig {
    pub flash_min_volume_usd: Decimal,
    pub standard_min_volume_usd: Decimal,
    pub standard_min_yes_price: Decimal,
    pub standard_max_yes_price: Decimal,
    pub gamma_poll_interval_secs: u64,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct NetworkConfig {
    pub clob_url: String,
    pub gamma_url: String,
    pub rtds_url: String,
    pub clob_ws_url: String,
    pub binance_ws_url: String,
    pub geoblock_url: String,
    pub rtds_ping_interval_secs: u64,
}

// ── New upgrade config sections ──────────────────────────────────────────────

/// Fractional Kelly bet sizing parameters.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct KellyConfig {
    /// Hard cap: never bet more than this fraction of bankroll per trade.
    pub max_fraction: Decimal,
    /// Hard floor: minimum bet size in USD.
    pub min_bet_usd: Decimal,
    /// Category multiplier for Flash BTC/ETH markets.
    pub flash_category_mult: Decimal,
    /// Category multiplier for standard (non-flash) markets.
    pub standard_category_mult: Decimal,
    /// Category multiplier for political/governance markets.
    pub political_category_mult: Decimal,
}

/// Market regime detection thresholds.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct RegimeConfig {
    /// BTC/ETH price swing % within 5 minutes that triggers VOLATILE regime.
    pub volatile_threshold_pct: Decimal,
    /// Volume spike ratio (vs 10-min avg) that triggers BREAKING regime.
    pub breaking_volume_multiplier: Decimal,
    /// Seconds to pause new Flash orders during a BREAKING regime.
    pub flash_pause_on_breaking_secs: u64,
}

/// Dynamic position exit parameters.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct ExitConfig {
    /// Activate trailing stop once position is up this fraction of max profit.
    pub profit_lock_threshold: Decimal,
    /// Exit if price reverses this fraction from the peak profit point.
    pub trailing_stop_reversal: Decimal,
    /// Seconds between Groq re-scores while a position is open.
    pub groq_rescore_interval_secs: u64,
    /// Exit a losing position if <5 min to resolution and loss exceeds this fraction.
    pub losing_exit_threshold: Decimal,
}

/// Dual-AI signal configuration.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct AiConfig {
    /// Groq model identifier.
    pub groq_model: String,
    /// Minimum Groq score to proceed to Claude deep-verify.
    pub groq_min_score: u8,
    /// Claude model identifier.
    pub claude_model: String,
    /// Minimum combined consensus score to fire a trade.
    pub consensus_min_score: u8,
    /// Weight applied to Groq score in consensus calculation.
    pub consensus_groq_weight: Decimal,
    /// Weight applied to Claude score in consensus calculation.
    pub consensus_claude_weight: Decimal,
}

// ── Internal TOML deserialisation target ────────────────────────────────────

/// Fields loaded from config.toml only (no secrets).
#[derive(Debug, Deserialize)]
struct TomlConfig {
    pub capital: CapitalConfig,
    pub signal: SignalConfig,
    pub filters: FiltersConfig,
    pub network: NetworkConfig,
    pub kelly: KellyConfig,
    pub regime: RegimeConfig,
    pub exit: ExitConfig,
    pub ai: AiConfig,
}

// ── Runtime config ───────────────────────────────────────────────────────────

/// Full runtime config: TOML strategy params + env secrets.
/// Constructed once in `main.rs` and passed (by clone or Arc) to tasks.
#[allow(dead_code)]
#[derive(Debug)]
pub struct Config {
    pub capital: CapitalConfig,
    pub signal: SignalConfig,
    pub filters: FiltersConfig,
    pub network: NetworkConfig,
    pub kelly: KellyConfig,
    pub regime: RegimeConfig,
    pub exit: ExitConfig,
    pub ai: AiConfig,
    /// Polygon EOA private key — from .env, never logged.
    pub private_key: String,
    /// Anthropic API key — from .env, never logged.
    pub anthropic_api_key: String,
    /// Groq API key — from .env, never logged.
    pub groq_api_key: String,
}

impl Config {
    pub fn load() -> Result<Self> {
        dotenvy::dotenv().ok();

        let private_key =
            std::env::var("PRIVATE_KEY").context("PRIVATE_KEY not set in .env")?;
        let anthropic_api_key =
            std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set in .env")?;
        let groq_api_key =
            std::env::var("GROQ_API_KEY").context("GROQ_API_KEY not set in .env")?;

        let raw = config::Config::builder()
            .add_source(config::File::with_name("config"))
            .build()
            .context("Failed to load config.toml")?;

        let toml: TomlConfig = raw.try_deserialize().context("Failed to parse config.toml")?;

        Ok(Config {
            capital: toml.capital,
            signal: toml.signal,
            filters: toml.filters,
            network: toml.network,
            kelly: toml.kelly,
            regime: toml.regime,
            exit: toml.exit,
            ai: toml.ai,
            private_key,
            anthropic_api_key,
            groq_api_key,
        })
    }
}
