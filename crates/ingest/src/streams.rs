use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use futures::{SinkExt, StreamExt};
use polymarket_client_sdk::auth::Credentials;
use polymarket_client_sdk::clob::ws::{Client as ClobWsClient, WsMessage};
use polymarket_client_sdk::rtds::Client as RtdsClient;
use polymarket_client_sdk::types::{Address, B256, U256};
use rust_decimal::Decimal;
use rust_decimal::MathematicalOps;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};
use url::Url;
use uuid::Uuid;

use polyhft_core::clock::MonotonicClock;
use polyhft_core::config::AppConfig;
use polyhft_core::types::{MarketMeta, NormalizedEvent, PriceSource, RtdsPriceEvent};

use crate::normalizer::{
    normalize_book, normalize_last_trade, normalize_order, normalize_price_change,
    normalize_sports, normalize_trade,
};

#[derive(Debug, Clone)]
pub struct IngestionService {
    cfg: AppConfig,
}

#[derive(Clone)]
pub struct UserStreamAuth {
    pub credentials: Credentials,
    pub wallet: Address,
}

impl IngestionService {
    #[must_use]
    pub fn new(cfg: AppConfig) -> Self {
        Self { cfg }
    }

    pub fn spawn(
        &self,
        markets: Vec<MarketMeta>,
        tx: mpsc::Sender<NormalizedEvent>,
    ) -> Vec<JoinHandle<()>> {
        self.spawn_with_user_auth(markets, tx, None)
    }

    pub fn spawn_with_user_auth(
        &self,
        markets: Vec<MarketMeta>,
        tx: mpsc::Sender<NormalizedEvent>,
        user_auth: Option<UserStreamAuth>,
    ) -> Vec<JoinHandle<()>> {
        let mut handles = vec![
            self.spawn_market_stream(markets.clone(), tx.clone()),
            self.spawn_rtds_stream(tx.clone()),
            self.spawn_pyth_stream(tx.clone()),
            self.spawn_sports_stream(tx.clone()),
        ];

        if let Some(user_auth) = user_auth.or_else(|| self.configured_user_auth()) {
            handles.push(self.spawn_user_stream(markets, tx, user_auth));
        } else {
            info!(
                "user websocket skipped (set api_key + api_secret + api_passphrase + wallet_address after `npm run derive:clob-creds`, or let the live execution client derive them automatically)"
            );
        }

        handles
    }

    fn configured_user_auth(&self) -> Option<UserStreamAuth> {
        if !is_present(self.cfg.auth.api_key.as_deref())
            || !is_present(self.cfg.auth.api_secret.as_deref())
            || !is_present(self.cfg.auth.api_passphrase.as_deref())
            || !is_present(self.cfg.auth.wallet_address.as_deref())
        {
            return None;
        }

        let api_key = match self.cfg.auth.api_key.as_deref() {
            Some(v) => match Uuid::parse_str(v) {
                Ok(u) => u,
                Err(err) => {
                    error!(error = %err, "invalid POLY API key UUID");
                    return None;
                }
            },
            None => return None,
        };
        let wallet = match self.cfg.auth.wallet_address.as_deref() {
            Some(v) => match Address::from_str(v) {
                Ok(addr) => addr,
                Err(err) => {
                    error!(error = %err, "invalid wallet address for user stream");
                    return None;
                }
            },
            None => return None,
        };

        Some(UserStreamAuth {
            credentials: Credentials::new(
                api_key,
                self.cfg.auth.api_secret.clone()?,
                self.cfg.auth.api_passphrase.clone()?,
            ),
            wallet,
        })
    }

    fn spawn_market_stream(
        &self,
        markets: Vec<MarketMeta>,
        tx: mpsc::Sender<NormalizedEvent>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut asset_ids = Vec::new();
            for market in &markets {
                for token in [&market.yes_token_id, &market.no_token_id] {
                    match U256::from_str(token) {
                        Ok(id) => asset_ids.push(id),
                        Err(err) => {
                            warn!(token_id = token, error = %err, "invalid token_id for market stream")
                        }
                    }
                }
            }

            if asset_ids.is_empty() {
                warn!("no valid asset ids for market stream");
                return;
            }

            let mut backoff_ms = 500u64;
            loop {
                let client = ClobWsClient::default();

                let books = match client.subscribe_orderbook(asset_ids.clone()) {
                    Ok(stream) => stream,
                    Err(err) => {
                        error!(error = %err, backoff_ms, "failed to subscribe orderbook stream; retrying");
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms * 2).min(30_000);
                        continue;
                    }
                };

                let prices = match client.subscribe_prices(asset_ids.clone()) {
                    Ok(stream) => stream,
                    Err(err) => {
                        error!(error = %err, backoff_ms, "failed to subscribe price_change stream; retrying");
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms * 2).min(30_000);
                        continue;
                    }
                };

