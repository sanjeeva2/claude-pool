//! claude-pool: a high-performance gateway that pools several Claude API
//! accounts behind one endpoint for a team.

pub mod account;
pub mod admin;
pub mod alerts;
pub mod config;
pub mod pricing;
pub mod proxy;
pub mod report;
pub mod router;
pub mod stats;
pub mod tap;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use account::Account;
use config::Config;
use pricing::PriceTable;
use stats::{Counters, ModelStats, PersistedState};

pub struct Client {
    pub name: String,
    pub token: Box<[u8]>,
    pub monthly_budget_usd: Option<f64>,
    pub counters: Counters,
}

impl Client {
    pub fn over_budget(&self) -> bool {
        self.monthly_budget_usd
            .is_some_and(|b| self.counters.month_cost_usd() >= b)
    }
}

pub struct AppState {
    pub config: Config,
    pub accounts: Vec<Account>,
    pub clients: Vec<Client>,
    pub client_by_token: HashMap<Box<[u8]>, usize>,
    pub models: ModelStats,
    pub global: Counters,
    pub prices: PriceTable,
    pub http: reqwest::Client,
    pub upstream: String,
    pub started: Instant,
}

impl AppState {
    pub fn new(config: Config) -> anyhow::Result<Arc<Self>> {
        let accounts = config
            .accounts
            .iter()
            .map(Account::new)
            .collect::<anyhow::Result<Vec<_>>>()?;
        let clients: Vec<Client> = config
            .clients
            .iter()
            .map(|c| Client {
                name: c.name.clone(),
                token: c.token.as_bytes().into(),
                monthly_budget_usd: c.monthly_budget_usd,
                counters: Counters::default(),
            })
            .collect();
        let client_by_token = clients
            .iter()
            .enumerate()
            .map(|(i, c)| (c.token.clone(), i))
            .collect();
        let s = &config.server;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(s.connect_timeout_secs))
            .read_timeout(Duration::from_secs(s.read_timeout_secs))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(256)
            .tcp_nodelay(true)
            .tcp_keepalive(Duration::from_secs(30))
            .http2_adaptive_window(true)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("claude-pool/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let state = Self {
            upstream: s.upstream.trim_end_matches('/').to_string(),
            prices: PriceTable::new(&config.pricing),
            accounts,
            clients,
            client_by_token,
            models: ModelStats::default(),
            global: Counters::default(),
            http,
            started: Instant::now(),
            config,
        };
        state.load_state();
        Ok(Arc::new(state))
    }

    fn load_state(&self) {
        let path = &self.config.server.state_file;
        if path.is_empty() {
            return;
        }
        let Some(p) = PersistedState::load(path) else {
            return;
        };
        for a in &self.accounts {
            if let Some(s) = p.accounts.get(&a.name) {
                a.counters.restore(s);
            }
        }
        for c in &self.clients {
            if let Some(s) = p.clients.get(&c.name) {
                c.counters.restore(s);
            }
        }
        self.global.restore(&p.global);
        self.models.restore(p.models);
        tracing::info!(%path, "restored usage counters");
    }

    pub fn persisted(&self) -> PersistedState {
        let accounts: std::collections::BTreeMap<_, _> = self
            .accounts
            .iter()
            .map(|a| (a.name.clone(), a.counters.snapshot()))
            .collect();
        PersistedState {
            global: self.global.snapshot(),
            accounts,
            clients: self
                .clients
                .iter()
                .map(|c| (c.name.clone(), c.counters.snapshot()))
                .collect(),
            models: self.models.snapshot(),
        }
    }

    pub fn save_state(&self) {
        let path = &self.config.server.state_file;
        if path.is_empty() {
            return;
        }
        if let Err(e) = self.persisted().save(path) {
            tracing::warn!(%path, error = %e, "failed to save state");
        }
    }
}

/// Build the HTTP application.
pub fn app(state: Arc<AppState>) -> axum::Router {
    use axum::routing::{any, get};
    axum::Router::new()
        .route("/v1/{*rest}", any(proxy::handle))
        .route("/pool/health", get(admin::health))
        .route("/pool/stats", get(admin::stats))
        .route("/metrics", get(admin::metrics))
        .with_state(state)
}

/// Background task: periodically persist counters.
pub fn spawn_snapshotter(state: Arc<AppState>) {
    let every = state.config.server.snapshot_interval_secs.max(1);
    if state.config.server.state_file.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(every));
        tick.tick().await;
        loop {
            tick.tick().await;
            let s = state.clone();
            let _ = tokio::task::spawn_blocking(move || s.save_state()).await;
        }
    });
}
