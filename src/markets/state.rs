use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rust_decimal::Decimal;
use std::sync::Arc;

// ── Market classification ────────────────────────────────────────────────────

/// FLASH markets resolve via Chainlink oracle (5-min BTC/ETH UP/DOWN windows).
/// STANDARD markets resolve via any other mechanism (news, events, etc.).
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarketType {
    /// 5-minute BTC/ETH UP/DOWN market resolved by Chainlink Data Streams.
    /// Edge: Chainlink oracle lag vs slow retail crowd on Polymarket UI.
    Flash,
    /// Any non-flash market: politics, finance, sports, macro events, etc.
    /// Edge: timezone arbitrage, primary source read-ahead during US sleep hours.
    Standard,
}

// ── Core market snapshot ─────────────────────────────────────────────────────

/// Live snapshot of a Polymarket market, updated by all feed tasks.
///
/// Stored in `MarketState` keyed by `condition_id`.
/// Token-level data (YES/NO) is stored inline — CLOB orderbook events
/// arrive per token_id; use `token_index` in `MarketState` for fast lookup.
///
/// MONEY: all USD values use `rust_decimal::Decimal` — never f64.
/// TIME:  all timestamps use `chrono::DateTime<Utc>` — never raw i64.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct MarketSnapshot {
    /// Polymarket condition ID — primary key, hex string.
    pub condition_id: String,

    /// Market question text shown to traders.
    /// Used by Claude/Groq for standard market scoring.
    pub question: String,

    /// Additional market description or context.
    /// Passed to AI scorers for richer signal quality.
    pub description: Option<String>,

    /// Polymarket token ID for the YES outcome.
    /// CLOB orderbook subscribes per token_id, not condition_id.
    pub token_id_yes: String,

    /// Polymarket token ID for the NO outcome.
    pub token_id_no: String,

    /// Current YES price (crowd-implied probability). Range: 0.00 – 1.00.
    /// Standard entry filter: 0.10 – 0.45 (avoid heavy favourites).
    pub yes_price: Decimal,

    /// Current NO price. Typically 1 - yes_price but can deviate slightly
    /// due to bid/ask spread in the CLOB.
    pub no_price: Decimal,

    /// Total traded volume in USD for this market.
    /// Flash minimum: $500. Standard minimum: $1,000.
    pub volume: Decimal,

    /// Best bid on the YES token (highest price a buyer will pay).
    /// Used to estimate entry slippage and liquidity Kelly multiplier.
    pub best_bid: Option<Decimal>,

    /// Best ask on the YES token (lowest price a seller will accept).
    pub best_ask: Option<Decimal>,

    /// UTC timestamp when this market resolves.
    /// Flash entry window: 30s – 120s to resolution.
    /// Standard entry window: up to 6h (or 24h if AI score >= 85).
    pub resolution_time: DateTime<Utc>,

    /// Whether this is a Flash or Standard market (set by classifier).
    pub market_type: MarketType,

    /// UTC timestamp of the last update to any field in this snapshot.
    /// Used to detect stale data from disconnected feeds.
    pub last_updated: DateTime<Utc>,

    /// True if the market has closed or been resolved.
    /// Closed markets are pruned from MarketState by the scanner task.
    pub is_closed: bool,
}

#[allow(dead_code)]
impl MarketSnapshot {
    /// Seconds until this market resolves from now. Returns 0 if already past.
    pub fn seconds_to_resolution(&self) -> u64 {
        let now = Utc::now();
        if self.resolution_time <= now {
            return 0;
        }
        (self.resolution_time - now).num_seconds().max(0) as u64
    }

    /// Spread between best ask and best bid on the YES token.
    /// None if either side of the book is absent.
    pub fn spread(&self) -> Option<Decimal> {
        match (self.best_ask, self.best_bid) {
            (Some(ask), Some(bid)) => Some(ask - bid),
            _ => None,
        }
    }
}

// ── Broadcast events ─────────────────────────────────────────────────────────

/// Events emitted by feed tasks and consumed by signal + trader tasks.
///
/// Channel type: `tokio::sync::broadcast` (many consumers).
/// Capacity: 1024 (set in main.rs).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum MarketEvent {
    /// YES price updated from CLOB price_change or best_bid_ask event.
    PriceUpdate {
        condition_id: String,
        yes_price: Decimal,
    },

    /// Best bid/ask on YES token updated from CLOB book event.
    BestBidAsk {
        condition_id: String,
        best_bid: Decimal,
        best_ask: Decimal,
    },

    /// New market discovered by Gamma poller and added to state.
    NewMarket { condition_id: String },

    /// Market resolved on-chain. Signal and trader tasks should close positions.
    MarketResolved {
        condition_id: String,
        /// Winning outcome: "YES" or "NO".
        outcome: String,
    },
}

// ── State container ──────────────────────────────────────────────────────────

/// Shared live market state. Wrapped in `Arc` so all tasks can hold a clone.
///
/// Primary index: condition_id → MarketSnapshot.
/// Token index: token_id → condition_id (for fast CLOB event routing).
///
/// Both maps use `DashMap` — concurrent reads/writes without a global lock.
#[derive(Debug, Clone)]
pub struct MarketState {
    /// condition_id → snapshot. Updated by all feed tasks.
    pub markets: Arc<DashMap<String, MarketSnapshot>>,

    /// token_id → condition_id. Populated when markets are added.
    /// Lets CLOB WS events (which arrive keyed by token_id) quickly
    /// find and update the correct MarketSnapshot.
    pub token_index: Arc<DashMap<String, String>>,
}

#[allow(dead_code)]
impl MarketState {
    pub fn new() -> Self {
        Self {
            markets: Arc::new(DashMap::new()),
            token_index: Arc::new(DashMap::new()),
        }
    }

    /// Insert or replace a market snapshot and update the token index.
    pub fn upsert(&self, snapshot: MarketSnapshot) {
        self.token_index
            .insert(snapshot.token_id_yes.clone(), snapshot.condition_id.clone());
        self.token_index
            .insert(snapshot.token_id_no.clone(), snapshot.condition_id.clone());
        self.markets.insert(snapshot.condition_id.clone(), snapshot);
    }

    /// Look up a condition_id from a token_id.
    pub fn condition_id_for_token(&self, token_id: &str) -> Option<String> {
        self.token_index.get(token_id).map(|r| r.value().clone())
    }

    /// Number of open (non-closed) markets currently tracked.
    pub fn open_market_count(&self) -> usize {
        self.markets.iter().filter(|e| !e.value().is_closed).count()
    }

    /// Remove markets that are closed or resolved. Called periodically by scanner.
    pub fn prune_closed(&self) {
        self.markets.retain(|_, v| !v.is_closed);
    }
}

impl Default for MarketState {
    fn default() -> Self {
        Self::new()
    }
}
