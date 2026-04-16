# Polymarket 5m BTC/ETH/SOL HFT Agent (Rust)

Production-oriented, low-latency trading system for Polymarket 5-minute BTC/ETH/SOL Up/Down binaries.

## What Is Implemented

- Rust hot path (`ingest -> normalize -> signal -> risk -> execution`) targeting 50-100 ms cycles.
- Maker-first quoting engine with post-only limit orders + cancel/replace flow.
- Taker flow only for high-edge mispricing or rebalance intents.
- Dynamic fee/rebate awareness from market `feeSchedule` (no hardcoded fee table).
- YES+NO `< 1.00` mispricing detection with fee-aware net-edge scoring.
- CTF V2 aligned execution/retry guards:
  - signature modes (`eoa`, `poly_proxy`, `gnosis_safe` / EIP-1271 path)
  - matching engine restart guard (`425`)
  - Cloudflare/throttle retry buckets (`408/429/502/503/504` configurable)
- WS ingest coverage:
  - CLOB Market WS
  - CLOB User WS
  - Sports WS (`wss://sports-api.polymarket.com/ws`)
  - RTDS Binance + Chainlink (BTC/ETH/SOL)
  - Pyth Hermes SSE (`parsed=true`) for BTC/ETH/SOL feed IDs
- Full user trade lifecycle handling in normalized events (`MATCHED`, `MINED`, `CONFIRMED`, unknown-safe fallback).
- Chainlink-vs-Pyth basis arbitrage signal:
  - quote skew via `chainlink_pyth_bias_weight`
  - taker trigger via `chainlink_pyth_arb_min_edge`
- EV-gated maker policy (quote only when expected value is positive after fill-probability, rebate, adverse-selection and cancel-cost terms).
- Requote hysteresis (`requote_*`) to reduce cancel/replace churn on tiny price jitters.
- Optional NDJSON training logger (`telemetry.training_log_path`) for live decision/fill datasets.
- Replay engine with event-time processing + latency injection (`p50/p95/p99`) and PnL attribution split.
- Telemetry: structured logs + Prometheus exporter.

## Workspace Layout

- `crates/core`: shared types, config, clock/drift monitor, lock-free ring, telemetry init.
- `crates/ingest`: market discovery (Gamma), WS subscriptions, event normalization.
- `crates/signal`: microstructure state, fee-aware edge, quote/taker decision intents.
- `crates/risk`: inventory/drawdown/stale-data/correlation gates.
- `crates/execution`: authenticated CLOB client, post-only quote placement, taker routing, retries, liveness.
- `crates/replay`: NDJSON replay, latency model, simulated fills, PnL attribution.
- `apps/live-agent`: live runtime orchestration loop.
- `apps/replay-cli`: offline replay CLI.
- `apps/dashboard`: browser monitoring dashboard (scrapes live-agent Prometheus endpoint).

## Build

```bash
cargo check
cargo build --release -p polyhft-live-agent -p polyhft-replay-cli
```

## Configuration

- Main config: `config.toml`
- Tiny live-risk preset: `config.live.tiny.toml`
- Env override prefix: `POLY_HFT__` (double underscore as nested separator)
- Example env file: `.env.example`

Critical live fields:

- `auth.private_key`
- `auth.signature_kind` (`gnosis_safe` for smart-wallet/EIP-1271 flow)
- `auth.funder_address` (required for proxy/safe flow if not auto-derived)
- `pyth.feed_ids` (BTC/ETH/SOL feed IDs for Hermes stream)
- `strategy.chainlink_pyth_arb_min_edge` (basis taker trigger)
- `strategy.chainlink_pyth_bias_weight` (basis contribution to quote skew)
- `strategy.maker_min_signal_edge`, `maker_min_quote_edge`, `maker_min_expected_value`
- `strategy.maker_min_fill_probability`, `maker_cancel_cost_per_quote`, `maker_max_quotes_per_market`
- `execution.requote_min_age_ms`, `requote_price_ticks`, `requote_size_bps`
- `telemetry.training_log_path` (optional decision/fill NDJSON)

Do not store private keys in config files; prefer environment overrides:

```bash
export POLY_HFT__AUTH__PRIVATE_KEY=0x...
export POLY_HFT__AUTH__API_KEY=...
export POLY_HFT__AUTH__API_SECRET=...
export POLY_HFT__AUTH__API_PASSPHRASE=...
export POLY_HFT__AUTH__WALLET_ADDRESS=0x...
```

