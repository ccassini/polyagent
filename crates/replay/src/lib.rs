use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};

use anyhow::{Context, anyhow};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use tracing::info;
use uuid::Uuid;

use polyhft_core::config::AppConfig;
use polyhft_core::types::{
    DecisionBatch, ExecutionReport, MarketMeta, NormalizedEvent, PnLAttribution, Side,
    TradeLifecycle,
};
use polyhft_risk::RiskManager;
use polyhft_signal::SignalEngine;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplaySummary {
    pub event_count: usize,
    pub simulated_trades: usize,
    pub pnl: PnLAttribution,
    pub maker_ratio: Decimal,
}

#[derive(Debug, Clone)]
struct TimedEvent {
    ts_ms: i64,
    event: NormalizedEvent,
}

#[derive(Debug)]
pub struct ReplayEngine {
    cfg: AppConfig,
    markets: Vec<MarketMeta>,
}

impl ReplayEngine {
    #[must_use]
    pub fn new(cfg: AppConfig, markets: Vec<MarketMeta>) -> Self {
        Self { cfg, markets }
    }

    pub fn run_from_ndjson(&self, path: &str) -> anyhow::Result<ReplaySummary> {
        let mut events = load_events(path)?;
        events.sort_by(|a, b| a.ts_ms.cmp(&b.ts_ms));

        let signal = SignalEngine::new(&self.cfg, self.markets.clone())?;
        let mut risk = RiskManager::new(&self.cfg, &self.markets);
        let mut mark_prices: HashMap<String, Decimal> = HashMap::new();
        let mut pnl = PnLAttribution::default();
        let mut trade_count = 0usize;
        let mut maker_count = 0usize;

        let mut rng = StdRng::seed_from_u64(self.cfg.replay.random_seed);

        for timed in events.iter() {
            let injected_latency = sample_latency_ms(&self.cfg, &mut rng) as i64;
            let sim_ts = timed.ts_ms + injected_latency;

            risk.on_event(&timed.event);
            signal.on_event(&timed.event);

            update_prices(&mut mark_prices, &timed.event);

            let decisions = signal.build_decisions(sim_ts)?;
            for (_snapshot, batch) in decisions {
                let bounded = risk.apply_limits(sim_ts, &batch, &mark_prices);
                let fills = simulate_fills(
                    &self.cfg.replay,
                    &self.markets,
                    &mark_prices,
                    bounded,
                    sim_ts,
                    &mut rng,
                );

                for fill in fills {
                    trade_count += 1;
                    if fill.maker {
                        maker_count += 1;
                    }
                    pnl.spread_capture += fill.spread_capture;
                    pnl.maker_rebate += fill.rebate_earned;
                    pnl.taker_fee += fill.fee_paid;
                    pnl.adverse_selection += fill.adverse_selection_cost;
                    pnl.total += fill.spread_capture + fill.rebate_earned
                        - fill.fee_paid
                        - fill.adverse_selection_cost;
                    risk.on_execution(&fill);
                }
            }
        }

        let maker_ratio = if trade_count == 0 {
            Decimal::ZERO
        } else {
            Decimal::from(maker_count as i64) / Decimal::from(trade_count as i64)
        };

        info!(
            events = events.len(),
            trades = trade_count,
            maker_ratio = %maker_ratio,
            pnl_total = %pnl.total,
            "replay complete"
        );

        Ok(ReplaySummary {
            event_count: events.len(),
            simulated_trades: trade_count,
            pnl,
            maker_ratio,
        })
    }
}

