use std::collections::{BTreeMap, HashSet, VecDeque};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use futures::{StreamExt, stream};
use regex::Regex;
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use polyhft_core::clob_ids::normalize_clob_token_id;
use polyhft_core::config::AppConfig;
use polyhft_core::types::{FeeSchedule, MarketMeta};

const PREDICTIONS_5M_URL: &str = "https://polymarket.com/predictions/5M";
const PREDICTIONS_15M_URL: &str = "https://polymarket.com/predictions/15M";
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36";

#[derive(Debug, Clone)]
pub struct MarketDiscovery {
    client: Client,
    cfg: AppConfig,
    include_patterns: Vec<Regex>,
    exclude_patterns: Vec<Regex>,
    include_slug_patterns: Vec<Regex>,
    exclude_slug_patterns: Vec<Regex>,
}

impl MarketDiscovery {
    pub fn new(cfg: AppConfig) -> anyhow::Result<Self> {
        let timeout = Duration::from_millis(cfg.gamma.request_timeout_ms);
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .context("failed to build reqwest client for discovery")?;

        let include_patterns = cfg
            .markets
            .include_question_patterns
            .iter()
            .map(|pattern| Regex::new(pattern))
            .collect::<Result<Vec<_>, _>>()
            .context("invalid include_question_patterns regex")?;

        let exclude_patterns = cfg
            .markets
            .exclude_question_patterns
            .iter()
            .map(|pattern| Regex::new(pattern))
            .collect::<Result<Vec<_>, _>>()
            .context("invalid exclude_question_patterns regex")?;

        let include_slug_patterns = cfg
            .markets
            .include_slug_patterns
            .iter()
            .map(|pattern| Regex::new(pattern))
            .collect::<Result<Vec<_>, _>>()
            .context("invalid include_slug_patterns regex")?;

        let exclude_slug_patterns = cfg
            .markets
            .exclude_slug_patterns
            .iter()
            .map(|pattern| Regex::new(pattern))
            .collect::<Result<Vec<_>, _>>()
            .context("invalid exclude_slug_patterns regex")?;

        if cfg.markets.include_question_patterns.is_empty()
            && cfg.markets.include_slug_patterns.is_empty()
            && cfg.markets.explicit_condition_ids.is_empty()
        {
            bail!(
                "markets: need include_question_patterns and/or include_slug_patterns and/or explicit_condition_ids (refusing unbounded discovery)"
            );
        }

        Ok(Self {
            client,
            cfg,
            include_patterns,
            exclude_patterns,
            include_slug_patterns,
            exclude_slug_patterns,
        })
    }

    pub async fn discover(&self) -> anyhow::Result<Vec<MarketMeta>> {
        let explicit: HashSet<&str> = self
            .cfg
            .markets
            .explicit_condition_ids
            .iter()
            .map(String::as_str)
            .collect();

        if self.should_use_predictions_hub() {
            match self.discover_from_predictions_hub(&explicit).await {
                Ok(markets) if !markets.is_empty() => return self.finalize_markets(markets),
                Ok(_) => warn!("predictions hub returned zero markets; falling back to gamma"),
                Err(err) => {
                    warn!(error = %err, "predictions hub discovery failed; falling back to gamma")
                }
            }
        }

        let markets = self.discover_from_gamma(&explicit).await?;
        self.finalize_markets(markets)
    }

    fn should_use_predictions_hub(&self) -> bool {
        self.cfg
            .markets
            .include_slug_patterns
            .iter()
            .any(|pattern| {
                let lowered = pattern.to_ascii_lowercase();
                lowered.contains("updown-5m")
                    || lowered.contains("updown-15m")
                    || lowered.contains("updown-(5m|15m)")
                    || (lowered.contains("5m") && lowered.contains("updown"))
                    || (lowered.contains("15m") && lowered.contains("updown"))
            })
    }

