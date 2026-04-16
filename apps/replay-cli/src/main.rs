use clap::Parser;
use tracing::info;

use polyhft_core::config::AppConfig;
use polyhft_core::telemetry::{init_metrics, init_tracing};
use polyhft_ingest::MarketDiscovery;
use polyhft_replay::ReplayEngine;

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Event-time replay + PnL attribution for Polymarket HFT"
)]
struct Args {
    #[arg(long, default_value = "config.toml")]
    config: String,

    #[arg(long)]
    input: Option<String>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = AppConfig::load(&args.config)?;

    init_tracing(&cfg.telemetry.log_level, cfg.telemetry.json_logs)?;
    init_metrics(&cfg.telemetry.prometheus_bind)?;

    let discovery = MarketDiscovery::new(cfg.clone())?;
    let markets = discovery.discover().await?;

    let replay = ReplayEngine::new(cfg.clone(), markets);
    let input = args
        .input
        .unwrap_or_else(|| cfg.replay.event_log_path.clone());
    let summary = replay.run_from_ndjson(&input)?;

    info!(
        events = summary.event_count,
        trades = summary.simulated_trades,
        maker_ratio = %summary.maker_ratio,
        total_pnl = %summary.pnl.total,
        "replay finished"
    );

    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}
