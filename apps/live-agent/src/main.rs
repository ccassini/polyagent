use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use metrics::{counter, gauge};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{error, info, warn};

use polyhft_core::clock::{DriftMonitor, MonotonicClock};
use polyhft_core::config::AppConfig;
use polyhft_core::telemetry::{init_metrics, init_tracing};
use polyhft_core::types::{MarketMeta, NormalizedEvent};
use polyhft_execution::ExecutionEngine;
use polyhft_ingest::{IngestionService, MarketDiscovery, UserStreamAuth, bootstrap_books, spawn_book_poller};
use polyhft_paper_engine::PaperEngine;
use polyhft_risk::RiskManager;
use polyhft_signal::SignalEngine;

mod activity_api;
mod training_log;

use activity_api::{ActivityHandle, spawn_activity_api};
use training_log::TrainingLogger;

/// Keep the process (and Prometheus listener) alive while Gamma is flaky or patterns match nothing.
const DISCOVERY_RETRY_SECS: u64 = 5;

#[derive(Debug, Parser)]
#[command(author, version, about = "Low-latency Polymarket HFT agent")]
struct Args {
    #[arg(long, default_value = "config.toml")]
    config: String,

    /// No CLOB auth/orders; fills are simulated (see `polyhft_replay::simulate_fills`) for risk + activity UI.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

#[derive(Debug, Default)]
struct RuntimeCounters {
    maker_reports: u64,
    taker_reports: u64,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = AppConfig::load(&args.config)?;
    let paper_mode = args.dry_run || cfg.execution.paper_trading;
    info!(
        config_path = %args.config,
        paper_mode,
        decision_interval_ms = cfg.strategy.decision_interval_ms,
        maker_quotes_enabled = cfg.strategy.maker_quotes_enabled,
        directional_market_enabled = cfg.strategy.directional_market_enabled,
        directional_market_min_abs_bias = %cfg.strategy.directional_market_min_abs_bias,
        directional_market_usdc = %cfg.strategy.directional_market_usdc,
        momentum_taker_max_ask_sum = %cfg.strategy.momentum_taker_max_ask_sum,
        chainlink_pyth_arb_min_edge = %cfg.strategy.chainlink_pyth_arb_min_edge,
        quote_size_usdc = %cfg.strategy.quote_size_usdc,
        taker_cooldown_ms = cfg.execution.taker_rebalance_cooldown_ms,
        "loaded runtime strategy config"
    );
    if paper_mode {
        if args.dry_run && cfg.execution.paper_trading {
            warn!(
                "paper mode: --dry-run and execution.paper_trading are both set; no live CLOB orders, fills are simulated"
            );
        } else if args.dry_run {
            warn!(
                "paper mode (--dry-run): no CLOB orders; fills simulated for risk + activity UI (backtest-style)"
            );
        } else {
            warn!(
                "paper mode (execution.paper_trading = true): no CLOB orders; fills simulated (same model as replay-cli)"
            );
        }
    }

    init_tracing(&cfg.telemetry.log_level, cfg.telemetry.json_logs)?;
    init_metrics(&cfg.telemetry.prometheus_bind)?;

    // Activity handle is created early; the execution engine is attached below
    // (after it is constructed) so the API can serve orders and accept manual trades.
    // In paper mode we track a $5,000 paper balance against simulated fills.
    let paper_start = Decimal::from(5_000u32);
    let activity_base = ActivityHandle::new(256).with_paper_mode(paper_mode, paper_start);
    let training_logger = TrainingLogger::maybe_spawn(
        &cfg.telemetry.training_log_path,
        cfg.telemetry.training_log_queue,
    )?;

    let markets = loop {
        let discovery = match MarketDiscovery::new(cfg.clone()) {
            Ok(d) => d,
            Err(err) => {
                warn!(
                    error = %err,
                    retry_secs = DISCOVERY_RETRY_SECS,
                    "market discovery init failed; retrying",
                );
                tokio::time::sleep(std::time::Duration::from_secs(DISCOVERY_RETRY_SECS)).await;
                continue;
            }
        };
        match discovery.discover().await {
            Ok(markets) => {
                info!(
                    market_count = markets.len(),
                    decision_interval_ms = cfg.strategy.decision_interval_ms,
                    target_post_only_ratio = %cfg.strategy.post_only_target_ratio,
                    "market discovery complete"
                );
                break markets;
            }
            Err(err) => {
                warn!(
                    error = %err,
                    retry_secs = DISCOVERY_RETRY_SECS,
                    "market discovery failed; retrying (metrics endpoint stays up)",
                );
                tokio::time::sleep(std::time::Duration::from_secs(DISCOVERY_RETRY_SECS)).await;
            }
        }
    };

