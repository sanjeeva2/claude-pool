//! Live per-account state: in-flight requests, rate-limit headroom learned from
//! response headers, cooldowns after 429/5xx, and usage counters.

use http::{HeaderMap, HeaderValue};
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::AccountConfig;
use crate::stats::Counters;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One rate-limit dimension (`requests`, `tokens`, ...). `-1` = unknown.
#[derive(Debug)]
pub struct Limit {
    pub limit: AtomicI64,
    pub remaining: AtomicI64,
    pub reset_ms: AtomicU64,
}

impl Default for Limit {
    fn default() -> Self {
        Self {
            limit: AtomicI64::new(-1),
            remaining: AtomicI64::new(-1),
            reset_ms: AtomicU64::new(0),
        }
    }
}

impl Limit {
    /// Remaining fraction in [0, 1]; 1.0 when unknown or the window has reset.
    pub fn headroom(&self, now: u64) -> f64 {
        let limit = self.limit.load(Relaxed);
        let rem = self.remaining.load(Relaxed);
        if limit <= 0 || rem < 0 {
            return 1.0;
        }
        let reset = self.reset_ms.load(Relaxed);
        if reset != 0 && now >= reset {
            return 1.0;
        }
        (rem as f64 / limit as f64).clamp(0.0, 1.0)
    }

    fn update(&self, headers: &HeaderMap, prefix: &str) {
        let get = |suffix: &str| {
            headers
                .get(format!("anthropic-ratelimit-{prefix}-{suffix}"))
                .and_then(|v| v.to_str().ok())
        };
        if let Some(v) = get("limit").and_then(|s| s.parse().ok()) {
            self.limit.store(v, Relaxed);
        }
        if let Some(v) = get("remaining").and_then(|s| s.parse().ok()) {
            self.remaining.store(v, Relaxed);
        }
        if let Some(ts) = get("reset").and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()) {
            self.reset_ms
                .store(ts.timestamp_millis().max(0) as u64, Relaxed);
        }
    }
}

#[derive(Debug)]
pub struct Account {
    pub name: String,
    pub api_key: HeaderValue,
    pub weight: u32,
    pub max_concurrency: u32,
    pub monthly_budget_usd: Option<f64>,

    pub inflight: AtomicU32,
    pub cooldown_until_ms: AtomicU64,
    pub consecutive_failures: AtomicU32,
    pub disabled_until_ms: AtomicU64,

    pub requests_limit: Limit,
    pub input_tokens_limit: Limit,
    pub output_tokens_limit: Limit,

    pub counters: Counters,
}

impl Account {
    pub fn new(cfg: &AccountConfig) -> anyhow::Result<Self> {
        let mut api_key = HeaderValue::from_str(&cfg.api_key).map_err(|_| {
            anyhow::anyhow!("account `{}` api_key contains invalid characters", cfg.name)
        })?;
        api_key.set_sensitive(true);
        Ok(Self {
            name: cfg.name.clone(),
            api_key,
            weight: cfg.weight,
            max_concurrency: cfg.max_concurrency,
            monthly_budget_usd: cfg.monthly_budget_usd,
            inflight: AtomicU32::new(0),
            cooldown_until_ms: AtomicU64::new(0),
            consecutive_failures: AtomicU32::new(0),
            disabled_until_ms: AtomicU64::new(0),
            requests_limit: Limit::default(),
            input_tokens_limit: Limit::default(),
            output_tokens_limit: Limit::default(),
            counters: Counters::default(),
        })
    }

    /// Lowest remaining fraction across all known limits.
    pub fn headroom(&self, now: u64) -> f64 {
        self.requests_limit
            .headroom(now)
            .min(self.input_tokens_limit.headroom(now))
            .min(self.output_tokens_limit.headroom(now))
    }

    pub fn over_budget(&self) -> bool {
        self.monthly_budget_usd
            .is_some_and(|b| self.counters.month_cost_usd() >= b)
    }

