use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::OpenOptions as StdOpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Router, response::Response};
use chrono::{DateTime, Utc};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use polyhft_core::config::AppConfig;
use polyhft_core::telemetry::init_tracing;

const DEFAULT_GAMMA_URL: &str = "https://gamma-api.polymarket.com";
const DEFAULT_CLOB_URL: &str = "https://clob.polymarket.com";
const DEFAULT_RTDS_URL: &str = "wss://ws-live-data.polymarket.com";
const DEFAULT_PYTH_BTC_FEED_ID: &str =
    "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43";

#[derive(Debug, Parser, Clone)]
#[command(author, version, about = "Browser dashboard for Polymarket HFT agent")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:3000")]
    bind: String,

    #[arg(long, default_value = "http://127.0.0.1:9901/")]
    target: String,

    /// When the agent Prometheus endpoint is unreachable (connection refused, timeout), use
    /// in-process zero metrics instead of failing every scrape. Disable with `false` if you
    /// want strict errors only.
    #[arg(long, default_value_t = true)]
    stub_metrics_if_unreachable: bool,

    #[arg(long, default_value_t = 1000)]
    refresh_ms: u64,

    #[arg(long, default_value_t = 900)]
    history_limit: usize,

    #[arg(long, default_value = "info")]
    log_level: String,

    #[arg(long, default_value_t = false)]
    json_logs: bool,

    #[arg(long, default_value_t = 1500)]
    request_timeout_ms: u64,

    #[arg(long, default_value = ".")]
    workspace: String,

    #[arg(long, default_value = "config.toml")]
    agent_default_config: String,

    #[arg(long)]
    agent_binary: Option<String>,

    #[arg(long, default_value_t = 1000)]
    panel_refresh_ms: u64,

    #[arg(long, default_value_t = 30)]
    market_refresh_secs: u64,

    #[arg(long, default_value_t = 300)]
    indicator_interval_secs: u64,

    /// URL of the live-agent activity API (port 9902 by default).
    /// Shown to the browser so the dashboard JS can call it directly.
    #[arg(long, default_value = "http://127.0.0.1:9902")]
    activity_url: String,

    #[arg(long, default_value = DEFAULT_GAMMA_URL)]
    gamma_url: String,

    #[arg(long, default_value = DEFAULT_CLOB_URL)]
    clob_url: String,

    #[arg(long, default_value = DEFAULT_RTDS_URL)]
    rtds_url: String,

    #[arg(long, default_value = DEFAULT_PYTH_BTC_FEED_ID)]
    btc_pyth_feed_id: String,
}

#[derive(Debug, Clone, Serialize, Default)]
struct CurrentMetrics {
    maker_reports_total: Option<f64>,
    taker_reports_total: Option<f64>,
    maker_ratio: Option<f64>,
    signal_edge: Option<f64>,
    chainlink_pyth_basis: Option<f64>,
    adverse_selection: Option<f64>,
    decision_cycle_ms: Option<f64>,
    queue_len: Option<f64>,
    events_ingested_total: Option<f64>,
    // Paper engine (favorite-side) metrics
    pe_fills_total: Option<f64>,
    pe_open_positions: Option<f64>,
    pe_total_pnl: Option<f64>,
    pe_live_orders_total: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Default)]
struct HistoryPoint {
    ts_ms: i64,
    maker_ratio: Option<f64>,
    maker_reports_total: Option<f64>,
    taker_reports_total: Option<f64>,
    signal_edge: Option<f64>,
    chainlink_pyth_basis: Option<f64>,
    adverse_selection: Option<f64>,
    decision_cycle_ms: Option<f64>,
    queue_len: Option<f64>,
    events_ingested_total: Option<f64>,
    pe_total_pnl: Option<f64>,
    pe_open_positions: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct DashboardSnapshot {
    target: String,
    up: bool,
    last_scrape_ms: i64,
    scrape_latency_ms: f64,
    error: Option<String>,
    current: CurrentMetrics,
    history: Vec<HistoryPoint>,
}

impl DashboardSnapshot {
    fn new(target: String) -> Self {
        Self {
            target,
            up: false,
            last_scrape_ms: 0,
            scrape_latency_ms: 0.0,
            error: Some("waiting for first scrape".to_string()),
            current: CurrentMetrics::default(),
            history: Vec::new(),
        }
    }