    activity_base.set_markets(&markets).await;

    let mut current_markets = markets.clone();
    let mut signal = SignalEngine::new(&cfg, markets.clone())?;
    let mut risk = RiskManager::new(&cfg, &markets);

    // Favorite-side paper engine — active in BOTH paper and live modes.
    // Paper mode: simulates fills internally for P&L tracking.
    // Live mode: generates TakerIntents routed through the ExecutionEngine.
    let paper_engine = Arc::new(PaperEngine::new(
        cfg.paper_engine.clone(),
        cfg.replay.random_seed,
    ));

    let execution = if paper_mode {
        None
    } else {
        let engine = Arc::new(
            ExecutionEngine::new(cfg.clone(), markets.clone())
                .await
                .context("failed to initialize execution engine")?,
        );
        if cfg.execution.enable_clob_heartbeats {
            info!("CLOB heartbeats enabled (POST /v1/heartbeats)");
            let _heartbeat_handle = engine.clone().spawn_heartbeat_task();
        } else {
            info!(
                "CLOB heartbeats disabled (set execution.enable_clob_heartbeats = true for order-priority pings)"
            );
        }
        Some(engine)
    };

    // Attach the execution engine to the activity handle (non-paper only),
    // then bind the HTTP server so it can serve /api/orders and /api/trade.
    let activity = match execution.as_ref() {
        Some(engine) => activity_base.with_execution(engine.clone()),
        None => activity_base,
    };
    spawn_activity_api(&cfg.telemetry.activity_api_bind, activity.clone())?;
    let user_stream_auth = execution.as_ref().map(|engine| {
        let (credentials, wallet) = engine.user_stream_auth();
        UserStreamAuth {
            credentials,
            wallet,
        }
    });

    let (tx, mut rx) = mpsc::channel(64 * 1024);
    let ingest = IngestionService::new(cfg.clone());
    let mut ingest_handles =
        ingest.spawn_with_user_auth(markets.clone(), tx.clone(), user_stream_auth.clone());

    // ── REST orderbook bootstrap ─────────────────────────────────────────────
    // Fetch initial book snapshots via REST so the paper engine has data
    // immediately — before WebSocket streams deliver their first events.
    info!("fetching initial orderbook snapshots via REST...");
    bootstrap_books(&cfg.clob.rest_url, &markets, &tx).await;

    // Periodic REST book polling as a safety net for WS gaps.
    // Keep REST fallback polling comfortably faster than the staleness threshold.
    // If websocket flow is sparse, 10s polling with 5s staleness guarantees
    // perpetual stale_quote skips.
    let poll_interval_ms = (cfg.paper_engine.quote_max_stale_ms / 2)
        .clamp(1_000, 5_000) as u64;
    let _book_poller_handle = spawn_book_poller(
        cfg.clob.rest_url.clone(),
        markets.clone(),
        tx.clone(),
        std::time::Duration::from_millis(poll_interval_ms),
    );

    let mut mark_prices: HashMap<String, Decimal> = HashMap::new();
    let mut counters = RuntimeCounters::default();
    let mut paper_rng = StdRng::seed_from_u64(cfg.replay.random_seed);

    let mut decision_ticker = tokio::time::interval(cfg.decision_interval());
    decision_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Periodic info-level summary of paper engine state (every ~10s).
    let mut pe_summary_ticker = tokio::time::interval(std::time::Duration::from_secs(10));
    pe_summary_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    pe_summary_ticker.tick().await; // skip immediate first tick

    let mut drift_ticker = tokio::time::interval(DriftMonitor::poll_interval());
    drift_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let refresh_secs = cfg.markets.refresh_secs.max(10);
    let mut refresh_ticker = tokio::time::interval(std::time::Duration::from_secs(refresh_secs));
    refresh_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    refresh_ticker.tick().await;

    let cfg_refresh = cfg.clone();

