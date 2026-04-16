//! JSON API for the flow-theatre UI and dashboard:
//! - GET  /api/activity  → markets + recent fills + stats
//! - GET  /api/orders    → open resting orders (requires execution engine)
//! - POST /api/trade     → manual FOK taker order
//! - POST /api/orders/cancel      → cancel a specific order
//! - POST /api/orders/cancel-all  → cancel all resting orders

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use polyhft_core::types::{ExecutionReport, MarketMeta, Side};
use polyhft_execution::{ExecutionEngine, ManualTakerRequest};
use polyhft_paper_engine::PaperFill;

// ── Public response / request types ─────────────────────────────────────────

#[derive(Clone, Serialize)]
pub struct MarketRow {
    pub market_id: String,
    pub condition_id: String,
    pub slug: String,
    pub question: String,
    pub tick_size: String,
    pub min_order_size: String,
}

#[derive(Clone, Serialize)]
pub struct TradeRow {
    pub ts_ms: i64,
    pub condition_id: String,
    pub slug: String,
    pub question: String,
    pub leg: String,
    pub side: String,
    pub price: String,
    pub size: String,
    pub role: String,
    pub reason: String,
    pub order_id: Option<String>,
}

#[derive(Clone, Serialize, Default)]
pub struct ActivityStats {
    pub maker_fills_session: u64,
    pub taker_fills_session: u64,
    pub trades_in_buffer: usize,
    pub session_volume_usdc: String,
    pub session_volume_shares: String,
}

#[derive(Clone, Serialize)]
pub struct ActivitySnapshot {
    pub server_time_ms: i64,
    pub market_count: usize,
    pub markets: Vec<MarketRow>,
    pub trades: Vec<TradeRow>,
    pub stats: ActivityStats,
    pub paper_mode: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paper_balance_usdc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paper_pnl_usdc: Option<String>,
}

#[derive(Serialize)]
pub struct OrdersResponse {
    pub server_time_ms: i64,
    pub orders: Vec<polyhft_execution::OrderSnapshot>,
}

#[derive(Deserialize)]
pub struct ManualTradeRequest {
    pub token_id: String,
    /// "buy" or "sell"
    pub side: String,
    pub usdc_notional: f64,
    pub reason: Option<String>,
}

#[derive(Deserialize)]
pub struct CancelRequest {
    pub order_id: String,
}

// ── Internal state ────────────────────────────────────────────────────────────

#[derive(Clone)]
struct TokenMeta {
    condition_id: String,
    slug: String,
    question: String,
    leg: String,
}

struct Inner {
    markets: Vec<MarketRow>,
    token_meta: HashMap<String, TokenMeta>,
    trades: Vec<TradeRow>,
    maker_fills: u64,
    taker_fills: u64,
    paper_balance_usdc: Decimal,
    session_volume_usdc: Decimal,
    session_volume_shares: Decimal,
}

/// Shared handle between the trading loop and the HTTP server.
///
/// Optionally holds an `Arc<ExecutionEngine>` — set via
/// [`ActivityHandle::with_execution`] in live (non-paper) mode.
/// In paper mode, balance is tracked via [`ActivityHandle::with_paper_mode`].
#[derive(Clone)]
pub struct ActivityHandle {
    inner: Arc<RwLock<Inner>>,
    max_trades: usize,
    execution: Option<Arc<ExecutionEngine>>,
    paper_mode: bool,
    paper_starting_balance: Decimal,
}