fn load_events(path: &str) -> anyhow::Result<Vec<TimedEvent>> {
    let file = File::open(path).with_context(|| format!("failed to open replay file: {path}"))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();

    for (idx, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("failed to read line {idx}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str::<NormalizedEvent>(&line)
            .with_context(|| format!("invalid event JSON at line {idx}"))?;
        let ts_ms = extract_ts(&event);
        out.push(TimedEvent { ts_ms, event });
    }

    if out.is_empty() {
        return Err(anyhow!("replay input has no events"));
    }

    Ok(out)
}

fn extract_ts(event: &NormalizedEvent) -> i64 {
    match event {
        NormalizedEvent::MarketBook(e) => e.ts_ms,
        NormalizedEvent::PriceChange(e) => e.ts_ms,
        NormalizedEvent::LastTradePrice(e) => e.ts_ms,
        NormalizedEvent::UserOrder(e) => e.ts_ms,
        NormalizedEvent::UserTrade(e) => e.ts_ms,
        NormalizedEvent::Sports(e) => e.ts_ms,
        NormalizedEvent::RtdsPrice(e) => e.ts_ms,
        NormalizedEvent::Heartbeat { ts_ms } => *ts_ms,
    }
}

fn sample_latency_ms(cfg: &AppConfig, rng: &mut StdRng) -> u64 {
    let u = rng.random::<f64>();
    if u <= 0.50 {
        rng.random_range(0..=cfg.replay.latency_p50_ms)
    } else if u <= 0.95 {
        rng.random_range(cfg.replay.latency_p50_ms..=cfg.replay.latency_p95_ms)
    } else {
        rng.random_range(cfg.replay.latency_p95_ms..=cfg.replay.latency_p99_ms)
    }
}

fn update_prices(map: &mut HashMap<String, Decimal>, event: &NormalizedEvent) {
    match event {
        NormalizedEvent::PriceChange(e) => {
            map.insert(e.token_id.clone(), e.price);
        }
        NormalizedEvent::LastTradePrice(e) => {
            map.insert(e.token_id.clone(), e.price);
        }
        NormalizedEvent::MarketBook(e) => {
            if let (Some(bid), Some(ask)) = (e.bids.first(), e.asks.first()) {
                map.insert(e.token_id.clone(), (bid.price + ask.price) / dec!(2));
            }
        }
        _ => {}
    }
}

/// Simulate fills for a post-risk [`DecisionBatch`] using the same heuristics as offline replay.
///
/// Used by the live agent in paper / `--dry-run` mode: no CLOB orders, no balance spent.
pub fn simulate_fills(
    paper: &polyhft_core::config::ReplayConfig,
    markets: &[MarketMeta],
    prices: &HashMap<String, Decimal>,
    batch: DecisionBatch,
    ts_ms: i64,
    rng: &mut StdRng,
) -> Vec<ExecutionReport> {
    let mut out = Vec::new();
    let maker_p = paper
        .paper_maker_backstop_fill_probability
        .clamp(0.0, 1.0);
    let taker_p = paper.paper_taker_fill_probability.clamp(0.0, 1.0);
    let slip_bps = paper.paper_taker_slippage_bps.min(2_000);

    for quote in batch.quote_intents {
        let px = prices.get(&quote.token_id).copied().unwrap_or(quote.price);
        let should_fill = match quote.side {
            Side::Buy => quote.price >= px || rng.random::<f64>() < maker_p,
            Side::Sell => quote.price <= px || rng.random::<f64>() < maker_p,
        };

        if should_fill {
            let market = markets
                .iter()
                .find(|m| m.condition_id == quote.condition_id);
            let rebate_rate = market
                .map(|m| m.fee_schedule.rebate_rate)
                .unwrap_or(Decimal::ZERO);
            let notional = quote.price * quote.size;
            let rebate = notional * rebate_rate;

            out.push(ExecutionReport {
                id: Uuid::new_v4(),
                timestamp_ms: ts_ms,
                condition_id: quote.condition_id,
                token_id: quote.token_id,
                side: quote.side,
                price: quote.price,
                size: quote.size,
                maker: true,
                lifecycle: TradeLifecycle::Confirmed,
                fee_paid: Decimal::ZERO,
                rebate_earned: rebate,
                spread_capture: (quote.price - px).abs(),
                adverse_selection_cost: match quote.side {
                    Side::Buy => (px - quote.price).max(Decimal::ZERO),
                    Side::Sell => (quote.price - px).max(Decimal::ZERO),
                },
                venue_order_id: None,
                tx_hash: None,
                reason: quote.reason,
            });
        }
    }

    for taker in batch.taker_intents {
        if rng.random::<f64>() >= taker_p {
            continue;
        }
        let px = prices.get(&taker.token_id).copied().unwrap_or(dec!(0.5));
        if px <= Decimal::ZERO {
            continue;
        }
        let slip = Decimal::from(slip_bps) / dec!(10_000);
        let fill_px = match taker.side {
            Side::Buy => (px * (Decimal::ONE + slip)).min(dec!(0.999)),
            Side::Sell => (px * (Decimal::ONE - slip)).max(dec!(0.001)),
        };
        if fill_px <= Decimal::ZERO {
            continue;
        }
        let size = (taker.usdc_notional / fill_px).round_dp(2);
        let fee_paid = taker.usdc_notional * dec!(0.01);
        out.push(ExecutionReport {
            id: Uuid::new_v4(),
            timestamp_ms: ts_ms,
            condition_id: taker.condition_id,
            token_id: taker.token_id,
            side: taker.side,
            price: fill_px,
            size,
            maker: false,
            lifecycle: TradeLifecycle::Confirmed,
            fee_paid,
            rebate_earned: Decimal::ZERO,
            spread_capture: Decimal::ZERO,
            adverse_selection_cost: Decimal::ZERO,
            venue_order_id: None,
            tx_hash: None,
            reason: taker.reason,
        });
    }

    out.sort_by(|a, b| {
        if a.timestamp_ms == b.timestamp_ms {
            Ordering::Equal
        } else if a.timestamp_ms < b.timestamp_ms {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    });

    out
}
