use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use config::{Config as RawConfig, Environment, File};
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::types::SignatureKind;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub environment: String,
    pub auth: AuthConfig,
    pub clob: ClobConfig,
    pub gamma: GammaConfig,
    pub pyth: PythConfig,
    pub markets: MarketsConfig,
    pub strategy: StrategyConfig,
    pub risk: RiskConfig,
    pub execution: ExecutionConfig,
    pub replay: ReplayConfig,
    pub telemetry: TelemetryConfig,
    pub infra: InfraConfig,
    pub paper_engine: PaperEngineConfig,
}

impl AppConfig {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path_ref = path.as_ref();
        let cfg = RawConfig::builder()
            .add_source(File::from(path_ref).required(false))
            .add_source(
                Environment::with_prefix("POLY_HFT")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()
            .with_context(|| format!("failed to load config from {}", path_ref.display()))?;

        cfg.try_deserialize::<Self>()
            .context("failed to deserialize AppConfig")
    }

    #[must_use]
    pub fn decision_interval(&self) -> Duration {
        Duration::from_millis(self.strategy.decision_interval_ms)
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            environment: "prod".to_string(),
            auth: AuthConfig::default(),
            clob: ClobConfig::default(),
            gamma: GammaConfig::default(),
            pyth: PythConfig::default(),
            markets: MarketsConfig::default(),
            strategy: StrategyConfig::default(),
            risk: RiskConfig::default(),
            execution: ExecutionConfig::default(),
            replay: ReplayConfig::default(),
            telemetry: TelemetryConfig::default(),
            infra: InfraConfig::default(),
            paper_engine: PaperEngineConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    pub private_key: Option<String>,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub api_passphrase: Option<String>,
    pub wallet_address: Option<String>,
    pub funder_address: Option<String>,
    pub signature_kind: SignatureKind,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            private_key: None,
            api_key: None,
            api_secret: None,
            api_passphrase: None,
            wallet_address: None,
            funder_address: None,
            signature_kind: SignatureKind::GnosisSafe,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ClobConfig {
    pub rest_url: String,
    pub ws_url: String,
    pub ws_user_url: String,
    pub ws_sports_url: String,
}

impl Default for ClobConfig {
    fn default() -> Self {
        Self {
            rest_url: "https://clob.polymarket.com".to_string(),
            ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_string(),
            ws_user_url: "wss://ws-subscriptions-clob.polymarket.com/ws/user".to_string(),
            ws_sports_url: "wss://sports-api.polymarket.com/ws".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GammaConfig {
    pub rest_url: String,
    pub markets_path: String,
    pub request_timeout_ms: u64,
    pub page_size: usize,
    pub max_pages: usize,
}

impl Default for GammaConfig {
    fn default() -> Self {
        Self {
            rest_url: "https://gamma-api.polymarket.com".to_string(),
            markets_path: "/markets".to_string(),
            request_timeout_ms: 2_000,
            page_size: 500,
            max_pages: 200,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PythConfig {
    pub enabled: bool,
    pub hermes_url: String,
    pub stream_path: String,
    pub connect_timeout_ms: u64,
    pub reconnect_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub feed_ids: Vec<PythFeedConfig>,
}

impl Default for PythConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hermes_url: "https://hermes.pyth.network".to_string(),
            stream_path: "/v2/updates/price/stream".to_string(),
            connect_timeout_ms: 2_500,
            reconnect_backoff_ms: 500,
            max_backoff_ms: 10_000,
            feed_ids: vec![
                PythFeedConfig {
                    symbol: "btcusd".to_string(),
                    feed_id: "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43"
                        .to_string(),
                },
                PythFeedConfig {
                    symbol: "ethusd".to_string(),
                    feed_id: "ff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace"
                        .to_string(),
                },
                PythFeedConfig {
                    symbol: "solusd".to_string(),
                    feed_id: "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d"
                        .to_string(),
                },
            ],
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PythFeedConfig {
    pub symbol: String,
    pub feed_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MarketsConfig {
    pub refresh_secs: u64,
    pub require_fee_enabled: bool,
    pub max_past_market_age_minutes: i64,
    pub enforce_short_lifetime: bool,
    pub min_lifetime_minutes: i64,
    pub max_lifetime_minutes: i64,
    pub max_markets: usize,
    pub include_question_patterns: Vec<String>,
    pub exclude_question_patterns: Vec<String>,
    /// If non-empty, market `slug` must match at least one regex (e.g. `(?i)updown-5m` for short crypto windows).
    pub include_slug_patterns: Vec<String>,
    pub exclude_slug_patterns: Vec<String>,
    pub explicit_condition_ids: Vec<String>,
}

impl Default for MarketsConfig {
    fn default() -> Self {
        Self {
            refresh_secs: 30,
            require_fee_enabled: true,
            max_past_market_age_minutes: 15,
            enforce_short_lifetime: true,
            min_lifetime_minutes: 4,
            max_lifetime_minutes: 8,
            max_markets: 24,
            include_question_patterns: vec![
                "(?i)\\b(bitcoin|btc)\\b.*\\b(up|down)\\b".to_string(),
                "(?i)\\b(ethereum|eth)\\b.*\\b(up|down)\\b".to_string(),
                "(?i)\\b(solana|sol)\\b.*\\b(up|down)\\b".to_string(),
                "(?i)\\b(bitcoin|btc)\\b.*(5-minute|5 minute|5 min|5m)".to_string(),
                "(?i)\\b(ethereum|eth)\\b.*(5-minute|5 minute|5 min|5m)".to_string(),
                "(?i)\\b(solana|sol)\\b.*(5-minute|5 minute|5 min|5m)".to_string(),
            ],
            exclude_question_patterns: vec!["(?i)ethereal".to_string(), "(?i)staked".to_string()],
            include_slug_patterns: vec!["(?i)^(btc|eth|sol)-updown-5m".to_string()],
            exclude_slug_patterns: Vec::new(),
            explicit_condition_ids: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StrategyConfig {
    pub decision_interval_ms: u64,
    pub post_only_target_ratio: Decimal,
    pub quote_half_spread_bps: Decimal,
    pub quote_levels: usize,
    pub quote_level_step_bps: Decimal,
    pub quote_size_usdc: Decimal,
    pub min_quote_size_usdc: Decimal,
    pub max_quote_size_usdc: Decimal,
    /// When > 0, each maker clip must have at least this USDC notional (size × price).
    pub clip_min_usdc: Decimal,
    /// When > 0, each maker clip must not exceed this USDC notional (best-effort, 0.01 share steps).
    pub clip_max_usdc: Decimal,
    pub fractional_kelly_cap: Decimal,
    pub mispricing_min_edge: Decimal,
    pub min_taker_edge: Decimal,
    pub queue_decay_lambda: Decimal,
    pub adverse_selection_windows_ms: Vec<u64>,
    pub adverse_selection_weight: Decimal,
    pub ref_price_max_staleness_ms: u64,
    pub chainlink_pyth_bias_weight: Decimal,
    pub chainlink_pyth_arb_min_edge: Decimal,
    /// Minimum absolute signal strength required to emit maker quotes.
    pub maker_min_signal_edge: Decimal,
    /// Minimum per-quote edge vs fair value required for maker quote placement.
    pub maker_min_quote_edge: Decimal,
    /// Minimum expected value per share for maker quote placement.
    pub maker_min_expected_value: Decimal,
    /// Minimum fill probability proxy required before maker quote placement.
    pub maker_min_fill_probability: Decimal,
    /// Per-attempt cancel/replace opportunity cost (USDC/share equivalent) in EV calc.
    pub maker_cancel_cost_per_quote: Decimal,
    /// Hard cap for number of maker quotes emitted per market per decision tick.
    pub maker_max_quotes_per_market: usize,
    /// If |directional_bias| exceeds this threshold, maker buys are one-sided (only aligned leg).
    pub maker_one_sided_bias_threshold: Decimal,
    /// Maker **sell** quotes (limit asks) so you can close Up/Down or Yes/No shares on the book before resolution. Needs outcome-token balance on the CLOB; off avoids log noise when you only hold USDC.
    pub maker_sell_quotes: bool,
    /// Post-only maker bids/asks (requires a book). Turn off to rely on takers only (e.g. directional FOK).
    pub maker_quotes_enabled: bool,
    /// When true, emit FOK **market buy** on YES when directional bias is sufficiently bullish, on NO when bearish (execution uses CLOB `market_order`).
    pub directional_market_enabled: bool,
    /// Minimum absolute directional bias (momentum + weighted Chainlink/Pyth basis) before opening a directional market leg.
    pub directional_market_min_abs_bias: Decimal,
    /// USDC notional per directional FOK; `0` means use `quote_size_usdc`.
    pub directional_market_usdc: Decimal,
    /// When > 0, directional and volume-spike takers require both asks on the book and
    /// `YES_ask + NO_ask ≤` this cap (skips expensive / one-sided books). `0` = no gate.
    pub momentum_taker_max_ask_sum: Decimal,

    // ── Tail-end strategy ────────────────────────────────────────────────────
    /// When true, buy the high-probability leg when its mid > `tail_end_min_prob` and the
    /// market closes within `tail_end_minutes_to_expiry` minutes.  Captures the final
    /// convergence yield on near-certain outcomes.
    pub tail_end_enabled: bool,
    /// Minimum outcome probability (e.g. 0.88) before we consider a leg "near-certain".
    pub tail_end_min_prob: Decimal,
    /// Buy tail-end only when ≤ this many minutes remain before market close.
    pub tail_end_minutes_to_expiry: i64,
    /// USDC notional per tail-end FOK; 0 falls back to `quote_size_usdc`.
    pub tail_end_usdc: Decimal,

    // ── Volume / activity spike strategy ────────────────────────────────────
    /// When true, fire a momentum-aligned taker when book update rate spikes above
    /// `volume_spike_multiplier × baseline` (online EWMA-based).
    pub volume_spike_enabled: bool,
    /// Ratio of fast EWMA to slow EWMA that qualifies as a "spike" (e.g. 2.5).
    pub volume_spike_multiplier: Decimal,
    /// USDC notional per volume-spike taker; 0 falls back to `quote_size_usdc`.
    pub volume_spike_usdc: Decimal,

    // ── Spread-farm strategy ─────────────────────────────────────────────────
    /// When true, take both YES and NO whenever ask_sum ≤ `spread_farm_max_ask_sum`
    /// (looser threshold than the full arbitrage `min_taker_edge`).  Accumulates
    /// both legs cheaply in wide-spread markets and profits on convergence or resolution.
    pub spread_farm_enabled: bool,
    /// Maximum YES-ask + NO-ask sum qualifying for spread-farm entry (e.g. 0.97).
    pub spread_farm_max_ask_sum: Decimal,
    /// USDC notional per leg for spread-farm entries; 0 falls back to `quote_size_usdc`.
    pub spread_farm_usdc: Decimal,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            decision_interval_ms: 50,
            post_only_target_ratio: Decimal::new(90, 2), // 0.90
            quote_half_spread_bps: Decimal::new(6, 0),
            quote_levels: 2,
            quote_level_step_bps: Decimal::new(4, 0),
            quote_size_usdc: Decimal::new(25, 0),
            min_quote_size_usdc: Decimal::new(5, 0),
            max_quote_size_usdc: Decimal::new(200, 0),
            clip_min_usdc: Decimal::ZERO,
            clip_max_usdc: Decimal::ZERO,
            fractional_kelly_cap: Decimal::new(25, 2), // 0.25
            mispricing_min_edge: Decimal::new(2, 3),   // 0.002
            min_taker_edge: Decimal::new(7, 3),
            queue_decay_lambda: Decimal::new(15, 2), // 0.15
            adverse_selection_windows_ms: vec![50, 200, 500],
            adverse_selection_weight: Decimal::new(10, 2),
            ref_price_max_staleness_ms: 1_500,
            chainlink_pyth_bias_weight: Decimal::new(35, 2),
            chainlink_pyth_arb_min_edge: Decimal::new(8, 4),
            maker_min_signal_edge: Decimal::new(12, 4), // 0.0012
            maker_min_quote_edge: Decimal::new(6, 4),   // 0.0006
            maker_min_expected_value: Decimal::new(3, 4), // 0.0003
            maker_min_fill_probability: Decimal::new(3, 2), // 0.03
            maker_cancel_cost_per_quote: Decimal::new(1, 4), // 0.0001
            maker_max_quotes_per_market: 4,
            maker_one_sided_bias_threshold: Decimal::new(15, 4), // 0.0015
            maker_sell_quotes: false,
            maker_quotes_enabled: true,
            directional_market_enabled: false,
            directional_market_min_abs_bias: Decimal::ZERO,
            directional_market_usdc: Decimal::ZERO,
            momentum_taker_max_ask_sum: Decimal::ZERO,
            tail_end_enabled: false,
            tail_end_min_prob: Decimal::new(88, 2),    // 0.88
            tail_end_minutes_to_expiry: 2,
            tail_end_usdc: Decimal::ZERO,
            volume_spike_enabled: false,
            volume_spike_multiplier: Decimal::new(25, 1), // 2.5
            volume_spike_usdc: Decimal::ZERO,
            spread_farm_enabled: false,
            spread_farm_max_ask_sum: Decimal::new(97, 2), // 0.97
            spread_farm_usdc: Decimal::ZERO,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RiskConfig {
    pub max_notional_per_market: Decimal,
    pub max_total_notional: Decimal,
    pub inventory_soft_limit: Decimal,
    pub inventory_hard_limit: Decimal,
    pub max_drawdown_usdc: Decimal,
    pub stale_data_kill_ms: u64,
    pub correlation_limit_btc_eth: Decimal,
    pub max_open_orders: usize,
    pub min_seconds_to_market_end: i64,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_notional_per_market: Decimal::new(2_500, 0),
            max_total_notional: Decimal::new(5_000, 0),
            inventory_soft_limit: Decimal::new(500, 0),
            inventory_hard_limit: Decimal::new(1000, 0),
            max_drawdown_usdc: Decimal::new(1_250, 0),
            stale_data_kill_ms: 2_500,
            correlation_limit_btc_eth: Decimal::new(85, 2), // 0.85
            max_open_orders: 60,
            min_seconds_to_market_end: 20,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ExecutionConfig {
    pub cancel_replace_interval_ms: u64,
    pub quote_ttl_ms: u64,
    pub taker_rebalance_cooldown_ms: u64,
    pub heartbeat_interval_secs: u64,
    pub liveness_timeout_secs: u64,
    pub retry_initial_ms: u64,
    pub retry_max_ms: u64,
    pub retry_max_attempts: usize,
    pub cloudflare_status_codes: Vec<u16>,
    pub matching_engine_restart_status: u16,
    pub use_server_time: bool,
    pub maker_only_default: bool,
    /// Do not cancel/replace a live maker order before this minimum age.
    pub requote_min_age_ms: u64,
    /// Cancel/replace only when quote price moved by at least this many ticks.
    pub requote_price_ticks: u32,
    /// Cancel/replace only when quote size changed by at least this relative threshold in bps.
    pub requote_size_bps: u32,
    pub enable_wrap_unwrap: bool,
    /// Polymarket CLOB `/v1/heartbeats` (order priority). Off by default — many accounts see 400 Invalid Heartbeat ID spam.
    pub enable_clob_heartbeats: bool,
    /// When true (or when the agent is started with `--dry-run`), no orders are sent; fills are simulated for risk + activity UI (see `polyhft-replay::simulate_fills`).
    pub paper_trading: bool,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            cancel_replace_interval_ms: 150,
            quote_ttl_ms: 800,
            taker_rebalance_cooldown_ms: 3_000,
            heartbeat_interval_secs: 5,
            liveness_timeout_secs: 15,
            retry_initial_ms: 250,
            retry_max_ms: 30_000,
            retry_max_attempts: 8,
            cloudflare_status_codes: vec![408, 429, 502, 503, 504],
            matching_engine_restart_status: 425,
            use_server_time: true,
            maker_only_default: true,
            requote_min_age_ms: 250,
            requote_price_ticks: 2,
            requote_size_bps: 1500,
            enable_wrap_unwrap: false,
            enable_clob_heartbeats: false,
            paper_trading: false,
        }
    }
}

fn default_paper_taker_slippage_bps() -> u32 {
    100
}

fn default_paper_taker_fill_probability() -> f64 {
    0.55
}

fn default_paper_maker_backstop_fill_probability() -> f64 {
    0.18
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ReplayConfig {
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    pub latency_p99_ms: u64,
    pub random_seed: u64,
    pub event_log_path: String,
    /// Paper / replay: taker fills execute at mid plus this slippage (crossing spread proxy).
    #[serde(default = "default_paper_taker_slippage_bps")]
    pub paper_taker_slippage_bps: u32,
    /// Paper / replay: Bernoulli probability each taker intent becomes a simulated fill (FOK often fails).
    #[serde(default = "default_paper_taker_fill_probability")]
    pub paper_taker_fill_probability: f64,
    /// Paper / replay: extra Bernoulli fill when maker quote has not crossed the mid.
    #[serde(default = "default_paper_maker_backstop_fill_probability")]
    pub paper_maker_backstop_fill_probability: f64,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            latency_p50_ms: 8,
            latency_p95_ms: 22,
            latency_p99_ms: 55,
            random_seed: 42,
            event_log_path: "./data/replay/events.ndjson".to_string(),
            paper_taker_slippage_bps: default_paper_taker_slippage_bps(),
            paper_taker_fill_probability: default_paper_taker_fill_probability(),
            paper_maker_backstop_fill_probability: default_paper_maker_backstop_fill_probability(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    pub log_level: String,
    pub json_logs: bool,
    pub prometheus_bind: String,
    /// JSON feed for the flow-theatre UI (`GET /api/activity`). Empty string disables the listener.
    pub activity_api_bind: String,
    /// Optional NDJSON file path for training dataset logging (decisions + fills).
    pub training_log_path: String,
    /// In-memory queue size for training log writer.
    pub training_log_queue: usize,
    pub metrics_prefix: String,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
            json_logs: false,
            prometheus_bind: "0.0.0.0:9901".to_string(),
            activity_api_bind: "127.0.0.1:9902".to_string(),
            training_log_path: String::new(),
            training_log_queue: 16_384,
            metrics_prefix: "polyhft".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct InfraConfig {
    pub enable_busy_poll_notes: bool,
    pub cpu_isolation: Vec<usize>,
    pub irq_affinity: Vec<usize>,
    pub ptp_drift_warn_ms: i64,
    pub ptp_drift_kill_ms: i64,
}

impl Default for InfraConfig {
    fn default() -> Self {
        Self {
            enable_busy_poll_notes: true,
            cpu_isolation: vec![2, 3, 4, 5],
            irq_affinity: vec![0, 1],
            ptp_drift_warn_ms: 5,
            ptp_drift_kill_ms: 25,
        }
    }
}

/// Configuration for the standalone favorite-side paper trading engine.
/// Loaded from `[paper_engine]` in config.toml; all fields have safe defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PaperEngineConfig {
    // ── Eligibility ──────────────────────────────────────────────────────────
    pub max_hours_to_expiry: f64,
    pub min_hours_to_expiry: f64,
    pub quote_max_stale_ms: i64,
    pub min_book_liquidity: Decimal,
    pub max_spread_fraction: Decimal,

    // ── Signal ───────────────────────────────────────────────────────────────
    pub fav_prob_low: Decimal,
    pub fav_prob_high: Decimal,
    pub min_net_edge: Decimal,
    pub taker_fee_fraction: Decimal,

    // ── Execution ────────────────────────────────────────────────────────────
    pub entry_usdc: Decimal,
    pub slippage_bps: Decimal,
    pub fill_probability: f64,
    /// Fraction of notional filled when a simulated fill succeeds (1.0 = full fill).
    pub partial_fill_fraction: Decimal,

    // ── Position management ──────────────────────────────────────────────────
    pub take_profit_fraction: Decimal,
    /// Minimum absolute net profit required for any early close (USDC).
    pub min_early_exit_profit_usdc: Decimal,
    /// Minimum net profit fraction required for any early close.
    pub min_early_exit_profit_fraction: Decimal,
    pub stop_loss_fraction: Decimal,
    pub max_hold_secs: i64,
    pub force_flat_secs_before_expiry: i64,
    pub entry_cooldown_secs: i64,
    pub max_open_positions: usize,
    pub max_exposure_per_market_usdc: Decimal,
}

impl Default for PaperEngineConfig {
    fn default() -> Self {
        Self {
            max_hours_to_expiry: 999.0,
            min_hours_to_expiry: 0.005,
            quote_max_stale_ms: 30_000,
            min_book_liquidity: Decimal::new(5, 0),
            max_spread_fraction: Decimal::new(1000, 2),  // 10.00
            fav_prob_low: Decimal::new(1, 2),            // 0.01
            fav_prob_high: Decimal::new(99, 2),          // 0.99
            min_net_edge: Decimal::new(-1, 0),           // -1.0 (paper mode permissive)
            taker_fee_fraction: Decimal::new(1, 2),      // 0.01
            entry_usdc: Decimal::new(10, 0),
            slippage_bps: Decimal::new(5, 0),
            fill_probability: 0.55,
            partial_fill_fraction: Decimal::ONE,
            take_profit_fraction: Decimal::new(5, 3),    // 0.005
            min_early_exit_profit_usdc: Decimal::new(10, 2), // 0.10
            min_early_exit_profit_fraction: Decimal::new(5, 2), // 0.05
            stop_loss_fraction: Decimal::new(4, 2),      // 0.04
            max_hold_secs: 90,
            force_flat_secs_before_expiry: 60,
            entry_cooldown_secs: 2,
            max_open_positions: 4,
            max_exposure_per_market_usdc: Decimal::new(50, 0),
        }
    }
}
