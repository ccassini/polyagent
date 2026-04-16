use polymarket_client_sdk::clob::types::Side as ClobSide;
use polymarket_client_sdk::clob::ws::types::response::{
    BookUpdate, LastTradePrice, OrderMessage, PriceChange, TradeMessage, TradeMessageStatus,
};
use rust_decimal::Decimal;

use polyhft_core::clock::normalize_exchange_ts_ms;
use polyhft_core::types::{
    BookLevel, LastTradePriceEvent, MarketBookEvent, MarketPriceChangeEvent, Side, SportsEvent,
    TradeLifecycle, UserOrderEvent, UserTradeEvent,
};

pub fn normalize_book(event: BookUpdate) -> MarketBookEvent {
    MarketBookEvent {
        condition_id: event.market.to_string(),
        token_id: event.asset_id.to_string(),
        bids: event
            .bids
            .into_iter()
            .map(|x| BookLevel {
                price: x.price,
                size: x.size,
            })
            .collect(),
        asks: event
            .asks
            .into_iter()
            .map(|x| BookLevel {
                price: x.price,
                size: x.size,
            })
            .collect(),
        ts_ms: normalize_exchange_ts_ms(event.timestamp),
    }
}

pub fn normalize_price_change(event: PriceChange) -> Vec<MarketPriceChangeEvent> {
    let mut out = Vec::with_capacity(event.price_changes.len());
    for change in event.price_changes {
        out.push(MarketPriceChangeEvent {
            condition_id: event.market.to_string(),
            token_id: change.asset_id.to_string(),
            side: map_side(change.side),
            price: change.price,
            size: change.size,
            best_bid: change.best_bid,
            best_ask: change.best_ask,
            ts_ms: normalize_exchange_ts_ms(event.timestamp),
        });
    }
    out
}

pub fn normalize_last_trade(event: LastTradePrice) -> LastTradePriceEvent {
    LastTradePriceEvent {
        condition_id: event.market.to_string(),
        token_id: event.asset_id.to_string(),
        price: event.price,
        side: event.side.map(map_side),
        size: event.size,
        fee_rate_bps: event.fee_rate_bps,
        ts_ms: normalize_exchange_ts_ms(event.timestamp),
    }
}

pub fn normalize_order(event: OrderMessage) -> UserOrderEvent {
    UserOrderEvent {
        order_id: event.id,
        condition_id: event.market.to_string(),
        token_id: event.asset_id.to_string(),
        side: map_side(event.side),
        price: event.price,
        original_size: event.original_size.unwrap_or(Decimal::ZERO),
        matched_size: event.size_matched.unwrap_or(Decimal::ZERO),
        status: format!("{:?}", event.status),
        ts_ms: normalize_exchange_ts_ms(event.timestamp.unwrap_or_default()),
    }
}

pub fn normalize_trade(event: TradeMessage) -> UserTradeEvent {
    UserTradeEvent {
        trade_id: event.id,
        order_id: event.taker_order_id,
        condition_id: event.market.to_string(),
        token_id: event.asset_id.to_string(),
        side: map_side(event.side),
        size: event.size,
        price: event.price,
        status: map_trade_status(event.status),
        taker_or_maker: event.trader_side.map(|x| format!("{:?}", x)),
        tx_hash: event.transaction_hash.map(|x| x.to_string()),
        ts_ms: normalize_exchange_ts_ms(
            event
                .timestamp
                .unwrap_or_else(|| event.last_update.unwrap_or_default()),
        ),
    }
}

pub fn normalize_sports(payload: serde_json::Value, ts_ms: i64) -> SportsEvent {
    let event_type = payload
        .get("event_type")
        .and_then(|v| v.as_str())
        .unwrap_or("sports_event")
        .to_string();
    let market = payload
        .get("market")
        .and_then(|v| v.as_str())
        .map(ToString::to_string);

    SportsEvent {
        event_type,
        market,
        payload,
        ts_ms,
    }
}

fn map_side(side: ClobSide) -> Side {
    // Avoid heap allocation: match directly on the enum discriminant
    // instead of formatting to a string and comparing.
    // ClobSide is non-exhaustive so a wildcard arm is required.
    match side {
        ClobSide::Buy => Side::Buy,
        ClobSide::Sell => Side::Sell,
        _ => Side::Buy, // safe default for any future SDK variants
    }
}

fn map_trade_status(status: TradeMessageStatus) -> TradeLifecycle {
    match status {
        TradeMessageStatus::Matched => TradeLifecycle::Matched,
        TradeMessageStatus::Mined => TradeLifecycle::Mined,
        TradeMessageStatus::Confirmed => TradeLifecycle::Confirmed,
        TradeMessageStatus::Unknown(_) => TradeLifecycle::Unknown,
        _ => TradeLifecycle::Unknown,
    }
}
