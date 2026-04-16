//! REST-based orderbook bootstrap and periodic polling.
//!
//! Fetches orderbook snapshots from the Polymarket CLOB REST API
//! (`GET /book?token_id=<id>`) so that the paper engine (and signal engine)
//! have book data immediately — without waiting for WebSocket events.

use std::time::Duration;

use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use polyhft_core::clock::MonotonicClock;
use polyhft_core::types::{BookLevel, MarketBookEvent, MarketMeta, NormalizedEvent};

/// A single price level returned by the CLOB REST `/book` endpoint.
#[derive(Debug, Deserialize)]
struct RestBookLevel {
    price: String,
    size: String,
}

/// The full orderbook snapshot from `GET /book?token_id=<id>`.
#[derive(Debug, Deserialize)]
struct RestBookResponse {
    #[allow(dead_code)]
    market: Option<String>,
    #[allow(dead_code)]
    asset_id: Option<String>,
    #[serde(default)]
    bids: Vec<RestBookLevel>,
    #[serde(default)]
    asks: Vec<RestBookLevel>,
    timestamp: Option<String>,
}

fn parse_level(level: &RestBookLevel) -> Option<BookLevel> {
    let price = level.price.parse::<Decimal>().ok()?;
    let size = level.size.parse::<Decimal>().ok()?;
    if price <= Decimal::ZERO || size <= Decimal::ZERO {
        return None;
    }
    Some(BookLevel { price, size })
}

/// Fetch orderbook snapshot for a single token from the CLOB REST API.
async fn fetch_book(
    client: &Client,
    clob_rest_url: &str,
    token_id: &str,
) -> anyhow::Result<RestBookResponse> {
    let url = format!("{}/book?token_id={}", clob_rest_url.trim_end_matches('/'), token_id);
    let resp = client
        .get(&url)
        .header("User-Agent", "polyhft-agent/0.1")
        .send()
        .await?
        .error_for_status()?
        .json::<RestBookResponse>()
        .await?;
    Ok(resp)
}

fn rest_book_to_event(
    condition_id: &str,
    token_id: &str,
    book: RestBookResponse,
) -> MarketBookEvent {
    let bids: Vec<BookLevel> = book.bids.iter().filter_map(parse_level).collect();
    let asks: Vec<BookLevel> = book.asks.iter().filter_map(parse_level).collect();
    let ts_ms = book
        .timestamp
        .and_then(|t| t.parse::<i64>().ok())
        .unwrap_or_else(MonotonicClock::unix_time_ms);

    MarketBookEvent {
        condition_id: condition_id.to_string(),
        token_id: token_id.to_string(),
        bids,
        asks,
        ts_ms,
    }
}

/// Fetch initial orderbook snapshots for all markets and send them through the event channel.
/// Called once after market discovery completes, before WebSocket streams start delivering data.
pub async fn bootstrap_books(
    clob_rest_url: &str,
    markets: &[MarketMeta],
    tx: &mpsc::Sender<NormalizedEvent>,
) {
    let client = match Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(err) => {
            warn!(error = %err, "book bootstrap: failed to build HTTP client");
            return;
        }
    };

    let mut success_count = 0u32;
    let mut fail_count = 0u32;

    for market in markets {
        for (token_id, is_yes) in [
            (&market.yes_token_id, true),
            (&market.no_token_id, false),
        ] {
            if token_id.is_empty() {
                continue;
            }

            match fetch_book(&client, clob_rest_url, token_id).await {
                Ok(book) => {
                    let has_data = !book.bids.is_empty() || !book.asks.is_empty();
                    let event = rest_book_to_event(&market.condition_id, token_id, book);

                    if has_data {
                        if tx.send(NormalizedEvent::MarketBook(event)).await.is_err() {
                            warn!("book bootstrap: event channel closed");
                            return;
                        }
                        success_count += 1;
                    }
                }
                Err(err) => {
                    debug!(
                        condition_id = %market.condition_id,
                        token_id = %token_id,
                        leg = if is_yes { "yes" } else { "no" },
                        error = %err,
                        "book bootstrap: failed to fetch book"
                    );
                    fail_count += 1;
                }
            }
        }
    }

    info!(
        success = success_count,
        failed = fail_count,
        markets = markets.len(),
        "book bootstrap complete"
    );
}

/// Spawn a background task that periodically fetches orderbook snapshots via REST.
/// This ensures the paper engine always has reasonably fresh data even if WebSocket
/// streams are slow or disconnected.
pub fn spawn_book_poller(
    clob_rest_url: String,
    markets: Vec<MarketMeta>,
    tx: mpsc::Sender<NormalizedEvent>,
    poll_interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let client = match Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(err) => {
                warn!(error = %err, "book poller: failed to build HTTP client");
                return;
            }
        };

        let mut ticker = tokio::time::interval(poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Skip the first immediate tick — bootstrap_books already handled it.
        ticker.tick().await;

        loop {
            ticker.tick().await;

            for market in &markets {
                for token_id in [&market.yes_token_id, &market.no_token_id] {
                    if token_id.is_empty() {
                        continue;
                    }

                    match fetch_book(&client, &clob_rest_url, token_id).await {
                        Ok(book) if !book.bids.is_empty() || !book.asks.is_empty() => {
                            let event = rest_book_to_event(
                                &market.condition_id,
                                token_id,
                                book,
                            );
                            if tx.send(NormalizedEvent::MarketBook(event)).await.is_err() {
                                return;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => {}
                    }
                }
            }
        }
    })
}
