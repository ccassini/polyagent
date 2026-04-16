//! Deterministic paper trading engine for short-horizon Polymarket markets.
//! The pipeline is explicit and observable:
//! MarketData -> Eligibility -> Signal -> Risk -> Execution -> Position -> Analytics

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use rand::{rngs::SmallRng, RngCore, SeedableRng};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::{debug, info};
use uuid::Uuid;

pub use polyhft_core::config::PaperEngineConfig;
use polyhft_core::types::{BookLevel, MarketMeta, Side};

mod market_data;
mod models;
mod reason_codes;

use market_data::{MarketDataStore, MarketState, MidPoint, NormalizedQuote};
pub use models::{DecisionFeatures, EngineStats, Leg, PaperFill, Position, PositionState};
pub use reason_codes::{ExitReason, OrderEventCode, SkipReason};

/// An actionable intent produced by the paper engine's decision logic.
/// In live mode these are converted to real CLOB orders via the ExecutionEngine.
#[derive(Debug, Clone)]
pub struct LiveIntent {
    pub condition_id: String,
    pub token_id: String,
    pub side: Side,
    pub usdc_notional: Decimal,
    pub reason: String,
    pub leg: Leg,
    pub features: DecisionFeatures,
}

const HYPER_MIN_FAVORITE_PROB: Decimal = dec!(0.68);
const HYPER_MAX_HOURS_TO_EXPIRY: f64 = 12.0;
const HYPER_MIN_ACTIVITY_5M: Decimal = dec!(1);
const HYPER_TAKE_PROFIT_FRACTION: Decimal = dec!(0.005);
const HYPER_ENTRY_COOLDOWN_SECS: i64 = 2;
const HYPER_MAX_SPREAD: Decimal = dec!(10.0);

#[derive(Debug, Clone)]
struct SignalCandidate {
    leg: Leg,
    token_id: String,
    best_ask: Decimal,
    features: DecisionFeatures,
}

#[derive(Debug)]
pub struct PaperEngine {
    cfg: PaperEngineConfig,
    market_data: MarketDataStore,
    positions: DashMap<String, Position>,
    last_entry_ms: DashMap<String, i64>,
    exposure: DashMap<String, Decimal>,
    stats: Mutex<EngineStats>,
    rng: Mutex<SmallRng>,
}

impl PaperEngine {
    pub fn new(cfg: PaperEngineConfig, seed: u64) -> Self {
        Self {
            cfg,
            market_data: MarketDataStore::new(1_000),
            positions: DashMap::new(),
            last_entry_ms: DashMap::new(),
            exposure: DashMap::new(),
            stats: Mutex::new(EngineStats::default()),
            rng: Mutex::new(SmallRng::seed_from_u64(seed)),
        }
    }

    fn next_uuid(&self) -> Uuid {
        let mut bytes = [0_u8; 16];
        self.rng.lock().unwrap().fill_bytes(&mut bytes);
        Uuid::from_bytes(bytes)
    }

    pub fn on_book_update(
        &self,
        condition_id: &str,
        is_yes_leg: bool,
        bids: &[BookLevel],
        asks: &[BookLevel],
        ts_ms: i64,
    ) {
        self.on_book_update_with_timestamps(
            condition_id,
            is_yes_leg,
            bids,
            asks,
            ts_ms,
            ts_ms,
        );
    }

    pub fn on_book_update_with_timestamps(
        &self,
        condition_id: &str,
        is_yes_leg: bool,
        bids: &[BookLevel],
        asks: &[BookLevel],
        ts_exchange_ms: i64,
        ts_received_ms: i64,
    ) {
        self.market_data.update_quote(
            condition_id,
            is_yes_leg,
            bids,
            asks,
            ts_exchange_ms,
            ts_received_ms,
        );
    }

