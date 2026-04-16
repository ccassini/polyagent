use std::net::SocketAddr;

use anyhow::Context;
use metrics_exporter_prometheus::{BuildError, PrometheusBuilder};
use once_cell::sync::OnceCell;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;

static METRICS_INIT: OnceCell<()> = OnceCell::new();

pub fn init_tracing(log_level: &str, json_logs: bool) -> anyhow::Result<()> {
    let filter = EnvFilter::try_new(log_level)
        .unwrap_or_else(|_| EnvFilter::new("info,hyper=off,reqwest=off,h2=off,rustls=off"));

    if json_logs {
        fmt()
            .with_env_filter(filter)
            .json()
            .with_current_span(false)
            .with_span_events(fmt::format::FmtSpan::NONE)
            .try_init()
            .map_err(|err| {
                anyhow::anyhow!("failed to initialize JSON tracing subscriber: {err}")
            })?;
    } else {
        fmt()
            .with_env_filter(filter)
            .with_target(true)
            .with_span_events(fmt::format::FmtSpan::NONE)
            .try_init()
            .map_err(|err| anyhow::anyhow!("failed to initialize tracing subscriber: {err}"))?;
    }

    Ok(())
}

pub fn init_metrics(bind: &str) -> anyhow::Result<()> {
    if METRICS_INIT.get().is_some() {
        return Ok(());
    }

    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid prometheus bind address: {bind}"))?;

    match PrometheusBuilder::new().with_http_listener(addr).install() {
        Ok(()) => {}
        Err(BuildError::FailedToCreateHTTPListener(_)) => {
            tracing::warn!(
                bind = %addr,
                "prometheus HTTP listener failed (usually address already in use); \
                 continuing with in-process metrics only (no /metrics on this port — stop the other process or change telemetry.prometheus_bind)"
            );
            PrometheusBuilder::new()
                .install_recorder()
                .context("failed to install prometheus recorder (fallback)")?;
        }
        Err(err) => {
            return Err(anyhow::anyhow!(
                "failed to install prometheus recorder: {err}"
            ));
        }
    }

    let _ = METRICS_INIT.set(());
    Ok(())
}
