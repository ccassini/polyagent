use std::collections::{HashMap, HashSet};

use anyhow::Context;
use dashmap::DashMap;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;

use polyhft_core::clob_ids::normalize_clob_token_id;
use polyhft_core::config::{AppConfig, StrategyConfig};
use polyhft_core::types::{
    BookLevel, BookTop, DecisionBatch, MarketMeta, NormalizedEvent, PriceSource, QuoteIntent,
    RtdsPriceEvent, Side, SignalSnapshot, TakerIntent, TradeLifecycle,
};

#[derive(Debug, Clone)]
struct MarketBookState {
    yes: BookTop,
    no: BookTop,
    adverse_selection_ewma: Decimal,
    /// Fast EWMA of book-update inter-arrival rate (events/sec proxy).
    /// Decays with α=0.30 toward the inverse of the last inter-event gap.
    book_rate_fast: Decimal,
    /// Slow EWMA (α=0.05) — the "baseline" rate for spike detection.
    book_rate_slow: Decimal,
    /// Timestamp of the last book event (ms) used to compute inter-arrival.
    book_last_update_ms: i64,
}

#[derive(Debug, Clone, Copy)]
struct QuoteControls {
    size_multiplier: Decimal,
    spread_multiplier: Decimal,
}

impl Default for MarketBookState {
    fn default() -> Self {
        Self {
            yes: BookTop {
                best_bid: None,
                best_ask: None,
                ts_ms: 0,
            },
            no: BookTop {
                best_bid: None,
                best_ask: None,
                ts_ms: 0,
            },
            adverse_selection_ewma: Decimal::ZERO,
            book_rate_fast: Decimal::ZERO,
            book_rate_slow: Decimal::ZERO,
            book_last_update_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct RefPriceState {
    last: Option<RtdsPriceEvent>,
    prev: Option<RtdsPriceEvent>,
}

impl RefPriceState {
    fn update(&mut self, evt: RtdsPriceEvent) {
        self.prev = self.last.take();
        self.last = Some(evt);
    }

    fn signed_return(&self) -> Decimal {
        let Some(last) = &self.last else {
            return Decimal::ZERO;
        };
        let Some(prev) = &self.prev else {
            return Decimal::ZERO;
        };
        if prev.value <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        (last.value - prev.value) / prev.value
    }

    fn latest_if_fresh(&self, now_ms: i64, max_stale_ms: i64) -> Option<Decimal> {
        let last = self.last.as_ref()?;
        if now_ms.saturating_sub(last.ts_ms) > max_stale_ms {
            return None;
        }
        Some(last.value)
    }

    fn signed_return_if_fresh(&self, now_ms: i64, max_stale_ms: i64) -> Decimal {
        let Some(last) = self.last.as_ref() else {
            return Decimal::ZERO;
        };
        let Some(prev) = self.prev.as_ref() else {
            return Decimal::ZERO;
        };

        if now_ms.saturating_sub(last.ts_ms) > max_stale_ms {
            return Decimal::ZERO;
        }
        if now_ms.saturating_sub(prev.ts_ms) > max_stale_ms {
            return Decimal::ZERO;
        }

        self.signed_return()
    }
}

#[derive(Debug, Clone, Default)]
struct UnderlyingRefState {
    binance: RefPriceState,
    chainlink: RefPriceState,
    pyth: RefPriceState,
}

impl UnderlyingRefState {
    fn update(&mut self, evt: RtdsPriceEvent) {
        match evt.source {
            PriceSource::Binance => self.binance.update(evt),
            PriceSource::Chainlink => self.chainlink.update(evt),
            PriceSource::Pyth => self.pyth.update(evt),
        }
    }

    fn momentum(&self, now_ms: i64, max_stale_ms: i64) -> Decimal {
        let binance = self.binance.signed_return_if_fresh(now_ms, max_stale_ms);
        if !binance.is_zero() {
            return binance;
        }
        let pyth = self.pyth.signed_return_if_fresh(now_ms, max_stale_ms);
        if !pyth.is_zero() {
            return pyth;
        }
        self.chainlink.signed_return_if_fresh(now_ms, max_stale_ms)
    }

    fn chainlink_pyth_basis(&self, now_ms: i64, max_stale_ms: i64) -> Option<Decimal> {
        let chainlink = self.chainlink.latest_if_fresh(now_ms, max_stale_ms)?;
        let pyth = self.pyth.latest_if_fresh(now_ms, max_stale_ms)?;
        let mid = (chainlink + pyth) / dec!(2);
        if mid <= Decimal::ZERO {
            return None;
        }
        Some((pyth - chainlink) / mid)
    }
}

#[derive(Debug)]
pub struct SignalEngine {
    cfg: StrategyConfig,
    markets: Vec<MarketMeta>,
    market_state: DashMap<String, MarketBookState>,
    token_to_market: HashMap<String, (String, bool)>, // token -> (condition_id, is_yes)
    /// Per-market underlying symbol ("btcusd" | "ethusd" | "solusd"), pre-computed at init
    /// to avoid repeated lowercase allocation on every decision cycle.
    market_underlying: HashMap<String, &'static str>, // condition_id -> underlying
    refs: DashMap<String, UnderlyingRefState>,
}

impl SignalEngine {
    pub fn new(cfg: &AppConfig, markets: Vec<MarketMeta>) -> anyhow::Result<Self> {
        if markets.is_empty() {
            anyhow::bail!("signal engine requires at least one market")
        }

        let mut token_to_market = HashMap::new();
        let mut market_underlying = HashMap::new();
        for market in &markets {
            token_to_market.insert(
                normalize_clob_token_id(market.yes_token_id.as_str()),
                (market.condition_id.clone(), true),
            );
            token_to_market.insert(
                normalize_clob_token_id(market.no_token_id.as_str()),
                (market.condition_id.clone(), false),
            );
            // Cache underlying once at startup; avoids heap allocation
            // on every decision cycle iteration over markets.
            if let Some(u) = market_underlying_from_question(&market.question) {
                market_underlying.insert(market.condition_id.clone(), u);
            }
        }

        Ok(Self {
            cfg: cfg.strategy.clone(),
            markets,
            market_state: DashMap::new(),
            token_to_market,
            market_underlying,
            refs: DashMap::new(),
        })
    }

    pub fn on_event(&self, event: &NormalizedEvent) {
        match event {
            NormalizedEvent::MarketBook(evt) => {
                let tid = normalize_clob_token_id(evt.token_id.as_str());
                if let Some((condition_id, is_yes)) = self.token_to_market.get(&tid) {
                    let mut state = self.market_state.entry(condition_id.clone()).or_default();
                    let top = snapshot_top(&evt.bids, &evt.asks, evt.ts_ms);
                    if *is_yes {
                        state.yes = top;
                    } else {
                        state.no = top;
                    }
                    update_book_rate(&mut state, evt.ts_ms);
                }
            }
            NormalizedEvent::PriceChange(evt) => {
                let tid = normalize_clob_token_id(evt.token_id.as_str());
                if let Some((condition_id, is_yes)) = self.token_to_market.get(&tid) {
                    let mut state = self.market_state.entry(condition_id.clone()).or_default();
                    let target = if *is_yes {
                        &mut state.yes
                    } else {
                        &mut state.no
                    };

                    if let Some(best_bid) = evt.best_bid {
                        target.best_bid = Some(BookLevel {
                            price: best_bid,
                            size: evt.size.unwrap_or(dec!(1)),
                        });
                    }
                    if let Some(best_ask) = evt.best_ask {
                        target.best_ask = Some(BookLevel {
                            price: best_ask,
                            size: evt.size.unwrap_or(dec!(1)),
                        });
                    }
                    target.ts_ms = evt.ts_ms;
                    update_book_rate(&mut state, evt.ts_ms);
                }
            }
            NormalizedEvent::RtdsPrice(evt) => {
                let Some(underlying) = market_underlying_from_symbol(&evt.symbol) else {
                    return;
                };
                let mut r = self.refs.entry(underlying.to_string()).or_default();
                r.update(evt.clone());
            }
            NormalizedEvent::UserTrade(trade) => {
                let Some(mut state) = self.market_state.get_mut(&trade.condition_id) else {
                    return;
                };
                if matches!(
                    trade.status,
                    TradeLifecycle::Matched | TradeLifecycle::Mined | TradeLifecycle::Confirmed
                ) && let Some(mid) =
                    midpoint_for_token(&state, &trade.token_id, &self.token_to_market)
                {
                    let signed_drift = match trade.side {
                        Side::Buy => mid - trade.price,
                        Side::Sell => trade.price - mid,
                    };
                    // EWMA as an online proxy for post-fill adverse selection over short horizons.
                    state.adverse_selection_ewma =
                        state.adverse_selection_ewma * dec!(0.9) + signed_drift * dec!(0.1);
                }
            }
            _ => {}
        }
    }

    pub fn replace_markets(&mut self, markets: Vec<MarketMeta>) -> anyhow::Result<()> {
        if markets.is_empty() {
            anyhow::bail!("signal engine requires at least one market")
        }

        let mut token_to_market = HashMap::new();
        let mut market_underlying = HashMap::new();
        let active_conditions = markets
            .iter()
            .map(|market| {
                token_to_market.insert(
                    normalize_clob_token_id(market.yes_token_id.as_str()),
                    (market.condition_id.clone(), true),
                );
                token_to_market.insert(
                    normalize_clob_token_id(market.no_token_id.as_str()),
                    (market.condition_id.clone(), false),
                );
                if let Some(u) = market_underlying_from_question(&market.question) {
                    market_underlying.insert(market.condition_id.clone(), u);
                }
                market.condition_id.clone()
            })
            .collect::<HashSet<_>>();

        self.markets = markets;
        self.token_to_market = token_to_market;
        self.market_underlying = market_underlying;
        self.market_state
            .retain(|condition_id, _| active_conditions.contains(condition_id));
        Ok(())
    }

    /// Single-leg momentum takers skip unless both asks exist and their sum is ≤ cap (when cap > 0).
    fn allow_momentum_taker_book(&self, ask_sum: Option<Decimal>) -> bool {
        let cap = self.cfg.momentum_taker_max_ask_sum;
        if cap <= Decimal::ZERO {
            return true;
        }
        match ask_sum {
            Some(s) if s > Decimal::ZERO && s <= cap => true,
            _ => false,
        }
    }

    pub fn build_decisions(
        &self,
        now_ms: i64,
    ) -> anyhow::Result<Vec<(SignalSnapshot, DecisionBatch)>> {
        let mut out = Vec::new();

        for market in &self.markets {
            let Some(state) = self.market_state.get(&market.condition_id) else {
                continue;
            };

            let (yes_mid, no_mid) = fair_values(&state.yes, &state.no);
            let yes_ask = state.yes.best_ask.as_ref().map(|x| x.price);
            let no_ask = state.no.best_ask.as_ref().map(|x| x.price);

            let ask_sum = yes_ask.zip(no_ask).map(|(y, n)| y + n);
            let gross_mispricing = ask_sum.map_or(Decimal::ZERO, |sum| dec!(1) - sum);

            let fee_drag = ask_sum
                .map(|sum| estimate_taker_fee_from_schedule(sum / dec!(2), &market.fee_schedule))
                .unwrap_or(Decimal::ZERO);
            let rebate_credit = ask_sum
                .map(|sum| estimate_maker_rebate(sum / dec!(2), &market.fee_schedule))
                .unwrap_or(Decimal::ZERO);

            let net_mispricing = gross_mispricing - fee_drag + rebate_credit;

            // Use pre-computed underlying map to avoid repeated string alloc.
            let chainlink_pyth_basis = self.chainlink_pyth_basis_cached(market, now_ms);
            let directional_bias =
                self.directional_bias_cached(market, now_ms, chainlink_pyth_basis);
            let fill_probability = fill_probability(&state);
            let queue_proxy = queue_position_proxy(&state);
            let adverse_selection = state.adverse_selection_ewma;
            let signal_strength = quote_signal_strength(
                net_mispricing,
                chainlink_pyth_basis.unwrap_or(Decimal::ZERO),
                directional_bias,
            );

            let snapshot = SignalSnapshot {
                condition_id: market.condition_id.clone(),
                timestamp_ms: now_ms,
                yes_mid,
                no_mid,
                yes_no_ask_sum: ask_sum,
                mispricing_edge: net_mispricing,
                chainlink_pyth_basis,
                adverse_selection_score: adverse_selection,
                fill_probability,
                queue_position_proxy: queue_proxy,
                directional_bias,
            };

            let mut decisions = DecisionBatch::default();

            if self.cfg.maker_quotes_enabled
                && let (Some(y_mid), Some(n_mid)) = (yes_mid, no_mid)
            {
                decisions.quote_intents.extend(self.make_quotes(
                    market,
                    y_mid,
                    n_mid,
                    directional_bias,
                    state.value().clone(),
                    fill_probability,
                    queue_proxy,
                    adverse_selection,
                    signal_strength,
                ));
            }

            if self.cfg.directional_market_enabled
                && self.market_underlying.contains_key(&market.condition_id)
                && self.allow_momentum_taker_book(ask_sum)
            {
                let thresh = self.cfg.directional_market_min_abs_bias;
                if thresh > Decimal::ZERO {
                    let dir_usdc = if self.cfg.directional_market_usdc > Decimal::ZERO {
                        self.cfg.directional_market_usdc
                    } else {
                        self.cfg.quote_size_usdc
                    };
                    let dir_usdc = dir_usdc.max(dec!(0.01));

                    if directional_bias >= thresh {
                        decisions.taker_intents.push(TakerIntent {
                            condition_id: market.condition_id.clone(),
                            token_id: market.yes_token_id.clone(),
                            side: Side::Buy,
                            usdc_notional: dir_usdc,
                            reason: format!(
                                "directional FOK market buy UP (bias={directional_bias} >= {thresh})"
                            ),
                        });
                    } else if directional_bias <= -thresh {
                        decisions.taker_intents.push(TakerIntent {
                            condition_id: market.condition_id.clone(),
                            token_id: market.no_token_id.clone(),
                            side: Side::Buy,
                            usdc_notional: dir_usdc,
                            reason: format!(
                                "directional FOK market buy DOWN (bias={directional_bias} <= -{thresh})"
                            ),
                        });
                    }
                }
            }

            if net_mispricing >= self.cfg.min_taker_edge {
                let notional = self.cfg.quote_size_usdc;
                decisions.taker_intents.push(TakerIntent {
                    condition_id: market.condition_id.clone(),
                    token_id: market.yes_token_id.clone(),
                    side: Side::Buy,
                    usdc_notional: notional,
                    reason: "YES+NO<1 fee-aware mispricing".to_string(),
                });
                decisions.taker_intents.push(TakerIntent {
                    condition_id: market.condition_id.clone(),
                    token_id: market.no_token_id.clone(),
                    side: Side::Buy,
                    usdc_notional: notional,
                    reason: "YES+NO<1 fee-aware mispricing".to_string(),
                });
            }

            if let Some(basis) = chainlink_pyth_basis
                && self.should_take_basis_arb(market, now_ms, basis)
            {
                let taker_fee_proxy =
                    estimate_taker_fee_from_schedule(dec!(0.5), &market.fee_schedule);
                let net_basis_edge = basis.abs() - taker_fee_proxy;
                let (token_id, reason) = if basis >= Decimal::ZERO {
                    (
                        market.yes_token_id.clone(),
                        format!(
                            "chainlink-pyth basis arb: bullish basis={basis} net_edge={net_basis_edge}"
                        ),
                    )
                } else {
                    (
                        market.no_token_id.clone(),
                        format!(
                            "chainlink-pyth basis arb: bearish basis={basis} net_edge={net_basis_edge}"
                        ),
                    )
                };

                decisions.taker_intents.push(TakerIntent {
                    condition_id: market.condition_id.clone(),
                    token_id,
                    side: Side::Buy,
                    usdc_notional: self.cfg.quote_size_usdc,
                    reason,
                });
            }

            // ── Tail-end strategy ─────────────────────────────────────────────
            // Buy the near-certain leg when market closes within N minutes.
            if self.cfg.tail_end_enabled {
                if let Some(end_time) = market.end_time {
                    let end_ms = end_time.timestamp_millis();
                    let minutes_left = (end_ms - now_ms).max(0) / 60_000;
                    if minutes_left <= self.cfg.tail_end_minutes_to_expiry {
                        let tail_usdc = if self.cfg.tail_end_usdc > Decimal::ZERO {
                            self.cfg.tail_end_usdc
                        } else {
                            self.cfg.quote_size_usdc
                        };
                        let min_prob = self.cfg.tail_end_min_prob;
                        // YES leg near-certain
                        if let Some(y) = yes_mid {
                            if y >= min_prob {
                                decisions.taker_intents.push(TakerIntent {
                                    condition_id: market.condition_id.clone(),
                                    token_id: market.yes_token_id.clone(),
                                    side: Side::Buy,
                                    usdc_notional: tail_usdc,
                                    reason: format!(
                                        "tail-end YES prob={y} mins_left={minutes_left}"
                                    ),
                                });
                            }
                        }
                        // NO leg near-certain
                        if let Some(n) = no_mid {
                            if n >= min_prob {
                                decisions.taker_intents.push(TakerIntent {
                                    condition_id: market.condition_id.clone(),
                                    token_id: market.no_token_id.clone(),
                                    side: Side::Buy,
                                    usdc_notional: tail_usdc,
                                    reason: format!(
                                        "tail-end NO prob={n} mins_left={minutes_left}"
                                    ),
                                });
                            }
                        }
                    }
                }
            }

            // ── Volume / activity spike strategy ──────────────────────────────
            // When book-update rate fast EWMA > slow EWMA × multiplier, emit a
            // momentum-aligned taker to ride the incoming price discovery wave.
            if self.cfg.volume_spike_enabled && self.allow_momentum_taker_book(ask_sum) {
                let spike_mul = self.cfg.volume_spike_multiplier;
                if spike_mul > Decimal::ZERO
                    && state.book_rate_slow > Decimal::ZERO
                    && state.book_rate_fast >= state.book_rate_slow * spike_mul
                {
                    let spike_usdc = if self.cfg.volume_spike_usdc > Decimal::ZERO {
                        self.cfg.volume_spike_usdc
                    } else {
                        self.cfg.quote_size_usdc
                    };
                    // Direction: follow directional bias; if flat, lean to the leg whose
                    // ask is thinner (less displayed supply at touch).
                    let yes_ask_sz = state
                        .yes
                        .best_ask
                        .as_ref()
                        .map(|l| l.size)
                        .unwrap_or(Decimal::ZERO);
                    let no_ask_sz = state
                        .no
                        .best_ask
                        .as_ref()
                        .map(|l| l.size)
                        .unwrap_or(Decimal::ZERO);
                    let (token_id, direction) = if directional_bias > Decimal::ZERO {
                        (market.yes_token_id.clone(), "UP")
                    } else if directional_bias < Decimal::ZERO {
                        (market.no_token_id.clone(), "DOWN")
                    } else if yes_ask_sz <= no_ask_sz {
                        (market.yes_token_id.clone(), "UP")
                    } else {
                        (market.no_token_id.clone(), "DOWN")
                    };
                    decisions.taker_intents.push(TakerIntent {
                        condition_id: market.condition_id.clone(),
                        token_id,
                        side: Side::Buy,
                        usdc_notional: spike_usdc,
                        reason: format!(
                            "volume-spike {direction} fast={:.3} slow={:.3} x{spike_mul}",
                            state.book_rate_fast,
                            state.book_rate_slow,
                        ),
                    });
                }
            }

            // ── Spread-farm strategy ──────────────────────────────────────────
            // When ask_sum ≤ threshold (looser than full arb), buy both legs cheaply.
            // Profits on convergence toward $1.00 or binary resolution.
            if self.cfg.spread_farm_enabled {
                let farm_max = self.cfg.spread_farm_max_ask_sum;
                if farm_max > Decimal::ZERO
                    && net_mispricing < self.cfg.min_taker_edge   // avoid double-counting full arb
                {
                    if let Some(sum) = ask_sum {
                        if sum <= farm_max && sum > Decimal::ZERO {
                            let farm_usdc = if self.cfg.spread_farm_usdc > Decimal::ZERO {
                                self.cfg.spread_farm_usdc
                            } else {
                                self.cfg.quote_size_usdc
                            };
                            decisions.taker_intents.push(TakerIntent {
                                condition_id: market.condition_id.clone(),
                                token_id: market.yes_token_id.clone(),
                                side: Side::Buy,
                                usdc_notional: farm_usdc,
                                reason: format!("spread-farm YES ask_sum={sum}"),
                            });
                            decisions.taker_intents.push(TakerIntent {
                                condition_id: market.condition_id.clone(),
                                token_id: market.no_token_id.clone(),
                                side: Side::Buy,
                                usdc_notional: farm_usdc,
                                reason: format!("spread-farm NO ask_sum={sum}"),
                            });
                        }
                    }
                }
            }

            if !decisions.quote_intents.is_empty() || !decisions.taker_intents.is_empty() {
                out.push((snapshot, decisions));
            }
        }

        Ok(out)
    }

    fn should_take_basis_arb(&self, market: &MarketMeta, now_ms: i64, basis: Decimal) -> bool {
        let taker_fee_proxy = estimate_taker_fee_from_schedule(dec!(0.5), &market.fee_schedule);
        let net_basis_edge = basis.abs() - taker_fee_proxy;
        if net_basis_edge < self.cfg.chainlink_pyth_arb_min_edge {
            return false;
        }

        // Require momentum agreement so we avoid taking stale-oracle basis against current drift.
        let momentum = self.momentum_cached(market, now_ms);
        if basis >= Decimal::ZERO {
            momentum >= Decimal::ZERO
        } else {
            momentum <= Decimal::ZERO
        }
    }

    /// Uses the pre-computed `market_underlying` map (no heap alloc).
    fn directional_bias_cached(
        &self,
        market: &MarketMeta,
        now_ms: i64,
        chainlink_pyth_basis: Option<Decimal>,
    ) -> Decimal {
        let Some(underlying) = self.market_underlying.get(&market.condition_id).copied() else {
            return Decimal::ZERO;
        };

        let max_stale_ms = i64::try_from(self.cfg.ref_price_max_staleness_ms).unwrap_or(i64::MAX);
        let momentum = self
            .refs
            .get(underlying)
            .map(|s| s.momentum(now_ms, max_stale_ms))
            .unwrap_or(Decimal::ZERO);

        let basis_skew =
            chainlink_pyth_basis.unwrap_or(Decimal::ZERO) * self.cfg.chainlink_pyth_bias_weight;
        momentum + basis_skew
    }

    fn momentum_cached(&self, market: &MarketMeta, now_ms: i64) -> Decimal {
        let Some(underlying) = self.market_underlying.get(&market.condition_id).copied() else {
            return Decimal::ZERO;
        };

        let max_stale_ms = i64::try_from(self.cfg.ref_price_max_staleness_ms).unwrap_or(i64::MAX);
        self.refs
            .get(underlying)
            .map(|s| s.momentum(now_ms, max_stale_ms))
            .unwrap_or(Decimal::ZERO)
    }

    /// Uses the pre-computed `market_underlying` map (no heap alloc).
    fn chainlink_pyth_basis_cached(&self, market: &MarketMeta, now_ms: i64) -> Option<Decimal> {
        let underlying = self.market_underlying.get(&market.condition_id).copied()?;
        let max_stale_ms = i64::try_from(self.cfg.ref_price_max_staleness_ms).unwrap_or(i64::MAX);
        self.refs
            .get(underlying)
            .and_then(|refs| refs.chainlink_pyth_basis(now_ms, max_stale_ms))
    }

    #[allow(clippy::too_many_arguments)]
    fn make_quotes(
        &self,
        market: &MarketMeta,
        yes_mid: Decimal,
        no_mid: Decimal,
        directional_bias: Decimal,
        books: MarketBookState,
        fill_probability: Decimal,
        queue_proxy: Decimal,
        adverse_selection: Decimal,
        signal_strength: Decimal,
    ) -> Vec<QuoteIntent> {
        let Some(controls) = self.quote_controls(
            signal_strength,
            fill_probability,
            queue_proxy,
            adverse_selection,
        ) else {
            return Vec::new();
        };

        let one_sided = self.cfg.maker_one_sided_bias_threshold > Decimal::ZERO
            && directional_bias.abs() >= self.cfg.maker_one_sided_bias_threshold;
        let allow_yes_buy = !one_sided || directional_bias >= Decimal::ZERO;
        let allow_no_buy = !one_sided || directional_bias <= Decimal::ZERO;
        let min_quote_edge = self.cfg.maker_min_quote_edge.max(Decimal::ZERO);
        let min_expected_value = self.cfg.maker_min_expected_value.max(Decimal::ZERO);
        let min_fill_probability = self
            .cfg
            .maker_min_fill_probability
            .clamp(Decimal::ZERO, Decimal::ONE);
        let cancel_cost = self.cfg.maker_cancel_cost_per_quote.max(Decimal::ZERO);

        let mut candidates = Vec::<(QuoteIntent, Decimal)>::new();
        let half_spread =
            (self.cfg.quote_half_spread_bps / dec!(10000)) * controls.spread_multiplier;
        let step = self.cfg.quote_level_step_bps / dec!(10000);
        let tick = market.tick_size.max(dec!(0.0001));
        let venue_min = market.min_order_size.max(dec!(0.01));
        let target_usdc = (self.cfg.quote_size_usdc * controls.size_multiplier)
            .max(self.cfg.min_quote_size_usdc * dec!(0.5));

        for level in 0..self.cfg.quote_levels {
            let level_delta = step * Decimal::from(level as u64);
            let spread = half_spread + level_delta;

            let y_bid_raw = clamp_01(yes_mid * (dec!(1) - spread + directional_bias));
            let y_ask_raw = clamp_01(yes_mid * (dec!(1) + spread + directional_bias));
            let n_bid_raw = clamp_01(no_mid * (dec!(1) - spread - directional_bias));
            let n_ask_raw = clamp_01(no_mid * (dec!(1) + spread - directional_bias));

            let y_bid = ensure_buy_post_only(y_bid_raw, &books.yes, tick);
            let y_ask = ensure_sell_post_only(y_ask_raw, &books.yes, tick);
            let n_bid = ensure_buy_post_only(n_bid_raw, &books.no, tick);
            let n_ask = ensure_sell_post_only(n_ask_raw, &books.no, tick);

            let clip = |price: Decimal| {
                clip_maker_shares(
                    target_usdc,
                    price,
                    self.cfg.min_quote_size_usdc,
                    self.cfg.max_quote_size_usdc,
                    self.cfg.clip_min_usdc,
                    self.cfg.clip_max_usdc,
                    venue_min,
                )
            };

            // Static reason strings avoid format!() heap allocation on every decision tick.
            let reason_yes = quote_reason_yes(level);
            let reason_no = quote_reason_no(level);

            if allow_yes_buy && let Some(p) = y_bid {
                let sz = clip(p);
                let fill_prob = level_fill_probability(
                    &books.yes,
                    fill_probability,
                    queue_proxy,
                    level,
                    self.cfg.queue_decay_lambda,
                );
                let edge = maker_edge(Side::Buy, p, yes_mid);
                let ev = expected_maker_value_per_share(
                    Side::Buy,
                    p,
                    yes_mid,
                    fill_prob,
                    &market.fee_schedule,
                    adverse_selection,
                    self.cfg.adverse_selection_weight,
                    cancel_cost,
                );
                if sz >= venue_min
                    && edge >= min_quote_edge
                    && fill_prob >= min_fill_probability
                    && ev >= min_expected_value
                {
                    candidates.push((
                        QuoteIntent {
                            condition_id: market.condition_id.clone(),
                            token_id: market.yes_token_id.clone(),
                            side: Side::Buy,
                            price: p,
                            size: sz,
                            post_only: true,
                            time_in_force: "GTC".to_string(),
                            reason: reason_yes.to_string(),
                        },
                        ev,
                    ));
                }
            }
            if self.cfg.maker_sell_quotes
                && let Some(p) = y_ask
            {
                let sz = clip(p);
                let fill_prob = level_fill_probability(
                    &books.yes,
                    fill_probability,
                    queue_proxy,
                    level,
                    self.cfg.queue_decay_lambda,
                );
                let edge = maker_edge(Side::Sell, p, yes_mid);
                let ev = expected_maker_value_per_share(
                    Side::Sell,
                    p,
                    yes_mid,
                    fill_prob,
                    &market.fee_schedule,
                    adverse_selection,
                    self.cfg.adverse_selection_weight,
                    cancel_cost,
                );
                if sz >= venue_min
                    && edge >= min_quote_edge
                    && fill_prob >= min_fill_probability
                    && ev >= min_expected_value
                {
                    candidates.push((
                        QuoteIntent {
                            condition_id: market.condition_id.clone(),
                            token_id: market.yes_token_id.clone(),
                            side: Side::Sell,
                            price: p,
                            size: sz,
                            post_only: true,
                            time_in_force: "GTC".to_string(),
                            reason: reason_yes.to_string(),
                        },
                        ev,
                    ));
                }
            }
            if allow_no_buy && let Some(p) = n_bid {
                let sz = clip(p);
                let fill_prob = level_fill_probability(
                    &books.no,
                    fill_probability,
                    queue_proxy,
                    level,
                    self.cfg.queue_decay_lambda,
                );
                let edge = maker_edge(Side::Buy, p, no_mid);
                let ev = expected_maker_value_per_share(
                    Side::Buy,
                    p,
                    no_mid,
                    fill_prob,
                    &market.fee_schedule,
                    adverse_selection,
                    self.cfg.adverse_selection_weight,
                    cancel_cost,
                );
                if sz >= venue_min
                    && edge >= min_quote_edge
                    && fill_prob >= min_fill_probability
                    && ev >= min_expected_value
                {
                    candidates.push((
                        QuoteIntent {
                            condition_id: market.condition_id.clone(),
                            token_id: market.no_token_id.clone(),
                            side: Side::Buy,
                            price: p,
                            size: sz,
                            post_only: true,
                            time_in_force: "GTC".to_string(),
                            reason: reason_no.to_string(),
                        },
                        ev,
                    ));
                }
            }
            if self.cfg.maker_sell_quotes
                && let Some(p) = n_ask
            {
                let sz = clip(p);
                let fill_prob = level_fill_probability(
                    &books.no,
                    fill_probability,
                    queue_proxy,
                    level,
                    self.cfg.queue_decay_lambda,
                );
                let edge = maker_edge(Side::Sell, p, no_mid);
                let ev = expected_maker_value_per_share(
                    Side::Sell,
                    p,
                    no_mid,
                    fill_prob,
                    &market.fee_schedule,
                    adverse_selection,
                    self.cfg.adverse_selection_weight,
                    cancel_cost,
                );
                if sz >= venue_min
                    && edge >= min_quote_edge
                    && fill_prob >= min_fill_probability
                    && ev >= min_expected_value
                {
                    candidates.push((
                        QuoteIntent {
                            condition_id: market.condition_id.clone(),
                            token_id: market.no_token_id.clone(),
                            side: Side::Sell,
                            price: p,
                            size: sz,
                            post_only: true,
                            time_in_force: "GTC".to_string(),
                            reason: reason_no.to_string(),
                        },
                        ev,
                    ));
                }
            }
        }

        if self.cfg.maker_max_quotes_per_market > 0
            && candidates.len() > self.cfg.maker_max_quotes_per_market
        {
            candidates.sort_by(|a, b| b.1.cmp(&a.1));
            candidates.truncate(self.cfg.maker_max_quotes_per_market);
        }

        candidates.into_iter().map(|(q, _)| q).collect()
    }

    fn quote_controls(
        &self,
        signal_strength: Decimal,
        fill_probability: Decimal,
        queue_proxy: Decimal,
        adverse_selection: Decimal,
    ) -> Option<QuoteControls> {
        let signal_floor_cfg = self.cfg.maker_min_signal_edge.max(Decimal::ZERO);
        if signal_floor_cfg > Decimal::ZERO && signal_strength < signal_floor_cfg {
            return None;
        }
        let signal_floor = signal_floor_cfg.max(dec!(0.0001));
        let edge_ratio = (signal_strength / signal_floor).clamp(Decimal::ZERO, dec!(2));
        let fill_component = fill_probability.clamp(Decimal::ZERO, Decimal::ONE);
        let queue_component = queue_proxy.clamp(Decimal::ZERO, Decimal::ONE);
        let quality = (fill_component * dec!(0.85) + queue_component * dec!(0.15))
            .clamp(Decimal::ZERO, Decimal::ONE);

        let adverse_penalty =
            (adverse_selection.abs() * self.cfg.adverse_selection_weight * dec!(10))
                .clamp(Decimal::ZERO, dec!(0.95));

        let net_quality = (quality - adverse_penalty).clamp(Decimal::ZERO, Decimal::ONE);
        if net_quality <= dec!(0.02) {
            return None;
        }

        let kelly_cap = self
            .cfg
            .fractional_kelly_cap
            .clamp(dec!(0.05), Decimal::ONE);
        let edge_bonus = edge_ratio.min(Decimal::ONE);
        let size_multiplier = ((net_quality * dec!(0.7) + edge_bonus * dec!(0.3))
            * (kelly_cap * dec!(2)))
        .clamp(dec!(0.10), Decimal::ONE);
        let spread_multiplier =
            (dec!(1) + adverse_penalty - edge_bonus * dec!(0.20) - quality * dec!(0.10))
                .clamp(dec!(0.60), dec!(2.00));

        Some(QuoteControls {
            size_multiplier,
            spread_multiplier,
        })
    }
}

// ---------------------------------------------------------------------------
// Static reason string tables
// ---------------------------------------------------------------------------

/// Returns a `&'static str` for levels 0-7; falls back to a leaked allocation
/// for larger levels (never expected in practice).
#[inline]
fn quote_reason_yes(level: usize) -> &'static str {
    const TABLE: [&str; 8] = [
        "maker-quote level=0 outcome=YES",
        "maker-quote level=1 outcome=YES",
        "maker-quote level=2 outcome=YES",
        "maker-quote level=3 outcome=YES",
        "maker-quote level=4 outcome=YES",
        "maker-quote level=5 outcome=YES",
        "maker-quote level=6 outcome=YES",
        "maker-quote level=7 outcome=YES",
    ];
    TABLE
        .get(level)
        .copied()
        .unwrap_or("maker-quote outcome=YES")
}

#[inline]
fn quote_reason_no(level: usize) -> &'static str {
    const TABLE: [&str; 8] = [
        "maker-quote level=0 outcome=NO",
        "maker-quote level=1 outcome=NO",
        "maker-quote level=2 outcome=NO",
        "maker-quote level=3 outcome=NO",
        "maker-quote level=4 outcome=NO",
        "maker-quote level=5 outcome=NO",
        "maker-quote level=6 outcome=NO",
        "maker-quote level=7 outcome=NO",
    ];
    TABLE
        .get(level)
        .copied()
        .unwrap_or("maker-quote outcome=NO")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn snapshot_top(bids: &[BookLevel], asks: &[BookLevel], ts_ms: i64) -> BookTop {
    BookTop {
        best_bid: bids.first().cloned(),
        best_ask: asks.first().cloned(),
        ts_ms,
    }
}

fn midpoint(top: &BookTop) -> Option<Decimal> {
    top.best_bid
        .as_ref()
        .zip(top.best_ask.as_ref())
        .map(|(bid, ask)| (bid.price + ask.price) / dec!(2))
}

fn fair_value(top: &BookTop) -> Option<Decimal> {
    midpoint(top)
        .or_else(|| top.best_bid.as_ref().map(|b| b.price))
        .or_else(|| top.best_ask.as_ref().map(|a| a.price))
        .map(clamp_01)
}

fn fair_values(yes: &BookTop, no: &BookTop) -> (Option<Decimal>, Option<Decimal>) {
    let mut yes_fair = fair_value(yes);
    let mut no_fair = fair_value(no);

    if yes_fair.is_none() {
        yes_fair = no_fair.map(|n| clamp_01(dec!(1) - n));
    }
    if no_fair.is_none() {
        no_fair = yes_fair.map(|y| clamp_01(dec!(1) - y));
    }

    (yes_fair, no_fair)
}

fn midpoint_for_token(
    state: &MarketBookState,
    token_id: &str,
    token_map: &HashMap<String, (String, bool)>,
) -> Option<Decimal> {
    let canon = normalize_clob_token_id(token_id);
    let (_, is_yes) = token_map
        .get(&canon)
        .with_context(|| format!("missing token mapping for {token_id}"))
        .ok()?;
    if *is_yes {
        midpoint(&state.yes)
    } else {
        midpoint(&state.no)
    }
}

fn fill_probability(state: &MarketBookState) -> Decimal {
    let yes_top = state
        .yes
        .best_bid
        .as_ref()
        .zip(state.yes.best_ask.as_ref())
        .map(|(bid, ask)| bid.size + ask.size)
        .unwrap_or(Decimal::ZERO);
    let no_top = state
        .no
        .best_bid
        .as_ref()
        .zip(state.no.best_ask.as_ref())
        .map(|(bid, ask)| bid.size + ask.size)
        .unwrap_or(Decimal::ZERO);

    let depth = yes_top + no_top;
    if depth <= Decimal::ZERO {
        return Decimal::ZERO;
    }

    depth / (depth + dec!(100))
}

fn queue_position_proxy(state: &MarketBookState) -> Decimal {
    let top_size = state
        .yes
        .best_bid
        .as_ref()
        .map(|x| x.size)
        .unwrap_or(Decimal::ZERO)
        + state
            .no
            .best_bid
            .as_ref()
            .map(|x| x.size)
            .unwrap_or(Decimal::ZERO);

    if top_size <= Decimal::ZERO {
        return Decimal::ONE;
    }

    Decimal::ONE / (Decimal::ONE + top_size)
}

fn quote_size_from_notional(notional: Decimal, price: Decimal) -> Decimal {
    if price <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    (notional / price).round_dp(2)
}

fn quote_signal_strength(
    mispricing_edge: Decimal,
    chainlink_pyth_basis: Decimal,
    directional_bias: Decimal,
) -> Decimal {
    mispricing_edge
        .max(Decimal::ZERO)
        .max(chainlink_pyth_basis.abs())
        .max(directional_bias.abs())
}

#[inline]
fn maker_edge(side: Side, quote_price: Decimal, fair: Decimal) -> Decimal {
    match side {
        Side::Buy => fair - quote_price,
        Side::Sell => quote_price - fair,
    }
}

fn level_fill_probability(
    book: &BookTop,
    base_fill_probability: Decimal,
    queue_proxy: Decimal,
    level: usize,
    queue_decay_lambda: Decimal,
) -> Decimal {
    let depth = book
        .best_bid
        .as_ref()
        .map(|x| x.size)
        .unwrap_or(Decimal::ZERO)
        + book
            .best_ask
            .as_ref()
            .map(|x| x.size)
            .unwrap_or(Decimal::ZERO);
    let depth_component = if depth > Decimal::ZERO {
        (depth / (depth + dec!(150))).clamp(Decimal::ZERO, Decimal::ONE)
    } else {
        Decimal::ZERO
    };

    let spread = book
        .best_bid
        .as_ref()
        .zip(book.best_ask.as_ref())
        .map(|(bid, ask)| (ask.price - bid.price).max(Decimal::ZERO))
        .unwrap_or(dec!(0.05));
    let spread_penalty = (spread / dec!(0.05)).clamp(Decimal::ZERO, Decimal::ONE);
    let queue_component = (Decimal::ONE - queue_proxy.clamp(Decimal::ZERO, Decimal::ONE))
        .clamp(Decimal::ZERO, Decimal::ONE);

    let lambda = queue_decay_lambda.max(dec!(0.05));
    let level_factor = Decimal::ONE / (Decimal::ONE + Decimal::from(level as u64) * lambda);

    let p = (base_fill_probability.clamp(Decimal::ZERO, Decimal::ONE) * dec!(0.50)
        + depth_component * dec!(0.25)
        + queue_component * dec!(0.20)
        + (Decimal::ONE - spread_penalty) * dec!(0.05))
        * level_factor;
    p.clamp(Decimal::ZERO, Decimal::ONE)
}

#[allow(clippy::too_many_arguments)]
fn expected_maker_value_per_share(
    side: Side,
    quote_price: Decimal,
    fair: Decimal,
    fill_probability: Decimal,
    fee: &polyhft_core::types::FeeSchedule,
    adverse_selection: Decimal,
    adverse_weight: Decimal,
    cancel_cost_per_quote: Decimal,
) -> Decimal {
    let p_fill = fill_probability.clamp(Decimal::ZERO, Decimal::ONE);
    let edge = maker_edge(side, quote_price, fair);
    let rebate = estimate_maker_rebate(fair, fee) * fair;
    let adverse_cost = adverse_selection.abs() * adverse_weight.clamp(Decimal::ZERO, Decimal::ONE);

    p_fill * (edge + rebate - adverse_cost)
        - (Decimal::ONE - p_fill) * cancel_cost_per_quote.max(Decimal::ZERO)
}

/// Size is in outcome shares; min/max_quote are legacy share clamps; clip_* are USDC notional bounds.
fn clip_maker_shares(
    target_usdc: Decimal,
    price: Decimal,
    min_shares: Decimal,
    max_shares: Decimal,
    clip_min_usdc: Decimal,
    clip_max_usdc: Decimal,
    venue_min_shares: Decimal,
) -> Decimal {
    let mut shares = quote_size_from_notional(target_usdc, price)
        .max(min_shares)
        .max(venue_min_shares)
        .min(max_shares);

    if price <= Decimal::ZERO {
        return Decimal::ZERO;
    }

    if clip_min_usdc > Decimal::ZERO {
        let mut n = shares * price;
        let mut guard = 0_u32;
        while n < clip_min_usdc && guard < 50_000 {
            shares += dec!(0.01);
            shares = shares.min(max_shares);
            n = shares * price;
            guard += 1;
            if shares >= max_shares && n < clip_min_usdc {
                break;
            }
        }
    }

    shares = shares.max(venue_min_shares);

    if clip_max_usdc > Decimal::ZERO {
        let mut n = shares * price;
        let mut guard = 0_u32;
        while n > clip_max_usdc && shares > venue_min_shares && guard < 50_000 {
            shares -= dec!(0.01);
            if shares < venue_min_shares {
                shares = venue_min_shares;
                break;
            }
            n = shares * price;
            guard += 1;
        }
    }

    shares.round_dp(2).max(venue_min_shares).max(Decimal::ZERO)
}

/// Post-only buy must rest strictly below the best ask (tick-aware).
fn ensure_buy_post_only(price: Decimal, book: &BookTop, tick: Decimal) -> Option<Decimal> {
    let Some(ask) = book.best_ask.as_ref() else {
        return Some(clamp_01(price));
    };
    if tick <= Decimal::ZERO {
        let p = price.min(ask.price - dec!(0.001));
        return Some(clamp_01(p));
    }
    let max_buy = ask.price - tick;
    if max_buy < dec!(0.01) {
        return None;
    }
    let mut p = price.min(max_buy);
    p = floor_to_tick(p, tick);
    if p >= ask.price {
        p = floor_to_tick(ask.price - tick, tick);
    }
    if p < dec!(0.01) || p >= ask.price {
        return None;
    }
    Some(clamp_01(p))
}

/// Post-only sell must rest strictly above the best bid.
fn ensure_sell_post_only(price: Decimal, book: &BookTop, tick: Decimal) -> Option<Decimal> {
    let Some(bid) = book.best_bid.as_ref() else {
        return Some(clamp_01(price));
    };
    if tick <= Decimal::ZERO {
        let p = price.max(bid.price + dec!(0.001));
        return Some(clamp_01(p));
    }
    let min_sell = bid.price + tick;
    if min_sell > dec!(0.99) {
        return None;
    }
    let mut p = price.max(min_sell);
    p = ceil_to_tick(p, tick);
    if p <= bid.price {
        p = ceil_to_tick(bid.price + tick, tick);
    }
    if p > dec!(0.99) || p <= bid.price {
        return None;
    }
    Some(clamp_01(p))
}

fn floor_to_tick(p: Decimal, tick: Decimal) -> Decimal {
    if tick <= Decimal::ZERO {
        return p;
    }
    (p / tick).floor() * tick
}

fn ceil_to_tick(p: Decimal, tick: Decimal) -> Decimal {
    if tick <= Decimal::ZERO {
        return p;
    }
    (p / tick).ceil() * tick
}

fn clamp_01(price: Decimal) -> Decimal {
    if price < dec!(0.01) {
        dec!(0.01)
    } else if price > dec!(0.99) {
        dec!(0.99)
    } else {
        price.round_dp(4)
    }
}

/// Case-insensitive search without heap allocation (ASCII only).
#[inline]
fn contains_ci(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

#[inline]
fn contains_ci_token(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).enumerate().any(|(i, w)| {
        if !w.eq_ignore_ascii_case(needle) {
            return false;
        }
        let left_ok = i == 0 || !haystack[i - 1].is_ascii_alphanumeric();
        let right_idx = i + needle.len();
        let right_ok = right_idx >= haystack.len() || !haystack[right_idx].is_ascii_alphanumeric();
        left_ok && right_ok
    })
}

/// Update fast/slow book-update rate EWMAs on each incoming book event.
/// `book_rate_fast` uses α=0.30; `book_rate_slow` uses α=0.05.
/// "Rate" is approximated as events-per-second from the inter-arrival gap.
fn update_book_rate(state: &mut MarketBookState, now_ms: i64) {
    if state.book_last_update_ms > 0 {
        let gap_ms = (now_ms - state.book_last_update_ms).max(1);
        // Convert gap to events/sec (clamp to avoid extreme values on reconnects).
        let rate = Decimal::from(1_000) / Decimal::from(gap_ms.min(30_000));
        let alpha_fast = dec!(0.30);
        let alpha_slow = dec!(0.05);
        if state.book_rate_fast.is_zero() {
            state.book_rate_fast = rate;
            state.book_rate_slow = rate;
        } else {
            state.book_rate_fast =
                alpha_fast * rate + (Decimal::ONE - alpha_fast) * state.book_rate_fast;
            state.book_rate_slow =
                alpha_slow * rate + (Decimal::ONE - alpha_slow) * state.book_rate_slow;
        }
    }
    state.book_last_update_ms = now_ms;
}

fn market_underlying_from_question(question: &str) -> Option<&'static str> {
    let q = question.as_bytes();
    if contains_ci(q, b"bitcoin") || contains_ci_token(q, b"btc") {
        Some("btcusd")
    } else if contains_ci(q, b"ethereum") || contains_ci_token(q, b"eth") {
        Some("ethusd")
    } else if contains_ci(q, b"solana") || contains_ci_token(q, b"sol") {
        Some("solusd")
    } else {
        None
    }
}

fn market_underlying_from_symbol(symbol: &str) -> Option<&'static str> {
    let s = symbol.as_bytes();
    if contains_ci(s, b"btc") {
        Some("btcusd")
    } else if contains_ci(s, b"eth") {
        Some("ethusd")
    } else if contains_ci(s, b"sol") {
        Some("solusd")
    } else {
        None
    }
}