    pub fn run_cycle(&self, markets: &[MarketMeta], now_ms: i64) -> Vec<PaperFill> {
        let mut fills = self.manage_positions(markets, now_ms);
        let mut stats = self.stats.lock().unwrap();

        // Per-cycle skip counters for diagnostics.
        let mut cycle_skips: HashMap<&'static str, u32> = HashMap::new();
        let mut cycle_evaluated = 0u32;
        let mut cycle_with_book = 0u32;

        for market in markets {
            stats.markets_evaluated += 1;
            cycle_evaluated += 1;

            if let Err(reason) = self.check_eligibility(market, now_ms) {
                self.emit_skip(reason, market, "eligibility", None);
                stats.record_skip(reason);
                *cycle_skips.entry(reason.as_str()).or_default() += 1;
                continue;
            }

            let Some(market_state) = self.market_data.market_state(&market.condition_id) else {
                let reason = SkipReason::MissingOrderbook;
                self.emit_skip(reason, market, "eligibility", None);
                stats.record_skip(reason);
                *cycle_skips.entry(reason.as_str()).or_default() += 1;
                continue;
            };
            cycle_with_book += 1;

            for leg in [Leg::Yes, Leg::No] {
                let candidate = match self.evaluate_signal(market, &market_state, leg, now_ms) {
                    Ok(c) => c,
                    Err(reason) => {
                        self.emit_skip(reason, market, "signal", None);
                        stats.record_skip(reason);
                        *cycle_skips.entry(reason.as_str()).or_default() += 1;
                        continue;
                    }
                };

                if let Err(reason) = self.check_risk(market, leg, now_ms, &candidate.features) {
                    self.emit_skip(reason, market, "risk", Some(&candidate.features));
                    stats.record_skip(reason);
                    *cycle_skips.entry(reason.as_str()).or_default() += 1;
                    continue;
                }

                match self.simulate_entry(market, &candidate, now_ms) {
                    Ok(fill) => {
                        for event in &fill.order_events {
                            stats.record_order_event(*event);
                        }
                        stats.entries += 1;
                        stats.total_fees_usdc += fill.fee_usdc;
                        self.last_entry_ms
                            .insert(cooldown_key(&market.condition_id, leg), now_ms);
                        *self
                            .exposure
                            .entry(market.condition_id.clone())
                            .or_default() += fill.fill_price * fill.filled_size;
                        info!(
                            decision_type = "entry",
                            reason_code = fill.fill_status.as_str(),
                            condition_id = %market.condition_id,
                            leg = ?leg,
                            fill_price = %fill.fill_price,
                            filled_size = %fill.filled_size,
                            quote_age_ms = fill.features.quote_age_ms,
                            expected_edge_after_costs = %fill.features.expected_edge_after_costs,
                            "paper_engine: order lifecycle completed"
                        );
                        fills.push(fill);
                    }
                    Err((reason, events, features)) => {
                        self.emit_skip(reason, market, "execution", Some(&features));
                        stats.record_skip(reason);
                        *cycle_skips.entry(reason.as_str()).or_default() += 1;
                        for event in events {
                            stats.record_order_event(event);
                        }
                    }
                }
            }
        }

        // Emit a per-cycle summary so operators can see WHY the engine isn't trading.
        if !cycle_skips.is_empty() || !fills.is_empty() {
            let skip_summary: String = cycle_skips
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ");
            let open_count = self
                .positions
                .iter()
                .filter(|p| p.state == PositionState::Open)
                .count();
            debug!(
                markets_total = cycle_evaluated,
                markets_with_book = cycle_with_book,
                new_fills = fills.len(),
                open_positions = open_count,
                skips = %skip_summary,
                "paper_engine: cycle summary"
            );
        }

        fills
    }

    pub fn stats(&self) -> EngineStats {
        self.stats.lock().unwrap().clone()
    }

    pub fn open_positions(&self) -> Vec<Position> {
        self.positions
            .iter()
            .filter(|p| p.state == PositionState::Open)
            .map(|p| p.clone())
            .collect()
    }

    /// Generate live trade intents using the same signal/risk pipeline.
    /// Returns entry and exit intents that can be sent to the ExecutionEngine.
    /// Also manages positions internally for exit tracking.
    pub fn generate_live_intents(
        &self,
        markets: &[MarketMeta],
        now_ms: i64,
    ) -> Vec<LiveIntent> {
        let mut intents = Vec::new();
        let market_map: HashMap<&str, &MarketMeta> = markets
            .iter()
            .map(|m| (m.condition_id.as_str(), m))
            .collect();

        // ── Exit intents for open positions ──────────────────────────────────
        let open_ids: Vec<String> = self
            .positions
            .iter()
            .filter(|p| p.state == PositionState::Open)
            .map(|p| p.key().clone())
            .collect();

        for pos_id in open_ids {
            let Some(pos) = self.positions.get(&pos_id).map(|p| p.clone()) else {
                continue;
            };
            let Some(exit_reason) = self.should_exit(&pos, &market_map, now_ms) else {
                continue;
            };

            // Mark position as closed in our tracking.
            if let Some(mut p) = self.positions.get_mut(&pos_id) {
                if p.state != PositionState::Open {
                    continue;
                }
                p.state = PositionState::Closed;
                p.exit_ms = Some(now_ms);
                p.exit_reason = Some(exit_reason);
            }
            if let Some(mut exp) = self.exposure.get_mut(&pos.condition_id) {
                *exp = (*exp - pos.entry_notional()).max(Decimal::ZERO);
            }

            let mut stats = self.stats.lock().unwrap();
            stats.exits += 1;
            drop(stats);

            info!(
                decision_type = "live_exit_intent",
                reason_code = exit_reason.as_str(),
                condition_id = %pos.condition_id,
                leg = ?pos.leg,
                entry_price = %pos.entry_price,
                size = %pos.size,
                "paper_engine: generating live exit intent"
            );

            intents.push(LiveIntent {
                condition_id: pos.condition_id.clone(),
                token_id: pos.token_id.clone(),
                side: Side::Sell,
                usdc_notional: pos.entry_price * pos.size,
                reason: format!("fav_exit_{}", exit_reason.as_str()),
                leg: pos.leg,
                features: DecisionFeatures::default(),
            });
        }

        // ── Entry intents for new positions ──────────────────────────────────
        let mut stats = self.stats.lock().unwrap();
        for market in markets {
            stats.markets_evaluated += 1;

            if let Err(reason) = self.check_eligibility(market, now_ms) {
                self.emit_skip(reason, market, "eligibility", None);
                stats.record_skip(reason);
                continue;
            }

            let Some(market_state) = self.market_data.market_state(&market.condition_id) else {
                let reason = SkipReason::MissingOrderbook;
                self.emit_skip(reason, market, "eligibility", None);
                stats.record_skip(reason);
                continue;
            };

            for leg in [Leg::Yes, Leg::No] {
                let candidate = match self.evaluate_signal(market, &market_state, leg, now_ms) {
                    Ok(c) => c,
                    Err(reason) => {
                        self.emit_skip(reason, market, "signal", None);
                        stats.record_skip(reason);
                        continue;
                    }
                };

                if let Err(reason) = self.check_risk(market, leg, now_ms, &candidate.features) {
                    self.emit_skip(reason, market, "risk", Some(&candidate.features));
                    stats.record_skip(reason);
                    continue;
                }

                // Pre-validate entry would succeed (depth, spread) before creating intent.
                let features = &candidate.features;
                if features.quote_age_ms > self.cfg.quote_max_stale_ms {
                    continue;
                }
                if features.spread_fraction > self.cfg.max_spread_fraction {
                    continue;
                }

                let token_id = if leg == Leg::Yes {
                    market.yes_token_id.clone()
                } else {
                    market.no_token_id.clone()
                };

                // Track the position internally so we can manage exits.
                let slip = self.cfg.slippage_bps / dec!(10_000);
                let est_fill_price = (candidate.best_ask * (Decimal::ONE + slip)).min(dec!(0.999));
                let est_size = (self.cfg.entry_usdc / est_fill_price).round_dp(4);
                let est_fee = est_fill_price * est_size * self.cfg.taker_fee_fraction;

                let position = Position {
                    id: self.next_uuid(),
                    condition_id: market.condition_id.clone(),
                    token_id: token_id.clone(),
                    leg,
                    state: PositionState::Open,
                    entry_price: est_fill_price,
                    size: est_size,
                    entry_fee_usdc: est_fee,
                    entry_ms: now_ms,
                    market_expiry_ms: market
                        .end_time
                        .map(datetime_to_ms)
                        .unwrap_or(now_ms + 86_400_000),
                    exit_price: None,
                    exit_ms: None,
                    realised_pnl: None,
                    exit_reason: None,
                };
                self.positions.insert(position.id.to_string(), position);

                self.last_entry_ms
                    .insert(cooldown_key(&market.condition_id, leg), now_ms);
                *self
                    .exposure
                    .entry(market.condition_id.clone())
                    .or_default() += est_fill_price * est_size;

                stats.entries += 1;

                info!(
                    decision_type = "live_entry_intent",
                    condition_id = %market.condition_id,
                    leg = ?leg,
                    est_fill_price = %est_fill_price,
                    usdc = %self.cfg.entry_usdc,
                    expected_edge = %candidate.features.expected_edge_after_costs,
                    "paper_engine: generating live entry intent"
                );

                intents.push(LiveIntent {
                    condition_id: market.condition_id.clone(),
                    token_id,
                    side: Side::Buy,
                    usdc_notional: self.cfg.entry_usdc,
                    reason: format!(
                        "fav_entry_{}_{:.4}",
                        if leg == Leg::Yes { "yes" } else { "no" },
                        candidate.features.expected_edge_after_costs
                    ),
                    leg,
                    features: candidate.features,
                });
            }
        }

        intents
    }

