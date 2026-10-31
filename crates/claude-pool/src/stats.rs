//! Lock-free usage counters plus snapshot/persistence.

use chrono::{Datelike, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

use crate::pricing::TokenUsage;

/// Current billing month as `yyyymm`.
pub fn month_key() -> u32 {
    let now = Utc::now();
    now.year() as u32 * 100 + now.month()
}

/// Counters updated on every request; all atomics so the hot path never locks.
#[derive(Debug, Default)]
pub struct Counters {
    pub requests: AtomicU64,
    pub errors: AtomicU64,
    pub rate_limited: AtomicU64,
    pub input_tokens: AtomicU64,
    pub output_tokens: AtomicU64,
    pub cache_read_tokens: AtomicU64,
    pub cache_write_tokens: AtomicU64,
    pub cost_micros: AtomicU64,
    /// Spend in the current calendar month (UTC), used for budgets.
    pub month_cost_micros: AtomicU64,
    pub month: AtomicU32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CountersSnapshot {
    pub requests: u64,
    pub errors: u64,
    pub rate_limited: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    pub month_cost_usd: f64,
    pub month: u32,
}

impl Counters {
    pub fn record_usage(&self, u: &TokenUsage, cost_micros: u64) {
        self.input_tokens.fetch_add(u.input, Relaxed);
        self.output_tokens.fetch_add(u.output, Relaxed);
        self.cache_read_tokens.fetch_add(u.cache_read, Relaxed);
        self.cache_write_tokens
            .fetch_add(u.cache_write_5m + u.cache_write_1h, Relaxed);
        self.cost_micros.fetch_add(cost_micros, Relaxed);
        self.roll_month();
        self.month_cost_micros.fetch_add(cost_micros, Relaxed);
    }

    /// Reset the monthly spend when the calendar month changes.
    fn roll_month(&self) {
        let now = month_key();
        let prev = self.month.load(Relaxed);
        if prev != now
            && self
                .month
                .compare_exchange(prev, now, Relaxed, Relaxed)
                .is_ok()
        {
            self.month_cost_micros.store(0, Relaxed);
        }
    }

    pub fn month_cost_usd(&self) -> f64 {
        if self.month.load(Relaxed) != month_key() {
            return 0.0;
        }
        self.month_cost_micros.load(Relaxed) as f64 / 1e6
    }

    pub fn snapshot(&self) -> CountersSnapshot {
        CountersSnapshot {
            requests: self.requests.load(Relaxed),
            errors: self.errors.load(Relaxed),
            rate_limited: self.rate_limited.load(Relaxed),
            input_tokens: self.input_tokens.load(Relaxed),
            output_tokens: self.output_tokens.load(Relaxed),
            cache_read_tokens: self.cache_read_tokens.load(Relaxed),
            cache_write_tokens: self.cache_write_tokens.load(Relaxed),
            cost_usd: self.cost_micros.load(Relaxed) as f64 / 1e6,
            month_cost_usd: self.month_cost_usd(),
            month: self.month.load(Relaxed),
        }
    }

    pub fn restore(&self, s: &CountersSnapshot) {
        self.requests.store(s.requests, Relaxed);
        self.errors.store(s.errors, Relaxed);
        self.rate_limited.store(s.rate_limited, Relaxed);
        self.input_tokens.store(s.input_tokens, Relaxed);
        self.output_tokens.store(s.output_tokens, Relaxed);
        self.cache_read_tokens.store(s.cache_read_tokens, Relaxed);
        self.cache_write_tokens.store(s.cache_write_tokens, Relaxed);
        self.cost_micros
            .store((s.cost_usd * 1e6).round() as u64, Relaxed);
        if s.month == month_key() {
            self.month.store(s.month, Relaxed);
            self.month_cost_micros
                .store((s.month_cost_usd * 1e6).round() as u64, Relaxed);
        }
    }
}

/// Per-model breakdown. Touched once per finished request, so a mutex is fine.
#[derive(Debug, Default)]
pub struct ModelStats {
    inner: Mutex<BTreeMap<String, CountersSnapshot>>,
}

impl ModelStats {
    pub fn record(&self, model: &str, u: &TokenUsage, cost_micros: u64) {
        let mut m = self.inner.lock();
        let e = match m.get_mut(model) {
            Some(e) => e,
            None => m.entry(model.to_string()).or_default(),
        };
        e.requests += 1;
        e.input_tokens += u.input;
        e.output_tokens += u.output;
        e.cache_read_tokens += u.cache_read;
        e.cache_write_tokens += u.cache_write_5m + u.cache_write_1h;
        e.cost_usd += cost_micros as f64 / 1e6;
    }

    pub fn snapshot(&self) -> BTreeMap<String, CountersSnapshot> {
        self.inner.lock().clone()
    }

    pub fn restore(&self, s: BTreeMap<String, CountersSnapshot>) {
        *self.inner.lock() = s;
    }
}

/// On-disk state file.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub global: CountersSnapshot,
    #[serde(default)]
    pub accounts: BTreeMap<String, CountersSnapshot>,
    #[serde(default)]
    pub clients: BTreeMap<String, CountersSnapshot>,
    #[serde(default)]
    pub models: BTreeMap<String, CountersSnapshot>,
}

impl PersistedState {
    pub fn load(path: &str) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        match serde_json::from_str(&text) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(%path, error = %e, "ignoring unreadable state file");
                None
            }
        }
    }

    /// Atomic write: temp file + rename, so a crash never leaves a torn file.
    pub fn save(&self, path: &str) -> std::io::Result<()> {
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)
    }
}
