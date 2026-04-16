use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;

#[derive(Debug, Clone)]
pub struct MonotonicClock {
    start: Instant,
}

impl MonotonicClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    #[must_use]
    pub fn elapsed_ns(&self) -> u64 {
        self.start
            .elapsed()
            .as_nanos()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    #[must_use]
    pub fn elapsed_ms(&self) -> u64 {
        self.start
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    #[must_use]
    pub fn unix_time_ms() -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_millis().try_into().unwrap_or(i64::MAX),
            Err(_) => 0,
        }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DriftSnapshot {
    pub sampled_at_ms: i64,
    pub local_unix_ms: i64,
    pub server_unix_ms: i64,
    pub drift_ms: i64,
}

#[derive(Debug, Clone)]
pub struct DriftMonitor {
    warn_ms: i64,
    kill_ms: i64,
    last: Arc<RwLock<DriftSnapshot>>,
}

impl DriftMonitor {
    #[must_use]
    pub fn new(warn_ms: i64, kill_ms: i64) -> Self {
        Self {
            warn_ms,
            kill_ms,
            last: Arc::new(RwLock::new(DriftSnapshot::default())),
        }
    }

    pub fn observe_server_time(&self, server_unix_ms: i64) -> DriftSnapshot {
        let local_unix_ms = MonotonicClock::unix_time_ms();
        let drift_ms = local_unix_ms - server_unix_ms;
        let snapshot = DriftSnapshot {
            sampled_at_ms: local_unix_ms,
            local_unix_ms,
            server_unix_ms,
            drift_ms,
        };
        *self.last.write() = snapshot;
        snapshot
    }

    #[must_use]
    pub fn last(&self) -> DriftSnapshot {
        *self.last.read()
    }

    #[must_use]
    pub fn should_warn(&self) -> bool {
        self.last().drift_ms.abs() >= self.warn_ms
    }

    #[must_use]
    pub fn should_kill(&self) -> bool {
        self.last().drift_ms.abs() >= self.kill_ms
    }

    #[must_use]
    pub fn poll_interval() -> Duration {
        Duration::from_secs(2)
    }
}

/// Polymarket CLOB websocket payloads sometimes use Unix **seconds**; this codebase compares
/// against [`MonotonicClock::unix_time_ms`]. Values below `1_000_000_000_000` (~year 2001 in ms)
/// are treated as seconds and scaled to milliseconds.
#[must_use]
pub fn normalize_exchange_ts_ms(raw: i64) -> i64 {
    const UNIX_MS_CUTOFF: i64 = 1_000_000_000_000;
    if raw > 0 && raw < UNIX_MS_CUTOFF {
        raw.saturating_mul(1000)
    } else {
        raw
    }
}

#[cfg(test)]
mod normalize_exchange_ts_tests {
    use super::normalize_exchange_ts_ms;

    #[test]
    fn leaves_modern_unix_ms_unchanged() {
        assert_eq!(
            normalize_exchange_ts_ms(1_774_000_000_000),
            1_774_000_000_000
        );
    }

    #[test]
    fn scales_unix_seconds() {
        assert_eq!(
            normalize_exchange_ts_ms(1_774_000_000),
            1_774_000_000_000_i64
        );
    }
}