    /// Acknowledge a live fill from the ExecutionEngine and update position tracking.
    pub fn on_live_fill(
        &self,
        condition_id: &str,
        token_id: &str,
        side: Side,
        price: Decimal,
        size: Decimal,
        fee: Decimal,
    ) {
        if side == Side::Sell {
            // Find the open position for this token and close it with actual fill data.
            let pos_id = self
                .positions
                .iter()
                .find(|p| {
                    p.state == PositionState::Open
                        && p.condition_id == condition_id
                        && p.token_id == token_id
                })
                .map(|p| p.key().clone());

            if let Some(pos_id) = pos_id {
                if let Some(mut p) = self.positions.get_mut(&pos_id) {
                    let pnl = (price - p.entry_price) * size - fee - p.entry_fee_usdc;
                    p.exit_price = Some(price);
                    p.realised_pnl = Some(pnl);
                    let mut stats = self.stats.lock().unwrap();
                    stats.total_pnl_usdc += pnl;
                    stats.total_fees_usdc += fee;
                }
            }
        } else {
            // Update the most recent open position for this token with actual fill data.
            let pos_id = self
                .positions
                .iter()
                .find(|p| {
                    p.state == PositionState::Open
                        && p.condition_id == condition_id
                        && p.token_id == token_id
                })
                .map(|p| p.key().clone());

            if let Some(pos_id) = pos_id {
                if let Some(mut p) = self.positions.get_mut(&pos_id) {
                    p.entry_price = price;
                    p.size = size;
                    p.entry_fee_usdc = fee;
                    let mut stats = self.stats.lock().unwrap();
                    stats.total_fees_usdc += fee;
                }
            }
        }
    }

    fn check_eligibility(&self, market: &MarketMeta, now_ms: i64) -> Result<(), SkipReason> {
        if market.condition_id.is_empty() || market.yes_token_id.is_empty() || market.no_token_id.is_empty() {
            return Err(SkipReason::MarketInactiveOrResolved);
        }
        let end_time = market.end_time.ok_or(SkipReason::MarketInactiveOrResolved)?;
        let end_ms = datetime_to_ms(end_time);
        if end_ms <= now_ms {
            return Err(SkipReason::Expired);
        }
        let hours_left = ms_to_hours(end_ms.saturating_sub(now_ms));
        if hours_left > self.cfg.max_hours_to_expiry {
            return Err(SkipReason::ExpiryTooFar);
        }
        Ok(())
    }