impl ActivityHandle {
    #[must_use]
    pub fn new(max_trades: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                markets: Vec::new(),
                token_meta: HashMap::new(),
                trades: Vec::new(),
                maker_fills: 0,
                taker_fills: 0,
                paper_balance_usdc: Decimal::ZERO,
                session_volume_usdc: Decimal::ZERO,
                session_volume_shares: Decimal::ZERO,
            })),
            max_trades: max_trades.max(16),
            execution: None,
            paper_mode: false,
            paper_starting_balance: Decimal::ZERO,
        }
    }

    /// Enable paper-mode balance tracking with the given starting balance (USDC).
    /// Must be called before the handle is cloned or shared across tasks.
    #[must_use]
    pub fn with_paper_mode(mut self, paper_mode: bool, starting_balance: Decimal) -> Self {
        self.paper_mode = paper_mode;
        self.paper_starting_balance = starting_balance;
        if paper_mode {
            if let Ok(mut g) = self.inner.try_write() {
                g.paper_balance_usdc = starting_balance;
            }
        }
        self
    }

    /// Attach an execution engine so the API can expose orders and accept
    /// manual trade requests. Call this after the engine is constructed.
    #[must_use]
    pub fn with_execution(mut self, engine: Arc<ExecutionEngine>) -> Self {
        self.execution = Some(engine);
        self
    }

    pub async fn set_markets(&self, markets: &[MarketMeta]) {
        let mut rows = Vec::with_capacity(markets.len());
        let mut token_meta = HashMap::new();

        for m in markets {
            rows.push(MarketRow {
                market_id: m.market_id.clone(),
                condition_id: m.condition_id.clone(),
                slug: m.slug.clone(),
                question: m.question.clone(),
                tick_size: m.tick_size.to_string(),
                min_order_size: m.min_order_size.to_string(),
            });

            let updown = m.slug.to_ascii_lowercase().contains("updown");
            let (yes_leg, no_leg) = if updown {
                ("UP".to_string(), "DOWN".to_string())
            } else {
                ("YES".to_string(), "NO".to_string())
            };

            token_meta.insert(
                m.yes_token_id.clone(),
                TokenMeta {
                    condition_id: m.condition_id.clone(),
                    slug: m.slug.clone(),
                    question: m.question.clone(),
                    leg: yes_leg,
                },
            );
            token_meta.insert(
                m.no_token_id.clone(),
                TokenMeta {
                    condition_id: m.condition_id.clone(),
                    slug: m.slug.clone(),
                    question: m.question.clone(),
                    leg: no_leg,
                },
            );
        }

        let mut g = self.inner.write().await;
        g.markets = rows;
        g.token_meta = token_meta;
    }

    pub async fn record_execution(&self, report: &ExecutionReport) {
        let meta = {
            let g = self.inner.read().await;
            g.token_meta.get(&report.token_id).cloned()
        };
        let Some(meta) = meta else {
            return;
        };

        let role = if report.maker { "MAKER" } else { "TAKER" };
        let side = match report.side {
            polyhft_core::types::Side::Buy => "BUY",
            polyhft_core::types::Side::Sell => "SELL",
        };

        let row = TradeRow {
            ts_ms: report.timestamp_ms,
            condition_id: meta.condition_id,
            slug: meta.slug,
            question: truncate_question(&meta.question),
            leg: meta.leg,
            side: side.to_string(),
            price: report.price.to_string(),
            size: report.size.to_string(),
            role: role.to_string(),
            reason: truncate_reason(&report.reason),
            order_id: report.venue_order_id.clone(),
        };

        let mut g = self.inner.write().await;
        if report.maker {
            g.maker_fills = g.maker_fills.saturating_add(1);
        } else {
            g.taker_fills = g.taker_fills.saturating_add(1);
        }
        g.trades.insert(0, row);
        g.trades.truncate(self.max_trades);
        g.session_volume_usdc += report.price * report.size;
        g.session_volume_shares += report.size;

        // Track paper balance: BUY spends USDC, SELL receives USDC.
        if self.paper_mode {
            let notional = report.price * report.size;
            match report.side {
                polyhft_core::types::Side::Buy => {
                    g.paper_balance_usdc -= notional + report.fee_paid;
                }
                polyhft_core::types::Side::Sell => {
                    g.paper_balance_usdc += notional - report.fee_paid;
                }
            }
        }
    }

    /// Record a paper-engine simulated fill so `/api/activity` and paper balance
    /// stay in sync with the favorite-side paper engine.
    pub async fn record_paper_fill(&self, fill: &PaperFill) {
        let (condition_id, slug, question, leg) = {
            let g = self.inner.read().await;
            if let Some(meta) = g.token_meta.get(&fill.token_id) {
                (
                    meta.condition_id.clone(),
                    meta.slug.clone(),
                    meta.question.clone(),
                    meta.leg.clone(),
                )
            } else if let Some(market) = g
                .markets
                .iter()
                .find(|m| m.condition_id == fill.condition_id)
            {
                (
                    market.condition_id.clone(),
                    market.slug.clone(),
                    market.question.clone(),
                    "UNKNOWN".to_string(),
                )
            } else {
                (
                    fill.condition_id.clone(),
                    fill.condition_id.clone(),
                    fill.condition_id.clone(),
                    "UNKNOWN".to_string(),
                )
            }
        };

        let side = match fill.side {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        };
        let reason = if let Some(exit) = fill.exit_reason {
            exit.as_str().to_string()
        } else {
            fill.fill_status.as_str().to_string()
        };

        let row = TradeRow {
            ts_ms: fill.timestamp_ms,
            condition_id,
            slug,
            question: truncate_question(&question),
            leg,
            side: side.to_string(),
            price: fill.fill_price.to_string(),
            size: fill.filled_size.to_string(),
            role: "PAPER".to_string(),
            reason: truncate_reason(&reason),
            order_id: Some(fill.id.to_string()),
        };

        let mut g = self.inner.write().await;
        g.taker_fills = g.taker_fills.saturating_add(1);
        g.trades.insert(0, row);
        g.trades.truncate(self.max_trades);
        g.session_volume_usdc += fill.fill_price * fill.filled_size;
        g.session_volume_shares += fill.filled_size;

        if self.paper_mode {
            let notional = fill.fill_price * fill.filled_size;
            match fill.side {
                Side::Buy => {
                    g.paper_balance_usdc -= notional + fill.fee_usdc;
                }
                Side::Sell => {
                    g.paper_balance_usdc += notional - fill.fee_usdc;
                }
            }
        }
    }

    async fn snapshot_json(&self) -> ActivitySnapshot {
        let g = self.inner.read().await;
        let (paper_balance, paper_pnl) = if self.paper_mode {
            let pnl = g.paper_balance_usdc - self.paper_starting_balance;
            (
                Some(format!("{:.2}", g.paper_balance_usdc)),
                Some(format!("{:.2}", pnl)),
            )
        } else {
            (None, None)
        };
        ActivitySnapshot {
            server_time_ms: unix_ms(),
            market_count: g.markets.len(),
            markets: g.markets.clone(),
            trades: g.trades.clone(),
            stats: ActivityStats {
                maker_fills_session: g.maker_fills,
                taker_fills_session: g.taker_fills,
                trades_in_buffer: g.trades.len(),
                session_volume_usdc: format!("{:.2}", g.session_volume_usdc),
                session_volume_shares: format!("{:.4}", g.session_volume_shares),
            },
            paper_mode: self.paper_mode,
            paper_balance_usdc: paper_balance,
            paper_pnl_usdc: paper_pnl,
        }
    }
}

