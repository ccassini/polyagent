use std::collections::VecDeque;

use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use polyhft_core::types::BookLevel;

#[derive(Debug, Clone, Default)]
pub struct NormalizedQuote {
    pub ts_exchange_ms: i64,
    pub ts_received_ms: i64,
    pub best_bid: Option<BookLevel>,
    pub best_ask: Option<BookLevel>,
}

impl NormalizedQuote {
    pub fn mid(&self) -> Option<Decimal> {
        self.best_bid
            .as_ref()
            .zip(self.best_ask.as_ref())
            .map(|(b, a)| (b.price + a.price) / dec!(2))
    }

    pub fn spread_fraction(&self) -> Option<Decimal> {
        let bid = self.best_bid.as_ref()?;
        let ask = self.best_ask.as_ref()?;
        let mid = (bid.price + ask.price) / dec!(2);
        if mid <= Decimal::ZERO {
            return None;
        }
        Some((ask.price - bid.price) / mid)
    }

    pub fn ask_depth(&self) -> Decimal {
        self.best_ask
            .as_ref()
            .map(|l| l.size)
            .unwrap_or(Decimal::ZERO)
    }

    pub fn age_ms(&self, now_ms: i64) -> i64 {
        now_ms.saturating_sub(self.ts_received_ms)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MidPoint {
    pub ts_ms: i64,
    pub mid: Decimal,
}

#[derive(Debug, Clone, Default)]
pub struct LegState {
    pub quote: NormalizedQuote,
    pub mids: VecDeque<MidPoint>,
}

#[derive(Debug, Clone, Default)]
pub struct MarketState {
    pub yes: LegState,
    pub no: LegState,
}

#[derive(Debug)]
pub struct MarketDataStore {
    books: DashMap<String, MarketState>,
    max_history_points: usize,
}

impl MarketDataStore {
    pub fn new(max_history_points: usize) -> Self {
        Self {
            books: DashMap::new(),
            max_history_points,
        }
    }

    pub fn update_quote(
        &self,
        condition_id: &str,
        is_yes_leg: bool,
        bids: &[BookLevel],
        asks: &[BookLevel],
        ts_exchange_ms: i64,
        ts_received_ms: i64,
    ) {
        let mut entry = self.books.entry(condition_id.to_string()).or_default();
        let leg = if is_yes_leg {
            &mut entry.yes
        } else {
            &mut entry.no
        };
        leg.quote = NormalizedQuote {
            ts_exchange_ms,
            ts_received_ms,
            best_bid: bids.first().cloned(),
            best_ask: asks.first().cloned(),
        };
        if let Some(mid) = leg.quote.mid() {
            leg.mids.push_back(MidPoint {
                ts_ms: ts_received_ms,
                mid,
            });
            while leg.mids.len() > self.max_history_points {
                let _ = leg.mids.pop_front();
            }
        }
    }

    pub fn market_state(&self, condition_id: &str) -> Option<MarketState> {
        self.books.get(condition_id).map(|m| m.clone())
    }
}