    fn evaluate_signal(
        &self,
        market: &MarketMeta,
        market_state: &MarketState,
        leg: Leg,
        now_ms: i64,
    ) -> Result<SignalCandidate, SkipReason> {
        let leg_state = if leg == Leg::Yes {
            &market_state.yes
        } else {
            &market_state.no
        };
        let quote = &leg_state.quote;
        let quote_age = quote.age_ms(now_ms);
        if quote_age < 0 || quote_age > self.cfg.quote_max_stale_ms {
            return Err(SkipReason::StaleQuote);
        }

        let best_bid = quote.best_bid.as_ref().ok_or(SkipReason::MissingOrderbook)?;
        let best_ask = quote.best_ask.as_ref().ok_or(SkipReason::MissingOrderbook)?;

        let spread = quote.spread_fraction().ok_or(SkipReason::SpreadTooWide)?;
        if spread > self.cfg.max_spread_fraction {
            return Err(SkipReason::SpreadTooWide);
        }
        if spread > HYPER_MAX_SPREAD {
            return Err(SkipReason::SpreadTooWide);
        }

        let depth = quote.ask_depth();
        if depth < self.cfg.min_book_liquidity {
            return Err(SkipReason::LowLiquidity);
        }

        let favorite_probability = quote.mid().ok_or(SkipReason::MissingOrderbook)?;
        if favorite_probability < self.cfg.fav_prob_low || favorite_probability > self.cfg.fav_prob_high {
            return Err(SkipReason::NotFavoriteZone);
        }

        let slip = self.cfg.slippage_bps / dec!(10_000);
        let fee = best_ask.price * self.cfg.taker_fee_fraction;
        let expected_edge_after_costs = (Decimal::ONE - best_ask.price) - fee - (best_ask.price * slip);
        if expected_edge_after_costs < self.cfg.min_net_edge {
            return Err(SkipReason::NegativeEdgeAfterCosts);
        }

        let end_ms = datetime_to_ms(market.end_time.ok_or(SkipReason::MarketInactiveOrResolved)?);
        let features = DecisionFeatures {
            ts_exchange_ms: quote.ts_exchange_ms,
            ts_received_ms: quote.ts_received_ms,
            hours_to_expiry: ms_to_hours(end_ms.saturating_sub(now_ms)),
            quote_age_ms: quote_age,
            best_bid: best_bid.price,
            best_ask: best_ask.price,
            momentum_30s: momentum_window(&leg_state.mids, now_ms, 30_000),
            momentum_2m: momentum_window(&leg_state.mids, now_ms, 120_000),
            volatility_5m: volatility_window(&leg_state.mids, now_ms, 300_000),
            volume_5m: activity_window(&leg_state.mids, now_ms, 300_000),
            orderbook_imbalance: orderbook_imbalance(quote),
            favorite_side: if leg == Leg::Yes { "yes".into() } else { "no".into() },
            favorite_probability,
            spread_fraction: spread,
            depth,
            expected_edge_after_costs,
        };

        Ok(SignalCandidate {
            leg,
            token_id: if leg == Leg::Yes {
                market.yes_token_id.clone()
            } else {
                market.no_token_id.clone()
            },
            best_ask: best_ask.price,
            features,
        })
    }

    fn check_risk(
        &self,
        market: &MarketMeta,
        leg: Leg,
        now_ms: i64,
        features: &DecisionFeatures,
    ) -> Result<(), SkipReason> {
        if features.expected_edge_after_costs < self.cfg.min_net_edge {
            return Err(SkipReason::NegativeEdgeAfterCosts);
        }

        // Max open positions across all markets.
        let open_count = self
            .positions
            .iter()
            .filter(|p| p.state == PositionState::Open)
            .count();
        if open_count >= self.cfg.max_open_positions {
            return Err(SkipReason::MaxOpenPositions);
        }

        // Per-market exposure cap.
        let market_exp = self
            .exposure
            .get(&market.condition_id)
            .map(|e| *e)
            .unwrap_or(Decimal::ZERO);
        if market_exp >= self.cfg.max_exposure_per_market_usdc {
            return Err(SkipReason::MaxMarketExposure);
        }

        if self
            .positions
            .iter()
            .any(|p| p.state == PositionState::Open && p.condition_id == market.condition_id && p.leg == leg)
        {
            return Err(SkipReason::PositionExists);
        }

        let key = cooldown_key(&market.condition_id, leg);
        let cooldown_secs = if is_hyper_market(features) {
            HYPER_ENTRY_COOLDOWN_SECS
        } else {
            self.cfg.entry_cooldown_secs.min(5)
        };
        if self
            .last_entry_ms
            .get(&key)
            .map(|ts| now_ms.saturating_sub(*ts) < cooldown_secs * 1_000)
            .unwrap_or(false)
        {
            return Err(SkipReason::CooldownActive);
        }

        Ok(())
    }