    /// Which prediction hub pages to scrape based on slug patterns.
    fn prediction_hub_urls(&self) -> Vec<&'static str> {
        let mut urls = Vec::new();
        let wants_5m = self
            .cfg
            .markets
            .include_slug_patterns
            .iter()
            .any(|p| {
                let l = p.to_ascii_lowercase();
                l.contains("5m")
            });
        let wants_15m = self
            .cfg
            .markets
            .include_slug_patterns
            .iter()
            .any(|p| {
                let l = p.to_ascii_lowercase();
                l.contains("15m")
            });
        if wants_5m {
            urls.push(PREDICTIONS_5M_URL);
        }
        if wants_15m {
            urls.push(PREDICTIONS_15M_URL);
        }
        if urls.is_empty() {
            urls.push(PREDICTIONS_5M_URL);
        }
        urls
    }

    async fn discover_from_predictions_hub(
        &self,
        explicit: &HashSet<&str>,
    ) -> anyhow::Result<Vec<MarketMeta>> {
        let hub_urls = self.prediction_hub_urls();
        let mut slugs = Vec::new();
        for url in &hub_urls {
            match self.fetch_text(url).await {
                Ok(html) => {
                    let page_slugs = extract_event_slugs(&html);
                    info!(url, count = page_slugs.len(), "predictions hub page scraped");
                    slugs.extend(page_slugs);
                }
                Err(err) => {
                    warn!(url, error = %err, "failed to fetch predictions hub page");
                }
            }
        }
        slugs.sort_unstable();
        slugs.dedup();
        if slugs.is_empty() {
            bail!("predictions hub pages contained no event slugs");
        }

        let now = Utc::now();
        let max_age = ChronoDuration::minutes(self.cfg.markets.max_past_market_age_minutes.max(0));
        slugs.retain(|slug| {
            market_window_end_from_slug(slug)
                .map(|end| end >= now - max_age)
                .unwrap_or(true)
        });
        slugs.retain(|slug| self.slug_passes_regex_filters(slug));
        slugs.sort_by_key(|slug| market_priority_from_slug(slug, now));

        let max_candidates = self.cfg.markets.max_markets.saturating_mul(3).max(12);
        slugs.truncate(max_candidates);

        let mut out = Vec::new();
        let mut fetches = stream::iter(slugs.into_iter().map(|slug| async move {
            let fetched = self.fetch_market_from_event_page(&slug, explicit).await;
            (slug, fetched)
        }))
        .buffer_unordered(6);

        while let Some((slug, fetched)) = fetches.next().await {
            match fetched {
                Ok(Some(market)) => out.push(market),
                Ok(None) => {}
                Err(err) => warn!(slug, error = %err, "failed to parse event page market"),
            }
        }

        if out.is_empty() {
            bail!("predictions hub produced zero detailed market records");
        }

        Ok(out)
    }

    async fn discover_from_gamma(
        &self,
        explicit: &HashSet<&str>,
    ) -> anyhow::Result<Vec<MarketMeta>> {
        let mut result = Vec::new();

        for page in 0..self.cfg.gamma.max_pages {
            let offset = page * self.cfg.gamma.page_size;
            let url = format!(
                "{}{path}?active=true&closed=false&limit={limit}&offset={offset}",
                self.cfg.gamma.rest_url,
                path = self.cfg.gamma.markets_path,
                limit = self.cfg.gamma.page_size,
                offset = offset,
            );

            let response = match self
                .client
                .get(url.clone())
                .header("User-Agent", "polyhft-agent/0.1")
                .send()
                .await
            {
                Ok(resp) => match resp.error_for_status() {
                    Ok(ok) => ok,
                    Err(err) => {
                        if result.is_empty() {
                            return Err(err).context("gamma markets returned non-success status");
                        }
                        warn!(
                            page,
                            offset,
                            error = %err,
                            "gamma page failed; stopping pagination with partial market set"
                        );
                        break;
                    }
                },
                Err(err) => {
                    if result.is_empty() {
                        return Err(err).context("gamma markets request failed");
                    }
                    warn!(
                        page,
                        offset,
                        error = %err,
                        "gamma request failed; stopping pagination with partial market set"
                    );
                    break;
                }
            };

            let markets = response
                .json::<Vec<RawMarket>>()
                .await
                .context("failed to parse gamma market list")?;

            if markets.is_empty() {
                break;
            }

            for raw in markets {
                if !explicit.is_empty() && explicit.contains(raw.condition_id.as_str()) {
                    if let Some(meta) = self.convert_gamma_market(raw)? {
                        result.push(meta);
                    }
                    continue;
                }

                if self.passes_filters(&raw)?
                    && let Some(meta) = self.convert_gamma_market(raw)?
                {
                    result.push(meta);
                }
            }
        }

        if result.is_empty() {
            bail!("market discovery found zero markets; verify patterns or explicit_condition_ids")
        }

        Ok(result)
    }

    async fn fetch_market_from_event_page(
        &self,
        slug: &str,
        explicit: &HashSet<&str>,
    ) -> anyhow::Result<Option<MarketMeta>> {
        let url = format!("https://polymarket.com/event/{slug}");
        let html = self
            .fetch_text(&url)
            .await
            .with_context(|| format!("failed to fetch event page for {slug}"))?;
        let next_data = extract_next_data_json(&html)?;
        let market = find_hub_market(&next_data, slug)
            .with_context(|| format!("event page contained no market payload for {slug}"))?;

        if !explicit.is_empty() && explicit.contains(market.condition_id.as_str()) {
            return self.convert_hub_market(market);
        }

        if !self.passes_market_filters(
            &market.question,
            &market.slug,
            market.fees_enabled,
            market.start_date,
            market.end_date,
        )? {
            return Ok(None);
        }

        self.convert_hub_market(market)
    }

    async fn fetch_text(&self, url: &str) -> anyhow::Result<String> {
        self.client
            .get(url)
            .header("User-Agent", BROWSER_USER_AGENT)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?
            .error_for_status()
            .with_context(|| format!("GET {url} returned non-success status"))?
            .text()
            .await
            .with_context(|| format!("GET {url} body decode failed"))
    }

    fn finalize_markets(&self, mut result: Vec<MarketMeta>) -> anyhow::Result<Vec<MarketMeta>> {
        if result.is_empty() {
            bail!("market discovery found zero markets; verify filters")
        }

        let now = Utc::now();
        result.retain(|market| {
            market_is_tradeable_nowish(market, now, self.cfg.markets.max_past_market_age_minutes)
        });
        if result.is_empty() {
            bail!(
                "market discovery found only stale markets after time filter; check slug patterns or max_past_market_age_minutes"
            )
        }

        result.sort_by(|a, b| {
            market_priority(a, now)
                .cmp(&market_priority(b, now))
                .then_with(|| b.fees_enabled.cmp(&a.fees_enabled))
                .then_with(|| b.event_start_time.cmp(&a.event_start_time))
                .then_with(|| a.slug.cmp(&b.slug))
        });

        let mut seen = HashSet::new();
        result.retain(|market| seen.insert(market.condition_id.clone()));

        if result.len() > self.cfg.markets.max_markets {
            warn!(
                discovered = result.len(),
                max_markets = self.cfg.markets.max_markets,
                "truncating discovered markets to max_markets"
            );
            result = balance_markets_by_asset(result, self.cfg.markets.max_markets);
        }

        Ok(result)
    }

    fn passes_filters(&self, raw: &RawMarket) -> anyhow::Result<bool> {
        self.passes_market_filters(
            &raw.question,
            &raw.slug,
            raw.fees_enabled,
            raw.start_date,
            raw.end_date,
        )
    }

    fn passes_market_filters(
        &self,
        question: &str,
        slug: &str,
        fees_enabled: bool,
        start_date: Option<DateTime<Utc>>,
        end_date: Option<DateTime<Utc>>,
    ) -> anyhow::Result<bool> {
        if self.cfg.markets.require_fee_enabled && !fees_enabled {
            return Ok(false);
        }

        if !self.include_patterns.is_empty()
            && !self.include_patterns.iter().any(|re| re.is_match(question))
        {
            return Ok(false);
        }
        if self.exclude_patterns.iter().any(|re| re.is_match(question)) {
            return Ok(false);
        }

        if !self.include_slug_patterns.is_empty()
            && !self
                .include_slug_patterns
                .iter()
                .any(|re| re.is_match(slug))
        {
            return Ok(false);
        }
        if self
            .exclude_slug_patterns
            .iter()
            .any(|re| re.is_match(slug))
        {
            return Ok(false);
        }

        if self.cfg.markets.enforce_short_lifetime
            && let (Some(start), Some(end)) = (start_date, end_date)
        {
            let duration_mins = (end - start).num_minutes();
            if duration_mins < self.cfg.markets.min_lifetime_minutes
                || duration_mins > self.cfg.markets.max_lifetime_minutes
            {
                return Ok(false);
            }
        }

        Ok(true)
    }

    fn slug_passes_regex_filters(&self, slug: &str) -> bool {
        if !self.include_slug_patterns.is_empty()
            && !self
                .include_slug_patterns
                .iter()
                .any(|re| re.is_match(slug))
        {
            return false;
        }
        if self
            .exclude_slug_patterns
            .iter()
            .any(|re| re.is_match(slug))
        {
            return false;
        }
        true
    }

    fn convert_gamma_market(&self, raw: RawMarket) -> anyhow::Result<Option<MarketMeta>> {
        let token_ids = parse_json_vec::<String>(&raw.clob_token_ids).unwrap_or_default();
        let outcomes = parse_json_vec::<String>(&raw.outcomes).unwrap_or_default();

        self.build_market_meta(BuildMarketArgs {
            market_id: raw.id,
            condition_id: raw.condition_id,
            slug: raw.slug,
            question: raw.question,
            fee_type: raw.fee_type,
            fees_enabled: raw.fees_enabled,
            maker_base_fee_bps: raw.maker_base_fee.unwrap_or(0),
            taker_base_fee_bps: raw.taker_base_fee.unwrap_or(0),
            maker_rebates_fee_share_bps: raw.maker_rebates_fee_share_bps,
            fee_schedule: raw.fee_schedule,
            token_ids,
            outcomes,
            tick_size: raw.order_price_min_tick_size,
            min_order_size: raw.order_min_size,
            event_start_time: raw.start_date,
            end_date: raw.end_date,
        })
    }

    fn convert_hub_market(&self, raw: HubMarket) -> anyhow::Result<Option<MarketMeta>> {
        self.build_market_meta(BuildMarketArgs {
            market_id: raw.id,
            condition_id: raw.condition_id,
            slug: raw.slug,
            question: raw.question,
            fee_type: raw.fee_type,
            fees_enabled: raw.fees_enabled,
            maker_base_fee_bps: raw.maker_base_fee.unwrap_or(0),
            taker_base_fee_bps: raw.taker_base_fee.unwrap_or(0),
            maker_rebates_fee_share_bps: raw.maker_rebates_fee_share_bps,
            fee_schedule: raw.fee_schedule,
            token_ids: raw.clob_token_ids,
            outcomes: raw.outcomes,
            tick_size: raw.order_price_min_tick_size,
            min_order_size: raw.order_min_size,
            event_start_time: raw.start_date,
            end_date: raw.end_date,
        })
    }

    fn build_market_meta(&self, args: BuildMarketArgs) -> anyhow::Result<Option<MarketMeta>> {
        if args.token_ids.len() != args.outcomes.len() || args.token_ids.len() < 2 {
            return Ok(None);
        }

        let mut yes_token = None;
        let mut no_token = None;

        for (token, outcome) in args.token_ids.iter().zip(args.outcomes.iter()) {
            let normalized = outcome.trim().to_ascii_lowercase();
            match normalized.as_str() {
                "yes" | "up" => yes_token = Some(token.clone()),
                "no" | "down" => no_token = Some(token.clone()),
                _ => {}
            }
        }

        let (yes_token_id, no_token_id) = match (yes_token, no_token) {
            (Some(y), Some(n)) => (
                normalize_clob_token_id(y.as_str()),
                normalize_clob_token_id(n.as_str()),
            ),
            _ => return Ok(None),
        };

        let end_time = market_window_end_from_slug(&args.slug).or(args.end_date);

        Ok(Some(MarketMeta {
            market_id: args.market_id,
            condition_id: args.condition_id,
            slug: args.slug,
            question: args.question,
            fee_type: args.fee_type,
            fees_enabled: args.fees_enabled,
            maker_base_fee_bps: args.maker_base_fee_bps,
            taker_base_fee_bps: args.taker_base_fee_bps,
            maker_rebates_fee_share_bps: args.maker_rebates_fee_share_bps,
            fee_schedule: args.fee_schedule.unwrap_or_default().into(),
            yes_token_id,
            no_token_id,
            tick_size: args.tick_size.unwrap_or(Decimal::new(1, 2)),
            min_order_size: args.min_order_size.unwrap_or(Decimal::new(5, 0)),
            event_start_time: args.event_start_time,
            end_time,
        }))
    }
}