// ── Route helpers ─────────────────────────────────────────────────────────────

fn truncate_question(q: &str) -> String {
    const MAX: usize = 120;
    if q.len() <= MAX {
        return q.to_string();
    }
    format!("{}…", &q[..MAX.saturating_sub(1)])
}

fn truncate_reason(r: &str) -> String {
    const MAX: usize = 80;
    if r.len() <= MAX {
        return r.to_string();
    }
    format!("{}…", &r[..MAX.saturating_sub(1)])
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn parse_side(s: &str) -> Option<Side> {
    match s.trim().to_ascii_lowercase().as_str() {
        "buy" => Some(Side::Buy),
        "sell" => Some(Side::Sell),
        _ => None,
    }
}

// ── Axum handlers ─────────────────────────────────────────────────────────────

async fn activity_handler(State(handle): State<ActivityHandle>) -> Json<ActivitySnapshot> {
    Json(handle.snapshot_json().await)
}

async fn orders_handler(
    State(handle): State<ActivityHandle>,
) -> (StatusCode, Json<serde_json::Value>) {
    let now_ms = unix_ms();
    match handle.execution.as_ref() {
        Some(exec) => {
            let orders = exec.open_orders_snapshot(now_ms);
            let resp = OrdersResponse { server_time_ms: now_ms, orders };
            (StatusCode::OK, Json(serde_json::to_value(resp).unwrap_or_default()))
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "execution engine not available (paper mode)" })),
        ),
    }
}

