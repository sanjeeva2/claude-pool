//! Configuration loading. Secrets can be given literally, as `env:VAR_NAME`,
//! or as `file:/path/to/secret` so that keys never have to live in the TOML.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use crate::pricing::ModelPrice;

/// Hard cap so account sets fit in a `u64` bitmask on the hot path.
pub const MAX_ACCOUNTS: usize = 64;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub clients: Vec<ClientConfig>,
    #[serde(default)]
    pub alerts: AlertConfig,
    /// Per-model price overrides (USD per million tokens), keyed by model id or prefix.
    #[serde(default)]
    pub pricing: HashMap<String, ModelPrice>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub listen: String,
    /// Upstream Claude API base URL.
    pub upstream: String,
    /// Bearer token protecting `/pool/stats` and `/metrics`. Optional.
    pub admin_token: Option<String>,
    /// Max accounts tried for one request before giving up.
    pub max_attempts: usize,
    pub max_body_mb: usize,
    pub connect_timeout_secs: u64,
    /// Idle read timeout on the upstream connection (not total time; streams can run long).
    pub read_timeout_secs: u64,
    /// Where usage counters are persisted. Empty string disables persistence.
    pub state_file: String,
    pub snapshot_interval_secs: u64,
    /// Route requests that share a prompt prefix to the same account (keeps prompt-cache hits).
    pub affinity: bool,
    /// Accounts below this rate-limit headroom (0..1) are skipped by affinity routing.
    pub affinity_min_headroom: f64,
    pub cooldown_base_secs: u64,
    pub cooldown_max_secs: u64,
    /// How long an account stays disabled after a 401 (bad / revoked key).
    pub auth_failure_disable_secs: u64,
    /// Sent upstream when the client omits `anthropic-version`.
    pub default_anthropic_version: String,
    /// Add an `x-claude-pool-account` response header naming the account used.
    pub expose_account_header: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8787".into(),
            upstream: "https://api.anthropic.com".into(),
            admin_token: None,
            max_attempts: 3,
            max_body_mb: 32,
            connect_timeout_secs: 10,
            read_timeout_secs: 600,
            state_file: "claude-pool-state.json".into(),
            snapshot_interval_secs: 30,
            affinity: true,
            affinity_min_headroom: 0.05,
            cooldown_base_secs: 2,
            cooldown_max_secs: 120,
            auth_failure_disable_secs: 3600,
            default_anthropic_version: "2023-06-01".into(),
            expose_account_header: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub name: String,
    /// API key used for `/v1/messages` etc.
    pub api_key: String,
    /// Optional Admin API key (`sk-ant-admin...`) used by `claude-pool usage`.
    pub admin_key: Option<String>,
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// 0 = unlimited.
    #[serde(default)]
    pub max_concurrency: u32,
    pub monthly_budget_usd: Option<f64>,
}

fn default_weight() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub name: String,
    /// Token the team member sends as `x-api-key` or `Authorization: Bearer`.
    pub token: String,
    pub monthly_budget_usd: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AlertConfig {
    /// Slack-compatible webhook (`{"text": ...}` JSON body). Alerts are always logged.
    pub webhook_url: Option<String>,
    pub check_interval_secs: u64,
    /// Alert when an account's rate-limit headroom drops below this fraction.
    pub headroom_threshold: f64,
    /// Alert when monthly spend crosses these fractions of the budget.
    pub budget_thresholds: Vec<f64>,
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            webhook_url: None,
            check_interval_secs: 30,
            headroom_threshold: 0.10,
            budget_thresholds: vec![0.8, 1.0],
        }
    }
}

/// Resolve `env:NAME`, `file:/path`, or a literal value.
pub fn resolve_secret(raw: &str) -> Result<String> {
    if let Some(var) = raw.strip_prefix("env:") {
        std::env::var(var).with_context(|| format!("environment variable `{var}` is not set"))
    } else if let Some(path) = raw.strip_prefix("file:") {
        Ok(std::fs::read_to_string(path)
            .with_context(|| format!("reading secret file `{path}`"))?
            .trim()
            .to_string())
    } else {
        Ok(raw.to_string())
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config `{}`", path.display()))?;
        Self::from_toml(&text)
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        let mut cfg: Config = toml::from_str(text).context("parsing config")?;
        cfg.resolve()?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Replace secret references with their values.
    fn resolve(&mut self) -> Result<()> {
        if let Some(t) = &self.server.admin_token {
            self.server.admin_token = Some(resolve_secret(t)?);
        }
        for a in &mut self.accounts {
            a.api_key =
                resolve_secret(&a.api_key).with_context(|| format!("account `{}`", a.name))?;
            if let Some(k) = &a.admin_key {
                a.admin_key =
                    Some(resolve_secret(k).with_context(|| format!("account `{}`", a.name))?);
            }
        }
        for c in &mut self.clients {
            c.token = resolve_secret(&c.token).with_context(|| format!("client `{}`", c.name))?;
        }
        if let Some(w) = &self.alerts.webhook_url {
            self.alerts.webhook_url = Some(resolve_secret(w)?);
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.accounts.is_empty() {
            bail!("at least one [[accounts]] entry is required");
        }
        if self.accounts.len() > MAX_ACCOUNTS {
            bail!("at most {MAX_ACCOUNTS} accounts are supported");
        }
        let mut names = std::collections::HashSet::new();
        for a in &self.accounts {
            if !names.insert(&a.name) {
                bail!("duplicate account name `{}`", a.name);
            }
            if a.api_key.is_empty() {
                bail!("account `{}` has an empty api_key", a.name);
            }
            if a.weight == 0 {
                bail!("account `{}` has weight 0", a.name);
            }
        }
        let mut tokens = std::collections::HashSet::new();
        let mut cnames = std::collections::HashSet::new();
        for c in &self.clients {
            if !cnames.insert(&c.name) {
                bail!("duplicate client name `{}`", c.name);
            }
            if c.token.len() < 16 {
                bail!(
                    "client `{}` token is too short (min 16 chars; use `claude-pool gen-token`)",
                    c.name
                );
            }
            if !tokens.insert(&c.token) {
                bail!("client `{}` reuses another client's token", c.name);
            }
        }
        if self.server.max_attempts == 0 {
            bail!("server.max_attempts must be >= 1");
        }
        Ok(())
    }
}
