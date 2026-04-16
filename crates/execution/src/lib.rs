use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy_signer_local::PrivateKeySigner;
use anyhow::{Context, anyhow};
use dashmap::DashMap;
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{Normal, Signer};
use polymarket_client_sdk::clob::types::request::{
    BalanceAllowanceRequest, UpdateBalanceAllowanceRequest,
};
use polymarket_client_sdk::clob::types::{
    Amount, AssetType, OrderType, Side as ClobSide, SignatureType,
};
use polymarket_client_sdk::clob::{Client as ClobClient, Config as ClobConfig};
use polymarket_client_sdk::error::{
    Error as PolyError, Kind as PolyErrorKind, Status as PolyStatus,
};
use polymarket_client_sdk::types::{Address, Decimal, U256};
use polymarket_client_sdk::{POLYGON, derive_proxy_wallet, derive_safe_wallet};
use rand::Rng;
use rust_decimal_macros::dec;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use polyhft_core::clock::{DriftMonitor, MonotonicClock};
use polyhft_core::config::AppConfig;
use polyhft_core::types::{
    DecisionBatch, ExecutionReport, MarketMeta, NormalizedEvent, QuoteIntent, Side, TakerIntent,
};

#[derive(Debug, Clone)]
struct ManagedOrder {
    token_id: String,
    side: Side,
    price: Decimal,
    size: Decimal,
    created_ms: i64,
    last_touch_ms: i64,
}

/// Public snapshot of one resting order — returned by [`ExecutionEngine::open_orders_snapshot`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct OrderSnapshot {
    pub order_id: String,
    pub token_id: String,
    pub side: Side,
    pub price: String,
    pub size: String,
    pub age_ms: i64,
}

/// Request body for [`ExecutionEngine::place_manual_taker`].
#[derive(Debug, Clone)]
pub struct ManualTakerRequest {
    pub token_id: String,
    pub side: Side,
    pub usdc_notional: Decimal,
    pub reason: String,
}

#[derive(Debug)]
pub struct ExecutionEngine {
    cfg: AppConfig,
    markets: DashMap<String, MarketMeta>,
    signer: PrivateKeySigner,
    client: ClobClient<Authenticated<Normal>>,
    open_orders: DashMap<String, ManagedOrder>,
    /// Secondary index: (token_id, side) -> order_id.
    /// Allows O(1) lookup in sync_quote instead of O(n) linear scan over open_orders.
    order_index: DashMap<(String, Side), String>,
    /// Last taker send timestamp keyed by token id so same-leg spam is suppressed
    /// without blocking legitimate paired YES+NO entries in the same market.
    last_taker_ms: DashMap<String, i64>,
    last_heartbeat_ms: Arc<Mutex<i64>>,
    drift_monitor: DriftMonitor,
    /// Last `balance_allowance` poll (unix ms) and USDC collateral the CLOB attributes to this session.
    balance_poll: Arc<Mutex<(i64, Decimal)>>,
    placement_pause_last_log_ms: Arc<Mutex<i64>>,
}

impl ExecutionEngine {
    pub async fn new(cfg: AppConfig, markets: Vec<MarketMeta>) -> anyhow::Result<Self> {
        let private_key = cfg.auth.private_key.as_deref().unwrap_or("").trim();
        if private_key.is_empty() {
            anyhow::bail!(
                "auth.private_key is missing or empty. Set `POLY_HFT__AUTH__PRIVATE_KEY=0x…` in your environment or `private_key` in [auth] (64 hex digits, optional 0x prefix). Never commit real keys."
            );
        }

        let signer = PrivateKeySigner::from_str(private_key)
            .with_context(|| {
                format!(
                    "failed to parse private_key (expected 32-byte hex, got {} chars after trim; check for typos, spaces, or newlines)",
                    private_key.len()
                )
            })?
            .with_chain_id(Some(POLYGON));

        let eoa = signer.address();
        info!(%eoa, "CLOB signing EOA (from private_key)");
        if let Some(proxy) = derive_proxy_wallet(eoa, POLYGON) {
            info!(
                %proxy,
                "CREATE2 Polymarket proxy for this EOA (used when funder_address is empty)"
            );
        }
        if let Some(safe) = derive_safe_wallet(eoa, POLYGON) {
            info!(
                %safe,
                "CREATE2 Polymarket Safe for this EOA (if your funds sit here, use signature_kind = gnosis_safe and set funder_address)"
            );
        }
        match cfg
            .auth
            .funder_address
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(f) => info!(funder = %f, "explicit funder_address from config"),
            None => info!("funder_address empty: collateral account is the CREATE2 proxy above"),
        }

