#!/usr/bin/env bash
# Derive Polymarket CLOB L2 credentials and merge api_key / api_secret / api_passphrase / wallet_address into [auth].
# Usage: ./scripts/apply-clob-creds.sh [path/to/config.toml]
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
CONFIG="${1:-config.live.tiny.toml}"
exec cargo run -p polyhft-derive-clob-creds -- --config "$CONFIG" --merge
