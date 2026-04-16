use std::collections::HashMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use polyhft_core::types::Side;

use crate::reason_codes::{ExitReason, OrderEventCode, SkipReason};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Leg {
    Yes,
    No,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionState {
    Open,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub id: Uuid,
    pub condition_id: String,
    pub token_id: String,
    pub leg: Leg,
    pub state: PositionState,
    pub entry_price: Decimal,
    pub size: Decimal,
    pub entry_fee_usdc: Decimal,
    pub entry_ms: i64,
    pub market_expiry_ms: i64,
    pub exit_price: Option<Decimal>,
    pub exit_ms: Option<i64>,
    pub realised_pnl: Option<Decimal>,
    pub exit_reason: Option<ExitReason>,
}

impl Position {
    pub fn age_ms(&self, now_ms: i64) -> i64 {
        now_ms.saturating_sub(self.entry_ms)
    }

    pub fn entry_notional(&self) -> Decimal {
        self.entry_price * self.size
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DecisionFeatures {
    pub ts_exchange_ms: i64,
    pub ts_received_ms: i64,
    pub hours_to_expiry: f64,
    pub quote_age_ms: i64,
    pub best_bid: Decimal,
    pub best_ask: Decimal,
    pub momentum_30s: Decimal,
    pub momentum_2m: Decimal,
    pub volatility_5m: Decimal,
    pub volume_5m: Decimal,
    pub orderbook_imbalance: Decimal,
    pub favorite_side: String,
    pub favorite_probability: Decimal,
    pub spread_fraction: Decimal,
    pub depth: Decimal,
    pub expected_edge_after_costs: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperFill {
    pub id: Uuid,
    pub timestamp_ms: i64,
    pub condition_id: String,
    pub token_id: String,
    pub leg: Leg,
    pub side: Side,
    pub fill_price: Decimal,
    pub requested_size: Decimal,
    pub filled_size: Decimal,
    pub fee_usdc: Decimal,
    pub slippage: Decimal,
    pub fill_delay_ms: i64,
    pub fill_status: OrderEventCode,
    pub order_events: Vec<OrderEventCode>,
    pub features: DecisionFeatures,
    pub skip_reason: Option<SkipReason>,
    pub exit_reason: Option<ExitReason>,
    pub position_id: Option<Uuid>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EngineStats {
    pub markets_evaluated: u64,
    pub entries: u64,
    pub exits: u64,
    pub total_pnl_usdc: Decimal,
    pub total_fees_usdc: Decimal,
    pub skip_counts: HashMap<String, u64>,
    pub order_event_counts: HashMap<String, u64>,
}

impl EngineStats {
    pub fn record_skip(&mut self, reason: SkipReason) {
        *self.skip_counts.entry(reason.as_str().to_string()).or_default() += 1;
    }

    pub fn record_order_event(&mut self, event: OrderEventCode) {
        *self
            .order_event_counts
            .entry(event.as_str().to_string())
            .or_default() += 1;
    }
}