                let last_trades = match client.subscribe_last_trade_price(asset_ids.clone()) {
                    Ok(stream) => stream,
                    Err(err) => {
                        error!(error = %err, backoff_ms, "failed to subscribe last_trade stream; retrying");
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms * 2).min(30_000);
                        continue;
                    }
                };

                backoff_ms = 500;
                info!("market websocket streams started");
                let mut books = Box::pin(books);
                let mut prices = Box::pin(prices);
                let mut last_trades = Box::pin(last_trades);

                loop {
                    tokio::select! {
                        item = books.next() => {
                            match item {
                                Some(Ok(book)) => {
                                    let event = NormalizedEvent::MarketBook(normalize_book(book));
                                    if tx.send(event).await.is_err() { return; }
                                }
                                Some(Err(err)) => { warn!(error = %err, "book stream error"); break; }
                                None => break,
                            }
                        }
                        item = prices.next() => {
                            match item {
                                Some(Ok(price)) => {
                                    for e in normalize_price_change(price) {
                                        if tx.send(NormalizedEvent::PriceChange(e)).await.is_err() { return; }
                                    }
                                }
                                Some(Err(err)) => { warn!(error = %err, "price stream error"); break; }
                                None => break,
                            }
                        }
                        item = last_trades.next() => {
                            match item {
                                Some(Ok(last_trade)) => {
                                    let event = NormalizedEvent::LastTradePrice(normalize_last_trade(last_trade));
                                    if tx.send(event).await.is_err() { return; }
                                }
                                Some(Err(err)) => { warn!(error = %err, "last_trade stream error"); break; }
                                None => break,
                            }
                        }
                    }
                }

                warn!(
                    backoff_ms,
                    "market websocket streams disconnected; reconnecting"
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(30_000);
            }
        })
    }

    fn spawn_user_stream(
        &self,
        markets: Vec<MarketMeta>,
        tx: mpsc::Sender<NormalizedEvent>,
        user_auth: UserStreamAuth,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let client = match ClobWsClient::default()
                .authenticate(user_auth.credentials, user_auth.wallet)
            {
                Ok(c) => c,
                Err(err) => {
                    error!(error = %err, "failed to authenticate user websocket client");
                    return;
                }
            };

            let market_ids = markets
                .iter()
                .filter_map(|m| B256::from_str(&m.condition_id).ok())
                .collect::<Vec<_>>();

            let mut stream = match client.subscribe_user_events(market_ids) {
                Ok(s) => Box::pin(s),
                Err(err) => {
                    error!(error = %err, "failed to subscribe user events");
                    return;
                }
            };

            info!("user websocket stream started");
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(WsMessage::Order(order)) => {
                        if tx
                            .send(NormalizedEvent::UserOrder(normalize_order(order)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(WsMessage::Trade(trade)) => {
                        if tx
                            .send(NormalizedEvent::UserTrade(normalize_trade(trade)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(err) => warn!(error = %err, "user stream error"),
                }
            }

            warn!("user websocket stream exited");
        })
    }

    fn spawn_rtds_stream(&self, tx: mpsc::Sender<NormalizedEvent>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut backoff_ms = 500u64;
            loop {
                let client = RtdsClient::default();

                let binance = match client.subscribe_crypto_prices(Some(vec![
                    "btcusdt".to_string(),
                    "ethusdt".to_string(),
                    "solusdt".to_string(),
                ])) {
                    Ok(s) => s,
                    Err(err) => {
                        error!(error = %err, backoff_ms, "failed to subscribe RTDS Binance prices; retrying");
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms * 2).min(30_000);
                        continue;
                    }
                };

                let chainlink_btc = match client
                    .subscribe_chainlink_prices(Some("btc/usd".to_string()))
                {
                    Ok(s) => s,
                    Err(err) => {
                        error!(error = %err, backoff_ms, "failed to subscribe RTDS Chainlink BTC; retrying");
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms * 2).min(30_000);
                        continue;
                    }
                };

                let chainlink_eth = match client
                    .subscribe_chainlink_prices(Some("eth/usd".to_string()))
                {
                    Ok(s) => s,
                    Err(err) => {
                        error!(error = %err, backoff_ms, "failed to subscribe RTDS Chainlink ETH; retrying");
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms * 2).min(30_000);
                        continue;
                    }
                };

                let chainlink_sol = match client
                    .subscribe_chainlink_prices(Some("sol/usd".to_string()))
                {
                    Ok(s) => s,
                    Err(err) => {
                        error!(error = %err, backoff_ms, "failed to subscribe RTDS Chainlink SOL; retrying");
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms * 2).min(30_000);
                        continue;
                    }
                };

                backoff_ms = 500;
                info!("RTDS streams started (Binance + Chainlink: BTC/ETH/SOL)");
                let mut binance = Box::pin(binance);
                let mut chainlink_btc = Box::pin(chainlink_btc);
                let mut chainlink_eth = Box::pin(chainlink_eth);
                let mut chainlink_sol = Box::pin(chainlink_sol);

                loop {
                    tokio::select! {
                        item = binance.next() => {
                            match item {
                                Some(Ok(price)) => {
                                    let e = NormalizedEvent::RtdsPrice(RtdsPriceEvent {
                                        symbol: price.symbol, value: price.value,
                                        source: PriceSource::Binance, ts_ms: price.timestamp,
                                    });
                                    if tx.send(e).await.is_err() { return; }
                                }
                                Some(Err(err)) => { warn!(error = %err, "RTDS binance stream error"); break; }
                                None => break,
                            }
                        }
                        item = chainlink_btc.next() => {
                            match item {
                                Some(Ok(price)) => {
                                    let e = NormalizedEvent::RtdsPrice(RtdsPriceEvent {
                                        symbol: price.symbol, value: price.value,
                                        source: PriceSource::Chainlink, ts_ms: price.timestamp,
                                    });
                                    if tx.send(e).await.is_err() { return; }
                                }
                                Some(Err(err)) => { warn!(error = %err, "RTDS chainlink BTC stream error"); break; }
                                None => break,
                            }
                        }
                        item = chainlink_eth.next() => {
                            match item {
                                Some(Ok(price)) => {
                                    let e = NormalizedEvent::RtdsPrice(RtdsPriceEvent {
                                        symbol: price.symbol, value: price.value,
                                        source: PriceSource::Chainlink, ts_ms: price.timestamp,
                                    });
                                    if tx.send(e).await.is_err() { return; }
                                }
                                Some(Err(err)) => { warn!(error = %err, "RTDS chainlink ETH stream error"); break; }
                                None => break,
                            }
                        }
                        item = chainlink_sol.next() => {
                            match item {
                                Some(Ok(price)) => {
                                    let e = NormalizedEvent::RtdsPrice(RtdsPriceEvent {
                                        symbol: price.symbol, value: price.value,
                                        source: PriceSource::Chainlink, ts_ms: price.timestamp,
                                    });
                                    if tx.send(e).await.is_err() { return; }
                                }
                                Some(Err(err)) => { warn!(error = %err, "RTDS chainlink SOL stream error"); break; }
                                None => break,
                            }
                        }
                    }
                }

                warn!(backoff_ms, "RTDS stream disconnected; reconnecting");
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(30_000);
            }
        })
    }

    fn spawn_sports_stream(&self, tx: mpsc::Sender<NormalizedEvent>) -> JoinHandle<()> {
        let ws_url = self.cfg.clob.ws_sports_url.clone();
        if ws_url.trim().is_empty() {
            info!("sports websocket disabled (clob.ws_sports_url is empty)");
            return tokio::spawn(async {});
        }
        tokio::spawn(async move {
            let (mut socket, _) = match connect_async(&ws_url).await {
                Ok(v) => v,
                Err(err) => {
                    warn!(
                        url = ws_url,
                        error = %err,
                        "sports websocket unavailable; continuing without sports stream (set clob.ws_sports_url = \"\" to skip)"
                    );
                    return;
                }
            };

            info!("sports websocket stream started");
            let mut ping_interval = tokio::time::interval(Duration::from_secs(5));

            loop {
                tokio::select! {
                    _ = ping_interval.tick() => {
                        // Keep local heartbeat flowing even when sports stream has no payload.
                        let _ = tx.send(NormalizedEvent::Heartbeat { ts_ms: MonotonicClock::unix_time_ms() }).await;
                    }
                    msg = socket.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                if text.trim().eq_ignore_ascii_case("ping") {
                                    let _ = socket.send(Message::Text("pong".to_string().into())).await;
                                    continue;
                                }
                                match serde_json::from_str::<serde_json::Value>(&text) {
                                    Ok(value) => {
                                        let ts_ms = MonotonicClock::unix_time_ms();
                                        let event = NormalizedEvent::Sports(normalize_sports(value, ts_ms));
                                        if tx.send(event).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(err) => debug!(error = %err, "unable to parse sports ws message as JSON"),
                                }
                            }
                            Some(Ok(Message::Ping(payload))) => {
                                let _ = socket.send(Message::Pong(payload)).await;
                            }
                            Some(Ok(Message::Close(frame))) => {
                                warn!(?frame, "sports websocket closed by server");
                                break;
                            }
                            Some(Ok(_)) => {}
                            Some(Err(err)) => {
                                warn!(error = %err, "sports stream error");
                                break;
                            }
                            None => break,
                        }
                    }
                }
            }

            warn!("sports websocket stream exited");
        })
    }

    fn spawn_pyth_stream(&self, tx: mpsc::Sender<NormalizedEvent>) -> JoinHandle<()> {
        let cfg = self.cfg.clone();
        tokio::spawn(async move {
            if !cfg.pyth.enabled {
                info!("pyth stream is disabled in config");
                return;
            }

            let feed_map = cfg
                .pyth
                .feed_ids
                .iter()
                .filter_map(|feed| {
                    if feed.feed_id.trim().is_empty() || feed.symbol.trim().is_empty() {
                        return None;
                    }
                    Some((
                        normalize_feed_id(&feed.feed_id),
                        canonical_ref_symbol(&feed.symbol),
                    ))
                })
                .collect::<HashMap<_, _>>();

            if feed_map.is_empty() {
                warn!("pyth stream enabled but no valid feed_ids configured");
                return;
            }

            let stream_url = match build_pyth_stream_url(&cfg) {
                Ok(url) => url,
                Err(err) => {
                    error!(error = %err, "failed to build pyth stream URL");
                    return;
                }
            };

            let client = match reqwest::Client::builder()
                .connect_timeout(Duration::from_millis(cfg.pyth.connect_timeout_ms))
                .build()
            {
                Ok(c) => c,
                Err(err) => {
                    error!(error = %err, "failed to build reqwest client for pyth stream");
                    return;
                }
            };

            let mut backoff_ms = cfg.pyth.reconnect_backoff_ms.max(100);
            let max_backoff_ms = cfg.pyth.max_backoff_ms.max(backoff_ms);

            loop {
                info!(
                    url = %stream_url,
                    feed_count = feed_map.len(),
                    "connecting pyth hermes SSE stream"
                );

                let response = match client
                    .get(stream_url.clone())
                    .header(reqwest::header::ACCEPT, "text/event-stream")
                    .send()
                    .await
                {
                    Ok(resp) => resp,
                    Err(err) => {
                        warn!(error = %err, backoff_ms, "pyth stream connect error");
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms.saturating_mul(2)).min(max_backoff_ms);
                        continue;
                    }
                };

                if !response.status().is_success() {
                    warn!(
                        status = %response.status(),
                        backoff_ms,
                        "pyth stream returned non-success status"
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms.saturating_mul(2)).min(max_backoff_ms);
                    continue;
                }

                backoff_ms = cfg.pyth.reconnect_backoff_ms.max(100);
                info!("pyth hermes SSE stream connected");

                let mut data_buffer = String::new();
                let mut line_buffer = String::new();
                let mut response = response;

                loop {
                    let chunk = match response.chunk().await {
                        Ok(Some(bytes)) => bytes,
                        Ok(None) => break,
                        Err(err) => {
                            warn!(error = %err, "pyth stream read error, reconnecting");
                            break;
                        }
                    };

                    line_buffer.push_str(&String::from_utf8_lossy(&chunk));
                    while let Some(newline_ix) = line_buffer.find('\n') {
                        let mut line = line_buffer[..newline_ix].to_string();
                        line_buffer.drain(..=newline_ix);
                        if line.ends_with('\r') {
                            line.pop();
                        }

                        if line.is_empty() {
                            if data_buffer.trim().is_empty() {
                                data_buffer.clear();
                                continue;
                            }

                            let payload = std::mem::take(&mut data_buffer);
                            for event in parse_pyth_payload(&payload, &feed_map) {
                                if tx.send(NormalizedEvent::RtdsPrice(event)).await.is_err() {
                                    return;
                                }
                            }
                            continue;
                        }

                        if let Some(data) = line.strip_prefix("data:") {
                            data_buffer.push_str(data.trim_start());
                            data_buffer.push('\n');
                        }
                    }
                }

                if !data_buffer.trim().is_empty() {
                    for event in parse_pyth_payload(&data_buffer, &feed_map) {
                        if tx.send(NormalizedEvent::RtdsPrice(event)).await.is_err() {
                            return;
                        }
                    }
                }

                warn!("pyth stream disconnected; reconnecting");
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms.saturating_mul(2)).min(max_backoff_ms);
            }
        })
    }
}