        let clob_cfg = ClobConfig::builder()
            .use_server_time(cfg.execution.use_server_time)
            .heartbeat_interval(Duration::from_secs(cfg.execution.heartbeat_interval_secs))
            .build();

        let mut auth_builder = ClobClient::new(&cfg.clob.rest_url, clob_cfg)
            .context("failed to create CLOB client")?
            .authentication_builder(&signer);

        let signature_type = match cfg.auth.signature_kind {
            polyhft_core::types::SignatureKind::Eoa => SignatureType::Eoa,
            polyhft_core::types::SignatureKind::PolyProxy => SignatureType::Proxy,
            polyhft_core::types::SignatureKind::GnosisSafe => SignatureType::GnosisSafe,
        };
        auth_builder = auth_builder.signature_type(signature_type);

        if let Some(funder_raw) = cfg.auth.funder_address.as_deref() {
            let funder = funder_raw.trim();
            if !funder.is_empty() {
                let funder = Address::from_str(funder)
                    .with_context(|| format!("invalid funder address: {funder}"))?;
                auth_builder = auth_builder.funder(funder);
            }
        }

        let client = auth_builder
            .authenticate()
            .await
            .context("failed to authenticate CLOB client")?;

        let map = DashMap::new();
        for market in markets {
            map.insert(market.condition_id.clone(), market);
        }

        let drift_monitor =
            DriftMonitor::new(cfg.infra.ptp_drift_warn_ms, cfg.infra.ptp_drift_kill_ms);

        let engine = Self {
            cfg,
            markets: map,
            signer,
            client,
            open_orders: DashMap::new(),
            order_index: DashMap::new(),
            last_taker_ms: DashMap::new(),
            last_heartbeat_ms: Arc::new(Mutex::new(0)),
            drift_monitor,
            balance_poll: Arc::new(Mutex::new((0_i64, dec!(0)))),
            placement_pause_last_log_ms: Arc::new(Mutex::new(0_i64)),
        };

