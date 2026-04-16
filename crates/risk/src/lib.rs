use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};

use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal::prelude::Signed;
use rust_decimal_macros::dec;
use tracing::warn;

use chrono::Utc;

use polyhft_core::clock::MonotonicClock;
use polyhft_core::config::{AppConfig, RiskConfig};
use polyhft_core::types::{
    DecisionBatch, ExecutionReport, MarketMeta, NormalizedEvent, QuoteIntent, Side, TakerIntent,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Underlying {
    Btc,
    Eth,
    Sol,
    Other,
}

#[derive(Debug, Clone, Default)]
struct MarketInventory {
    yes: Decimal,
    no: Decimal,
}

#[derive(Debug, Clone)]
struct MarketInfo {
    underlying: Underlying,
    yes_token_id: String,
    no_token_id: String,
}

struct MarketMaps {
    market_info: HashMap<String, MarketInfo>,
    token_to_condition: HashMap<String, (String, bool)>,
    market_end_times: HashMap<String, Option<chrono::DateTime<Utc>>>,
}

/// Build the three lookup maps used by `RiskManager` from a slice of markets.
/// Extracted to eliminate the identical code in `new()` and `replace_markets()`.
fn build_market_maps(markets: &[MarketMeta]) -> MarketMaps {
    let mut market_info = HashMap::with_capacity(markets.len());
    let mut token_to_condition = HashMap::with_capacity(markets.len() * 2);
    let mut market_end_times = HashMap::with_capacity(markets.len());

    for market in markets {
        let underlying = if contains_any(&market.question, &["bitcoin", "btc"]) {
            Underlying::Btc
        } else if contains_any(&market.question, &["ethereum", "eth"]) {
            Underlying::Eth
        } else if contains_any(&market.question, &["solana", "sol"]) {
            Underlying::Sol
        } else {
            Underlying::Other
        };

        market_info.insert(
            market.condition_id.clone(),
            MarketInfo {
                underlying,
                yes_token_id: market.yes_token_id.clone(),
                no_token_id: market.no_token_id.clone(),
            },
        );
        token_to_condition.insert(
            market.yes_token_id.clone(),
            (market.condition_id.clone(), true),
        );
        token_to_condition.insert(
            market.no_token_id.clone(),
            (market.condition_id.clone(), false),
        );
        market_end_times.insert(market.condition_id.clone(), market.end_time);
    }

    MarketMaps {
        market_info,
        token_to_condition,
        market_end_times,
    }
}

#[derive(Debug)]
pub struct RiskManager {
    cfg: RiskConfig,
    taker_cooldown_ms: u64,
    inventory: DashMap<String, MarketInventory>,
    last_taker_ms: DashMap<String, i64>,
    market_info: HashMap<String, MarketInfo>,
    token_to_condition: HashMap<String, (String, bool)>,
    /// Market end times for `min_seconds_to_market_end` enforcement.
    market_end_times: HashMap<String, Option<chrono::DateTime<Utc>>>,
    last_event_ts_ms: i64,
    peak_equity: Decimal,
    equity: Decimal,
    stale_warn_ms: AtomicI64,
    drawdown_warn_ms: AtomicI64,
    corr_warn_ms: AtomicI64,
}

impl RiskManager {
    #[must_use]
    pub fn new(cfg: &AppConfig, markets: &[MarketMeta]) -> Self {
        let maps = build_market_maps(markets);
        Self {
            cfg: cfg.risk.clone(),
            taker_cooldown_ms: cfg.execution.taker_rebalance_cooldown_ms,
            inventory: DashMap::new(),
            last_taker_ms: DashMap::new(),
            market_info: maps.market_info,
            token_to_condition: maps.token_to_condition,
            market_end_times: maps.market_end_times,
            last_event_ts_ms: 0,
            peak_equity: Decimal::ZERO,
            equity: Decimal::ZERO,
            stale_warn_ms: AtomicI64::new(0),
            drawdown_warn_ms: AtomicI64::new(0),
            corr_warn_ms: AtomicI64::new(0),
        }
    }

    pub fn on_event(&mut self, _event: &NormalizedEvent) {
        // Use local wall-clock receipt time, not payload timestamp.
        // Pyth `publish_time` can lag wall clock by minutes and would
        // falsely trip the stale_data_kill_ms guard while the agent is healthy.
        self.last_event_ts_ms = MonotonicClock::unix_time_ms();
    }

    pub fn replace_markets(&mut self, markets: &[MarketMeta]) {
        let maps = build_market_maps(markets);

        // Retain inventory for markets that are still open but not in the new
        // market list (e.g. delayed rotation). We carry their lookup info
        // forward so position limits still work until they are flushed.
        let mut final_info = maps.market_info;
        let mut final_tokens = maps.token_to_condition;
        let mut final_end_times = maps.market_end_times;

        for entry in &self.inventory {
            if entry.yes.is_zero() && entry.no.is_zero() {
                continue;
            }
            if final_info.contains_key(entry.key()) {
                continue;
            }
            if let Some(info) = self.market_info.get(entry.key()) {
                final_info.insert(entry.key().clone(), info.clone());
                final_tokens
                    .insert(info.yes_token_id.clone(), (entry.key().clone(), true));
                final_tokens
                    .insert(info.no_token_id.clone(), (entry.key().clone(), false));
                final_end_times.insert(
                    entry.key().clone(),
                    self.market_end_times
                        .get(entry.key())
                        .copied()
                        .flatten()
                        .map(Some)
                        .unwrap_or(None),
                );
            }
        }

        self.market_info = final_info;
        self.token_to_condition = final_tokens;
        self.market_end_times = final_end_times;
    }

    pub fn on_execution(&mut self, report: &ExecutionReport) {
        if let Some((condition_id, is_yes)) = self.token_to_condition.get(&report.token_id) {
            let mut inv = self.inventory.entry(condition_id.clone()).or_default();
            let qty = report.size;
            match (is_yes, report.side) {
                (true, Side::Buy) => inv.yes += qty,
                (true, Side::Sell) => inv.yes -= qty,
                (false, Side::Buy) => inv.no += qty,
                (false, Side::Sell) => inv.no -= qty,
            }
        }

        self.equity += report.spread_capture + report.rebate_earned
            - report.fee_paid
            - report.adverse_selection_cost;
        if self.equity > self.peak_equity {
            self.peak_equity = self.equity;
        }
    }

    pub fn apply_limits(
        &self,
        now_ms: i64,
        batch: &DecisionBatch,
        prices: &HashMap<String, Decimal>,
    ) -> DecisionBatch {
        // ── Stale data kill-switch ─────────────────────────────────────────
        if self.last_event_ts_ms > 0
            && now_ms.saturating_sub(self.last_event_ts_ms) > self.cfg.stale_data_kill_ms as i64
        {
            self.warn_rate_limited(&self.stale_warn_ms, now_ms, || {
                warn!(
                    now_ms,
                    last_event = self.last_event_ts_ms,
                    stale_ms = now_ms - self.last_event_ts_ms,
                    "risk kill-switch: ingestion idle (no events within stale_data_kill_ms)"
                )
            });
            return DecisionBatch::default();
        }

        // ── Max drawdown kill-switch ───────────────────────────────────────
        if (self.peak_equity - self.equity) >= self.cfg.max_drawdown_usdc {
            self.warn_rate_limited(&self.drawdown_warn_ms, now_ms, || {
                warn!(
                    peak_equity = %self.peak_equity,
                    equity = %self.equity,
                    max_drawdown = %self.cfg.max_drawdown_usdc,
                    "risk kill-switch: max drawdown exceeded"
                )
            });
            return DecisionBatch::default();
        }

        // ── Correlation block ─────────────────────────────────────────────
        if self.correlation_risk_blocked() {
            self.warn_rate_limited(&self.corr_warn_ms, now_ms, || {
                warn!("risk block: BTC/ETH/SOL directional correlation exposure limit exceeded")
            });
            return DecisionBatch::default();
        }

        // ── Per-intent filtering ──────────────────────────────────────────
        let mut out = DecisionBatch::default();

        for quote in &batch.quote_intents {
            if self.market_near_end(&quote.condition_id, now_ms) && quote.side != Side::Sell {
                continue;
            }
            if self.accept_quote(quote, prices) {
                out.quote_intents.push(quote.clone());
            }
        }

        for taker in &batch.taker_intents {
            if self.market_near_end(&taker.condition_id, now_ms) && taker.side != Side::Sell {
                continue;
            }
            if self.accept_taker(now_ms, taker, prices) {
                out.taker_intents.push(taker.clone());
            }
        }

        out
    }

    /// Returns `true` when the market ends within `min_seconds_to_market_end` seconds.
    fn market_near_end(&self, condition_id: &str, now_ms: i64) -> bool {
        let Some(Some(end_time)) = self.market_end_times.get(condition_id) else {
            return false;
        };
        let end_ms = end_time.timestamp_millis();
        let remaining_ms = end_ms.saturating_sub(now_ms);
        let threshold_ms = self.cfg.min_seconds_to_market_end * 1_000;
        remaining_ms < threshold_ms
    }

    fn accept_quote(&self, quote: &QuoteIntent, prices: &HashMap<String, Decimal>) -> bool {
        let px = prices.get(&quote.token_id).copied().unwrap_or(quote.price);
        let notional = px * quote.size;

        if notional > self.cfg.max_notional_per_market {
            return false;
        }

        let gross = self.total_gross_notional(prices);
        if gross + notional > self.cfg.max_total_notional {
            return false;
        }

        let (condition_id, is_yes_token) = self
            .token_to_condition
            .get(&quote.token_id)
            .map(|(cid, is_yes)| (cid.as_str(), *is_yes))
            .unwrap_or((quote.condition_id.as_str(), true));

        let inv = self
            .inventory
            .get(condition_id)
            .map(|v| v.clone())
            .unwrap_or_default();
        let cur_pos = if is_yes_token { inv.yes } else { inv.no };
        let projected = match quote.side {
            Side::Buy => cur_pos + quote.size,
            Side::Sell => cur_pos - quote.size,
        };

        projected.abs() <= self.cfg.inventory_hard_limit
    }

    fn accept_taker(
        &self,
        now_ms: i64,
        taker: &TakerIntent,
        prices: &HashMap<String, Decimal>,
    ) -> bool {
        let cooldown_applies = taker.side == Side::Buy && self.taker_cooldown_ms > 0;
        if cooldown_applies {
            let last = self.last_taker_ms.get(&taker.token_id).map(|v| *v).unwrap_or(0);
            if last > 0 && now_ms.saturating_sub(last) < self.taker_cooldown_ms as i64 {
                return false;
            }
        }

        let px = prices.get(&taker.token_id).copied().unwrap_or(dec!(0.5));
        if px <= Decimal::ZERO {
            return false;
        }

        let shares = (taker.usdc_notional / px).round_dp(2);

        let (condition_id, is_yes_token) = self
            .token_to_condition
            .get(&taker.token_id)
            .map(|(cid, is_yes)| (cid.as_str(), *is_yes))
            .unwrap_or((taker.condition_id.as_str(), true));

        let inv = self
            .inventory
            .get(condition_id)
            .map(|v| v.clone())
            .unwrap_or_default();
        let cur_pos = if is_yes_token { inv.yes } else { inv.no };

        let projected = match taker.side {
            Side::Buy => cur_pos + shares,
            Side::Sell => cur_pos - shares,
        };

        if projected.abs() > self.cfg.inventory_hard_limit {
            return false;
        }

        if cooldown_applies {
            self.last_taker_ms.insert(taker.token_id.clone(), now_ms);
        }

        true
    }

    fn total_gross_notional(&self, prices: &HashMap<String, Decimal>) -> Decimal {
        let mut total = Decimal::ZERO;
        for entry in &self.inventory {
            let Some(info) = self.market_info.get(entry.key()) else {
                continue;
            };
            let yes_px = prices.get(&info.yes_token_id).copied().unwrap_or(dec!(0.5));
            let no_px = prices.get(&info.no_token_id).copied().unwrap_or(dec!(0.5));
            total += entry.yes.abs() * yes_px + entry.no.abs() * no_px;
        }
        total
    }

    /// Block new orders when 2+ correlated underlyings (BTC/ETH/SOL) are
    /// all leaning the same direction above `inventory_soft_limit`.
    ///
    /// The `correlation_limit_btc_eth` config value is used as a gate:
    /// - values < 1.0  → block is active (default 0.85)
    /// - value  = 1.0  → block is disabled (set this to opt out)
    fn correlation_risk_blocked(&self) -> bool {
        // Gate: setting correlation_limit_btc_eth = 1.0 disables this check entirely.
        if self.cfg.correlation_limit_btc_eth >= dec!(1) {
            return false;
        }

        let mut btc_exposure = Decimal::ZERO;
        let mut eth_exposure = Decimal::ZERO;
        let mut sol_exposure = Decimal::ZERO;

        for entry in &self.inventory {
            let net = entry.yes - entry.no;
            match self
                .market_info
                .get(entry.key())
                .map(|x| x.underlying)
                .unwrap_or(Underlying::Other)
            {
                Underlying::Btc => btc_exposure += net,
                Underlying::Eth => eth_exposure += net,
                Underlying::Sol => sol_exposure += net,
                Underlying::Other => {}
            }
        }

        let active: Vec<Decimal> = [btc_exposure, eth_exposure, sol_exposure]
            .into_iter()
            .filter(|exp| !exp.is_zero() && exp.abs() > self.cfg.inventory_soft_limit)
            .collect();

        if active.len() < 2 {
            return false;
        }

        let first_sign = active[0].signum();
        active.iter().all(|exp| exp.signum() == first_sign)
    }

    fn warn_rate_limited<F>(&self, gate: &AtomicI64, now_ms: i64, emit: F)
    where
        F: FnOnce(),
    {
        let last = gate.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) >= 1_000
            && gate
                .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            emit();
        }
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    let lower = haystack.to_ascii_lowercase();
    needles.iter().any(|n| contains_token(&lower, n))
}

