use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    MarketInactiveOrResolved,
    Expired,
    ExpiryTooFar,
    MissingOrderbook,
    SpreadTooWide,
    LowLiquidity,
    NotFavoriteZone,
    NegativeEdgeAfterCosts,
    PositionExists,
    CooldownActive,
    StaleQuote,
    InsufficientDepth,
    MaxOpenPositions,
    MaxMarketExposure,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MarketInactiveOrResolved => "market_inactive_or_resolved",
            Self::Expired => "expired",
            Self::ExpiryTooFar => "expiry_too_far",
            Self::MissingOrderbook => "missing_orderbook",
            Self::SpreadTooWide => "spread_too_wide",
            Self::LowLiquidity => "low_liquidity",
            Self::NotFavoriteZone => "not_favorite_zone",
            Self::NegativeEdgeAfterCosts => "negative_edge_after_costs",
            Self::PositionExists => "position_exists",
            Self::CooldownActive => "cooldown_active",
            Self::StaleQuote => "stale_quote",
            Self::InsufficientDepth => "insufficient_depth",
            Self::MaxOpenPositions => "max_open_positions",
            Self::MaxMarketExposure => "max_market_exposure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderEventCode {
    OrderSubmitted,
    OrderRejected,
    OrderPending,
    OrderFilled,
    OrderPartiallyFilled,
    OrderCancelled,
    OrderExpired,
}

impl OrderEventCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OrderSubmitted => "order_submitted",
            Self::OrderRejected => "order_rejected",
            Self::OrderPending => "order_pending",
            Self::OrderFilled => "order_filled",
            Self::OrderPartiallyFilled => "order_partially_filled",
            Self::OrderCancelled => "order_cancelled",
            Self::OrderExpired => "order_expired",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    PositionClosedTp,
    PositionClosedSl,
    PositionClosedTimeout,
    PositionClosedForcedExpiry,
    PositionClosedSignalFlip,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PositionClosedTp => "position_closed_tp",
            Self::PositionClosedSl => "position_closed_sl",
            Self::PositionClosedTimeout => "position_closed_timeout",
            Self::PositionClosedForcedExpiry => "position_closed_forced_expiry",
            Self::PositionClosedSignalFlip => "position_closed_signal_flip",
        }
    }
}