    fn push_history(&mut self, point: HistoryPoint, limit: usize) {
        self.history.push(point);
        if self.history.len() > limit {
            let drop_count = self.history.len().saturating_sub(limit);
            self.history.drain(0..drop_count);
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
struct PriceFeedPoint {
    value: Option<f64>,
    source_ts_ms: Option<i64>,
    observed_ts_ms: Option<i64>,
    stale: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
struct BtcFeeds {
    binance: PriceFeedPoint,
    coinbase: PriceFeedPoint,
    pyth: PriceFeedPoint,
    chainlink: PriceFeedPoint,
    blended: PriceFeedPoint,
}

#[derive(Debug, Clone, Serialize)]
struct PolyMarketPanel {
    horizon: String,
    market_found: bool,
    slug: Option<String>,
    condition_id: Option<String>,
    question: Option<String>,
    start_ts_ms: Option<i64>,
    start_level: Option<f64>,
    start_level_captured_ms: Option<i64>,
    end_ts_ms: Option<i64>,
    up_mid: Option<f64>,
    down_mid: Option<f64>,
    sum_mid: Option<f64>,
    updated_ms: Option<i64>,
}

impl PolyMarketPanel {
    fn empty(horizon: &str) -> Self {
        Self {
            horizon: horizon.to_string(),
            market_found: false,
            slug: None,
            condition_id: None,
            question: None,
            start_ts_ms: None,
            start_level: None,
            start_level_captured_ms: None,
            end_ts_ms: None,
            up_mid: None,
            down_mid: None,
            sum_mid: None,
            updated_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct IndicatorPanel {
    horizon: String,
    decision: String,
    score: f64,
    confidence: f64,
    fair_return: Option<f64>,
    poly_up_prob: Option<f64>,
    next_eval_ms: i64,
    updated_ms: i64,
    rationale: String,
}

impl IndicatorPanel {
    fn bootstrap(horizon: &str, next_eval_ms: i64, now_ms: i64, reason: &str) -> Self {
        Self {
            horizon: horizon.to_string(),
            decision: "UP".to_string(),
            score: 0.0001,
            confidence: 0.01,
            fair_return: None,
            poly_up_prob: None,
            next_eval_ms,
            updated_ms: now_ms,
            rationale: reason.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct ProPanelSnapshot {
    last_update_ms: i64,
    feed: BtcFeeds,
    poly_5m: PolyMarketPanel,
    poly_15m: PolyMarketPanel,
    indicator_5m: IndicatorPanel,
    indicator_15m: IndicatorPanel,
    errors: Vec<String>,
}

impl ProPanelSnapshot {
    fn new() -> Self {
        let now = unix_time_ms();
        let next_eval = align_next_eval(now, 300);
        Self {
            last_update_ms: now,
            feed: BtcFeeds::default(),
            poly_5m: PolyMarketPanel::empty("5m"),
            poly_15m: PolyMarketPanel::empty("15m"),
            indicator_5m: IndicatorPanel::bootstrap("5m", next_eval, now, "bootstrap default"),
            indicator_15m: IndicatorPanel::bootstrap("15m", next_eval, now, "bootstrap default"),
            errors: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct AppState {
    snapshot: std::sync::Arc<RwLock<DashboardSnapshot>>,
    panel: std::sync::Arc<RwLock<ProPanelSnapshot>>,
    agent: AgentController,
    activity_url: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum AgentMode {
    Paper,
    Live,
}

#[derive(Debug, Clone, Serialize)]
struct AgentLastExit {
    mode: AgentMode,
    config: String,
    ended_ms: i64,
    code: Option<i32>,
    success: bool,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
struct AgentSecretsStatus {
    has_private_key: bool,
    has_api_key: bool,
    has_api_secret: bool,
    has_api_passphrase: bool,
    has_wallet_address: bool,
    has_funder_address: bool,
    signature_kind: Option<String>,
    updated_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct AgentStatus {
    running: bool,
    mode: Option<AgentMode>,
    pid: Option<u32>,
    config: Option<String>,
    started_ms: Option<i64>,
    uptime_sec: Option<f64>,
    command: Option<Vec<String>>,
    last_exit: Option<AgentLastExit>,
    default_config: String,
    secrets: AgentSecretsStatus,
}

#[derive(Debug, Deserialize)]
struct AgentStartRequest {
    mode: Option<AgentMode>,
    config: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgentSecretsRequest {
    private_key: Option<String>,
    api_key: Option<String>,
    api_secret: Option<String>,
    api_passphrase: Option<String>,
    wallet_address: Option<String>,
    funder_address: Option<String>,
    signature_kind: Option<String>,
    clear: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
}

#[derive(Debug, Clone, Default)]
struct AgentSecrets {
    private_key: Option<String>,
    api_key: Option<String>,
    api_secret: Option<String>,
    api_passphrase: Option<String>,
    wallet_address: Option<String>,
    funder_address: Option<String>,
    signature_kind: Option<String>,
    updated_ms: Option<i64>,
}

impl AgentSecrets {
    fn has_private_key(&self) -> bool {
        self.private_key
            .as_deref()
            .map(str::trim)
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }

    fn status(&self) -> AgentSecretsStatus {
        AgentSecretsStatus {
            has_private_key: self
                .private_key
                .as_deref()
                .map(str::trim)
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            has_api_key: self
                .api_key
                .as_deref()
                .map(str::trim)
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            has_api_secret: self
                .api_secret
                .as_deref()
                .map(str::trim)
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            has_api_passphrase: self
                .api_passphrase
                .as_deref()
                .map(str::trim)
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            has_wallet_address: self
                .wallet_address
                .as_deref()
                .map(str::trim)
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            has_funder_address: self
                .funder_address
                .as_deref()
                .map(str::trim)
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            signature_kind: self.signature_kind.clone(),
            updated_ms: self.updated_ms,
        }
    }
}

struct RunningAgent {
    child: Child,
    mode: AgentMode,
    config: String,
    started_ms: i64,
    command: Vec<String>,
}

#[derive(Default)]
struct AgentRuntime {
    running: Option<RunningAgent>,
    last_exit: Option<AgentLastExit>,
}

#[derive(Clone)]
struct AgentController {
    inner: std::sync::Arc<AgentControllerInner>,
}

struct AgentControllerInner {
    runtime: Mutex<AgentRuntime>,
    secrets: Mutex<AgentSecrets>,
    workspace: PathBuf,
    default_config: String,
    agent_binary: Option<PathBuf>,
}

impl AgentController {
    fn new(workspace: PathBuf, default_config: String, agent_binary: Option<PathBuf>) -> Self {
        Self {
            inner: std::sync::Arc::new(AgentControllerInner {
                runtime: Mutex::new(AgentRuntime::default()),
                secrets: Mutex::new(AgentSecrets::default()),
                workspace,
                default_config,
                agent_binary,
            }),
        }
    }

    async fn set_secrets(&self, req: AgentSecretsRequest) -> anyhow::Result<AgentSecretsStatus> {
        let mut secrets = self.inner.secrets.lock().await;
        if req.clear.unwrap_or(false) {
            *secrets = AgentSecrets::default();
            return Ok(secrets.status());
        }

        if let Some(sig) = req.signature_kind {
            let normalized = sig.trim().to_ascii_lowercase();
            let valid = ["eoa", "poly_proxy", "gnosis_safe"];
            if valid.contains(&normalized.as_str()) {
                secrets.signature_kind = Some(normalized);
            } else {
                bail!("signature_kind must be one of: eoa, poly_proxy, gnosis_safe (got {sig})");
            }
        }

        update_secret_field(&mut secrets.private_key, req.private_key);
        update_secret_field(&mut secrets.api_key, req.api_key);
        update_secret_field(&mut secrets.api_secret, req.api_secret);
        update_secret_field(&mut secrets.api_passphrase, req.api_passphrase);
        update_secret_field(&mut secrets.wallet_address, req.wallet_address);
        update_secret_field(&mut secrets.funder_address, req.funder_address);
        secrets.updated_ms = Some(unix_time_ms());

        Ok(secrets.status())
    }

    async fn secrets_status(&self) -> AgentSecretsStatus {
        let secrets = self.inner.secrets.lock().await;
        secrets.status()
    }

    async fn start(&self, req: AgentStartRequest) -> anyhow::Result<AgentStatus> {
        let mode = req.mode.unwrap_or(AgentMode::Paper);
        let config = req
            .config
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| self.inner.default_config.clone());
        let config_path = resolve_config_path(&self.inner.workspace, &config);
        let cfg = AppConfig::load(&config_path).with_context(|| {
            format!(
                "failed to load config for agent start: {}",
                config_path.display()
            )
        })?;

        let secrets = { self.inner.secrets.lock().await.clone() };

        if mode == AgentMode::Live && auth_private_key_missing(&cfg) && !secrets.has_private_key() {
            bail!(
                "live mode requires a private key (set dashboard secret or POLY_HFT__AUTH__PRIVATE_KEY / auth.private_key)"
            );
        }

        {
            let mut runtime = self.inner.runtime.lock().await;
            self.reap_locked(&mut runtime);
            if runtime.running.is_some() {
                bail!("agent is already running");
            }
        }

        let (mut cmd, command_repr) = self.build_command(mode, &config, &secrets)?;
        let child = cmd.spawn().with_context(|| {
            format!("failed to spawn agent command: {}", command_repr.join(" "))
        })?;

        let pid = child.id();
        let started_ms = unix_time_ms();
        {
            let mut runtime = self.inner.runtime.lock().await;
            runtime.running = Some(RunningAgent {
                child,
                mode,
                config: config.clone(),
                started_ms,
                command: command_repr.clone(),
            });
            runtime.last_exit = None;
        }

        info!(
            mode = ?mode,
            pid = ?pid,
            config = %config,
            command = %command_repr.join(" "),
            "dashboard started agent process"
        );

        Ok(self.status().await)
    }

    async fn stop(&self) -> anyhow::Result<AgentStatus> {
        let running = {
            let mut runtime = self.inner.runtime.lock().await;
            self.reap_locked(&mut runtime);
            runtime.running.take()
        };

        let Some(mut running) = running else {
            return Ok(self.status().await);
        };

        let _ = running.child.start_kill();
        let wait_result = running.child.wait().await;
        let ended_ms = unix_time_ms();
        let (code, success, detail) = match wait_result {
            Ok(status) => (
                status.code(),
                status.success(),
                format!("stopped ({status})"),
            ),
            Err(err) => (None, false, format!("stop wait failed: {err}")),
        };

        {
            let mut runtime = self.inner.runtime.lock().await;
            runtime.last_exit = Some(AgentLastExit {
                mode: running.mode,
                config: running.config,
                ended_ms,
                code,
                success,
                detail,
            });
        }

        info!("dashboard stopped agent process");
        Ok(self.status().await)
    }

    async fn status(&self) -> AgentStatus {
        let mut runtime = self.inner.runtime.lock().await;
        self.reap_locked(&mut runtime);

        let secrets = {
            let secrets = self.inner.secrets.lock().await;
            secrets.status()
        };

        if let Some(running) = runtime.running.as_ref() {
            let now_ms = unix_time_ms();
            AgentStatus {
                running: true,
                mode: Some(running.mode),
                pid: running.child.id(),
                config: Some(running.config.clone()),
                started_ms: Some(running.started_ms),
                uptime_sec: Some((now_ms.saturating_sub(running.started_ms) as f64) / 1_000.0),
                command: Some(running.command.clone()),
                last_exit: runtime.last_exit.clone(),
                default_config: self.inner.default_config.clone(),
                secrets,
            }
        } else {
            AgentStatus {
                running: false,
                mode: None,
                pid: None,
                config: None,
                started_ms: None,
                uptime_sec: None,
                command: None,
                last_exit: runtime.last_exit.clone(),
                default_config: self.inner.default_config.clone(),
                secrets,
            }
        }
    }

    fn reap_locked(&self, runtime: &mut AgentRuntime) {
        let Some(running) = runtime.running.as_mut() else {
            return;
        };

        match running.child.try_wait() {
            Ok(Some(status)) => {
                runtime.last_exit = Some(AgentLastExit {
                    mode: running.mode,
                    config: running.config.clone(),
                    ended_ms: unix_time_ms(),
                    code: status.code(),
                    success: status.success(),
                    detail: status.to_string(),
                });
                runtime.running = None;
            }
            Ok(None) => {}
            Err(err) => {
                runtime.last_exit = Some(AgentLastExit {
                    mode: running.mode,
                    config: running.config.clone(),
                    ended_ms: unix_time_ms(),
                    code: None,
                    success: false,
                    detail: format!("try_wait failed: {err}"),
                });
                runtime.running = None;
            }
        }
    }

    fn build_command(
        &self,
        mode: AgentMode,
        config: &str,
        secrets: &AgentSecrets,
    ) -> anyhow::Result<(Command, Vec<String>)> {
        let workspace = &self.inner.workspace;
        let mut command_repr = Vec::<String>::new();
        let mut cmd = if let Some(bin) = self.resolve_agent_binary() {
            let mut cmd = Command::new(&bin);
            command_repr.push(bin.display().to_string());
            cmd.arg("--config").arg(config);
            command_repr.push("--config".to_string());
            command_repr.push(config.to_string());
            if mode == AgentMode::Paper {
                cmd.arg("--dry-run");
                command_repr.push("--dry-run".to_string());
            }
            cmd
        } else {
            let mut cmd = Command::new("cargo");
            cmd.args([
                "run",
                "--release",
                "-p",
                "polyhft-live-agent",
                "--",
                "--config",
                config,
            ]);
            command_repr.extend_from_slice(&[
                "cargo".to_string(),
                "run".to_string(),
                "--release".to_string(),
                "-p".to_string(),
                "polyhft-live-agent".to_string(),
                "--".to_string(),
                "--config".to_string(),
                config.to_string(),
            ]);
            if mode == AgentMode::Paper {
                cmd.arg("--dry-run");
                command_repr.push("--dry-run".to_string());
            }
            cmd
        };

        if let Some(v) = secrets.private_key.as_deref() {
            cmd.env("POLY_HFT__AUTH__PRIVATE_KEY", v);
        }
        if let Some(v) = secrets.api_key.as_deref() {
            cmd.env("POLY_HFT__AUTH__API_KEY", v);
        }
        if let Some(v) = secrets.api_secret.as_deref() {
            cmd.env("POLY_HFT__AUTH__API_SECRET", v);
        }
        if let Some(v) = secrets.api_passphrase.as_deref() {
            cmd.env("POLY_HFT__AUTH__API_PASSPHRASE", v);
        }
        if let Some(v) = secrets.wallet_address.as_deref() {
            cmd.env("POLY_HFT__AUTH__WALLET_ADDRESS", v);
        }
        if let Some(v) = secrets.funder_address.as_deref() {
            cmd.env("POLY_HFT__AUTH__FUNDER_ADDRESS", v);
        }
        if let Some(v) = secrets.signature_kind.as_deref() {
            cmd.env("POLY_HFT__AUTH__SIGNATURE_KIND", v);
        }

        let logs_dir = workspace.join("data/live");
        std::fs::create_dir_all(&logs_dir)?;
        let out_path = logs_dir.join("dashboard-agent.stdout.log");
        let err_path = logs_dir.join("dashboard-agent.stderr.log");

        if let Ok(out_file) = StdOpenOptions::new()
            .create(true)
            .append(true)
            .open(&out_path)
        {
            cmd.stdout(Stdio::from(out_file));
        } else {
            cmd.stdout(Stdio::null());
        }
        if let Ok(err_file) = StdOpenOptions::new()
            .create(true)
            .append(true)
            .open(&err_path)
        {
            cmd.stderr(Stdio::from(err_file));
        } else {
            cmd.stderr(Stdio::null());
        }

        cmd.stdin(Stdio::null()).current_dir(workspace);

        Ok((cmd, command_repr))
    }

    fn resolve_agent_binary(&self) -> Option<PathBuf> {
        if let Some(bin) = self.inner.agent_binary.clone()
            && bin.exists()
        {
            return Some(bin);
        }
        None
    }
}

#[derive(Debug, Clone)]
struct PriceTick {
    value: f64,
    source_ts_ms: i64,
    seen_ts_ms: i64,
}

#[derive(Debug, Clone, Default)]
struct RtdsRuntime {
    connected: bool,
    last_msg_ms: Option<i64>,
    last_error: Option<String>,
    binance: Option<PriceTick>,
    chainlink: Option<PriceTick>,
}

#[derive(Debug, Clone)]
struct PolyMarketTarget {
    horizon: u32,
    slug: String,
    question: String,
    condition_id: String,
    up_token_id: String,
    down_token_id: String,
    start_ts_ms: Option<i64>,
    end_ts_ms: Option<i64>,
}

#[derive(Debug, Clone, Default)]
struct PolyMarketCache {
    last_refresh_ms: i64,
    market_5m: Option<PolyMarketTarget>,
    market_15m: Option<PolyMarketTarget>,
}

#[derive(Debug, Clone)]
struct StartLevelSnapshot {
    condition_id: String,
    start_ts_ms: Option<i64>,
    level: f64,
    captured_ts_ms: i64,
}

struct PanelRuntime {
    fair_history: VecDeque<PriceTick>,
    next_eval_5m: i64,
    next_eval_15m: i64,
    indicator_5m: IndicatorPanel,
    indicator_15m: IndicatorPanel,
    markets: PolyMarketCache,
    start_level_5m: Option<StartLevelSnapshot>,
    start_level_15m: Option<StartLevelSnapshot>,
}

impl PanelRuntime {
    fn new(now_ms: i64, eval_secs: u64) -> Self {
        let next_eval = align_next_eval(now_ms, eval_secs);
        Self {
            fair_history: VecDeque::with_capacity(12_000),
            next_eval_5m: now_ms,
            next_eval_15m: now_ms,
            indicator_5m: IndicatorPanel::bootstrap("5m", next_eval, now_ms, "bootstrap default"),
            indicator_15m: IndicatorPanel::bootstrap("15m", next_eval, now_ms, "bootstrap default"),
            markets: PolyMarketCache::default(),
            start_level_5m: None,
            start_level_15m: None,
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_tracing(&args.log_level, args.json_logs)?;

    let client = Client::builder()
        .timeout(Duration::from_millis(args.request_timeout_ms))
        .build()?;

    let workspace = PathBuf::from(&args.workspace);
    let agent = AgentController::new(
        workspace,
        args.agent_default_config.clone(),
        args.agent_binary.clone().map(PathBuf::from),
    );

    let panel = std::sync::Arc::new(RwLock::new(ProPanelSnapshot::new()));
    let state = AppState {
        snapshot: std::sync::Arc::new(RwLock::new(DashboardSnapshot::new(args.target.clone()))),
        panel: panel.clone(),
        agent,
        activity_url: args.activity_url.clone(),
    };

    let scraper_state = state.clone();
    let scraper_args = args.clone();
    let scrape_client = client.clone();
    tokio::spawn(async move {
        run_scraper(scraper_state, scrape_client, scraper_args).await;
    });

    let rtds_state = std::sync::Arc::new(RwLock::new(RtdsRuntime::default()));
    let rtds_args = args.clone();
    let rtds_state_clone = rtds_state.clone();
    tokio::spawn(async move {
        run_rtds_stream(rtds_args, rtds_state_clone).await;
    });

    let panel_args = args.clone();
    let panel_state = state.clone();
    let panel_client = client.clone();
    tokio::spawn(async move {
        run_panel_collector(panel_state, panel_client, panel_args, rtds_state).await;
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/api/config", get(config_handler))
        .route("/api/snapshot", get(snapshot))
        .route("/api/panel/snapshot", get(panel_snapshot))
        .route("/api/health", get(health))
        .route("/api/agent/status", get(agent_status))
        .route("/api/agent/start", post(agent_start))
        .route("/api/agent/stop", post(agent_stop))
        .route("/api/agent/secrets", post(agent_set_secrets))
        .route("/api/agent/secrets/status", get(agent_secrets_status))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&args.bind).await?;
    info!(bind = %args.bind, target = %args.target, "dashboard listening");
    axum::serve(listener, app).await?;

    Ok(())
}

async fn run_panel_collector(
    state: AppState,
    client: Client,
    args: Args,
    rtds_state: std::sync::Arc<RwLock<RtdsRuntime>>,
) {
    let mut ticker = tokio::time::interval(Duration::from_millis(args.panel_refresh_ms.max(250)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut runtime = PanelRuntime::new(unix_time_ms(), args.indicator_interval_secs.max(60));

    loop {
        ticker.tick().await;
        let now_ms = unix_time_ms();
        let mut errors = Vec::new();

        if now_ms.saturating_sub(runtime.markets.last_refresh_ms)
            >= i64::try_from(args.market_refresh_secs.saturating_mul(1_000)).unwrap_or(i64::MAX)
        {
            match discover_poly_targets(&client, &args).await {
                Ok((m5, m15)) => {
                    runtime.markets.market_5m = m5;
                    runtime.markets.market_15m = m15;
                    runtime.markets.last_refresh_ms = now_ms;
                }
                Err(err) => {
                    errors.push(format!("market discovery failed: {err}"));
                }
            }
        }

        let rtds = rtds_state.read().await.clone();
        if let Some(err) = rtds.last_error {
            errors.push(format!("rtds: {err}"));
        }

        let coinbase_fut = fetch_coinbase_btc(&client);
        let pyth_fut = fetch_pyth_btc(&client, &args.btc_pyth_feed_id);

        let (coinbase_res, pyth_res) = tokio::join!(coinbase_fut, pyth_fut);

        let mut binance_tick = rtds.binance.clone();
        let binance_stale = binance_tick
            .as_ref()
            .map(|x| now_ms.saturating_sub(x.seen_ts_ms) > 5_000)
            .unwrap_or(true);
        if binance_stale {
            match fetch_binance_btc(&client).await {
                Ok(t) => binance_tick = Some(t),
                Err(err) => errors.push(format!("binance fetch failed: {err}")),
            }
        }

        let coinbase_tick = match coinbase_res {
            Ok(v) => Some(v),
            Err(err) => {
                errors.push(format!("coinbase fetch failed: {err}"));
                None
            }
        };
        let pyth_tick = match pyth_res {
            Ok(v) => Some(v),
            Err(err) => {
                errors.push(format!("pyth fetch failed: {err}"));
                None
            }
        };

        let chainlink_tick = rtds.chainlink.clone();

        let mut feed = BtcFeeds {
            binance: point_from_tick(binance_tick.as_ref(), now_ms, 6_000),
            coinbase: point_from_tick(coinbase_tick.as_ref(), now_ms, 6_000),
            pyth: point_from_tick(pyth_tick.as_ref(), now_ms, 6_000),
            chainlink: point_from_tick(chainlink_tick.as_ref(), now_ms, 6_000),
            blended: PriceFeedPoint::default(),
        };

        let mut blended_values = Vec::new();
        for p in [&feed.binance, &feed.coinbase, &feed.pyth, &feed.chainlink] {
            if let Some(v) = p.value {
                blended_values.push(v);
            }
        }

        if let Some(blended) = median(&mut blended_values) {
            feed.blended = PriceFeedPoint {
                value: Some(blended),
                source_ts_ms: Some(now_ms),
                observed_ts_ms: Some(now_ms),
                stale: false,
            };
            runtime.fair_history.push_back(PriceTick {
                value: blended,
                source_ts_ms: now_ms,
                seen_ts_ms: now_ms,
            });
            trim_history(&mut runtime.fair_history, now_ms, 4 * 60 * 60 * 1_000);
        }

        let mut poly_5m = match runtime.markets.market_5m.clone() {
            Some(target) => match fetch_poly_market_panel(&client, &args, &target).await {
                Ok(panel) => panel,
                Err(err) => {
                    errors.push(format!("poly 5m fetch failed: {err}"));
                    PolyMarketPanel {
                        horizon: "5m".to_string(),
                        market_found: true,
                        slug: Some(target.slug),
                        condition_id: Some(target.condition_id),
                        question: Some(target.question),
                        start_ts_ms: target.start_ts_ms,
                        start_level: None,
                        start_level_captured_ms: None,
                        end_ts_ms: target.end_ts_ms,
                        up_mid: None,
                        down_mid: None,
                        sum_mid: None,
                        updated_ms: None,
                    }
                }
            },
            None => PolyMarketPanel::empty("5m"),
        };

        let mut poly_15m = match runtime.markets.market_15m.clone() {
            Some(target) => match fetch_poly_market_panel(&client, &args, &target).await {
                Ok(panel) => panel,
                Err(err) => {
                    errors.push(format!("poly 15m fetch failed: {err}"));
                    PolyMarketPanel {
                        horizon: "15m".to_string(),
                        market_found: true,
                        slug: Some(target.slug),
                        condition_id: Some(target.condition_id),
                        question: Some(target.question),
                        start_ts_ms: target.start_ts_ms,
                        start_level: None,
                        start_level_captured_ms: None,
                        end_ts_ms: target.end_ts_ms,
                        up_mid: None,
                        down_mid: None,
                        sum_mid: None,
                        updated_ms: None,
                    }
                }
            },
            None => PolyMarketPanel::empty("15m"),
        };

        let start_ref_price = feed
            .chainlink
            .value
            .or(feed.pyth.value)
            .or(feed.blended.value);

        refresh_start_level_cache(
            &mut runtime.start_level_5m,
            runtime.markets.market_5m.as_ref(),
            start_ref_price,
            &runtime.fair_history,
            now_ms,
        );
        refresh_start_level_cache(
            &mut runtime.start_level_15m,
            runtime.markets.market_15m.as_ref(),
            start_ref_price,
            &runtime.fair_history,
            now_ms,
        );

        apply_start_level_to_panel(&mut poly_5m, runtime.start_level_5m.as_ref());
        apply_start_level_to_panel(&mut poly_15m, runtime.start_level_15m.as_ref());

        if now_ms >= runtime.next_eval_5m {
            let last_decision_5m = runtime.indicator_5m.decision.clone();
            runtime.indicator_5m = evaluate_indicator(
                "5m",
                5,
                now_ms,
                args.indicator_interval_secs,
                &runtime.fair_history,
                &feed,
                &poly_5m,
                runtime.start_level_5m.as_ref().map(|x| x.level),
                Some(last_decision_5m.as_str()),
            );
            runtime.next_eval_5m = if indicator_needs_fast_retry(&runtime.indicator_5m) {
                now_ms.saturating_add(30_000)
            } else {
                align_next_eval(now_ms, args.indicator_interval_secs)
            };
            runtime.indicator_5m.next_eval_ms = runtime.next_eval_5m;
        }
        if now_ms >= runtime.next_eval_15m {
            let last_decision_15m = runtime.indicator_15m.decision.clone();
            runtime.indicator_15m = evaluate_indicator(
                "15m",
                15,
                now_ms,
                args.indicator_interval_secs,
                &runtime.fair_history,
                &feed,
                &poly_15m,
                runtime.start_level_15m.as_ref().map(|x| x.level),
                Some(last_decision_15m.as_str()),
            );
            runtime.next_eval_15m = if indicator_needs_fast_retry(&runtime.indicator_15m) {
                now_ms.saturating_add(45_000)
            } else {
                align_next_eval(now_ms, args.indicator_interval_secs)
            };
            runtime.indicator_15m.next_eval_ms = runtime.next_eval_15m;
        }

        {
            let mut panel = state.panel.write().await;
            *panel = ProPanelSnapshot {
                last_update_ms: now_ms,
                feed,
                poly_5m,
                poly_15m,
                indicator_5m: runtime.indicator_5m.clone(),
                indicator_15m: runtime.indicator_15m.clone(),
                errors,
            };
        }
    }
}

async fn run_rtds_stream(args: Args, state: std::sync::Arc<RwLock<RtdsRuntime>>) {
    loop {
        match connect_async(&args.rtds_url).await {
            Ok((ws, _)) => {
                {
                    let mut s = state.write().await;
                    s.connected = true;
                    s.last_error = None;
                }

                let (mut writer, mut reader) = ws.split();
                let sub = serde_json::json!({
                    "action": "subscribe",
                    "subscriptions": [
                        {
                            "topic": "crypto_prices",
                            "type": "*",
                            "filters": "{\"symbol\":\"btcusdt\"}"
                        },
                        {
                            "topic": "crypto_prices_chainlink",
                            "type": "*",
                            "filters": "{\"symbol\":\"btc/usd\"}"
                        }
                    ]
                })
                .to_string();

                if let Err(err) = writer.send(Message::Text(sub.into())).await {
                    let mut s = state.write().await;
                    s.connected = false;
                    s.last_error = Some(format!("rtds subscribe failed: {err}"));
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }

                let mut ping = tokio::time::interval(Duration::from_secs(5));
                ping.set_missed_tick_behavior(MissedTickBehavior::Skip);

                let disconnected_error: Option<String> = loop {
                    tokio::select! {
                        _ = ping.tick() => {
                            if let Err(err) = writer.send(Message::Text("PING".to_string().into())).await {
                                break Some(format!("rtds ping failed: {err}"));
                            }
                        }
                        msg = reader.next() => {
                            match msg {
                                Some(Ok(Message::Text(text))) => {
                                    if let Some((topic, value, source_ts)) = parse_rtds_price_message(&text) {
                                        let now_ms = unix_time_ms();
                                        let tick = PriceTick { value, source_ts_ms: source_ts.unwrap_or(now_ms), seen_ts_ms: now_ms };
                                        let mut s = state.write().await;
                                        s.last_msg_ms = Some(now_ms);
                                        match topic.as_str() {
                                            "crypto_prices" => s.binance = Some(tick),
                                            "crypto_prices_chainlink" => s.chainlink = Some(tick),
                                            _ => {}
                                        }
                                    }
                                }
                                Some(Ok(Message::Ping(payload))) => {
                                    if writer.send(Message::Pong(payload)).await.is_err() {
                                        break Some("rtds pong failed".to_string());
                                    }
                                }
                                Some(Ok(Message::Close(frame))) => {
                                    break Some(format!("rtds closed: {frame:?}"));
                                }
                                Some(Ok(_)) => {}
                                Some(Err(err)) => {
                                    break Some(format!("rtds read failed: {err}"));
                                }
                                None => {
                                    break Some("rtds stream ended".to_string());
                                }
                            }
                        }
                    }
                };

                let mut s = state.write().await;
                s.connected = false;
                s.last_error = disconnected_error;
            }
            Err(err) => {
                let mut s = state.write().await;
                s.connected = false;
                s.last_error = Some(format!("rtds connect failed: {err}"));
            }
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn parse_rtds_price_message(text: &str) -> Option<(String, f64, Option<i64>)> {
    let v = serde_json::from_str::<Value>(text).ok()?;
    let topic = v.get("topic")?.as_str()?.to_string();
    let payload = v.get("payload")?;
    let symbol = payload
        .get("symbol")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();

    match topic.as_str() {
        "crypto_prices" if symbol == "btcusdt" => {
            let value = value_as_f64(payload.get("value")?)?;
            let ts = payload
                .get("timestamp")
                .and_then(value_as_f64)
                .map(|x| x as i64);
            Some((topic, value, ts))
        }
        "crypto_prices_chainlink" if symbol == "btc/usd" => {
            let value = value_as_f64(payload.get("value")?)?;
            let ts = payload
                .get("timestamp")
                .and_then(value_as_f64)
                .map(|x| x as i64);
            Some((topic, value, ts))
        }
        _ => None,
    }
}

async fn discover_poly_targets(
    client: &Client,
    args: &Args,
) -> anyhow::Result<(Option<PolyMarketTarget>, Option<PolyMarketTarget>)> {
    let now_ms = unix_time_ms();
    let mut m5_candidates = Vec::new();
    let mut m15_candidates = Vec::new();

    if let Ok(rows) = fetch_gamma_active_rows(client, &args.gamma_url).await {
        for row in rows {
            if let Some(target) = parse_target_from_gamma_row(&row) {
                if target.horizon == 5 {
                    m5_candidates.push(target);
                } else if target.horizon == 15 {
                    m15_candidates.push(target);
                }
            }
        }
    }

    if m5_candidates.is_empty()
        && let Ok(extra) = discover_from_predictions_hub(client, args, 5).await
    {
        m5_candidates.extend(extra);
    }
    if m15_candidates.is_empty()
        && let Ok(extra) = discover_from_predictions_hub(client, args, 15).await
    {
        m15_candidates.extend(extra);
    }

    let best_5m = pick_best_market(m5_candidates, now_ms);
    let best_15m = pick_best_market(m15_candidates, now_ms);
    Ok((best_5m, best_15m))
}

async fn fetch_gamma_active_rows(client: &Client, gamma_url: &str) -> anyhow::Result<Vec<Value>> {
    let url = format!(
        "{}/markets?active=true&closed=false&limit=1000&offset=0",
        gamma_url.trim_end_matches('/')
    );

    client
        .get(&url)
        .header("User-Agent", "polyhft-dashboard/0.1")
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<Value>>()
        .await
        .context("failed to parse gamma markets")
}

async fn discover_from_predictions_hub(
    client: &Client,
    args: &Args,
    horizon: u32,
) -> anyhow::Result<Vec<PolyMarketTarget>> {
    let slugs = fetch_prediction_hub_slugs(client, horizon).await?;
    if slugs.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for slug in slugs.into_iter().take(16) {
        if let Some(target) = fetch_gamma_target_by_slug(client, &args.gamma_url, &slug).await? {
            out.push(target);
        }
    }

    Ok(out)
}

async fn fetch_prediction_hub_slugs(client: &Client, horizon: u32) -> anyhow::Result<Vec<String>> {
    let page = format!("https://polymarket.com/predictions/{horizon}M");
    let html = client
        .get(&page)
        .header("User-Agent", "polyhft-dashboard/0.1")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await
        .context("failed to read predictions hub html")?;

    let pattern = format!(r"\bbtc-updown-{horizon}m-\d+\b");
    let re = Regex::new(&pattern).context("invalid slug regex")?;

    let mut unique = HashSet::new();
    for capture in re.captures_iter(&html) {
        if let Some(m) = capture.get(0) {
            unique.insert(m.as_str().to_string());
        }
    }

    let now_sec = unix_time_ms().div_euclid(1_000);
    let mut slugs: Vec<String> = unique.into_iter().collect();
    slugs.sort_by_key(|slug| {
        let end = parse_slug_tail_unix_seconds(slug).unwrap_or(i64::MAX);
        if end >= now_sec {
            (0_u8, end)
        } else {
            (1_u8, -end)
        }
    });
    Ok(slugs)
}

async fn fetch_gamma_target_by_slug(
    client: &Client,
    gamma_url: &str,
    slug: &str,
) -> anyhow::Result<Option<PolyMarketTarget>> {
    let url = format!("{}/markets?slug={}", gamma_url.trim_end_matches('/'), slug);

    let response = client
        .get(&url)
        .header("User-Agent", "polyhft-dashboard/0.1")
        .send()
        .await?;
    if !response.status().is_success() {
        return Ok(None);
    }

    let rows = response
        .json::<Vec<Value>>()
        .await
        .context("failed to decode gamma slug response")?;
    for row in rows {
        if let Some(target) = parse_target_from_gamma_row(&row) {
            return Ok(Some(target));
        }
    }
    Ok(None)
}

fn parse_target_from_gamma_row(row: &Value) -> Option<PolyMarketTarget> {
    let slug = row
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let question = row
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if slug.is_empty() || !is_btc_updown(&slug, &question) {
        return None;
    }

    let horizon = classify_horizon(&slug, &question)?;

    let condition_id = row
        .get("conditionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if condition_id.is_empty() {
        return None;
    }

    let token_ids = parse_string_list(row.get("clobTokenIds"));
    let outcomes = parse_string_list(row.get("outcomes"));
    if token_ids.len() < 2 || outcomes.len() < 2 {
        return None;
    }

    let (up_token_id, down_token_id) = map_up_down_tokens(&token_ids, &outcomes)?;
    let start_ts_ms = parse_market_start_ms(
        row.get("eventStartTime"),
        row.get("startTime"),
        row.get("startDate"),
    );
    let end_ts_ms = parse_market_end_ms(&slug, row.get("endDate"));

    Some(PolyMarketTarget {
        horizon,
        slug,
        question,
        condition_id,
        up_token_id,
        down_token_id,
        start_ts_ms,
        end_ts_ms,
    })
}

async fn fetch_poly_market_panel(
    client: &Client,
    args: &Args,
    target: &PolyMarketTarget,
) -> anyhow::Result<PolyMarketPanel> {
    let (up_res, down_res) = tokio::join!(
        fetch_midpoint(client, &args.clob_url, &target.up_token_id),
        fetch_midpoint(client, &args.clob_url, &target.down_token_id)
    );

    let up_mid = up_res.ok();
    let down_mid = down_res.ok();

    let sum_mid = match (up_mid, down_mid) {
        (Some(u), Some(d)) => Some(u + d),
        _ => None,
    };

    Ok(PolyMarketPanel {
        horizon: format!("{}m", target.horizon),
        market_found: true,
        slug: Some(target.slug.clone()),
        condition_id: Some(target.condition_id.clone()),
        question: Some(target.question.clone()),
        start_ts_ms: target.start_ts_ms,
        start_level: None,
        start_level_captured_ms: None,
        end_ts_ms: target.end_ts_ms,
        up_mid,
        down_mid,
        sum_mid,
        updated_ms: Some(unix_time_ms()),
    })
}

async fn fetch_midpoint(client: &Client, clob_url: &str, token_id: &str) -> anyhow::Result<f64> {
    let url = format!(
        "{}/midpoint?token_id={token_id}",
        clob_url.trim_end_matches('/')
    );
    let v = client
        .get(url)
        .header("User-Agent", "polyhft-dashboard/0.1")
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await
        .context("failed to parse midpoint payload")?;

    let mid = v
        .get("mid")
        .and_then(value_as_f64)
        .context("midpoint response missing mid")?;
    Ok(mid)
}

async fn fetch_coinbase_btc(client: &Client) -> anyhow::Result<PriceTick> {
    let v = client
        .get("https://api.coinbase.com/v2/prices/BTC-USD/spot")
        .header("User-Agent", "polyhft-dashboard/0.1")
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await
        .context("failed to parse coinbase payload")?;

    let amount = v
        .get("data")
        .and_then(|x| x.get("amount"))
        .and_then(value_as_f64)
        .context("coinbase payload missing amount")?;

    let now_ms = unix_time_ms();
    Ok(PriceTick {
        value: amount,
        source_ts_ms: now_ms,
        seen_ts_ms: now_ms,
    })
}

async fn fetch_binance_btc(client: &Client) -> anyhow::Result<PriceTick> {
    let v = client
        .get("https://api.binance.com/api/v3/ticker/price?symbol=BTCUSDT")
        .header("User-Agent", "polyhft-dashboard/0.1")
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await
        .context("failed to parse binance payload")?;

    let px = v
        .get("price")
        .and_then(value_as_f64)
        .context("binance payload missing price")?;

    let now_ms = unix_time_ms();
    Ok(PriceTick {
        value: px,
        source_ts_ms: now_ms,
        seen_ts_ms: now_ms,
    })
}

async fn fetch_pyth_btc(client: &Client, feed_id: &str) -> anyhow::Result<PriceTick> {
    let url =
        format!("https://hermes.pyth.network/v2/updates/price/latest?ids[]={feed_id}&parsed=true");
    let v = client
        .get(url)
        .header("User-Agent", "polyhft-dashboard/0.1")
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await
        .context("failed to parse pyth payload")?;

    let price = v
        .get("parsed")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|row| row.get("price"))
        .context("pyth payload missing parsed[0].price")?;

    let raw_price = price
        .get("price")
        .and_then(value_as_f64)
        .context("pyth payload missing price.price")?;
    let expo = price
        .get("expo")
        .and_then(value_as_f64)
        .map(|x| x as i32)
        .context("pyth payload missing price.expo")?;
    let publish_time_ms = price
        .get("publish_time")
        .and_then(value_as_f64)
        .map(|x| (x as i64).saturating_mul(1_000))
        .unwrap_or_else(unix_time_ms);

    let scaled = raw_price * 10f64.powi(expo);
    Ok(PriceTick {
        value: scaled,
        source_ts_ms: publish_time_ms,
        seen_ts_ms: unix_time_ms(),
    })
}

fn refresh_start_level_cache(
    cache: &mut Option<StartLevelSnapshot>,
    target: Option<&PolyMarketTarget>,
    ref_price: Option<f64>,
    history: &VecDeque<PriceTick>,
    now_ms: i64,
) {
    let Some(target) = target else {
        *cache = None;
        return;
    };

    if cache
        .as_ref()
        .map(|c| c.condition_id.as_str() != target.condition_id.as_str())
        .unwrap_or(false)
    {
        *cache = None;
    }

    if cache.is_some() {
        return;
    }

    let live_px = ref_price.filter(|x| *x > 0.0 && x.is_finite());

    let level = if let Some(start_ts_ms) = target.start_ts_ms {
        if now_ms < start_ts_ms {
            return;
        }

        find_reference_fair(history, start_ts_ms)
            .and_then(|(px, seen_ts_ms)| {
                if px.is_finite() && px > 0.0 && seen_ts_ms >= start_ts_ms.saturating_sub(90_000) {
                    Some(px)
                } else {
                    None
                }
            })
            .or(live_px)
    } else {
        live_px
    };

    let Some(level) = level else {
        return;
    };

    *cache = Some(StartLevelSnapshot {
        condition_id: target.condition_id.clone(),
        start_ts_ms: target.start_ts_ms,
        level,
        captured_ts_ms: now_ms,
    });
}

fn apply_start_level_to_panel(
    panel: &mut PolyMarketPanel,
    start_level: Option<&StartLevelSnapshot>,
) {
    if let Some(level) = start_level {
        panel.start_level = Some(level.level);
        panel.start_level_captured_ms = Some(level.captured_ts_ms);
        if panel.start_ts_ms.is_none() {
            panel.start_ts_ms = level.start_ts_ms;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn evaluate_indicator(
    horizon_label: &str,
    horizon_minutes: u64,
    now_ms: i64,
    eval_secs: u64,
    history: &VecDeque<PriceTick>,
    feed: &BtcFeeds,
    poly: &PolyMarketPanel,
    start_level: Option<f64>,
    last_decision: Option<&str>,
) -> IndicatorPanel {
    let next_eval_ms = align_next_eval(now_ms, eval_secs);
    let basis = chainlink_pyth_basis_from_feed(feed);

    let Some(fair_now) = feed.blended.value else {
        return fallback_indicator(
            horizon_label,
            next_eval_ms,
            now_ms,
            "missing blended price",
            None,
            None,
            basis,
            last_decision,
        );
    };

    let lookback_ms = i64::try_from(horizon_minutes.saturating_mul(60_000)).unwrap_or(i64::MAX);
    let Some((fair_prev, prev_ts_ms)) =
        find_reference_fair(history, now_ms.saturating_sub(lookback_ms))
    else {
        return fallback_indicator(
            horizon_label,
            next_eval_ms,
            now_ms,
            "insufficient history for lookback",
            Some(0.0),
            None,
            basis,
            last_decision,
        );
    };

    if fair_prev <= 0.0 || !fair_prev.is_finite() || !fair_now.is_finite() {
        return fallback_indicator(
            horizon_label,
            next_eval_ms,
            now_ms,
            "invalid fair values",
            None,
            None,
            basis,
            last_decision,
        );
    }

    let fair_return = (fair_now / fair_prev) - 1.0;
    let start_return = start_level
        .filter(|x| *x > 0.0 && x.is_finite())
        .map(|x| (fair_now / x) - 1.0);
    let actual_lookback_ms = now_ms.saturating_sub(prev_ts_ms).max(1);
    let lookback_quality = ((actual_lookback_ms as f64) / (lookback_ms as f64)).clamp(0.0, 1.0);

    let mut values = Vec::new();
    for p in [&feed.binance, &feed.coinbase, &feed.pyth, &feed.chainlink] {
        if let Some(v) = p.value {
            values.push(v);
        }
    }

    let agreement = if values.len() >= 2 {
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        if mean > 0.0 {
            let var = values
                .iter()
                .map(|v| {
                    let d = v - mean;
                    d * d
                })
                .sum::<f64>()
                / values.len() as f64;
            let dispersion = var.sqrt() / mean;
            (1.0 - (dispersion / 0.003).min(1.0)).clamp(0.0, 1.0)
        } else {
            0.0
        }
    } else {
        0.0
    };

    let poly_up_prob = match (poly.up_mid, poly.down_mid) {
        (Some(up), Some(down)) if up > 0.0 && down > 0.0 && (up + down) > 0.001 => {
            Some(up / (up + down))
        }
        _ => None,
    };

    let horizon_scale = ((horizon_minutes as f64) / 5.0).sqrt().max(1.0);
    let directional_return = start_return.unwrap_or(fair_return);
    let momentum_raw = (directional_return / (0.0015 * horizon_scale)).tanh();
    let momentum_component = momentum_raw * lookback_quality.max(0.25);
    let implied_up_from_momentum = (0.5 + 0.35 * momentum_component).clamp(0.05, 0.95);

    let dislocation_component = poly_up_prob
        .map(|p| ((implied_up_from_momentum - p) / 0.08).tanh())
        .unwrap_or(0.0);
    let agreement_component = (2.0 * agreement - 1.0).clamp(-1.0, 1.0);

    let score =
        (0.50 * dislocation_component) + (0.35 * momentum_component) + (0.15 * agreement_component);

    let decision = choose_direction(
        Some(score),
        Some(momentum_component),
        Some(directional_return),
        basis,
        last_decision,
    );

    let confidence =
        (score.abs() * (0.65 + 0.35 * agreement) * lookback_quality.max(0.35)).clamp(0.0, 1.0);
    let rationale = format!(
        "ret={:.5} momentum={:.3} disloc={:.3} agreement={:.3} lq={:.2} lookback={}s fair={:.2}",
        directional_return,
        momentum_component,
        dislocation_component,
        agreement,
        lookback_quality,
        actual_lookback_ms / 1000,
        fair_now
    );

    IndicatorPanel {
        horizon: horizon_label.to_string(),
        decision: decision.to_string(),
        score,
        confidence: confidence.max(0.05),
        fair_return: Some(directional_return),
        poly_up_prob,
        next_eval_ms,
        updated_ms: now_ms,
        rationale,
    }
}

#[allow(clippy::too_many_arguments)]
fn fallback_indicator(
    horizon_label: &str,
    next_eval_ms: i64,
    now_ms: i64,
    reason: &str,
    score_hint: Option<f64>,
    fair_return: Option<f64>,
    basis: Option<f64>,
    last_decision: Option<&str>,
) -> IndicatorPanel {
    let decision = choose_direction(score_hint, None, fair_return, basis, last_decision);
    let signed_score =
        score_hint.unwrap_or_else(|| if decision == "UP" { 0.0001 } else { -0.0001 });
    IndicatorPanel {
        horizon: horizon_label.to_string(),
        decision: decision.to_string(),
        score: signed_score,
        confidence: 0.03,
        fair_return,
        poly_up_prob: None,
        next_eval_ms,
        updated_ms: now_ms,
        rationale: format!("fallback {reason}"),
    }
}

fn choose_direction(
    score: Option<f64>,
    momentum: Option<f64>,
    fair_return: Option<f64>,
    basis: Option<f64>,
    last_decision: Option<&str>,
) -> &'static str {
    if let Some(s) = score {
        if s > 0.0 {
            return "UP";
        }
        if s < 0.0 {
            return "DOWN";
        }
    }
    if let Some(m) = momentum {
        if m > 0.0 {
            return "UP";
        }
        if m < 0.0 {
            return "DOWN";
        }
    }
    if let Some(r) = fair_return {
        if r > 0.0 {
            return "UP";
        }
        if r < 0.0 {
            return "DOWN";
        }
    }
    if let Some(b) = basis {
        if b > 0.0 {
            return "UP";
        }
        if b < 0.0 {
            return "DOWN";
        }
    }
    match last_decision.map(|x| x.trim().to_ascii_uppercase()) {
        Some(ref d) if d == "DOWN" => "DOWN",
        Some(ref d) if d == "UP" => "UP",
        _ => "UP",
    }
}

fn chainlink_pyth_basis_from_feed(feed: &BtcFeeds) -> Option<f64> {
    let cl = feed.chainlink.value?;
    let py = feed.pyth.value?;
    let mid = (cl + py) * 0.5;
    if !mid.is_finite() || mid <= 0.0 {
        return None;
    }
    Some((py - cl) / mid)
}

fn find_reference_fair(history: &VecDeque<PriceTick>, target_ts_ms: i64) -> Option<(f64, i64)> {
    if let Some(sample) = history
        .iter()
        .rev()
        .find(|sample| sample.seen_ts_ms <= target_ts_ms)
    {
        return Some((sample.value, sample.seen_ts_ms));
    }

    history.front().map(|x| (x.value, x.seen_ts_ms))
}

fn indicator_needs_fast_retry(ind: &IndicatorPanel) -> bool {
    let reason = ind.rationale.to_ascii_lowercase();
    reason.contains("insufficient history")
        || reason.contains("missing blended")
        || reason.contains("invalid fair values")
}

fn trim_history(history: &mut VecDeque<PriceTick>, now_ms: i64, keep_ms: i64) {
    while let Some(front) = history.front() {
        if now_ms.saturating_sub(front.seen_ts_ms) > keep_ms {
            let _ = history.pop_front();
        } else {
            break;
        }
    }
}

fn point_from_tick(tick: Option<&PriceTick>, now_ms: i64, stale_ms: i64) -> PriceFeedPoint {
    match tick {
        Some(t) => PriceFeedPoint {
            value: Some(t.value),
            source_ts_ms: Some(t.source_ts_ms),
            observed_ts_ms: Some(t.seen_ts_ms),
            stale: now_ms.saturating_sub(t.seen_ts_ms) > stale_ms,
        },
        None => PriceFeedPoint {
            value: None,
            source_ts_ms: None,
            observed_ts_ms: None,
            stale: true,
        },
    }
}

fn pick_best_market(mut items: Vec<PolyMarketTarget>, now_ms: i64) -> Option<PolyMarketTarget> {
    if items.is_empty() {
        return None;
    }

    items.sort_by_key(|m| {
        if let Some(end) = m.end_ts_ms {
            if end >= now_ms {
                (0_u8, end)
            } else {
                (1_u8, -end)
            }
        } else {
            (2_u8, i64::MAX)
        }
    });

    items.into_iter().next()
}

fn map_up_down_tokens(token_ids: &[String], outcomes: &[String]) -> Option<(String, String)> {
    if token_ids.len() < 2 || outcomes.len() < 2 {
        return None;
    }

    let mut up_idx = None;
    let mut down_idx = None;

    for (idx, outcome) in outcomes.iter().enumerate() {
        let o = outcome.to_ascii_lowercase();
        if up_idx.is_none() && (o.contains("up") || o == "yes" || o.contains("higher")) {
            up_idx = Some(idx);
        }
        if down_idx.is_none() && (o.contains("down") || o == "no" || o.contains("lower")) {
            down_idx = Some(idx);
        }
    }

    let up_idx = up_idx.unwrap_or(0);
    let down_idx = down_idx.unwrap_or(1.min(token_ids.len().saturating_sub(1)));
    if up_idx == down_idx || up_idx >= token_ids.len() || down_idx >= token_ids.len() {
        return None;
    }

    Some((token_ids[up_idx].clone(), token_ids[down_idx].clone()))
}

fn classify_horizon(slug: &str, question: &str) -> Option<u32> {
    let s = slug.to_ascii_lowercase();
    let q = question.to_ascii_lowercase();
    if s.contains("15m") || q.contains("15 minute") || q.contains("15-minute") {
        return Some(15);
    }
    if s.contains("5m") || q.contains("5 minute") || q.contains("5-minute") {
        return Some(5);
    }
    None
}

fn is_btc_updown(slug: &str, question: &str) -> bool {
    let s = slug.to_ascii_lowercase();
    let q = question.to_ascii_lowercase();
    let btc =
        s.contains("btc") || s.contains("bitcoin") || q.contains("btc") || q.contains("bitcoin");
    let updown = s.contains("updown")
        || (q.contains("up") && q.contains("down"))
        || q.contains("up or down");
    btc && updown
}

fn parse_market_end_ms(slug: &str, end_date: Option<&Value>) -> Option<i64> {
    let end = end_date
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc));
    if let Some(end) = end {
        return Some(end.timestamp_millis());
    }

    parse_slug_tail_unix_seconds(slug).map(|ts| ts.saturating_mul(1_000))
}

fn parse_market_start_ms(
    event_start: Option<&Value>,
    start_time: Option<&Value>,
    start_date: Option<&Value>,
) -> Option<i64> {
    parse_datetime_ms(event_start)
        .or_else(|| parse_datetime_ms(start_time))
        .or_else(|| parse_datetime_ms(start_date))
}

fn parse_datetime_ms(v: Option<&Value>) -> Option<i64> {
    let s = v?.as_str()?;
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc).timestamp_millis())
}

fn parse_slug_tail_unix_seconds(slug: &str) -> Option<i64> {
    let (_, tail) = slug.rsplit_once('-')?;
    if !tail.as_bytes().iter().all(u8::is_ascii_digit) {
        return None;
    }
    tail.parse::<i64>().ok()
}

fn parse_string_list(value: Option<&Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };

    match value {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|x| match x {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .collect(),
        Value::String(s) => serde_json::from_str::<Vec<String>>(s).unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        Some(values[mid])
    } else {
        Some((values[mid - 1] + values[mid]) / 2.0)
    }
}

/// Prometheus text used when live-agent is not listening on `--target`.
const STUB_AGENT_PROMETHEUS: &str = "# TYPE polyhft_maker_reports_total counter\n\
polyhft_maker_reports_total 0\n\
polyhft_taker_reports_total 0\n\
polyhft_signal_edge 0\n\
polyhft_chainlink_pyth_basis 0\n\
polyhft_adverse_selection 0\n\
polyhft_decision_cycle_ms 0\n\
polyhft_event_queue_len 0\n\
polyhft_events_ingested_total 0\n";

static LAST_AGENT_UNREACHABLE_WARN_MS: AtomicI64 = AtomicI64::new(0);

fn stub_current_metrics() -> CurrentMetrics {
    let parsed = parse_prometheus_metrics(STUB_AGENT_PROMETHEUS);
    build_current_metrics(&parsed)
}

fn metrics_unreachable_transient(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        if let Some(re) = e.downcast_ref::<reqwest::Error>() {
            return re.is_connect() || re.is_timeout();
        }
        let s = e.to_string().to_ascii_lowercase();
        s.contains("connection refused")
            || s.contains("tcp connect error")
            || s.contains("failed to connect")
            || s.contains("host unreachable")
            || s.contains("timed out")
    })
}

fn warn_agent_unreachable_throttled(now_ms: i64, err: &anyhow::Error) {
    const WINDOW_MS: i64 = 60_000;
    let prev = LAST_AGENT_UNREACHABLE_WARN_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(prev) >= WINDOW_MS {
        LAST_AGENT_UNREACHABLE_WARN_MS.store(now_ms, Ordering::Relaxed);
        warn!(
            error = %err,
            "agent metrics unreachable, using stub zeros (start live-agent on :9901 or pass --target); next similar warn in {}s",
            WINDOW_MS / 1000
        );
    } else {
        debug!(error = %err, "agent metrics still unreachable (stub metrics)");
    }
}

async fn run_scraper(state: AppState, client: Client, args: Args) {
    let mut ticker = tokio::time::interval(Duration::from_millis(args.refresh_ms));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        let scrape_started = Instant::now();
        let scrape_ts_ms = unix_time_ms();

        match scrape_once(&client, &args.target).await {
            Ok(current) => {
                let mut snapshot = state.snapshot.write().await;
                snapshot.up = true;
                snapshot.last_scrape_ms = scrape_ts_ms;
                snapshot.scrape_latency_ms = scrape_started.elapsed().as_secs_f64() * 1_000.0;
                snapshot.error = None;
                snapshot.current = current.clone();
                snapshot.push_history(
                    HistoryPoint {
                        ts_ms: scrape_ts_ms,
                        maker_ratio: current.maker_ratio,
                        maker_reports_total: current.maker_reports_total,
                        taker_reports_total: current.taker_reports_total,
                        signal_edge: current.signal_edge,
                        chainlink_pyth_basis: current.chainlink_pyth_basis,
                        adverse_selection: current.adverse_selection,
                        decision_cycle_ms: current.decision_cycle_ms,
                        queue_len: current.queue_len,
                        events_ingested_total: current.events_ingested_total,
                        pe_total_pnl: current.pe_total_pnl,
                        pe_open_positions: current.pe_open_positions,
                    },
                    args.history_limit,
                );
            }
            Err(err) => {
                if args.stub_metrics_if_unreachable && metrics_unreachable_transient(&err) {
                    warn_agent_unreachable_throttled(scrape_ts_ms, &err);
                    let current = stub_current_metrics();
                    let mut snapshot = state.snapshot.write().await;
                    snapshot.up = false;
                    snapshot.last_scrape_ms = scrape_ts_ms;
                    snapshot.scrape_latency_ms = scrape_started.elapsed().as_secs_f64() * 1_000.0;
                    snapshot.error = Some(format!("{err} (stub metrics until agent is up)"));
                    snapshot.current = current.clone();
                    snapshot.push_history(
                        HistoryPoint {
                            ts_ms: scrape_ts_ms,
                            maker_ratio: current.maker_ratio,
                            maker_reports_total: current.maker_reports_total,
                            taker_reports_total: current.taker_reports_total,
                            signal_edge: current.signal_edge,
                            chainlink_pyth_basis: current.chainlink_pyth_basis,
                            adverse_selection: current.adverse_selection,
                            decision_cycle_ms: current.decision_cycle_ms,
                            queue_len: current.queue_len,
                            events_ingested_total: current.events_ingested_total,
                            pe_total_pnl: current.pe_total_pnl,
                            pe_open_positions: current.pe_open_positions,
                        },
                        args.history_limit,
                    );
                } else {
                    warn!(error = %err, "scrape failed");
                    let mut snapshot = state.snapshot.write().await;
                    snapshot.up = false;
                    snapshot.last_scrape_ms = scrape_ts_ms;
                    snapshot.scrape_latency_ms = scrape_started.elapsed().as_secs_f64() * 1_000.0;
                    snapshot.error = Some(err.to_string());
                    snapshot.push_history(
                        HistoryPoint {
                            ts_ms: scrape_ts_ms,
                            maker_ratio: None,
                            maker_reports_total: None,
                            taker_reports_total: None,
                            signal_edge: None,
                            chainlink_pyth_basis: None,
                            adverse_selection: None,
                            decision_cycle_ms: None,
                            queue_len: None,
                            events_ingested_total: None,
                            pe_total_pnl: None,
                            pe_open_positions: None,
                        },
                        args.history_limit,
                    );
                }
            }
        }
    }
}

async fn scrape_once(client: &Client, target: &str) -> anyhow::Result<CurrentMetrics> {
    let response = client.get(target).send().await?.error_for_status()?;
    let payload = response.text().await?;
    let parsed = parse_prometheus_metrics(&payload);
    Ok(build_current_metrics(&parsed))
}

fn parse_prometheus_metrics(input: &str) -> HashMap<String, f64> {
    let mut map = HashMap::new();

    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut parts = line.split_whitespace();
        let Some(name_with_labels) = parts.next() else {
            continue;
        };
        let Some(value_raw) = parts.next() else {
            continue;
        };

        let Ok(value) = value_raw.parse::<f64>() else {
            continue;
        };

        let name = name_with_labels
            .split('{')
            .next()
            .unwrap_or(name_with_labels)
            .to_string();

        map.insert(name, value);
    }

    map
}

fn build_current_metrics(map: &HashMap<String, f64>) -> CurrentMetrics {
    let maker_reports_total = metric(
        map,
        &[
            "polyhft_maker_reports_total",
            "polyhft_maker_reports_total_total",
        ],
    );
    let taker_reports_total = metric(
        map,
        &[
            "polyhft_taker_reports_total",
            "polyhft_taker_reports_total_total",
        ],
    );

    let mut maker_ratio = metric(map, &["polyhft_maker_ratio"]);
    if maker_ratio.is_none()
        && let (Some(maker), Some(taker)) = (maker_reports_total, taker_reports_total)
    {
        let total = maker + taker;
        if total > 0.0 {
            maker_ratio = Some(maker / total);
        }
    }

    CurrentMetrics {
        maker_reports_total,
        taker_reports_total,
        maker_ratio,
        signal_edge: metric(map, &["polyhft_signal_edge"]),
        chainlink_pyth_basis: metric(map, &["polyhft_chainlink_pyth_basis"]),
        adverse_selection: metric(map, &["polyhft_adverse_selection"]),
        decision_cycle_ms: metric(map, &["polyhft_decision_cycle_ms"]),
        queue_len: metric(map, &["polyhft_event_queue_len"]),
        events_ingested_total: metric(
            map,
            &[
                "polyhft_events_ingested_total",
                "polyhft_events_ingested_total_total",
            ],
        ),
        pe_fills_total: metric(
            map,
            &[
                "polyhft_paper_engine_fills_total",
                "polyhft_paper_engine_fills_total_total",
            ],
        ),
        pe_open_positions: metric(map, &["polyhft_paper_engine_open_positions"]),
        pe_total_pnl: metric(map, &["polyhft_paper_engine_total_pnl"]),
        pe_live_orders_total: metric(
            map,
            &[
                "polyhft_paper_engine_live_orders_total",
                "polyhft_paper_engine_live_orders_total_total",
            ],
        ),
    }
}

fn metric(map: &HashMap<String, f64>, names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| map.get(*name).copied())
}

async fn index() -> impl IntoResponse {
    Html(include_str!("index.html"))
}

#[derive(Serialize)]
struct DashboardConfig {
    activity_url: String,
}

async fn config_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(DashboardConfig { activity_url: state.activity_url.clone() })
}

async fn snapshot(State(state): State<AppState>) -> impl IntoResponse {
    let snapshot = state.snapshot.read().await.clone();
    Json(snapshot)
}

async fn panel_snapshot(State(state): State<AppState>) -> impl IntoResponse {
    let snapshot = state.panel.read().await.clone();
    Json(snapshot)
}

#[derive(Serialize)]
struct HealthResponse {
    ok: bool,
    now_ms: i64,
}

async fn health() -> impl IntoResponse {
    Json(HealthResponse {
        ok: true,
        now_ms: unix_time_ms(),
    })
}

async fn agent_status(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.agent.status().await)
}

async fn agent_start(
    State(state): State<AppState>,
    Json(req): Json<AgentStartRequest>,
) -> Response {
    match state.agent.start(req).await {
        Ok(status) => (StatusCode::OK, Json(status)).into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: err.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn agent_stop(State(state): State<AppState>) -> Response {
    match state.agent.stop().await {
        Ok(status) => (StatusCode::OK, Json(status)).into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: err.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn agent_set_secrets(
    State(state): State<AppState>,
    Json(req): Json<AgentSecretsRequest>,
) -> Response {
    match state.agent.set_secrets(req).await {
        Ok(status) => (StatusCode::OK, Json(status)).into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: err.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn agent_secrets_status(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.agent.secrets_status().await)
}

fn resolve_config_path(workspace: &Path, config: &str) -> PathBuf {
    let raw = PathBuf::from(config);
    if raw.is_absolute() {
        raw
    } else {
        workspace.join(raw)
    }
}

fn auth_private_key_missing(cfg: &AppConfig) -> bool {
    cfg.auth
        .private_key
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
}

fn update_secret_field(slot: &mut Option<String>, incoming: Option<String>) {
    if let Some(value) = incoming {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            *slot = None;
        } else {
            *slot = Some(trimmed);
        }
    }
}

fn value_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn align_next_eval(now_ms: i64, step_secs: u64) -> i64 {
    let step_ms = i64::try_from(step_secs.saturating_mul(1_000))
        .unwrap_or(300_000)
        .max(1_000);
    let buckets = now_ms.div_euclid(step_ms) + 1;
    buckets.saturating_mul(step_ms)
}

fn unix_time_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis().try_into().unwrap_or(i64::MAX),
        Err(err) => {
            error!(error = %err, "system clock is behind UNIX_EPOCH");
            0
        }
    }
}