fn estimate_taker_fee_from_schedule(
    probability: Decimal,
    fee: &polyhft_core::types::FeeSchedule,
) -> Decimal {
    let p = probability.to_f64().unwrap_or(0.5);
    let exponent = fee.exponent.to_f64().unwrap_or(1.0);
    let rate = fee.rate.to_f64().unwrap_or(0.0);
    let fee_fraction = rate * p * (1.0 - p).powf(exponent);
    Decimal::from_f64_retain(fee_fraction).unwrap_or(Decimal::ZERO)
}

fn estimate_maker_rebate(probability: Decimal, fee: &polyhft_core::types::FeeSchedule) -> Decimal {
    let taker_fee = estimate_taker_fee_from_schedule(probability, fee);
    taker_fee * fee.rebate_rate
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use rust_decimal_macros::dec;

    use super::*;
    use polyhft_core::config::AppConfig;
    use polyhft_core::types::{
        BookLevel, FeeSchedule, MarketBookEvent, MarketMeta, NormalizedEvent, Side, TradeLifecycle,
        UserTradeEvent,
    };

    fn market(condition_id: &str, yes_token: &str, no_token: &str, question: &str) -> MarketMeta {
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
            yes_token_id: yes_token.to_string(),
            no_token_id: no_token.to_string(),
            tick_size: dec!(0.01),
            min_order_size: dec!(1),
            event_start_time: Some(Utc::now()),
            end_time: Some(Utc::now()),
        }
    }

    fn book_event(
        condition_id: &str,
        token_id: &str,
        bid_px: Option<Decimal>,
        ask_px: Option<Decimal>,
        size: Decimal,
        ts_ms: i64,
    ) -> NormalizedEvent {
        NormalizedEvent::MarketBook(MarketBookEvent {
            condition_id: condition_id.to_string(),
            token_id: token_id.to_string(),
            bids: bid_px
                .map(|price| vec![BookLevel { price, size }])
                .unwrap_or_default(),
            asks: ask_px
                .map(|price| vec![BookLevel { price, size }])
                .unwrap_or_default(),
            ts_ms,
        })
    }

    fn first_buy_size(batch: &DecisionBatch, token_id: &str) -> Decimal {
        batch
            .quote_intents
            .iter()
            .find(|q| q.token_id == token_id && q.side == Side::Buy)
            .map(|q| q.size)
            .unwrap_or(Decimal::ZERO)
    }

    #[test]
    fn quotes_even_when_one_side_of_one_book_is_missing() {
        let mut cfg = AppConfig::default();
        cfg.strategy.maker_min_signal_edge = Decimal::ZERO;
        cfg.strategy.maker_min_quote_edge = Decimal::ZERO;
        cfg.strategy.maker_min_expected_value = Decimal::ZERO;
        cfg.strategy.maker_min_fill_probability = Decimal::ZERO;
        cfg.strategy.maker_cancel_cost_per_quote = Decimal::ZERO;
        let market = market("cond-1", "1001", "1002", "Bitcoin Up or Down 5m?");
        let engine = SignalEngine::new(&cfg, vec![market.clone()]).unwrap();

        engine.on_event(&book_event(
            &market.condition_id,
            &market.yes_token_id,
            Some(dec!(0.47)),
            None,
            dec!(40),
            1_000,
        ));
        engine.on_event(&book_event(
            &market.condition_id,
            &market.no_token_id,
            Some(dec!(0.51)),
            Some(dec!(0.53)),
            dec!(40),
            1_000,
        ));

        let decisions = engine.build_decisions(1_050).unwrap();
        assert!(
            !decisions.is_empty(),
            "expected maker quotes even with a one-sided YES book"
        );
        assert!(
            !decisions[0].1.quote_intents.is_empty(),
            "expected non-empty quote intents"
        );
    }

    #[test]
    fn quote_size_changes_with_depth_and_adverse_selection() {
        let mut cfg = AppConfig::default();
        cfg.strategy.quote_levels = 1;
        cfg.strategy.quote_size_usdc = dec!(25);
        cfg.strategy.min_quote_size_usdc = dec!(1);
        cfg.strategy.max_quote_size_usdc = dec!(200);
        cfg.strategy.maker_min_signal_edge = Decimal::ZERO;
        cfg.strategy.maker_min_quote_edge = Decimal::ZERO;
        cfg.strategy.maker_min_expected_value = Decimal::ZERO;
        cfg.strategy.maker_min_fill_probability = Decimal::ZERO;
        cfg.strategy.maker_cancel_cost_per_quote = Decimal::ZERO;

        let market = market("cond-2", "2001", "2002", "Bitcoin Up or Down 5m?");
        let engine = SignalEngine::new(&cfg, vec![market.clone()]).unwrap();

        engine.on_event(&book_event(
            &market.condition_id,
            &market.yes_token_id,
            Some(dec!(0.48)),
            Some(dec!(0.52)),
            dec!(5),
            1_000,
        ));
        engine.on_event(&book_event(
            &market.condition_id,
            &market.no_token_id,
            Some(dec!(0.48)),
            Some(dec!(0.52)),
            dec!(5),
            1_000,
        ));

        let low_quality = engine.build_decisions(1_050).unwrap();
        let low_size = first_buy_size(&low_quality[0].1, &market.yes_token_id);

        engine.on_event(&book_event(
            &market.condition_id,
            &market.yes_token_id,
            Some(dec!(0.48)),
            Some(dec!(0.52)),
            dec!(250),
            1_100,
        ));
        engine.on_event(&book_event(
            &market.condition_id,
            &market.no_token_id,
            Some(dec!(0.48)),
            Some(dec!(0.52)),
            dec!(250),
            1_100,
        ));

        let high_quality = engine.build_decisions(1_150).unwrap();
        let high_size = first_buy_size(&high_quality[0].1, &market.yes_token_id);

        engine.on_event(&NormalizedEvent::UserTrade(UserTradeEvent {
            trade_id: "t1".to_string(),
            order_id: Some("o1".to_string()),
            condition_id: market.condition_id.clone(),
            token_id: market.yes_token_id.clone(),
            side: Side::Buy,
            size: dec!(10),
            price: dec!(0.10),
            status: TradeLifecycle::Confirmed,
            taker_or_maker: Some("maker".to_string()),
            tx_hash: None,
            ts_ms: 1_200,
        }));

        let adverse = engine.build_decisions(1_250).unwrap();
        let adverse_size = first_buy_size(&adverse[0].1, &market.yes_token_id);

        assert!(
            high_size > low_size,
            "expected deeper book / higher fill quality to increase quote size"
        );
        assert!(
            adverse_size < high_size,
            "expected adverse selection to reduce quote size"
        );
    }
}