async fn trade_handler(
    State(handle): State<ActivityHandle>,
    Json(req): Json<ManualTradeRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(exec) = handle.execution.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "ok": false, "error": "paper mode — no live execution engine" })),
        );
    };

    let Some(side) = parse_side(&req.side) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": "side must be 'buy' or 'sell'" })),
        );
    };

    if req.usdc_notional <= 0.0 || !req.usdc_notional.is_finite() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": "usdc_notional must be a positive finite number" })),
        );
    }

    let usdc = match Decimal::try_from(req.usdc_notional) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "ok": false, "error": format!("invalid usdc_notional: {e}") })),
            );
        }
    };

    let reason = req.reason.unwrap_or_else(|| "dashboard-manual".to_string());
    let manual_req = ManualTakerRequest {
        token_id: req.token_id.clone(),
        side,
        usdc_notional: usdc,
        reason: reason.clone(),
    };

    match exec.place_manual_taker(manual_req).await {
        Ok(order_id) => {
            info!(order_id, token_id = %req.token_id, side = %req.side, usdc = %usdc, "manual trade placed via API");
            (
                StatusCode::OK,
                Json(serde_json::json!({ "ok": true, "order_id": order_id, "message": format!("taker FOK placed: {order_id}") })),
            )
        }
        Err(e) => {
            warn!(error = %e, token_id = %req.token_id, "manual trade via API failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
            )
        }
    }
}

async fn cancel_order_handler(
    State(handle): State<ActivityHandle>,
    Json(req): Json<CancelRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(exec) = handle.execution.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "ok": false, "error": "paper mode" })),
        );
    };

    match exec.cancel_order_pub(&req.order_id).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "message": format!("cancelled {}", req.order_id) })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        ),
    }
}

async fn cancel_all_handler(
    State(handle): State<ActivityHandle>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(exec) = handle.execution.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "ok": false, "error": "paper mode" })),
        );
    };

    match exec.cancel_all_pub().await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "ok": true, "message": "all resting orders cancelled" })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        ),
    }
}

// ── Server bootstrap ─────────────────────────────────────────────────────────