        engine.ensure_collateral_ready().await?;
        Ok(engine)
    }

    pub async fn replace_markets(&self, markets: Vec<MarketMeta>) -> anyhow::Result<()> {
        let incoming_ids = markets
            .iter()
            .map(|market| market.condition_id.clone())
            .collect::<HashSet<_>>();
        let removed_tokens = self
            .markets
            .iter()
            .filter(|entry| !incoming_ids.contains(entry.key()))
            .flat_map(|entry| [entry.yes_token_id.clone(), entry.no_token_id.clone()])
            .collect::<HashSet<_>>();

        if !removed_tokens.is_empty() {
            self.cancel_orders_for_tokens(&removed_tokens).await?;
        }

        let stale_conditions = self
            .markets
            .iter()
            .filter(|entry| !incoming_ids.contains(entry.key()))
            .map(|entry| entry.key().clone())
            .collect::<Vec<_>>();
        for condition_id in stale_conditions {
            self.markets.remove(&condition_id);
        }
        for market in markets {
            self.markets.insert(market.condition_id.clone(), market);
        }

        Ok(())
    }

    pub fn user_stream_auth(&self) -> (polymarket_client_sdk::auth::Credentials, Address) {
        (self.client.credentials().clone(), self.client.address())
    }

    pub fn spawn_heartbeat_task(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(
                self.cfg.execution.heartbeat_interval_secs,
            ));
            let mut heartbeat_id = None;

            loop {
                ticker.tick().await;
                match self.client.post_heartbeat(heartbeat_id).await {
                    Ok(resp) => {
                        heartbeat_id = Some(resp.heartbeat_id);
                        let now = MonotonicClock::unix_time_ms();
                        *self.last_heartbeat_ms.lock().await = now;
                        debug!(heartbeat_id = ?heartbeat_id, "heartbeat sent");
                    }
                    Err(err) => {
                        heartbeat_id = None;
                        warn!(error = %err, "heartbeat request failed; next tick starts a new heartbeat");
                    }
                }
            }
        })
    }

    pub async fn maintain_liveness(&self, now_ms: i64) -> anyhow::Result<()> {
        let last = *self.last_heartbeat_ms.lock().await;
        let threshold = self.cfg.execution.liveness_timeout_secs as i64 * 1000;

        if last > 0 && now_ms - last > threshold {
            warn!(
                now_ms,
                last_heartbeat = last,
                timeout_ms = threshold,
                "liveness breach, canceling all orders"
            );
            self.cancel_all_with_retry().await?;
        }

        Ok(())
    }

    pub async fn poll_drift(&self) {
        match self.client.server_time().await {
            Ok(server_secs) => {
                // SDK `Timestamp` is Unix seconds; drift monitor uses ms like `MonotonicClock::unix_time_ms`.
                let server_ms = server_secs.saturating_mul(1000);
                let snapshot = self.drift_monitor.observe_server_time(server_ms);
                if self.drift_monitor.should_warn() {
                    warn!(drift_ms = snapshot.drift_ms, "clock drift warning");
                }
                if self.drift_monitor.should_kill() {
                    error!(
                        drift_ms = snapshot.drift_ms,
                        "clock drift kill threshold exceeded"
                    );
                }
            }
            Err(err) => warn!(error = %err, "unable to fetch server time for drift monitor"),
        }
    }

    pub async fn apply_decisions(&self, now_ms: i64, batch: DecisionBatch) -> anyhow::Result<()> {
        let bal = self.refresh_collateral_balance(now_ms).await;
        let has_exit_sells = batch.quote_intents.iter().any(|q| q.side == Side::Sell)
            || batch.taker_intents.iter().any(|t| t.side == Side::Sell);

        if bal <= Decimal::ZERO {
            self.log_placement_paused_if_needed(now_ms, bal).await;
            if !has_exit_sells {
                self.cancel_stale_orders(now_ms).await?;
                return Ok(());
            }
        }

        for quote in batch.quote_intents {
            if quote.side == Side::Buy && bal <= Decimal::ZERO {
                continue;
            }
            self.sync_quote(now_ms, quote).await?;
        }

        for taker in batch.taker_intents {
            if taker.side == Side::Buy && bal <= Decimal::ZERO {
                continue;
            }
            self.maybe_send_taker(now_ms, taker).await?;
        }

        self.cancel_stale_orders(now_ms).await?;
        Ok(())
    }

    async fn sync_quote(&self, now_ms: i64, quote: QuoteIntent) -> anyhow::Result<()> {
        // O(1) lookup via secondary index instead of O(n) linear scan.
        let existing = self
            .order_index
            .get(&(quote.token_id.clone(), quote.side))
            .map(|v| v.clone());

        if let Some(existing_id) = existing {
            let mut needs_replace = true;
            if let Some(mut order) = self.open_orders.get_mut(&existing_id) {
                let price_diff = (order.price - quote.price).abs();
                let size_diff = (order.size - quote.size).abs();
                let tick = self
                    .markets
                    .get(&quote.condition_id)
                    .map(|m| m.tick_size)
                    .unwrap_or(dec!(0.01));
                let min_age_ms = self.cfg.execution.requote_min_age_ms as i64;
                let price_threshold = tick * Decimal::from(self.cfg.execution.requote_price_ticks);
                let price_threshold = price_threshold.max(tick);
                let size_threshold =
                    Decimal::from(self.cfg.execution.requote_size_bps) / Decimal::from(10_000_u32);
                let size_rel_diff = if order.size > Decimal::ZERO {
                    size_diff / order.size
                } else {
                    Decimal::ONE
                };
                let replace_interval_ms = self.cfg.execution.cancel_replace_interval_ms as i64;
                let too_young = now_ms.saturating_sub(order.created_ms) < min_age_ms;
                let throttled = now_ms.saturating_sub(order.last_touch_ms) < replace_interval_ms;
                let meaningful_price_change = price_diff >= price_threshold;
                let meaningful_size_change = size_rel_diff >= size_threshold;

                if too_young || throttled || (!meaningful_price_change && !meaningful_size_change) {
                    order.last_touch_ms = now_ms;
                    needs_replace = false;
                }
            }

            if needs_replace {
                self.cancel_order_with_retry(&existing_id).await?;
                self.open_orders.remove(&existing_id);
                // Remove from secondary index.
                self.order_index
                    .remove(&(quote.token_id.clone(), quote.side));
            } else {
                return Ok(());
            }
        }

        if self.open_orders.len() >= self.cfg.risk.max_open_orders {
            debug!(
                cap = self.cfg.risk.max_open_orders,
                open = self.open_orders.len(),
                "skip new maker quote: open-order cap reached"
            );
            return Ok(());
        }

        self.place_post_only_quote(now_ms, quote).await?;

        Ok(())
    }

    async fn place_post_only_quote(
        &self,
        now_ms: i64,
        mut quote: QuoteIntent,
    ) -> anyhow::Result<()> {
        let (tick, venue_min) = self.market_quote_params(&quote.condition_id);
        let price = round_price_to_tick(quote.price, tick);
        let token_id = U256::from_str(&quote.token_id)
            .with_context(|| format!("invalid token id {}", quote.token_id))?;

        if quote.side == Side::Sell {
            match self.spendable_conditional_shares(token_id).await {
                Ok(bal) => {
                    quote.size = quote.size.min(bal);
                    if quote.size < venue_min {
                        debug!(
                            token_id = %quote.token_id,
                            balance = %bal,
                            size = %quote.size,
                            venue_min = %venue_min,
                            "skip maker sell: not enough outcome-token balance for min size"
                        );
                        return Ok(());
                    }
                }
                Err(err) => {
                    warn!(
                        error = %err,
                        token_id = %quote.token_id,
                        "conditional balance-allowance check failed; attempting sell anyway"
                    );
                }
            }
        }

        let order = self
            .client
            .limit_order()
            .token_id(token_id)
            .price(price)
            .size(quote.size)
            .side(map_side(quote.side))
            .order_type(OrderType::GTC)
            .post_only(quote.post_only)
            .build()
            .await
            .context("failed to build post-only limit order")?;

        let signed = self
            .client
            .sign(&self.signer, order)
            .await
            .context("failed to sign limit order")?;

        match self.client.post_order(signed).await {
            Ok(resp) => {
                // Update both open_orders and the secondary index atomically.
                self.open_orders.insert(
                    resp.order_id.clone(),
                    ManagedOrder {
                        token_id: quote.token_id.clone(),
                        side: quote.side,
                        price,
                        size: quote.size,
                        created_ms: now_ms,
                        last_touch_ms: now_ms,
                    },
                );
                self.order_index
                    .insert((quote.token_id.clone(), quote.side), resp.order_id.clone());

                Ok(())
            }
            Err(err) => {
                if is_retryable(&self.cfg, &err) {
                    warn!(error = %err, "retryable post_order failure (maker quote)");
                    tokio::time::sleep(Duration::from_millis(self.cfg.execution.retry_initial_ms))
                        .await;
                    return Ok(());
                }
                if is_insufficient_balance_or_allowance(&err) {
                    debug!(error = %err, "post_order skipped: insufficient CLOB balance or allowance");
                    return Ok(());
                }
                if is_benign_maker_rejection(&err) {
                    debug!(error = %err, "post_order skipped: venue rejected maker (size/spread/post-only)");
                    return Ok(());
                }
                Err(anyhow!("post_order failed: {err}"))
            }
        }
    }

    async fn maybe_send_taker(&self, now_ms: i64, taker: TakerIntent) -> anyhow::Result<()> {
        let last = self
            .last_taker_ms
            .get(&taker.token_id)
            .map(|v| *v)
            .unwrap_or(0);

        if taker.side == Side::Buy
            && now_ms - last < self.cfg.execution.taker_rebalance_cooldown_ms as i64
        {
            return Ok(());
        }

        let token_id = U256::from_str(&taker.token_id)
            .with_context(|| format!("invalid token id {}", taker.token_id))?;

        let order = self
            .client
            .market_order()
            .token_id(token_id)
            .amount(Amount::usdc(taker.usdc_notional).context("invalid taker usdc size")?)
            .side(map_side(taker.side))
            .order_type(OrderType::FOK)
            .build()
            .await
            .context("failed to build taker market order")?;

        let signed = self
            .client
            .sign(&self.signer, order)
            .await
            .context("failed to sign taker order")?;

        match self.client.post_order(signed).await {
            Ok(resp) => {
                if taker.side == Side::Buy {
                    self.last_taker_ms.insert(taker.token_id.clone(), now_ms);
                }
                info!(
                    order_id = %resp.order_id,
                    success = resp.success,
                    status = ?resp.status,
                    usdc = %taker.usdc_notional,
                    reason = %taker.reason,
                    "taker FOK market post_order response"
                );
                Ok(())
            }
            Err(err) => {
                if is_retryable(&self.cfg, &err) {
                    warn!(error = %err, "retryable post_order failure (taker)");
                    return Ok(());
                }
                if is_insufficient_balance_or_allowance(&err) {
                    debug!(error = %err, "taker post_order skipped: insufficient balance or allowance");
                    return Ok(());
                }
                Err(anyhow!("taker post_order failed: {err}"))
            }
        }
    }

    async fn cancel_stale_orders(&self, now_ms: i64) -> anyhow::Result<()> {
        let ttl = self.cfg.execution.quote_ttl_ms as i64;
        let stale = self
            .open_orders
            .iter()
            .filter(|entry| now_ms - entry.last_touch_ms > ttl)
            .map(|entry| (entry.key().clone(), entry.token_id.clone(), entry.side))
            .collect::<Vec<_>>();

        for (order_id, token_id, side) in stale {
            self.cancel_order_with_retry(&order_id).await?;
            self.open_orders.remove(&order_id);
            // Keep secondary index consistent.
            self.order_index.remove(&(token_id, side));
        }

        Ok(())
    }

    async fn cancel_orders_for_tokens(&self, token_ids: &HashSet<String>) -> anyhow::Result<()> {
        let stale = self
            .open_orders
            .iter()
            .filter(|entry| token_ids.contains(&entry.token_id))
            .map(|entry| (entry.key().clone(), entry.token_id.clone(), entry.side))
            .collect::<Vec<_>>();

        for (order_id, token_id, side) in stale {
            self.cancel_order_with_retry(&order_id).await?;
            self.open_orders.remove(&order_id);
            self.order_index.remove(&(token_id, side));
        }

        Ok(())
    }

    pub async fn on_event(&self, event: &NormalizedEvent) -> Option<ExecutionReport> {
        let NormalizedEvent::UserTrade(trade) = event else {
            return None;
        };

        let maker = trade
            .taker_or_maker
            .as_deref()
            .map(|s| s.eq_ignore_ascii_case("maker"))
            .unwrap_or(false);

        Some(ExecutionReport {
            id: Uuid::new_v4(),
            timestamp_ms: trade.ts_ms,
            condition_id: trade.condition_id.clone(),
            token_id: trade.token_id.clone(),
            side: trade.side,
            price: trade.price,
            size: trade.size,
            maker,
            lifecycle: trade.status,
            fee_paid: dec!(0),
            rebate_earned: dec!(0),
            spread_capture: dec!(0),
            adverse_selection_cost: dec!(0),
            venue_order_id: trade.order_id.clone(),
            tx_hash: trade.tx_hash.clone(),
            reason: "user_trade_ws".to_string(),
        })
    }

    async fn cancel_order_with_retry(&self, order_id: &str) -> anyhow::Result<()> {
        let mut delay_ms = self.cfg.execution.retry_initial_ms;
        for attempt in 0..self.cfg.execution.retry_max_attempts {
            match self.client.cancel_order(order_id).await {
                Ok(_) => return Ok(()),
                Err(err) if is_retryable(&self.cfg, &err) => {
                    let jitter = rand::rng().random_range(0..=50_u64);
                    let sleep_ms = (delay_ms + jitter).min(self.cfg.execution.retry_max_ms);
                    warn!(attempt, sleep_ms, error = %err, "cancel_order retry");
                    tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                    delay_ms = (delay_ms * 2).min(self.cfg.execution.retry_max_ms);
                }
                Err(err) => return Err(anyhow!("cancel_order failed: {err}")),
            }
        }
        Err(anyhow!("cancel_order exhausted retries for {order_id}"))
    }

    async fn cancel_all_with_retry(&self) -> anyhow::Result<()> {
        let mut delay_ms = self.cfg.execution.retry_initial_ms;
        for attempt in 0..self.cfg.execution.retry_max_attempts {
            match self.client.cancel_all_orders().await {
                Ok(_) => return Ok(()),
                Err(err) if is_retryable(&self.cfg, &err) => {
                    let jitter = rand::rng().random_range(0..=50_u64);
                    let sleep_ms = (delay_ms + jitter).min(self.cfg.execution.retry_max_ms);
                    warn!(attempt, sleep_ms, error = %err, "cancel_all retry");
                    tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                    delay_ms = (delay_ms * 2).min(self.cfg.execution.retry_max_ms);
                }
                Err(err) => return Err(anyhow!("cancel_all_orders failed: {err}")),
            }
        }
        Err(anyhow!("cancel_all_orders exhausted retries"))
    }

    async fn ensure_collateral_ready(&self) -> anyhow::Result<()> {
        let bal = self
            .client
            .balance_allowance(BalanceAllowanceRequest::default())
            .await
            .context("failed to fetch balance_allowance")?;
        info!(
            clob_collateral_balance = %bal.balance,
            "CLOB balance_allowance (USDC the API uses for /order). If UI shows $ but this is 0, wrong key/funder/signature_kind vs where you deposited"
        );
        let now_ms = MonotonicClock::unix_time_ms();
        *self.balance_poll.lock().await = (now_ms, bal.balance);

        if let Err(err) = self
            .client
            .update_balance_allowance(UpdateBalanceAllowanceRequest::default())
            .await
        {
            warn!(error = %err, "update_balance_allowance failed; execution may be blocked");
        }

        if self.cfg.execution.enable_wrap_unwrap {
            // Polymarket USD collateral handling may require chain-specific wrap/unwrap transactions.
            // This hook is intentionally explicit for operator-controlled activation.
            warn!("wrap/unwrap is enabled but no onchain wrapper is configured in this build");
        }

        Ok(())
    }

    /// CLOB `balance-allowance` for conditional (outcome) tokens — used to gate maker **sells**.
    async fn spendable_conditional_shares(&self, token_id: U256) -> anyhow::Result<Decimal> {
        let req = || {
            BalanceAllowanceRequest::builder()
                .asset_type(AssetType::Conditional)
                .token_id(token_id)
                .build()
        };
        let _ = self.client.update_balance_allowance(req()).await;
        let resp = self.client.balance_allowance(req()).await?;
        Ok(resp.balance)
    }

    /// Polls CLOB `balance_allowance` at most once per [`BALANCE_POLL_MS`].
    async fn refresh_collateral_balance(&self, now_ms: i64) -> Decimal {
        const BALANCE_POLL_MS: i64 = 10_000;
        {
            let g = self.balance_poll.lock().await;
            let (last_ms, bal) = *g;
            if last_ms != 0 && now_ms.saturating_sub(last_ms) < BALANCE_POLL_MS {
                return bal;
            }
        }
        match self
            .client
            .balance_allowance(BalanceAllowanceRequest::default())
            .await
        {
            Ok(b) => {
                let mut g = self.balance_poll.lock().await;
                let prev = g.1;
                *g = (now_ms, b.balance);
                if prev <= Decimal::ZERO && b.balance > Decimal::ZERO {
                    info!(
                        clob_usdc = %b.balance,
                        "CLOB collateral now > 0; placing orders"
                    );
                }
                b.balance
            }
            Err(err) => {
                warn!(error = %err, "balance_allowance poll failed; using cached collateral");
                self.balance_poll.lock().await.1
            }
        }
    }

    async fn log_placement_paused_if_needed(&self, now_ms: i64, bal: Decimal) {
        const LOG_EVERY_MS: i64 = 60_000;
        let mut last = self.placement_pause_last_log_ms.lock().await;
        if now_ms.saturating_sub(*last) < LOG_EVERY_MS {
            return;
        }
        *last = now_ms;
        warn!(
            clob_usdc = %bal,
            "order placement paused — CLOB reports no USDC for this trading account (no POST /order). If Polymarket UI shows funds, set funder_address to your deposit/proxy address and signature_kind (poly_proxy vs gnosis_safe) to match; compare startup CREATE2 addresses to Polygonscan."
        );
    }

    fn market_quote_params(&self, condition_id: &str) -> (Decimal, Decimal) {
        self.markets
            .get(condition_id)
            .map(|m| (m.tick_size, m.min_order_size.max(dec!(0.01))))
            .unwrap_or((dec!(0.01), dec!(5)))
    }

    // ── Public management surface (used by activity API / dashboard) ──────────

    /// Snapshot of all currently tracked resting orders.
    pub fn open_orders_snapshot(&self, now_ms: i64) -> Vec<OrderSnapshot> {
        self.open_orders
            .iter()
            .map(|entry| OrderSnapshot {
                order_id: entry.key().clone(),
                token_id: entry.token_id.clone(),
                side: entry.side,
                price: entry.price.to_string(),
                size: entry.size.to_string(),
                age_ms: now_ms.saturating_sub(entry.created_ms),
            })
            .collect()
    }

    /// Place a manual FOK taker order. Returns the venue order ID on success.
    pub async fn place_manual_taker(&self, req: ManualTakerRequest) -> anyhow::Result<String> {
        let token_id = U256::from_str(&req.token_id)
            .with_context(|| format!("invalid token id {}", req.token_id))?;

        let order = self
            .client
            .market_order()
            .token_id(token_id)
            .amount(
                Amount::usdc(req.usdc_notional)
                    .context("invalid manual taker usdc size")?,
            )
            .side(map_side(req.side))
            .order_type(OrderType::FOK)
            .build()
            .await
            .context("failed to build manual taker order")?;

        let signed = self
            .client
            .sign(&self.signer, order)
            .await
            .context("failed to sign manual taker order")?;

        let resp = self
            .client
            .post_order(signed)
            .await
            .with_context(|| format!("manual taker post_order failed (reason: {})", req.reason))?;

        info!(
            order_id = %resp.order_id,
            success = resp.success,
            status = ?resp.status,
            usdc = %req.usdc_notional,
            reason = %req.reason,
            "manual taker placed"
        );

        Ok(resp.order_id)
    }

    /// Cancel a single resting order by ID and remove it from local tracking.
    pub async fn cancel_order_pub(&self, order_id: &str) -> anyhow::Result<()> {
        self.cancel_order_with_retry(order_id).await?;
        if let Some((_, managed)) = self.open_orders.remove(order_id) {
            self.order_index.remove(&(managed.token_id, managed.side));
        }
        Ok(())
    }

    /// Cancel all resting orders (emergency / manual reset).
    pub async fn cancel_all_pub(&self) -> anyhow::Result<()> {
        self.cancel_all_with_retry().await?;
        self.open_orders.clear();
        self.order_index.clear();
        Ok(())
    }
}

fn round_price_to_tick(price: Decimal, tick: Decimal) -> Decimal {
    if tick <= Decimal::ZERO {
        return price;
    }
    (price / tick).round() * tick
}

fn is_insufficient_balance_or_allowance(err: &PolyError) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("not enough balance") || msg.contains("insufficient balance")
}

fn is_benign_maker_rejection(err: &PolyError) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("crosses book")
        || msg.contains("lower than the minimum")
        || msg.contains("minimum:")
        || msg.contains("invalid post-only")
}

fn map_side(side: Side) -> ClobSide {
    match side {
        Side::Buy => ClobSide::Buy,
        Side::Sell => ClobSide::Sell,
    }
}

fn is_retryable(cfg: &AppConfig, err: &PolyError) -> bool {
    if err.kind() != PolyErrorKind::Status {
        return false;
    }

    let Some(status) = err.downcast_ref::<PolyStatus>() else {
        return false;
    };

    if status.status_code.as_u16() == cfg.execution.matching_engine_restart_status {
        return true;
    }

    cfg.execution
        .cloudflare_status_codes
        .contains(&status.status_code.as_u16())
}