    fn simulate_entry(
        &self,
        market: &MarketMeta,
        candidate: &SignalCandidate,
        now_ms: i64,
    ) -> Result<PaperFill, (SkipReason, Vec<OrderEventCode>, DecisionFeatures)> {
        let mut events = vec![OrderEventCode::OrderSubmitted, OrderEventCode::OrderPending];
        let features = candidate.features.clone();

        if features.quote_age_ms > self.cfg.quote_max_stale_ms {
            events.push(OrderEventCode::OrderRejected);
            return Err((SkipReason::StaleQuote, events, features));
        }
        if features.spread_fraction > self.cfg.max_spread_fraction {
            events.push(OrderEventCode::OrderCancelled);
            return Err((SkipReason::SpreadTooWide, events, features));
        }

        let slip = self.cfg.slippage_bps / dec!(10_000);
        let fill_price = (candidate.best_ask * (Decimal::ONE + slip)).min(dec!(0.999));
        let requested_size = (self.cfg.entry_usdc / fill_price).round_dp(4);

        let depth_ratio = (features.depth / requested_size).min(Decimal::ONE);
        if depth_ratio < dec!(0.30) {
            events.push(OrderEventCode::OrderExpired);
            return Err((SkipReason::InsufficientDepth, events, features));
        }

        let filled_size = requested_size.min(features.depth).round_dp(4);
        if filled_size < market.min_order_size {
            events.push(OrderEventCode::OrderRejected);
            return Err((SkipReason::InsufficientDepth, events, features));
        }

        let fill_status = if filled_size < requested_size {
            events.push(OrderEventCode::OrderPartiallyFilled);
            OrderEventCode::OrderPartiallyFilled
        } else {
            events.push(OrderEventCode::OrderFilled);
            OrderEventCode::OrderFilled
        };

        let depth_ratio_f = depth_ratio.to_f64().unwrap_or(0.0);
        let fill_delay_ms = (100.0 + (1.0 - depth_ratio_f) * 500.0) as i64;
        let fee_usdc = fill_price * filled_size * self.cfg.taker_fee_fraction;

        let position = Position {
            id: self.next_uuid(),
            condition_id: market.condition_id.clone(),
            token_id: candidate.token_id.clone(),
            leg: candidate.leg,
            state: PositionState::Open,
            entry_price: fill_price,
            size: filled_size,
            entry_fee_usdc: fee_usdc,
            entry_ms: now_ms + fill_delay_ms,
            market_expiry_ms: datetime_to_ms(
                market
                    .end_time
                    .ok_or((SkipReason::MarketInactiveOrResolved, events.clone(), features.clone()))?,
            ),
            exit_price: None,
            exit_ms: None,
            realised_pnl: None,
            exit_reason: None,
        };

        let position_id = position.id;
        self.positions.insert(position.id.to_string(), position);

        Ok(PaperFill {
            id: self.next_uuid(),
            timestamp_ms: now_ms,
            condition_id: market.condition_id.clone(),
            token_id: candidate.token_id.clone(),
            leg: candidate.leg,
            side: Side::Buy,
            fill_price,
            requested_size,
            filled_size,
            fee_usdc,
            slippage: candidate.best_ask * slip,
            fill_delay_ms,
            fill_status,
            order_events: events,
            features,
            skip_reason: None,
            exit_reason: None,
            position_id: Some(position_id),
        })
    }

    fn manage_positions(&self, markets: &[MarketMeta], now_ms: i64) -> Vec<PaperFill> {
        let mut fills = Vec::new();
        let market_map: HashMap<&str, &MarketMeta> = markets
            .iter()
            .map(|m| (m.condition_id.as_str(), m))
            .collect();

        let open_ids: Vec<String> = self
            .positions
            .iter()
            .filter(|p| p.state == PositionState::Open)
            .map(|p| p.key().clone())
            .collect();

        for pos_id in open_ids {
            let Some(pos) = self.positions.get(&pos_id).map(|p| p.clone()) else {
                continue;
            };
            let Some(exit_reason) = self.should_exit(&pos, &market_map, now_ms) else {
                continue;
            };

            let Some(quote_state) = self.market_data.market_state(&pos.condition_id) else {
                continue;
            };
            let quote = if pos.leg == Leg::Yes {
                &quote_state.yes.quote
            } else {
                &quote_state.no.quote
            };
            let best_bid = quote.best_bid.as_ref().map(|b| b.price).unwrap_or(pos.entry_price);
            let slip = self.cfg.slippage_bps / dec!(10_000);
            let exit_price = (best_bid * (Decimal::ONE - slip)).max(dec!(0.001));
            let pnl_gross = (exit_price - pos.entry_price) * pos.size;
            let exit_fee = exit_price * pos.size * self.cfg.taker_fee_fraction;
            let pnl_net = pnl_gross - exit_fee - pos.entry_fee_usdc;

            if let Some(mut p) = self.positions.get_mut(&pos_id) {
                if p.state != PositionState::Open {
                    continue;
                }
                p.state = PositionState::Closed;
                p.exit_price = Some(exit_price);
                p.exit_ms = Some(now_ms);
                p.realised_pnl = Some(pnl_net);
                p.exit_reason = Some(exit_reason);
            }
            if let Some(mut exp) = self.exposure.get_mut(&pos.condition_id) {
                *exp = (*exp - pos.entry_notional()).max(Decimal::ZERO);
            }

            let mut stats = self.stats.lock().unwrap();
            stats.exits += 1;
            stats.total_pnl_usdc += pnl_net;
            stats.total_fees_usdc += exit_fee;
            stats.record_order_event(OrderEventCode::OrderFilled);
            drop(stats);

            info!(
                decision_type = "exit",
                reason_code = exit_reason.as_str(),
                condition_id = %pos.condition_id,
                leg = ?pos.leg,
                entry_price = %pos.entry_price,
                exit_price = %exit_price,
                pnl_net = %pnl_net,
                hold_time_ms = pos.age_ms(now_ms),
                "paper_engine: position closed"
            );

            fills.push(PaperFill {
                id: self.next_uuid(),
                timestamp_ms: now_ms,
                condition_id: pos.condition_id.clone(),
                token_id: pos.token_id.clone(),
                leg: pos.leg,
                side: Side::Sell,
                fill_price: exit_price,
                requested_size: pos.size,
                filled_size: pos.size,
                fee_usdc: exit_fee,
                slippage: best_bid * slip,
                fill_delay_ms: 0,
                fill_status: OrderEventCode::OrderFilled,
                order_events: vec![
                    OrderEventCode::OrderSubmitted,
                    OrderEventCode::OrderPending,
                    OrderEventCode::OrderFilled,
                ],
                features: DecisionFeatures {
                    quote_age_ms: quote.age_ms(now_ms),
                    favorite_side: if pos.leg == Leg::Yes { "yes".into() } else { "no".into() },
                    favorite_probability: quote.mid().unwrap_or(exit_price),
                    expected_edge_after_costs: pnl_net / pos.entry_notional().max(dec!(0.0001)),
                    ..DecisionFeatures::default()
                },
                skip_reason: None,
                exit_reason: Some(exit_reason),
                position_id: Some(pos.id),
            });
        }

        fills
    }