#[derive(Debug)]
struct BuildMarketArgs {
    market_id: String,
    condition_id: String,
    slug: String,
    question: String,
    fee_type: Option<String>,
    fees_enabled: bool,
    maker_base_fee_bps: u32,
    taker_base_fee_bps: u32,
    maker_rebates_fee_share_bps: Option<u32>,
    fee_schedule: Option<RawFeeSchedule>,
    token_ids: Vec<String>,
    outcomes: Vec<String>,
    tick_size: Option<Decimal>,
    min_order_size: Option<Decimal>,
    event_start_time: Option<DateTime<Utc>>,
    end_date: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct HubMarket {
    id: String,
    question: String,
    condition_id: String,
    slug: String,
    fees_enabled: bool,
    fee_type: Option<String>,
    maker_base_fee: Option<u32>,
    taker_base_fee: Option<u32>,
    maker_rebates_fee_share_bps: Option<u32>,
    fee_schedule: Option<RawFeeSchedule>,
    clob_token_ids: Vec<String>,
    outcomes: Vec<String>,
    order_price_min_tick_size: Option<Decimal>,
    order_min_size: Option<Decimal>,
    start_date: Option<DateTime<Utc>>,
    end_date: Option<DateTime<Utc>>,
}

impl HubMarket {
    fn from_value(value: &Value) -> anyhow::Result<Self> {
        let obj = value
            .as_object()
            .context("hub market payload is not an object")?;

        Ok(Self {
            id: obj
                .get("id")
                .and_then(value_to_string)
                .context("hub market missing id")?,
            question: obj
                .get("question")
                .and_then(value_to_string)
                .context("hub market missing question")?,
            condition_id: obj
                .get("conditionId")
                .and_then(value_to_string)
                .context("hub market missing conditionId")?,
            slug: obj
                .get("slug")
                .and_then(value_to_string)
                .context("hub market missing slug")?,
            fees_enabled: obj
                .get("feesEnabled")
                .and_then(value_to_bool)
                .unwrap_or(false),
            fee_type: obj.get("feeType").and_then(value_to_string),
            maker_base_fee: obj.get("makerBaseFee").and_then(value_to_u32),
            taker_base_fee: obj.get("takerBaseFee").and_then(value_to_u32),
            maker_rebates_fee_share_bps: obj.get("makerRebatesFeeShareBps").and_then(value_to_u32),
            fee_schedule: obj.get("feeSchedule").and_then(value_to_fee_schedule),
            clob_token_ids: obj
                .get("clobTokenIds")
                .and_then(value_to_string_vec)
                .context("hub market missing clobTokenIds")?,
            outcomes: obj
                .get("outcomes")
                .and_then(value_to_string_vec)
                .context("hub market missing outcomes")?,
            order_price_min_tick_size: obj.get("orderPriceMinTickSize").and_then(value_to_decimal),
            order_min_size: obj.get("orderMinSize").and_then(value_to_decimal),
            start_date: obj.get("startDate").and_then(value_to_datetime),
            end_date: obj.get("endDate").and_then(value_to_datetime),
        })
    }
}

fn extract_event_slugs(html: &str) -> Vec<String> {
    let re = Regex::new(r#"/event/([a-z0-9-]+-updown-(?:5m|15m)-\d+)"#).expect("valid slug regex");
    let mut slugs = re
        .captures_iter(html)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        .collect::<Vec<_>>();
    slugs.sort_unstable();
    slugs.dedup();
    slugs
}

fn extract_next_data_json(html: &str) -> anyhow::Result<Value> {
    let re =
        Regex::new(r#"(?s)<script id="__NEXT_DATA__" type="application/json"[^>]*>(.*?)</script>"#)
            .expect("valid __NEXT_DATA__ regex");
    let json = re
        .captures(html)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str())
        .context("event page missing __NEXT_DATA__ payload")?;
    serde_json::from_str(json).context("failed to parse __NEXT_DATA__ json")
}

fn find_hub_market(value: &Value, target_slug: &str) -> Option<HubMarket> {
    match value {
        Value::Object(map) => {
            let matches_slug = map.get("slug").and_then(Value::as_str) == Some(target_slug)
                && map.contains_key("conditionId");
            if matches_slug && map.contains_key("clobTokenIds") && map.contains_key("outcomes") {
                return HubMarket::from_value(value).ok();
            }

            for child in map.values() {
                if let Some(market) = find_hub_market(child, target_slug) {
                    return Some(market);
                }
            }
            None
        }
        Value::Array(items) => {
            for item in items {
                if let Some(market) = find_hub_market(item, target_slug) {
                    return Some(market);
                }
            }
            None
        }
        _ => None,
    }
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn value_to_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::String(s) => s.parse::<bool>().ok(),
        _ => None,
    }
}

fn value_to_u32(value: &Value) -> Option<u32> {
    match value {
        Value::Number(n) => n.as_u64().and_then(|v| u32::try_from(v).ok()),
        Value::String(s) => s.parse::<u32>().ok(),
        _ => None,
    }
}

fn value_to_decimal(value: &Value) -> Option<Decimal> {
    match value {
        Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        Value::String(s) => Decimal::from_str(s).ok(),
        _ => None,
    }
}

fn value_to_datetime(value: &Value) -> Option<DateTime<Utc>> {
    value
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn value_to_string_vec(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Array(items) => items.iter().map(value_to_string).collect(),
        Value::String(s) => parse_json_vec::<String>(s).ok(),
        _ => None,
    }
}

fn value_to_fee_schedule(value: &Value) -> Option<RawFeeSchedule> {
    let obj = value.as_object()?;
    Some(RawFeeSchedule {
        exponent: obj.get("exponent").and_then(value_to_decimal),
        rate: obj.get("rate").and_then(value_to_decimal),
        taker_only: obj.get("takerOnly").and_then(value_to_bool),
        rebate_rate: obj.get("rebateRate").and_then(value_to_decimal),
    })
}

fn market_window_end_from_slug(slug: &str) -> Option<DateTime<Utc>> {
    let (_, suffix) = slug.rsplit_once('-')?;
    if !suffix.as_bytes().iter().all(u8::is_ascii_digit) {
        return None;
    }
    let ts = suffix.parse::<i64>().ok()?;
    Utc.timestamp_opt(ts, 0).single()
}

fn market_is_tradeable_nowish(
    market: &MarketMeta,
    now: DateTime<Utc>,
    max_past_market_age_minutes: i64,
) -> bool {
    let Some(end_time) = market.end_time else {
        return true;
    };
    let max_age = max_past_market_age_minutes.max(0);
    end_time >= now - ChronoDuration::minutes(max_age)
}

fn market_priority(market: &MarketMeta, now: DateTime<Utc>) -> (u8, i64) {
    let Some(end_time) = market.end_time else {
        return (2, i64::MAX);
    };
    let ts = end_time.timestamp();
    if end_time >= now { (0, ts) } else { (1, -ts) }
}

fn market_priority_from_slug(slug: &str, now: DateTime<Utc>) -> (u8, i64) {
    let Some(end_time) = market_window_end_from_slug(slug) else {
        return (2, i64::MAX);
    };
    let ts = end_time.timestamp();
    if end_time >= now { (0, ts) } else { (1, -ts) }
}

fn balance_markets_by_asset(markets: Vec<MarketMeta>, max_markets: usize) -> Vec<MarketMeta> {
    if markets.len() <= max_markets {
        return markets;
    }

    let mut buckets = BTreeMap::<String, VecDeque<MarketMeta>>::new();
    for market in markets {
        buckets
            .entry(slug_asset_key(&market.slug).to_string())
            .or_default()
            .push_back(market);
    }

    let mut out = Vec::with_capacity(max_markets);
    while out.len() < max_markets {
        let mut progressed = false;
        for bucket in buckets.values_mut() {
            if let Some(market) = bucket.pop_front() {
                out.push(market);
                progressed = true;
                if out.len() >= max_markets {
                    break;
                }
            }
        }
        if !progressed {
            break;
        }
    }

    out
}

fn slug_asset_key(slug: &str) -> &str {
    slug.split('-').next().unwrap_or(slug)
}

fn parse_json_vec<T: for<'de> Deserialize<'de>>(value: &str) -> anyhow::Result<Vec<T>> {
    serde_json::from_str::<Vec<T>>(value).context("failed to parse serialized JSON array field")
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMarket {
    id: String,
    question: String,
    condition_id: String,
    slug: String,
    fees_enabled: bool,
    fee_type: Option<String>,
    maker_base_fee: Option<u32>,
    taker_base_fee: Option<u32>,
    maker_rebates_fee_share_bps: Option<u32>,
    fee_schedule: Option<RawFeeSchedule>,
    clob_token_ids: String,
    outcomes: String,
    order_price_min_tick_size: Option<Decimal>,
    order_min_size: Option<Decimal>,
    start_date: Option<DateTime<Utc>>,
    end_date: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawFeeSchedule {
    exponent: Option<Decimal>,
    rate: Option<Decimal>,
    taker_only: Option<bool>,
    rebate_rate: Option<Decimal>,
}

impl From<RawFeeSchedule> for FeeSchedule {
    fn from(value: RawFeeSchedule) -> Self {
        Self {
            exponent: value.exponent.unwrap_or(Decimal::ONE),
            rate: value.rate.unwrap_or(Decimal::ZERO),
            taker_only: value.taker_only.unwrap_or(true),
            rebate_rate: value.rebate_rate.unwrap_or(Decimal::ZERO),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_updown_window_end_from_slug() {
        let end = market_window_end_from_slug("btc-updown-5m-1766162100").unwrap();
        assert_eq!(end, Utc.timestamp_opt(1_766_162_100, 0).single().unwrap());
    }

    #[test]
    fn filters_stale_markets_and_prefers_nearest_future() {
        let now = Utc.timestamp_opt(1_775_000_000, 0).single().unwrap();
        let stale = MarketMeta {
            market_id: "1".into(),
            condition_id: "a".into(),
            slug: "btc-updown-5m-1766162100".into(),
            question: "stale".into(),
            fee_type: None,
            fees_enabled: true,
            maker_base_fee_bps: 0,
            taker_base_fee_bps: 0,
            maker_rebates_fee_share_bps: None,
            fee_schedule: FeeSchedule::default(),
            yes_token_id: "yes-a".into(),
            no_token_id: "no-a".into(),
            tick_size: Decimal::new(1, 2),
            min_order_size: Decimal::new(5, 0),
            event_start_time: None,
            end_time: Some(Utc.timestamp_opt(1_766_162_100, 0).single().unwrap()),
        };
        let near = MarketMeta {
            market_id: "2".into(),
            condition_id: "b".into(),
            slug: "btc-updown-5m-1775000100".into(),
            question: "near".into(),
            fee_type: None,
            fees_enabled: true,
            maker_base_fee_bps: 0,
            taker_base_fee_bps: 0,
            maker_rebates_fee_share_bps: None,
            fee_schedule: FeeSchedule::default(),
            yes_token_id: "yes-b".into(),
            no_token_id: "no-b".into(),
            tick_size: Decimal::new(1, 2),
            min_order_size: Decimal::new(5, 0),
            event_start_time: None,
            end_time: Some(Utc.timestamp_opt(1_775_000_100, 0).single().unwrap()),
        };
        let far = MarketMeta {
            market_id: "3".into(),
            condition_id: "c".into(),
            slug: "btc-updown-5m-1775003000".into(),
            question: "far".into(),
            fee_type: None,
            fees_enabled: true,
            maker_base_fee_bps: 0,
            taker_base_fee_bps: 0,
            maker_rebates_fee_share_bps: None,
            fee_schedule: FeeSchedule::default(),
            yes_token_id: "yes-c".into(),
            no_token_id: "no-c".into(),
            tick_size: Decimal::new(1, 2),
            min_order_size: Decimal::new(5, 0),
            event_start_time: None,
            end_time: Some(Utc.timestamp_opt(1_775_003_000, 0).single().unwrap()),
        };

        assert!(!market_is_tradeable_nowish(&stale, now, 15));
        assert!(market_is_tradeable_nowish(&near, now, 15));
        assert!(market_priority(&near, now) < market_priority(&far, now));
    }

    #[test]
    fn extracts_market_from_next_data_payload() {
        let payload = json!({
            "props": {
                "pageProps": {
                    "dehydratedState": {
                        "queries": [
                            {
                                "state": {
                                    "data": {
                                        "markets": [
                                            {
                                                "id": "123",
                                                "conditionId": "0xabc",
                                                "slug": "btc-updown-5m-1775863500",
                                                "question": "Bitcoin Up or Down - test",
                                                "feesEnabled": true,
                                                "feeType": "crypto_fees_v2",
                                                "makerBaseFee": 1000,
                                                "takerBaseFee": 1000,
                                                "makerRebatesFeeShareBps": 10000,
                                                "feeSchedule": {
                                                    "exponent": 1,
                                                    "rate": 0.072,
                                                    "takerOnly": true,
                                                    "rebateRate": 0.2
                                                },
                                                "clobTokenIds": ["yes-token", "no-token"],
                                                "outcomes": ["Up", "Down"],
                                                "orderPriceMinTickSize": 0.01,
                                                "orderMinSize": 5,
                                                "startDate": "2026-04-10T23:25:00Z",
                                                "endDate": "2026-04-10T23:30:00Z"
                                            }
                                        ]
                                    }
                                }
                            }
                        ]
                    }
                }
            }
        });
        let html = format!(
            r#"<html><head></head><body><script id="__NEXT_DATA__" type="application/json">{payload}</script></body></html>"#
        );

        let next = extract_next_data_json(&html).unwrap();
        let market = find_hub_market(&next, "btc-updown-5m-1775863500").unwrap();
        assert_eq!(market.condition_id, "0xabc");
        assert_eq!(market.clob_token_ids, vec!["yes-token", "no-token"]);
        assert_eq!(market.outcomes, vec!["Up", "Down"]);
        assert_eq!(
            market.end_date,
            Some(Utc.timestamp_opt(1_775_863_800, 0).single().unwrap())
        );
    }
}