    /// Can this account take a request right now?
    pub fn available(&self, now: u64) -> bool {
        self.disabled_until_ms.load(Relaxed) <= now
            && self.cooldown_until_ms.load(Relaxed) <= now
            && (self.max_concurrency == 0 || self.inflight.load(Relaxed) < self.max_concurrency)
            && !self.over_budget()
    }

    pub fn state_label(&self, now: u64) -> &'static str {
        if self.disabled_until_ms.load(Relaxed) > now {
            "disabled"
        } else if self.cooldown_until_ms.load(Relaxed) > now {
            "cooling_down"
        } else if self.over_budget() {
            "over_budget"
        } else {
            "ok"
        }
    }

    pub fn update_limits(&self, headers: &HeaderMap) {
        self.requests_limit.update(headers, "requests");
        // Older APIs report a combined `tokens` dimension; newer split input/output.
        if headers.contains_key("anthropic-ratelimit-input-tokens-remaining") {
            self.input_tokens_limit.update(headers, "input-tokens");
        } else {
            self.input_tokens_limit.update(headers, "tokens");
        }
        self.output_tokens_limit.update(headers, "output-tokens");
    }

    pub fn mark_success(&self) {
        self.consecutive_failures.store(0, Relaxed);
    }

    /// Put the account on cooldown. `retry_after_secs` from the API wins over
    /// our exponential backoff.
    pub fn mark_failure(&self, retry_after_secs: Option<u64>, base: u64, max: u64) -> u64 {
        let n = self.consecutive_failures.fetch_add(1, Relaxed).min(16);
        let secs = retry_after_secs
            .unwrap_or_else(|| base.saturating_mul(1u64 << n))
            .clamp(1, max.max(1));
        let until = now_ms() + secs * 1000;
        self.cooldown_until_ms.fetch_max(until, Relaxed);
        secs
    }

    pub fn disable_for(&self, secs: u64) {
        self.disabled_until_ms
            .store(now_ms() + secs * 1000, Relaxed);
    }
}

/// RAII guard that keeps `inflight` accurate even if a request is cancelled.
pub struct InflightGuard<'a>(&'a Account);

impl<'a> InflightGuard<'a> {
    pub fn new(a: &'a Account) -> Self {
        a.inflight.fetch_add(1, Relaxed);
        Self(a)
    }
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct() -> Account {
        Account::new(&AccountConfig {
            name: "a".into(),
            api_key: "k".into(),
            admin_key: None,
            weight: 1,
            max_concurrency: 0,
            monthly_budget_usd: None,
        })
        .unwrap()
    }

    #[test]
    fn parses_rate_limit_headers() {
        let a = acct();
        let mut h = HeaderMap::new();
        h.insert("anthropic-ratelimit-requests-limit", "100".parse().unwrap());
        h.insert(
            "anthropic-ratelimit-requests-remaining",
            "25".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-requests-reset",
            "2999-01-01T00:00:00Z".parse().unwrap(),
        );
        a.update_limits(&h);
        assert!((a.headroom(now_ms()) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn reset_window_restores_headroom() {
        let a = acct();
        let mut h = HeaderMap::new();
        h.insert("anthropic-ratelimit-requests-limit", "100".parse().unwrap());
        h.insert(
            "anthropic-ratelimit-requests-remaining",
            "0".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-requests-reset",
            "2000-01-01T00:00:00Z".parse().unwrap(),
        );
        a.update_limits(&h);
        assert_eq!(a.headroom(now_ms()), 1.0);
    }

    #[test]
    fn backoff_grows_and_caps() {
        let a = acct();
        assert_eq!(a.mark_failure(None, 2, 120), 2);
        assert_eq!(a.mark_failure(None, 2, 120), 4);
        assert_eq!(a.mark_failure(Some(500), 2, 120), 120);
        assert!(!a.available(now_ms()));
    }
}