    fn should_exit(
        &self,
        pos: &Position,
        market_map: &HashMap<&str, &MarketMeta>,
        now_ms: i64,
    ) -> Option<ExitReason> {
        let expiry_ms = market_map
            .get(pos.condition_id.as_str())
            .and_then(|m| m.end_time)
            .map(datetime_to_ms)
            .unwrap_or(pos.market_expiry_ms);

        let quote_state = self.market_data.market_state(&pos.condition_id)?;
        let quote = if pos.leg == Leg::Yes {
            &quote_state.yes.quote
        } else {
            &quote_state.no.quote
        };
        let leg_state = if pos.leg == Leg::Yes {
            &quote_state.yes
        } else {
            &quote_state.no
        };
        let mid = quote.mid()?;

        let activity_5m = activity_window(&leg_state.mids, now_ms, 300_000);
        let hours_to_expiry = ms_to_hours(expiry_ms.saturating_sub(now_ms));
        let hyper = mid >= HYPER_MIN_FAVORITE_PROB
            && hours_to_expiry <= HYPER_MAX_HOURS_TO_EXPIRY
            && activity_5m >= HYPER_MIN_ACTIVITY_5M;

        let bid = quote.best_bid.as_ref()?.price;
        let slip = self.cfg.slippage_bps / dec!(10_000);
        let preview_exit_price = (bid * (Decimal::ONE - slip)).max(dec!(0.001));
        let pnl_fraction =
            (preview_exit_price - pos.entry_price) / pos.entry_price.max(dec!(0.0001));
        let preview_exit_fee = preview_exit_price * pos.size * self.cfg.taker_fee_fraction;
        let pnl_net =
            (preview_exit_price - pos.entry_price) * pos.size - preview_exit_fee - pos.entry_fee_usdc;
        let min_profit_fraction = self
            .cfg
            .take_profit_fraction
            .max(self.cfg.min_early_exit_profit_fraction)
            .max(if hyper {
                HYPER_TAKE_PROFIT_FRACTION
            } else {
                Decimal::ZERO
            });

        // Never close at a loss. Early exit only with sufficient absolute + fractional profit.
        if pnl_net >= self.cfg.min_early_exit_profit_usdc && pnl_fraction >= min_profit_fraction {
            return Some(ExitReason::PositionClosedTp);
        }
        let _ = (expiry_ms, mid);
        None
    }

    fn emit_skip(
        &self,
        reason: SkipReason,
        market: &MarketMeta,
        stage: &str,
        features: Option<&DecisionFeatures>,
    ) {
        debug!(
            decision_type = "skip",
            stage = stage,
            reason_code = reason.as_str(),
            condition_id = %market.condition_id,
            slug = %market.slug,
            quote_age_ms = features.map(|f| f.quote_age_ms).unwrap_or_default(),
            spread_fraction = %features.map(|f| f.spread_fraction).unwrap_or(Decimal::ZERO),
            depth = %features.map(|f| f.depth).unwrap_or(Decimal::ZERO),
            fav_prob = %features.map(|f| f.favorite_probability).unwrap_or(Decimal::ZERO),
            expected_edge = %features.map(|f| f.expected_edge_after_costs).unwrap_or(Decimal::ZERO),
            "paper_engine: skipped"
        );
    }
}

pub fn reason_code_table() -> &'static [(&'static str, &'static str)] {
    &[
        ("market_inactive_or_resolved", "Market metadata is invalid or market is not tradable"),
        ("expired", "Market already expired"),
        ("expiry_too_far", "Market expiry is beyond max_hours_to_expiry"),
        ("missing_orderbook", "No top-of-book snapshot for this leg"),
        ("spread_too_wide", "Spread exceeds configured threshold"),
        ("low_liquidity", "Top-of-book depth below min_book_liquidity"),
        ("not_favorite_zone", "Favorite probability outside [fav_prob_low, fav_prob_high]"),
        ("negative_edge_after_costs", "Expected edge after fees and slippage is negative"),
        ("position_exists", "An open position for this market leg already exists"),
        ("cooldown_active", "Entry cooldown is still active for this market leg"),
        ("stale_quote", "Quote age exceeds quote_max_stale_ms"),
        ("insufficient_depth", "Top-of-book depth cannot support requested size"),
        ("order_submitted", "Order accepted by paper execution simulator"),
        ("order_rejected", "Order rejected by paper execution simulator"),
        ("order_pending", "Order is queued and awaiting simulated fill"),
        ("order_filled", "Order fully filled"),
        ("order_partially_filled", "Order partially filled"),
        ("order_cancelled", "Order cancelled before fill"),
        ("order_expired", "Order expired before fill"),
        ("position_closed_tp", "Position closed due to take-profit"),
        ("position_closed_sl", "Position closed due to stop-loss"),
        ("position_closed_timeout", "Position closed due to max hold time"),
        ("position_closed_forced_expiry", "Position force-closed before expiry"),
        ("position_closed_signal_flip", "Position closed because favorite signal weakened"),
    ]
}