fn contains_token(haystack_lower: &str, needle_lower: &str) -> bool {
    if needle_lower.len() > 3 {
        return haystack_lower.contains(needle_lower);
    }

    let hb = haystack_lower.as_bytes();
    let nb = needle_lower.as_bytes();
    if hb.len() < nb.len() {
        return false;
    }

    hb.windows(nb.len()).enumerate().any(|(i, w)| {
        if w != nb {
            return false;
        }
        let left_ok = i == 0 || !hb[i - 1].is_ascii_alphanumeric();
        let right_idx = i + nb.len();
        let right_ok = right_idx >= hb.len() || !hb[right_idx].is_ascii_alphanumeric();
        left_ok && right_ok
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use polyhft_core::config::{AppConfig, RiskConfig};
    use polyhft_core::types::{
        ExecutionReport, FeeSchedule, MarketMeta, QuoteIntent, Side, TakerIntent, TradeLifecycle,
    };
    use rust_decimal_macros::dec;
    use uuid::Uuid;

    fn test_market(condition_id: &str, question: &str) -> MarketMeta {
        MarketMeta {
            market_id: format!("m-{condition_id}"),
            condition_id: condition_id.to_string(),
            slug: format!("{condition_id}-slug"),
            question: question.to_string(),
            fee_type: Some("balanced".to_string()),
            fees_enabled: true,
            maker_base_fee_bps: 0,
            taker_base_fee_bps: 0,
            maker_rebates_fee_share_bps: Some(5000),
            fee_schedule: FeeSchedule::default(),
            yes_token_id: format!("{condition_id}-yes"),
            no_token_id: format!("{condition_id}-no"),
            tick_size: dec!(0.01),
            min_order_size: dec!(1),
            event_start_time: Some(Utc::now()),
            end_time: Some(Utc::now() + chrono::Duration::minutes(5)),
        }
    }

    fn cfg_with_risk(risk: RiskConfig) -> AppConfig {
        AppConfig {
            risk,
            ..AppConfig::default()
        }
    }

    fn default_risk() -> RiskConfig {
        RiskConfig {
            max_notional_per_market: dec!(1000),
            max_total_notional: dec!(3000),
            inventory_soft_limit: dec!(200),
            inventory_hard_limit: dec!(500),
            max_drawdown_usdc: dec!(100),
            stale_data_kill_ms: 5000,
            correlation_limit_btc_eth: dec!(0.85),
            max_open_orders: 40,
            min_seconds_to_market_end: 10,
        }
    }

    fn buy_quote(condition_id: &str, token_id: &str, size: Decimal, price: Decimal) -> QuoteIntent {
        QuoteIntent {
            condition_id: condition_id.to_string(),
            token_id: token_id.to_string(),
            side: Side::Buy,
            price,
            size,
            post_only: true,
            time_in_force: "GTC".to_string(),
            reason: "test".to_string(),
        }
    }

    fn buy_taker(condition_id: &str, token_id: &str, usdc_notional: Decimal) -> TakerIntent {
        TakerIntent {
            condition_id: condition_id.to_string(),
            token_id: token_id.to_string(),
            side: Side::Buy,
            usdc_notional,
            reason: "test".to_string(),
        }
    }

    fn fill_report(
        condition_id: &str,
        token_id: &str,
        side: Side,
        size: Decimal,
        price: Decimal,
        spread_capture: Decimal,
    ) -> ExecutionReport {
        ExecutionReport {
            id: Uuid::new_v4(),
            timestamp_ms: 0,
            condition_id: condition_id.to_string(),
            token_id: token_id.to_string(),
            side,
            price,
            size,
            maker: true,
            lifecycle: TradeLifecycle::Confirmed,
            fee_paid: Decimal::ZERO,
            rebate_earned: Decimal::ZERO,
            spread_capture,
            adverse_selection_cost: Decimal::ZERO,
            venue_order_id: None,
            tx_hash: None,
            reason: "test".to_string(),
        }
    }

    #[test]
    fn taker_cooldown_blocks_same_token_reentry() {
        let mut cfg = AppConfig::default();
        cfg.execution.taker_rebalance_cooldown_ms = 2_000;
        cfg.risk = default_risk();

        let market = test_market("cond-cooldown", "Bitcoin Up or Down 5m?");
        let risk = RiskManager::new(&cfg, std::slice::from_ref(&market));
        let prices = HashMap::from([(market.yes_token_id.clone(), dec!(0.5))]);
        let batch = DecisionBatch {
            quote_intents: vec![],
            taker_intents: vec![buy_taker(&market.condition_id, &market.yes_token_id, dec!(10))],
        };

        let first = risk.apply_limits(1_000, &batch, &prices);
        assert_eq!(first.taker_intents.len(), 1, "first taker should pass");

        let second = risk.apply_limits(2_000, &batch, &prices);
        assert!(
            second.taker_intents.is_empty(),
            "same-token taker should be blocked during cooldown"
        );

        let third = risk.apply_limits(3_200, &batch, &prices);
        assert_eq!(third.taker_intents.len(), 1, "cooldown expiry should allow re-entry");
    }

    #[test]
    fn taker_cooldown_allows_paired_legs_in_same_market() {
        let mut cfg = AppConfig::default();
        cfg.execution.taker_rebalance_cooldown_ms = 2_000;
        cfg.risk = default_risk();

        let market = test_market("cond-paired", "Bitcoin Up or Down 5m?");
        let risk = RiskManager::new(&cfg, std::slice::from_ref(&market));
        let prices = HashMap::from([
            (market.yes_token_id.clone(), dec!(0.5)),
            (market.no_token_id.clone(), dec!(0.5)),
        ]);
        let batch = DecisionBatch {
            quote_intents: vec![],
            taker_intents: vec![
                buy_taker(&market.condition_id, &market.yes_token_id, dec!(10)),
                buy_taker(&market.condition_id, &market.no_token_id, dec!(10)),
            ],
        };

        let result = risk.apply_limits(1_000, &batch, &prices);
        assert_eq!(
            result.taker_intents.len(),
            2,
            "paired YES/NO takers should survive token-level cooldown gating"
        );
    }

    // ── stale data ────────────────────────────────────────────────────────

    #[test]
    fn stale_data_kills_all_decisions() {
        let market = test_market("cond-1", "Will BTC go up?");
        let cfg = cfg_with_risk(RiskConfig {
            stale_data_kill_ms: 1000,
            ..default_risk()
        });
        let mut rm = RiskManager::new(&cfg, &[market.clone()]);

        // Prime last_event_ts_ms at t=0
        rm.last_event_ts_ms = 1000;

        let batch = DecisionBatch {
            quote_intents: vec![buy_quote("cond-1", "cond-1-yes", dec!(10), dec!(0.5))],
            taker_intents: vec![],
        };
        let prices = HashMap::new();

        // Now = 3000 ms → 2000 ms elapsed > 1000 ms stale threshold
        let result = rm.apply_limits(3000, &batch, &prices);
        assert!(result.quote_intents.is_empty(), "stale data should kill quotes");
        assert!(result.taker_intents.is_empty(), "stale data should kill takers");
    }

    // ── drawdown ──────────────────────────────────────────────────────────

    #[test]
    fn drawdown_kills_all_decisions() {
        let market = test_market("cond-1", "Will ETH go up?");
        let cfg = cfg_with_risk(RiskConfig {
            max_drawdown_usdc: dec!(50),
            ..default_risk()
        });
        let mut rm = RiskManager::new(&cfg, &[market.clone()]);

        // Simulate peak equity = 100, then lose 60 (> max_drawdown=50)
        rm.peak_equity = dec!(100);
        rm.equity = dec!(40);

        let batch = DecisionBatch {
            quote_intents: vec![buy_quote("cond-1", "cond-1-yes", dec!(5), dec!(0.5))],
            taker_intents: vec![TakerIntent {
                condition_id: "cond-1".to_string(),
                token_id: "cond-1-yes".to_string(),
                side: Side::Buy,
                usdc_notional: dec!(10),
                reason: "test".to_string(),
            }],
        };
        let prices = HashMap::new();

        let result = rm.apply_limits(9999, &batch, &prices);
        assert!(result.quote_intents.is_empty(), "drawdown should kill quotes");
        assert!(result.taker_intents.is_empty(), "drawdown should kill takers");
    }

    // ── per-market notional cap ───────────────────────────────────────────

    #[test]
    fn per_market_notional_cap_rejects_oversized_quote() {
        let market = test_market("cond-1", "Will BTC go up?");
        let cfg = cfg_with_risk(RiskConfig {
            max_notional_per_market: dec!(100),
            ..default_risk()
        });
        let mut rm = RiskManager::new(&cfg, &[market.clone()]);
        rm.last_event_ts_ms = 999_999_999;

        // size=300 @ price=0.5 → notional=150 > max_notional_per_market=100
        let batch = DecisionBatch {
            quote_intents: vec![buy_quote("cond-1", "cond-1-yes", dec!(300), dec!(0.5))],
            taker_intents: vec![],
        };
        let mut prices = HashMap::new();
        prices.insert("cond-1-yes".to_string(), dec!(0.5));

        let result = rm.apply_limits(999_999_999, &batch, &prices);
        assert!(result.quote_intents.is_empty(), "oversized quote should be rejected");
    }

    #[test]
    fn small_quote_passes_notional_cap() {
        let market = test_market("cond-1", "Will BTC go up?");
        let cfg = cfg_with_risk(RiskConfig {
            max_notional_per_market: dec!(100),
            ..default_risk()
        });
        let mut rm = RiskManager::new(&cfg, &[market.clone()]);
        rm.last_event_ts_ms = 999_999_999;

        // size=10 @ price=0.5 → notional=5 ≪ 100
        let batch = DecisionBatch {
            quote_intents: vec![buy_quote("cond-1", "cond-1-yes", dec!(10), dec!(0.5))],
            taker_intents: vec![],
        };
        let mut prices = HashMap::new();
        prices.insert("cond-1-yes".to_string(), dec!(0.5));

        let result = rm.apply_limits(999_999_999, &batch, &prices);
        assert_eq!(result.quote_intents.len(), 1, "small quote should pass");
    }

    // ── correlation block ─────────────────────────────────────────────────

    #[test]
    fn correlation_block_triggers_when_two_underlyings_same_direction() {
        let btc = test_market("btc-1", "Will BTC go up?");
        let eth = test_market("eth-1", "Will ETH go up?");
        let cfg = cfg_with_risk(default_risk());
        let mut rm = RiskManager::new(&cfg, &[btc.clone(), eth.clone()]);
        rm.last_event_ts_ms = 999_999_999;

        // Both BTC-YES and ETH-YES above soft_limit (200) in same direction
        rm.on_execution(&fill_report("btc-1", "btc-1-yes", Side::Buy, dec!(250), dec!(0.5), dec!(1)));
        rm.on_execution(&fill_report("eth-1", "eth-1-yes", Side::Buy, dec!(250), dec!(0.5), dec!(1)));

        let batch = DecisionBatch {
            quote_intents: vec![buy_quote("btc-1", "btc-1-yes", dec!(5), dec!(0.5))],
            taker_intents: vec![],
        };
        let prices = HashMap::new();

        let result = rm.apply_limits(999_999_999, &batch, &prices);
        assert!(result.quote_intents.is_empty(), "corr block should fire");
    }

    #[test]
    fn correlation_block_disabled_when_limit_set_to_one() {
        let btc = test_market("btc-1", "Will BTC go up?");
        let eth = test_market("eth-1", "Will ETH go up?");
        let cfg = cfg_with_risk(RiskConfig {
            // Setting to 1.0 disables the correlation check
            correlation_limit_btc_eth: dec!(1),
            ..default_risk()
        });
        let mut rm = RiskManager::new(&cfg, &[btc.clone(), eth.clone()]);
        rm.last_event_ts_ms = 999_999_999;

        rm.on_execution(&fill_report("btc-1", "btc-1-yes", Side::Buy, dec!(250), dec!(0.5), dec!(1)));
        rm.on_execution(&fill_report("eth-1", "eth-1-yes", Side::Buy, dec!(250), dec!(0.5), dec!(1)));

        let batch = DecisionBatch {
            quote_intents: vec![buy_quote("btc-1", "btc-1-yes", dec!(5), dec!(0.5))],
            taker_intents: vec![],
        };
        let mut prices = HashMap::new();
        prices.insert("btc-1-yes".to_string(), dec!(0.5));

        let result = rm.apply_limits(999_999_999, &batch, &prices);
        assert_eq!(result.quote_intents.len(), 1, "corr block disabled → quote passes");
    }

    #[test]
    fn correlation_block_does_not_fire_when_directions_opposite() {
        let btc = test_market("btc-1", "Will BTC go up?");
        let eth = test_market("eth-1", "Will ETH go up?");
        let cfg = cfg_with_risk(default_risk());
        let mut rm = RiskManager::new(&cfg, &[btc.clone(), eth.clone()]);
        rm.last_event_ts_ms = 999_999_999;

        // BTC long, ETH short — different directions, no correlation risk
        rm.on_execution(&fill_report("btc-1", "btc-1-yes", Side::Buy, dec!(250), dec!(0.5), dec!(1)));
        rm.on_execution(&fill_report("eth-1", "eth-1-no", Side::Buy, dec!(250), dec!(0.5), dec!(1)));

        let batch = DecisionBatch {
            quote_intents: vec![buy_quote("btc-1", "btc-1-yes", dec!(5), dec!(0.5))],
            taker_intents: vec![],
        };
        let mut prices = HashMap::new();
        prices.insert("btc-1-yes".to_string(), dec!(0.5));

        let result = rm.apply_limits(999_999_999, &batch, &prices);
        assert_eq!(result.quote_intents.len(), 1, "opposite directions → no corr block");
    }

    // ── inventory hard limit ──────────────────────────────────────────────

    #[test]
    fn inventory_hard_limit_blocks_new_buys() {
        let market = test_market("cond-1", "Will SOL go up?");
        let cfg = cfg_with_risk(RiskConfig {
            inventory_hard_limit: dec!(100),
            ..default_risk()
        });
        let mut rm = RiskManager::new(&cfg, &[market.clone()]);
        rm.last_event_ts_ms = 999_999_999;

        // Already at 90 shares
        rm.on_execution(&fill_report("cond-1", "cond-1-yes", Side::Buy, dec!(90), dec!(0.5), Decimal::ZERO));

        // Trying to buy 20 more → projected=110 > hard_limit=100
        let batch = DecisionBatch {
            quote_intents: vec![buy_quote("cond-1", "cond-1-yes", dec!(20), dec!(0.5))],
            taker_intents: vec![],
        };
        let mut prices = HashMap::new();
        prices.insert("cond-1-yes".to_string(), dec!(0.5));

        let result = rm.apply_limits(999_999_999, &batch, &prices);
        assert!(result.quote_intents.is_empty(), "over hard limit → rejected");
    }

    // ── replace_markets retains open positions ───────────────────────────

    #[test]
    fn replace_markets_retains_open_position_info() {
        let old_market = test_market("old-cond", "Will BTC go up?");
        let new_market = test_market("new-cond", "Will ETH go up?");
        let cfg = cfg_with_risk(default_risk());
        let mut rm = RiskManager::new(&cfg, &[old_market.clone()]);

        // Take a position in the old market
        rm.on_execution(&fill_report("old-cond", "old-cond-yes", Side::Buy, dec!(10), dec!(0.5), Decimal::ZERO));

        // Replace with a completely different market set
        rm.replace_markets(&[new_market.clone()]);

        // Old market info should still be retained (non-zero position)
        assert!(
            rm.market_info.contains_key("old-cond"),
            "old market with open position should be retained"
        );
    }
}