For ultra-small live tests:

```bash
POLY_HFT__AUTH__PRIVATE_KEY=0x... \
cargo run --release -p polyhft-live-agent -- --config config.live.tiny.toml
```

Note: if venue min order size is above `$0.001`, orders may be rejected by exchange constraints.

## Run Live Agent

```bash
cargo run --release -p polyhft-live-agent -- --config config.toml
```

Dry-run mode (no order submission):

```bash
cargo run --release -p polyhft-live-agent -- --config config.toml --dry-run
```

## Run Replay

```bash
cargo run --release -p polyhft-replay-cli -- --config config.toml --input ./data/replay/events.ndjson
```

Output is JSON summary including:

- `event_count`
- `simulated_trades`
- `maker_ratio`
- `pnl` attribution (`spread_capture`, `maker_rebate`, `taker_fee`, `adverse_selection`, `total`)

## Run Dashboard

Local browser dashboard against running live-agent metrics:

```bash
cargo run --release -p polyhft-dashboard -- \
  --bind 127.0.0.1:3000 \
  --target http://127.0.0.1:9901/ \
  --workspace . \
  --agent-default-config config.live.tiny.toml
```

Then open:

```text
http://127.0.0.1:3000
```

Dashboard features:

- Start/stop the trading agent directly from the browser.
- Select `paper` or `live` mode before starting.
- Optional config path override per start action.
- Runtime visibility: mode, pid, uptime, last exit status, launch command.
- Session-scope auth secret injection (`private_key`, API key/secret/passphrase, wallet/funder, signature kind).
- Multi-source BTC tape panel: Binance, Coinbase, Pyth, Chainlink, blended fair.
- Polymarket BTC Up/Down market panel for `5m` and `15m` (when active markets are found).
- Independent indicator engine (agent-independent) with `5m` and `15m` decisions, refreshed on 5-minute cadence.

Notes:

- If nothing is listening on `--target` (e.g. live-agent not started yet), the dashboard uses **stub zero metrics** and a **throttled** log warning instead of failing every scrape. Use `--stub-metrics-if-unreachable=false` to treat that as a hard error only.
- `paper` mode starts agent with `--dry-run`.
- `live` mode is rejected unless a private key is available (env or config).
- Dashboard writes child process logs to:
  - `data/live/dashboard-agent.stdout.log`
  - `data/live/dashboard-agent.stderr.log`

## Latency Tuning Notes (Host)

Container tuning alone is not enough for sub-100 ms competitiveness. Apply host-level tuning:

1. CPU isolation and pinning

- Isolate dedicated cores for agent threads (e.g., kernel `isolcpus`, `nohz_full`, `rcu_nocbs`).
- Pin NIC interrupts away from trading cores (`/proc/irq/*/smp_affinity_list`).

2. Clock discipline

- Run PTP/NTP discipline and monitor drift.
- Agent drift guard uses `infra.ptp_drift_warn_ms` and `infra.ptp_drift_kill_ms`.

3. Network and sockets

- Increase file descriptors (`ulimit -n 262144`).
- Tune socket buffers and backlog for bursty WS traffic.

4. Scheduling

- Prefer dedicated bare-metal/vCPU placement.
- Keep co-located noisy workloads off trading cores.

## Docker

Build and start:

```bash
docker compose up --build -d
```

Services:

- `live-agent` on metrics port `9901`
- `prometheus` on `9090`
- `dashboard` on `8080` (includes Chainlink-Pyth basis panel)

Files:

- `Dockerfile`
- `docker-compose.yml`
- `docker/prometheus.yml`

## Safety and Reliability Guards

- Maker-only default in quote path (`post_only=true`).
- Heartbeat task + liveness breach auto `cancel_all_orders`.
- Stale-data kill-switch.
- Max drawdown guard.
- BTC/ETH/SOL directional correlation exposure block (coarse proxy).
- Retry backoff + jitter for CLOB status/network pressure.

## Current Limitations / Operator Hooks

- Collateral wrap/unwrap is intentionally a guarded hook (`execution.enable_wrap_unwrap`) and defaults off.
- Fill probability and queue-position models are lightweight online proxies; replace with venue-calibrated models for production deployment.
- Replay fill simulator is deterministic/probabilistic approximation, not exchange-exact matching logic.

## Verification

```bash
cargo fmt --all
cargo check
```