fn cooldown_key(condition_id: &str, leg: Leg) -> String {
    format!("{condition_id}:{}", if leg == Leg::Yes { "yes" } else { "no" })
}

fn datetime_to_ms(dt: DateTime<Utc>) -> i64 {
    dt.timestamp_millis()
}

fn ms_to_hours(ms: i64) -> f64 {
    ms as f64 / 3_600_000.0
}

fn pick_anchor_mid(points: &[MidPoint], now_ms: i64, window_ms: i64) -> Option<Decimal> {
    points
        .iter()
        .rev()
        .find(|p| now_ms.saturating_sub(p.ts_ms) >= window_ms)
        .map(|p| p.mid)
}

fn momentum_window(points: &std::collections::VecDeque<MidPoint>, now_ms: i64, window_ms: i64) -> Decimal {
    let Some(current) = points.back().map(|p| p.mid) else {
        return Decimal::ZERO;
    };
    let snapshot: Vec<MidPoint> = points.iter().copied().collect();
    let Some(past) = pick_anchor_mid(&snapshot, now_ms, window_ms) else {
        return Decimal::ZERO;
    };
    if past <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    (current - past) / past
}

fn volatility_window(points: &std::collections::VecDeque<MidPoint>, now_ms: i64, window_ms: i64) -> Decimal {
    let in_window: Vec<Decimal> = points
        .iter()
        .filter(|p| now_ms.saturating_sub(p.ts_ms) <= window_ms)
        .map(|p| p.mid)
        .collect();
    if in_window.len() < 2 {
        return Decimal::ZERO;
    }
    let vals: Vec<f64> = in_window.iter().filter_map(|v| v.to_f64()).collect();
    if vals.len() < 2 {
        return Decimal::ZERO;
    }
    let mean = vals.iter().sum::<f64>() / vals.len() as f64;
    if mean == 0.0 {
        return Decimal::ZERO;
    }
    let var = vals.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / vals.len() as f64;
    Decimal::from_f64_retain(var.sqrt() / mean).unwrap_or(Decimal::ZERO)
}

fn activity_window(points: &std::collections::VecDeque<MidPoint>, now_ms: i64, window_ms: i64) -> Decimal {
    let count = points
        .iter()
        .filter(|p| now_ms.saturating_sub(p.ts_ms) <= window_ms)
        .count();
    Decimal::from(count as u64)
}

fn orderbook_imbalance(quote: &NormalizedQuote) -> Decimal {
    let bid_size = quote.best_bid.as_ref().map(|b| b.size).unwrap_or(Decimal::ZERO);
    let ask_size = quote.best_ask.as_ref().map(|a| a.size).unwrap_or(Decimal::ZERO);
    let denom = bid_size + ask_size;
    if denom <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    (bid_size - ask_size) / denom
}