    loop {
        tokio::select! {
            _ = decision_ticker.tick() => {
                let now_ms = MonotonicClock::unix_time_ms();
                let cycle_start = std::time::Instant::now();
                if let Some(exec) = execution.as_ref()
                    && let Err(err) = exec.maintain_liveness(now_ms).await
                {
                    error!(error = %err, "liveness maintenance failed");
                }

                // ── Favorite-side engine (paper: simulate fills / live: real CLOB orders) ─
                if paper_mode {
                    // Paper mode: simulate fills internally.
                    let pe_fills = paper_engine.run_cycle(&current_markets, now_ms);
                    let pe_stats = paper_engine.stats();
                    for fill in &pe_fills {
                        activity.record_paper_fill(fill).await;
                        if let Some(logger) = training_logger.as_ref() {
                            logger.log_paper_fill(fill);
                        }
                    }
                    if !pe_fills.is_empty() {
                        info!(
                            fills = pe_fills.len(),
                            open_positions = paper_engine.open_positions().len(),
                            total_pnl = %pe_stats.total_pnl_usdc,
                            total_fees = %pe_stats.total_fees_usdc,
                            entries = pe_stats.entries,
                            exits = pe_stats.exits,
                            "paper_engine: cycle produced fills"
                        );
                        counter!("polyhft_paper_engine_fills_total").increment(pe_fills.len() as u64);
                    }
                    gauge!("polyhft_paper_engine_open_positions")
                        .set(paper_engine.open_positions().len() as f64);
                    gauge!("polyhft_paper_engine_total_pnl")
                        .set(pe_stats.total_pnl_usdc.to_f64().unwrap_or(0.0));
                } else if let Some(exec) = execution.as_ref() {
                    // Live mode: generate intents and route through execution engine.
                    let live_intents = paper_engine.generate_live_intents(&current_markets, now_ms);
                    let pe_stats = paper_engine.stats();

                    for intent in &live_intents {
                        let taker = polyhft_core::types::TakerIntent {
                            condition_id: intent.condition_id.clone(),
                            token_id: intent.token_id.clone(),
                            side: intent.side,
                            usdc_notional: intent.usdc_notional,
                            reason: intent.reason.clone(),
                        };
                        let batch = polyhft_core::types::DecisionBatch {
                            quote_intents: vec![],
                            taker_intents: vec![taker],
                        };
                        if let Err(err) = exec.apply_decisions(now_ms, batch).await {
                            warn!(
                                error = %err,
                                reason = %intent.reason,
                                "paper_engine live: apply_decisions failed"
                            );
                        } else {
                            info!(
                                side = ?intent.side,
                                token_id = %intent.token_id,
                                usdc = %intent.usdc_notional,
                                reason = %intent.reason,
                                "paper_engine live: order sent to CLOB"
                            );
                            counter!("polyhft_paper_engine_live_orders_total").increment(1);
                        }
                    }

                    gauge!("polyhft_paper_engine_open_positions")
                        .set(paper_engine.open_positions().len() as f64);
                    gauge!("polyhft_paper_engine_total_pnl")
                        .set(pe_stats.total_pnl_usdc.to_f64().unwrap_or(0.0));
                }

                match signal.build_decisions(now_ms) {
                    Ok(batches) => {
                        for (snapshot, batch) in batches {
                            let bounded = risk.apply_limits(now_ms, &batch, &mark_prices);
                            if bounded.quote_intents.is_empty() && bounded.taker_intents.is_empty() {
                                // Diagnostic: log why the batch produced no actionable intents.
                                // This was previously a silent drop — operators had no visibility.
                                tracing::debug!(
                                    condition_id = %snapshot.condition_id,
                                    mispricing_edge = %snapshot.mispricing_edge,
                                    "signal: batch produced no intents after risk filter (all gated)"
                                );
                                continue;
                            }

                            gauge!("polyhft_signal_edge").set(snapshot.mispricing_edge.to_f64().unwrap_or(0.0));
                            gauge!("polyhft_chainlink_pyth_basis").set(
                                snapshot
                                    .chainlink_pyth_basis
                                    .unwrap_or(Decimal::ZERO)
                                    .to_f64()
                                    .unwrap_or(0.0),
                            );
                            gauge!("polyhft_adverse_selection").set(
                                snapshot.adverse_selection_score.to_f64().unwrap_or(0.0),
                            );
                            if let Some(logger) = training_logger.as_ref() {
                                logger.log_decision(now_ms, paper_mode, &snapshot, &bounded);
                            }

                            if paper_mode {
                                let fills = polyhft_replay::simulate_fills(
                                    &cfg.replay,
                                    &current_markets,
                                    &mark_prices,
                                    bounded,
                                    now_ms,
                                    &mut paper_rng,
                                );
                                for fill in fills {
                                    risk.on_execution(&fill);
                                    activity.record_execution(&fill).await;
                                    if let Some(logger) = training_logger.as_ref() {
                                        logger.log_fill(&fill);
                                    }
                                    update_execution_counters(&mut counters, &fill);
                                    counter!("polyhft_paper_simulated_fills_total").increment(1);
                                }
                                continue;
                            }

                            let Some(exec) = execution.as_ref() else {
                                continue;
                            };
                            if let Err(err) = exec.apply_decisions(now_ms, bounded).await {
                                warn!(error = %err, "execution.apply_decisions failed");
                            }
                        }

                        let total = counters.maker_reports + counters.taker_reports;
                        if total > 0 {
                            let maker_ratio = counters.maker_reports as f64 / total as f64;
                            gauge!("polyhft_maker_ratio").set(maker_ratio);
                        }
                    }
                    Err(err) => warn!(error = %err, "signal generation failed"),
                }
                gauge!("polyhft_decision_cycle_ms").set(cycle_start.elapsed().as_secs_f64() * 1_000.0);
            }
            _ = pe_summary_ticker.tick() => {
                let pe_stats = paper_engine.stats();
                let open = paper_engine.open_positions();
                info!(
                    markets = current_markets.len(),
                    pe_entries = pe_stats.entries,
                    pe_exits = pe_stats.exits,
                    pe_open = open.len(),
                    pe_pnl = %pe_stats.total_pnl_usdc,
                    pe_fees = %pe_stats.total_fees_usdc,
                    pe_evaluated = pe_stats.markets_evaluated,
                    mode = if paper_mode { "paper" } else { "live" },
                    "agent status"
                );
                // Log top skip reasons so operator can see what's blocking trades.
                if !pe_stats.skip_counts.is_empty() {
                    let mut top_skips: Vec<_> = pe_stats.skip_counts.iter().collect();
                    top_skips.sort_by(|a, b| b.1.cmp(a.1));
                    top_skips.truncate(5);
                    let summary: String = top_skips
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    info!(top_skips = %summary, "paper_engine skip reasons (cumulative)");
                }
            }
            _ = drift_ticker.tick() => {
                if let Some(exec) = execution.as_ref() {
                    exec.poll_drift().await;
                }
            }
            _ = refresh_ticker.tick() => {
                match MarketDiscovery::new(cfg_refresh.clone()) {
                    Ok(d) => match d.discover().await {
                        Ok(new_markets) => {
                            if let Some(exec) = execution.as_ref()
                                && let Err(err) = exec.replace_markets(new_markets.clone()).await
                            {
                                warn!(error = %err, "market refresh: execution registry update failed; keeping existing market set");
                                continue;
                            }
                            if let Err(err) = signal.replace_markets(new_markets.clone()) {
                                warn!(error = %err, "market refresh: signal registry update failed; keeping existing market set");
                                continue;
                            }

                            risk.replace_markets(&new_markets);
                            retain_mark_prices(&mut mark_prices, &new_markets);
                            activity.set_markets(&new_markets).await;

                            let current_tokens = tracked_token_ids(&current_markets);
                            let new_tokens = tracked_token_ids(&new_markets);
                            if current_tokens != new_tokens {
                                restart_ingest_handles(
                                    &mut ingest_handles,
                                    &cfg_refresh,
                                    new_markets.clone(),
                                    tx.clone(),
                                    user_stream_auth.clone(),
                                );
                                info!(count = new_markets.len(), "market refresh complete: streams restarted");
                            } else {
                                info!(count = new_markets.len(), "market refresh complete: metadata updated without stream restart");
                            }

                            current_markets = new_markets;
                        }
                        Err(err) => warn!(error = %err, "market refresh discovery failed; keeping existing streams"),
                    },
                    Err(err) => warn!(error = %err, "market refresh: failed to build discovery"),
                }
            }
            item = rx.recv() => {
                let Some(event) = item else {
                    warn!("ingestion channel closed, shutting down live agent");
                    break;
                };
                gauge!("polyhft_event_queue_len").set((rx.len() + 1) as f64);
                if let Some(report) = process_event(
                    &event,
                    &mut mark_prices,
                    &signal,
                    &mut risk,
                    execution.as_deref(),
                    &paper_engine,
                    &current_markets,
                    &activity,
                    training_logger.as_ref(),
                )
                .await {
                    update_execution_counters(&mut counters, &report);
                }

                for _ in 0..255 {
                    let Ok(event) = rx.try_recv() else {
                        break;
                    };
                    if let Some(report) = process_event(
                        &event,
                        &mut mark_prices,
                        &signal,
                        &mut risk,
                        execution.as_deref(),
                        &paper_engine,
                        &current_markets,
                        &activity,
                        training_logger.as_ref(),
                    )
                    .await {
                        update_execution_counters(&mut counters, &report);
                    }
                }
                gauge!("polyhft_event_queue_len").set(rx.len() as f64);
            }
        }
    }