fn is_present(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.trim().is_empty())
}

fn build_pyth_stream_url(cfg: &AppConfig) -> anyhow::Result<String> {
    let base = cfg.pyth.hermes_url.trim_end_matches('/');
    let path = if cfg.pyth.stream_path.starts_with('/') {
        cfg.pyth.stream_path.clone()
    } else {
        format!("/{}", cfg.pyth.stream_path)
    };

    let mut url = Url::parse(&format!("{base}{path}")).context("invalid pyth hermes URL")?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("parsed", "true");
        for feed in &cfg.pyth.feed_ids {
            if !feed.feed_id.trim().is_empty() {
                query.append_pair("ids[]", feed.feed_id.trim());
            }
        }
    }

    Ok(url.to_string())
}

fn parse_pyth_payload(payload: &str, feed_map: &HashMap<String, String>) -> Vec<RtdsPriceEvent> {
    let trimmed = payload.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" {
        return Vec::new();
    }

    let envelope = match serde_json::from_str::<PythSseEnvelope>(trimmed) {
        Ok(v) => v,
        Err(err) => {
            debug!(error = %err, "unable to parse pyth SSE payload");
            return Vec::new();
        }
    };

    let now_ms = MonotonicClock::unix_time_ms();
    let mut out = Vec::with_capacity(envelope.parsed.len());

    for row in envelope.parsed {
        let Some(symbol) = feed_map.get(&normalize_feed_id(&row.id)) else {
            continue;
        };
        let Some(raw) = decimal_from_json(&row.price.price) else {
            continue;
        };

        let scaled = apply_exponent(raw, row.price.expo);
        if scaled <= Decimal::ZERO {
            continue;
        }

        let ts_ms = row.price.publish_time.map(unix_to_millis).unwrap_or(now_ms);
        out.push(RtdsPriceEvent {
            symbol: symbol.clone(),
            value: scaled,
            source: PriceSource::Pyth,
            ts_ms,
        });
    }

    out
}