/// Binds `bind` address and serves the activity API.
///
/// Routes:
/// - `GET  /api/activity`            — flow-theatre feed
/// - `GET  /api/orders`              — resting orders snapshot
/// - `POST /api/trade`               — manual FOK taker
/// - `POST /api/orders/cancel`       — cancel one order `{ order_id }`
/// - `POST /api/orders/cancel-all`   — cancel all
///
/// Port-scan logic: if the preferred port is busy, scans up to +32 ports.
pub fn spawn_activity_api(bind: &str, handle: ActivityHandle) -> anyhow::Result<()> {
    let bind = bind.trim();
    if bind.is_empty() {
        return Ok(());
    }
    let preferred: SocketAddr = bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid telemetry.activity_api_bind {bind:?}: {e}"))?;

    let app = Router::new()
        .route("/api/activity", get(activity_handler))
        .route("/api/orders", get(orders_handler))
        .route("/api/trade", post(trade_handler))
        .route("/api/orders/cancel", post(cancel_order_handler))
        .route("/api/orders/cancel-all", post(cancel_all_handler))
        .with_state(handle);

    tokio::spawn(async move {
        let ip = preferred.ip();
        let base_port = preferred.port();
        let max_port = base_port.saturating_add(32);
        let mut listener: Option<tokio::net::TcpListener> = None;
        let mut chosen = preferred;

        for port in base_port..=max_port {
            let candidate = SocketAddr::new(ip, port);
            match tokio::net::TcpListener::bind(candidate).await {
                Ok(l) => {
                    listener = Some(l);
                    chosen = candidate;
                    if port != base_port {
                        warn!(
                            configured = %preferred,
                            actual = %chosen,
                            "activity API: configured port was busy; using adjacent port"
                        );
                    }
                    break;
                }
                Err(e) if e.kind() == ErrorKind::AddrInUse => continue,
                Err(e) => {
                    error!(addr = %candidate, error = %e, "activity API bind failed");
                    return;
                }
            }
        }

        let Some(listener) = listener else {
            error!(first = %base_port, last = %max_port, "activity API: no free port in range");
            return;
        };

        info!(%chosen, "activity API listening (GET /api/activity, GET /api/orders, POST /api/trade)");
        if let Err(e) = axum::serve(listener, app).await {
            error!(error = %e, "activity API server ended");
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use polyhft_paper_engine::{Leg, PaperFill};

    fn sample_paper_fill(side: Side, price: Decimal, size: Decimal, fee: Decimal) -> PaperFill {
        serde_json::from_value(serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "timestamp_ms": 1_777_000_000_000i64,
            "condition_id": "cond-test",
            "token_id": "token-test",
            "leg": match Leg::Yes { Leg::Yes => "yes", Leg::No => "no" },
            "side": match side { Side::Buy => "buy", Side::Sell => "sell" },
            "fill_price": price.to_string(),
            "requested_size": size.to_string(),
            "filled_size": size.to_string(),
            "fee_usdc": fee.to_string(),
            "slippage": "0",
            "fill_delay_ms": 0,
            "fill_status": "order_filled",
            "order_events": ["order_submitted", "order_filled"],
            "features": {
                "ts_exchange_ms": 0,
                "ts_received_ms": 0,
                "hours_to_expiry": 0.0,
                "quote_age_ms": 0,
                "best_bid": "0",
                "best_ask": "0",
                "momentum_30s": "0",
                "momentum_2m": "0",
                "volatility_5m": "0",
                "volume_5m": "0",
                "orderbook_imbalance": "0",
                "favorite_side": "",
                "favorite_probability": "0",
                "spread_fraction": "0",
                "depth": "0",
                "expected_edge_after_costs": "0"
            },
            "skip_reason": serde_json::Value::Null,
            "exit_reason": serde_json::Value::Null,
            "position_id": serde_json::Value::Null
        }))
        .expect("paper fill fixture should deserialize")
    }

    #[tokio::test]
    async fn paper_fill_updates_balance_and_volume() {
        let handle = ActivityHandle::new(32).with_paper_mode(true, Decimal::new(5_000, 0));
        let buy = sample_paper_fill(
            Side::Buy,
            Decimal::new(80, 2),  // 0.80
            Decimal::new(10, 0),  // 10
            Decimal::new(1, 2),   // 0.01
        );
        let sell = sample_paper_fill(
            Side::Sell,
            Decimal::new(90, 2),  // 0.90
            Decimal::new(10, 0),  // 10
            Decimal::new(1, 2),   // 0.01
        );

        handle.record_paper_fill(&buy).await;
        handle.record_paper_fill(&sell).await;
        let snapshot = handle.snapshot_json().await;

        assert_eq!(snapshot.stats.trades_in_buffer, 2);
        assert_eq!(snapshot.stats.taker_fills_session, 2);
        assert_eq!(snapshot.stats.session_volume_usdc, "17.00");
        assert_eq!(snapshot.stats.session_volume_shares, "20.0000");
        assert_eq!(snapshot.paper_balance_usdc.as_deref(), Some("5000.98"));
        assert_eq!(snapshot.paper_pnl_usdc.as_deref(), Some("0.98"));
    }
}
