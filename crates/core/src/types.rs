use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Outcome {
    Yes,
    No,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SignatureKind {
    Eoa,
    PolyProxy,
    GnosisSafe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeSchedule {
    pub exponent: Decimal,
    pub rate: Decimal,
    pub taker_only: bool,
    pub rebate_rate: Decimal,
}

impl Default for FeeSchedule {
    fn default() -> Self {
        Self {
            exponent: Decimal::ONE,
            rate: Decimal::ZERO,
            taker_only: true,
            rebate_rate: Decimal::ZERO,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketMeta {
    pub market_id: String,
    pub condition_id: String,
    pub slug: String,
    pub question: String,
    pub fee_type: Option<String>,
    pub fees_enabled: bool,
    pub maker_base_fee_bps: u32,
    pub taker_base_fee_bps: u32,
    pub maker_rebates_fee_share_bps: Option<u32>,
    pub fee_schedule: FeeSchedule,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub tick_size: Decimal,
    pub min_order_size: Decimal,
    pub event_start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookLevel {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookTop {
    pub best_bid: Option<BookLevel>,
    pub best_ask: Option<BookLevel>,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketBookEvent {
    pub condition_id: String,
    pub token_id: String,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketPriceChangeEvent {
    pub condition_id: String,
    pub token_id: String,
    pub side: Side,
    pub price: Decimal,
    pub size: Option<Decimal>,
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastTradePriceEvent {
    pub condition_id: String,
    pub token_id: String,
    pub price: Decimal,
    pub side: Option<Side>,
    pub size: Option<Decimal>,
    pub fee_rate_bps: Option<Decimal>,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserOrderEvent {
    pub order_id: String,
    pub condition_id: String,
    pub token_id: String,
    pub side: Side,
    pub price: Decimal,
    pub original_size: Decimal,
    pub matched_size: Decimal,
    pub status: String,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserTradeEvent {
    pub trade_id: String,
    pub order_id: Option<String>,
    pub condition_id: String,
    pub token_id: String,
    pub side: Side,
    pub size: Decimal,
    pub price: Decimal,
    pub status: TradeLifecycle,
    pub taker_or_maker: Option<String>,
    pub tx_hash: Option<String>,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SportsEvent {
    pub event_type: String,
    pub market: Option<String>,
    pub payload: serde_json::Value,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PriceSource {
    Binance,
    Chainlink,
    Pyth,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtdsPriceEvent {
    pub symbol: String,
    pub value: Decimal,
    pub source: PriceSource,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NormalizedEvent {
    MarketBook(MarketBookEvent),
    PriceChange(MarketPriceChangeEvent),
    LastTradePrice(LastTradePriceEvent),
    UserOrder(UserOrderEvent),
    UserTrade(UserTradeEvent),
    Sports(SportsEvent),
    RtdsPrice(RtdsPriceEvent),
    Heartbeat { ts_ms: i64 },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TradeLifecycle {
    Matched,
    Mined,
    Confirmed,
    Retrying,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteIntent {
    pub condition_id: String,
    pub token_id: String,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub post_only: bool,
    pub time_in_force: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakerIntent {
    pub condition_id: String,
    pub token_id: String,
    pub side: Side,
    pub usdc_notional: Decimal,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DecisionBatch {
    pub quote_intents: Vec<QuoteIntent>,
    pub taker_intents: Vec<TakerIntent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalSnapshot {
    pub condition_id: String,
    pub timestamp_ms: i64,
    pub yes_mid: Option<Decimal>,
    pub no_mid: Option<Decimal>,
    pub yes_no_ask_sum: Option<Decimal>,
    pub mispricing_edge: Decimal,
    pub chainlink_pyth_basis: Option<Decimal>,
    pub adverse_selection_score: Decimal,
    pub fill_probability: Decimal,
    pub queue_position_proxy: Decimal,
    pub directional_bias: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventorySnapshot {
    pub timestamp_ms: i64,
    pub condition_id: String,
    pub yes_position: Decimal,
    pub no_position: Decimal,
    pub net_delta: Decimal,
    pub gross_notional: Decimal,
    pub realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionReport {
    pub id: Uuid,
    pub timestamp_ms: i64,
    pub condition_id: String,
    pub token_id: String,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub maker: bool,
    pub lifecycle: TradeLifecycle,
    pub fee_paid: Decimal,
    pub rebate_earned: Decimal,
    pub spread_capture: Decimal,
    pub adverse_selection_cost: Decimal,
    pub venue_order_id: Option<String>,
    pub tx_hash: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PnLAttribution {
    pub spread_capture: Decimal,
    pub maker_rebate: Decimal,
    pub taker_fee: Decimal,
    pub adverse_selection: Decimal,
    pub slippage: Decimal,
    pub total: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyStats {
    pub ingest_to_signal_ms: u64,
    pub signal_to_order_ms: u64,
    pub order_to_ack_ms: u64,
    pub end_to_end_ms: u64,
}
