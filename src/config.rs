use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::Deserialize;

// ── Capital ───────────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct CapitalConfig {
    pub total_usd: Decimal,
    pub max_flash_bet_usd: Decimal,
    pub max_standard_bet_usd: Decimal,
    pub max_total_exposure_usd: Decimal,
    pub daily_loss_limit_usd: Decimal,
}

// ── Standard signal engine (non-fee markets: politics, macro) ─────────────────

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct StandardConfig {
    pub enabled: bool,
    pub min_consensus_score: u8,
    pub min_volume_usd: Decimal,
    pub min_yes_price: Decimal,
    pub max_yes_price: Decimal,
    pub max_hours_to_resolve: Decimal,
    pub high_certainty_score: u8,
    pub high_certainty_max_hours: Decimal,
}

// ── Filters ───────────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct FiltersConfig {
    pub standard_min_volume_usd: Decimal,
    pub standard_min_yes_price: Decimal,
    pub standard_max_yes_price: Decimal,
    pub gamma_poll_interval_secs: u64,
}

// ── Network ───────────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct NetworkConfig {
    pub clob_url: String,
    pub gamma_url: String,
    pub rtds_url: String,
    pub clob_ws_url: String,
    pub binance_ws_url: String,
    pub geoblock_url: String,
    pub data_api_url: String,
    pub rtds_ping_interval_secs: u64,
}

// ── Kelly ─────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct KellyConfig {
    pub max_fraction: Decimal,
    pub min_bet_usd: Decimal,
    pub flash_category_mult: Decimal,
    pub standard_category_mult: Decimal,
    pub political_category_mult: Decimal,
}

// ── Regime ────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct RegimeConfig {
    pub volatile_threshold_pct: Decimal,
    pub breaking_volume_multiplier: Decimal,
    pub flash_pause_on_breaking_secs: u64,
}

// ── Exit ──────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct ExitConfig {
    pub profit_lock_threshold: Decimal,
    pub trailing_stop_reversal: Decimal,
    pub groq_rescore_interval_secs: u64,
    pub losing_exit_threshold: Decimal,
}

// ── AI ────────────────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct AiConfig {
    pub groq_model: String,
    pub groq_min_score: u8,
    pub claude_model: String,
    pub consensus_min_score: u8,
    pub consensus_groq_weight: Decimal,
    pub consensus_claude_weight: Decimal,
}

// ── Maker (market-making engine on 5-min/15-min crypto markets) ───────────────

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct MakerConfig {
    pub enabled: bool,
    /// Base spread in probability units (e.g. 0.025 = 2.5 cents).
    pub target_spread_pct: Decimal,
    /// Flatten inventory aggressively when imbalance exceeds this share count.
    pub max_imbalance_shares: Decimal,
    /// Cancel ALL maker orders this many seconds before resolution. Hard rule.
    pub cancel_before_resolution_secs: u64,
    /// Target milliseconds for a full cancel+replace cycle.
    pub replace_loop_ms: u64,
    /// Only quote markets with total volume above this USD threshold.
    pub min_market_volume_usd: Decimal,
    /// Max USD per side (bid or ask) in any single market.
    pub max_per_side_usd: Decimal,
    /// Skip entering markets where yes_price is in the high-fee zone [min, max].
    pub skip_probability_min: Decimal,
    pub skip_probability_max: Decimal,
    /// Spread multiplier when 1-minute volatility exceeds 0.5%.
    pub volatility_spread_mult: Decimal,
    /// Spread multiplier when time_to_resolution < 60s.
    pub low_time_spread_mult_60s: Decimal,
    /// Spread multiplier when time_to_resolution < 30s.
    pub low_time_spread_mult_30s: Decimal,
}

// ── Arb (Kalshi cross-platform arbitrage) ─────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct ArbConfig {
    /// When false: scanner runs in alert-only mode. No orders placed.
    pub enabled: bool,
    /// Minimum locked profit per dollar deployed to fire an arb trade.
    pub min_locked_profit: Decimal,
    /// Both legs must have at least this volume to ensure fills.
    pub min_market_volume: Decimal,
    /// Maximum USD deployed per arb pair.
    pub max_position_usd: Decimal,
    /// How often to poll the Kalshi API.
    pub kalshi_poll_interval_secs: u64,
}

// ── Internal TOML target ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TomlConfig {
    pub capital: CapitalConfig,
    pub standard: StandardConfig,
    pub filters: FiltersConfig,
    pub network: NetworkConfig,
    pub kelly: KellyConfig,
    pub regime: RegimeConfig,
    pub exit: ExitConfig,
    pub ai: AiConfig,
    pub maker: MakerConfig,
    pub arb: ArbConfig,
}

// ── Runtime config ────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug)]
pub struct Config {
    pub capital: CapitalConfig,
    pub standard: StandardConfig,
    pub filters: FiltersConfig,
    pub network: NetworkConfig,
    pub kelly: KellyConfig,
    pub regime: RegimeConfig,
    pub exit: ExitConfig,
    pub ai: AiConfig,
    pub maker: MakerConfig,
    pub arb: ArbConfig,
    /// Polygon EOA private key — from .env, never logged.
    pub private_key: String,
    /// Anthropic API key — from .env, never logged.
    pub anthropic_api_key: String,
    /// Groq API key — from .env, never logged.
    pub groq_api_key: String,
    /// Kalshi credentials — from .env, never logged.
    pub kalshi_email: String,
    pub kalshi_password: String,
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
        // Kalshi creds optional — arb runs in alert-only mode without them.
        let kalshi_email = std::env::var("KALSHI_EMAIL").unwrap_or_default();
        let kalshi_password = std::env::var("KALSHI_PASSWORD").unwrap_or_default();

        let raw = config::Config::builder()
            .add_source(config::File::with_name("config"))
            .build()
            .context("Failed to load config.toml")?;

        let toml: TomlConfig = raw.try_deserialize().context("Failed to parse config.toml")?;

        Ok(Config {
            capital: toml.capital,
            standard: toml.standard,
            filters: toml.filters,
            network: toml.network,
            kelly: toml.kelly,
            regime: toml.regime,
            exit: toml.exit,
            ai: toml.ai,
            maker: toml.maker,
            arb: toml.arb,
            private_key,
            anthropic_api_key,
            groq_api_key,
            kalshi_email,
            kalshi_password,
        })
    }
}