fn canonical_ref_symbol(symbol: &str) -> String {
    let lower = symbol.to_ascii_lowercase();
    if lower.contains("btc") {
        "btcusd".to_string()
    } else if lower.contains("eth") {
        "ethusd".to_string()
    } else if lower.contains("sol") {
        "solusd".to_string()
    } else {
        lower.replace(['/', '-', '_'], "")
    }
}

fn normalize_feed_id(feed_id: &str) -> String {
    feed_id.trim().trim_start_matches("0x").to_ascii_lowercase()
}

fn decimal_from_json(value: &serde_json::Value) -> Option<Decimal> {
    match value {
        serde_json::Value::String(s) => Decimal::from_str(s).ok(),
        serde_json::Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        _ => None,
    }
}

fn apply_exponent(value: Decimal, expo: i32) -> Decimal {
    if expo == 0 {
        return value;
    }
    // Use checked_powi to avoid a manual loop over abs(expo) iterations.
    // For Pyth price exponents the value is typically -8..=-4, so this is
    // always a small integer power – no loop overhead.
    let ten = Decimal::new(10, 0);
    match ten.checked_powi(expo.unsigned_abs() as i64) {
        Some(factor) if expo > 0 => value * factor,
        Some(factor) => value / factor,
        None => value, // guard against absurd expo values
    }
}

fn unix_to_millis(ts: i64) -> i64 {
    if ts >= 1_000_000_000_000 {
        ts
    } else {
        ts.saturating_mul(1000)
    }
}

#[derive(Debug, Deserialize)]
struct PythSseEnvelope {
    #[serde(default)]
    parsed: Vec<PythParsedUpdate>,
}

#[derive(Debug, Deserialize)]
struct PythParsedUpdate {
    id: String,
    price: PythPricePayload,
}

#[derive(Debug, Deserialize)]
struct PythPricePayload {
    price: serde_json::Value,
    expo: i32,
    #[serde(default)]
    publish_time: Option<i64>,
}
