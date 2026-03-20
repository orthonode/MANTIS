use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::Deserialize;

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

/// Fields loaded from config.toml only (no secrets).
#[derive(Debug, Deserialize)]
struct TomlConfig {
    pub capital: CapitalConfig,
    pub signal: SignalConfig,
    pub filters: FiltersConfig,
    pub network: NetworkConfig,
}

/// Full runtime config: TOML fields + env secrets.
#[allow(dead_code)]
#[derive(Debug)]
pub struct Config {
    pub capital: CapitalConfig,
    pub signal: SignalConfig,
    pub filters: FiltersConfig,
    pub network: NetworkConfig,
    pub private_key: String,
    pub anthropic_api_key: String,
}

impl Config {
    pub fn load() -> Result<Self> {
        dotenvy::dotenv().ok();

        let private_key =
            std::env::var("PRIVATE_KEY").context("PRIVATE_KEY not set in .env")?;
        let anthropic_api_key =
            std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set in .env")?;

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
            private_key,
            anthropic_api_key,
        })
    }
}