    Ok(())
}

async fn process_event(
    event: &NormalizedEvent,
    mark_prices: &mut HashMap<String, Decimal>,
    signal: &SignalEngine,
    risk: &mut RiskManager,
    execution: Option<&ExecutionEngine>,
    paper_engine: &PaperEngine,
    markets: &[MarketMeta],
    activity: &ActivityHandle,
    training_logger: Option<&TrainingLogger>,
) -> Option<polyhft_core::types::ExecutionReport> {
    counter!("polyhft_events_ingested_total").increment(1);
    update_mark_prices(mark_prices, event);
    signal.on_event(event);
    risk.on_event(event);

    // Feed raw book events to the paper engine so it can track top-of-book
    // for eligibility, signal, and exit management.
    feed_paper_engine(paper_engine, event, markets);

    if let Some(exec) = execution
        && let Some(report) = exec.on_event(event).await
    {
        // Feed live fills back to paper engine for position tracking.
        paper_engine.on_live_fill(
            &report.condition_id,
            &report.token_id,
            report.side,
            report.price,
            report.size,
            report.fee_paid,
        );

        risk.on_execution(&report);
        activity.record_execution(&report).await;
        if let Some(logger) = training_logger {
            logger.log_fill(&report);
        }
        return Some(report);
    }

    None
}

/// Forward `MarketBook` events from the ingest stream to the paper engine's
/// internal book state. Determines YES/NO leg by matching token_id against
/// the current market meta list.
fn feed_paper_engine(pe: &PaperEngine, event: &NormalizedEvent, markets: &[MarketMeta]) {
    use polyhft_core::types::NormalizedEvent::MarketBook;
    let MarketBook(e) = event else { return };
    // Determine which leg this token_id belongs to.
    let is_yes = markets.iter().any(|m| m.yes_token_id == e.token_id);
    let is_no  = !is_yes && markets.iter().any(|m| m.no_token_id == e.token_id);
    if !is_yes && !is_no {
        return; // unknown token; not a tracked market
    }
    // Use exchange timestamp for market-time provenance, but use local receive time
    // for staleness gating to avoid falsely rejecting delayed packets as stale.
    pe.on_book_update_with_timestamps(
        &e.condition_id,
        is_yes,
        &e.bids,
        &e.asks,
        e.ts_ms,
        MonotonicClock::unix_time_ms(),
    );
}

fn update_execution_counters(
    counters: &mut RuntimeCounters,
    report: &polyhft_core::types::ExecutionReport,
) {
    if report.maker {
        counters.maker_reports = counters.maker_reports.saturating_add(1);
        counter!("polyhft_maker_reports_total").increment(1);
    } else {
        counters.taker_reports = counters.taker_reports.saturating_add(1);
        counter!("polyhft_taker_reports_total").increment(1);
    }
}

fn update_mark_prices(map: &mut HashMap<String, Decimal>, event: &NormalizedEvent) {
    match event {
        NormalizedEvent::PriceChange(e) => {
            map.insert(e.token_id.clone(), e.price);
        }
        NormalizedEvent::LastTradePrice(e) => {
            map.insert(e.token_id.clone(), e.price);
        }
        NormalizedEvent::MarketBook(e) => {
            if let (Some(bid), Some(ask)) = (e.bids.first(), e.asks.first()) {
                map.insert(
                    e.token_id.clone(),
                    (bid.price + ask.price) / Decimal::from(2_u8),
                );
            }
        }
        _ => {}
    }
}

fn tracked_token_ids(markets: &[MarketMeta]) -> HashSet<String> {
    markets
        .iter()
        .flat_map(|market| [market.yes_token_id.clone(), market.no_token_id.clone()])
        .collect()
}

fn retain_mark_prices(map: &mut HashMap<String, Decimal>, markets: &[MarketMeta]) {
    let tracked = tracked_token_ids(markets);
    map.retain(|token_id, _| tracked.contains(token_id));
}

fn restart_ingest_handles(
    handles: &mut Vec<JoinHandle<()>>,
    cfg: &AppConfig,
    markets: Vec<MarketMeta>,
    tx: mpsc::Sender<NormalizedEvent>,
    user_stream_auth: Option<UserStreamAuth>,
) {
    for handle in handles.drain(..) {
        handle.abort();
    }

    let ingest = IngestionService::new(cfg.clone());
    *handles = ingest.spawn_with_user_auth(markets, tx, user_stream_auth);
}