fn is_hyper_market(features: &DecisionFeatures) -> bool {
    features.favorite_probability >= HYPER_MIN_FAVORITE_PROB
        && features.hours_to_expiry <= HYPER_MAX_HOURS_TO_EXPIRY
        && features.volume_5m >= HYPER_MIN_ACTIVITY_5M
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn now_ms() -> i64 {
        Utc::now().timestamp_millis()
    }

    fn make_config() -> PaperEngineConfig {
        PaperEngineConfig {
            max_hours_to_expiry: 6.0,
            max_spread_fraction: dec!(0.10),
            quote_max_stale_ms: 5_000,
            min_net_edge: dec!(0.0001),
            slippage_bps: dec!(10),
            ..PaperEngineConfig::default()
        }
    }

    fn market_expiring_in(secs: i64) -> MarketMeta {
        let end = Utc::now() + Duration::seconds(secs);
        MarketMeta {
            market_id: "m1".into(),
            condition_id: "cond-1".into(),
            slug: "test".into(),
            question: "test?".into(),
            fee_type: None,
            fees_enabled: true,
            maker_base_fee_bps: 0,
            taker_base_fee_bps: 0,
            maker_rebates_fee_share_bps: None,
            fee_schedule: polyhft_core::types::FeeSchedule::default(),
            yes_token_id: "yes".into(),
            no_token_id: "no".into(),
            tick_size: dec!(0.01),
            min_order_size: dec!(1),
            event_start_time: None,
            end_time: Some(end),
        }
    }

    fn seed_book(engine: &PaperEngine, market: &MarketMeta, bid: Decimal, ask: Decimal, ts_ms: i64) {
        let bids = vec![BookLevel { price: bid, size: dec!(100) }];
        let asks = vec![BookLevel { price: ask, size: dec!(100) }];
        for i in 0..4 {
            let t = ts_ms - ((3 - i) * 1_000);
            engine.on_book_update(&market.condition_id, true, &bids, &asks, t);
            engine.on_book_update(&market.condition_id, false, &bids, &asks, t);
        }
    }

    #[test]
    fn expiry_calculation_uses_ms_not_secs() {
        let engine = PaperEngine::new(make_config(), 1);
        let market = market_expiring_in(3600);
        let now = now_ms();
        assert!(engine.check_eligibility(&market, now).is_ok());
        assert_eq!(engine.check_eligibility(&market, now / 1000), Err(SkipReason::ExpiryTooFar));
    }

    #[test]
    fn eligibility_filter_rejects_expiry_too_far() {
        let engine = PaperEngine::new(make_config(), 2);
        let market = market_expiring_in(8 * 3600);
        assert_eq!(engine.check_eligibility(&market, now_ms()), Err(SkipReason::ExpiryTooFar));
    }

    #[test]
    fn spread_filter_rejects_wide_books() {
        let engine = PaperEngine::new(make_config(), 3);
        let market = market_expiring_in(3600);
        let ts = now_ms();
        seed_book(&engine, &market, dec!(0.60), dec!(0.90), ts);
        let fills = engine.run_cycle(&[market], ts);
        assert!(fills.is_empty());
        assert!(engine.stats().skip_counts.contains_key("spread_too_wide"));
    }

    #[test]
    fn stale_quote_is_rejected() {
        let engine = PaperEngine::new(make_config(), 4);
        let market = market_expiring_in(3600);
        let now = now_ms();
        seed_book(&engine, &market, dec!(0.69), dec!(0.71), now - 20_000);
        let fills = engine.run_cycle(&[market], now);
        assert!(fills.is_empty());
        assert!(engine.stats().skip_counts.contains_key("stale_quote"));
    }

    #[test]
    fn execution_pricing_uses_ask_plus_slippage() {
        let mut cfg = make_config();
        cfg.slippage_bps = dec!(20);
        let engine = PaperEngine::new(cfg, 5);
        let market = market_expiring_in(1800);
        let now = now_ms();
        seed_book(&engine, &market, dec!(0.70), dec!(0.71), now);
        let fills = engine.run_cycle(&[market], now);
        let buy = fills.iter().find(|f| f.side == Side::Buy).unwrap();
        let expected = dec!(0.71) * dec!(1.002);
        assert!((buy.fill_price - expected).abs() < dec!(0.0001));
    }

    #[test]
    fn order_lifecycle_transitions_are_recorded() {
        let engine = PaperEngine::new(make_config(), 6);
        let market = market_expiring_in(1800);
        let now = now_ms();
        seed_book(&engine, &market, dec!(0.70), dec!(0.71), now);
        let fills = engine.run_cycle(&[market], now);
        let buy = fills.iter().find(|f| f.side == Side::Buy).unwrap();
        assert_eq!(buy.order_events[0], OrderEventCode::OrderSubmitted);
        assert_eq!(buy.order_events[1], OrderEventCode::OrderPending);
        assert!(
            buy.order_events
                .iter()
                .any(|e| *e == OrderEventCode::OrderFilled || *e == OrderEventCode::OrderPartiallyFilled)
        );
    }

    #[test]
    fn position_opens_and_closes() {
        let mut cfg = make_config();
        cfg.take_profit_fraction = dec!(0.01);
        let engine = PaperEngine::new(cfg, 7);
        let market = market_expiring_in(1800);
        let now = now_ms();
        seed_book(&engine, &market, dec!(0.70), dec!(0.71), now);
        let first = engine.run_cycle(&[market.clone()], now);
        assert!(first.iter().any(|f| f.side == Side::Buy));
        let bids = vec![BookLevel { price: dec!(0.80), size: dec!(100) }];
        let asks = vec![BookLevel { price: dec!(0.81), size: dec!(100) }];
        engine.on_book_update(&market.condition_id, true, &bids, &asks, now + 1_000);
        engine.on_book_update(&market.condition_id, false, &bids, &asks, now + 1_000);
        let second = engine.run_cycle(&[market], now + 1_000);
        assert!(second.iter().any(|f| f.exit_reason == Some(ExitReason::PositionClosedTp)));
    }

    #[test]
    fn reason_code_determinism_uses_single_skip_reason() {
        let engine = PaperEngine::new(make_config(), 8);
        let market = market_expiring_in(9 * 3600);
        let now = now_ms();
        let _ = engine.run_cycle(&[market], now);
        let stats = engine.stats();
        assert_eq!(stats.skip_counts.values().sum::<u64>(), 1);
        assert!(stats.skip_counts.contains_key("expiry_too_far"));
    }

    #[test]
    fn hyper_mode_reopens_quickly_after_tp() {
        let mut cfg = make_config();
        cfg.take_profit_fraction = dec!(0.02);
        cfg.entry_cooldown_secs = 30;
        let engine = PaperEngine::new(cfg, 9);
        let market = market_expiring_in(30 * 60);
        let now = now_ms();
        seed_book(&engine, &market, dec!(0.71), dec!(0.72), now);

        let entry = engine.run_cycle(&[market.clone()], now);
        assert!(entry.iter().any(|f| f.side == Side::Buy));

        let bids_up = vec![BookLevel { price: dec!(0.80), size: dec!(100) }];
        let asks_up = vec![BookLevel { price: dec!(0.81), size: dec!(100) }];
        for i in 0..4 {
            let t = now + 2_000 + (i * 200);
            engine.on_book_update(&market.condition_id, true, &bids_up, &asks_up, t);
            engine.on_book_update(&market.condition_id, false, &bids_up, &asks_up, t);
        }
        let exit = engine.run_cycle(&[market.clone()], now + 3_000);
        assert!(exit.iter().any(|f| f.exit_reason == Some(ExitReason::PositionClosedTp)));

        let reentry = engine.run_cycle(&[market], now + 4_500);
        let buy_count = exit
            .iter()
            .chain(reentry.iter())
            .filter(|f| f.side == Side::Buy)
            .count();
        assert!(buy_count >= 1, "expected quick re-entry after TP in hyper mode");
    }
}
