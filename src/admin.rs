//! Operational endpoints: `/pool/health`, `/pool/stats` (JSON), `/metrics` (Prometheus).

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use subtle::ConstantTimeEq;

use crate::AppState;
use crate::account::now_ms;

fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(expected) = &state.config.server.admin_token else {
        return true;
    };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.as_bytes().strip_prefix(b"Bearer "))
        .is_some_and(|t| bool::from(t.ct_eq(expected.as_bytes())))
}

pub async fn health(State(state): State<Arc<AppState>>) -> Response {
    let now = now_ms();
    let available = state.accounts.iter().filter(|a| a.available(now)).count();
    let code = if available > 0 {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(json!({ "status": if available > 0 { "ok" } else { "degraded" }, "accounts_available": available, "accounts_total": state.accounts.len() }))).into_response()
}

pub fn stats_json(state: &AppState) -> serde_json::Value {
    let now = now_ms();
    let accounts: Vec<_> = state
        .accounts
        .iter()
        .map(|a| {
            json!({
                "name": a.name,
                "state": a.state_label(now),
                "weight": a.weight,
                "inflight": a.inflight.load(Relaxed),
                "headroom": a.headroom(now),
                "cooldown_remaining_secs": a.cooldown_until_ms.load(Relaxed).saturating_sub(now).div_ceil(1000),
                "monthly_budget_usd": a.monthly_budget_usd,
                "usage": a.counters.snapshot(),
            })
        })
        .collect();
    let clients: Vec<_> = state
        .clients
        .iter()
        .map(|c| json!({ "name": c.name, "monthly_budget_usd": c.monthly_budget_usd, "usage": c.counters.snapshot() }))
        .collect();
    json!({
        "uptime_secs": state.started.elapsed().as_secs(),
        "total": state.global.snapshot(),
        "accounts": accounts,
        "clients": clients,
        "models": state.models.snapshot(),
    })
}

pub async fn stats(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(stats_json(&state)).into_response()
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

pub async fn metrics(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let now = now_ms();
    let mut out = String::with_capacity(4096);
    let mut family = |name: &str, kind: &str, help: &str, rows: Vec<(String, f64)>| {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
        for (labels, v) in rows {
            let _ = writeln!(out, "{name}{{{labels}}} {v}");
        }
    };
    macro_rules! per {
        ($iter:expr, $label:literal, $f:expr) => {
            $iter
                .map(|x| (format!(concat!($label, "=\"{}\""), esc(&x.name)), $f(x)))
                .collect()
        };
    }
    let accts = || state.accounts.iter();
    let clients = || state.clients.iter();
    family(
        "claude_pool_account_up",
        "gauge",
        "1 if the account can take requests",
        per!(
            accts(),
            "account",
            |a: &crate::account::Account| a.available(now) as u8 as f64
        ),
    );
    family(
        "claude_pool_account_inflight",
        "gauge",
        "In-flight requests",
        per!(
            accts(),
            "account",
            |a: &crate::account::Account| a.inflight.load(Relaxed) as f64
        ),
    );
    family(
        "claude_pool_account_headroom",
        "gauge",
        "Lowest remaining rate-limit fraction",
        per!(accts(), "account", |a: &crate::account::Account| a
            .headroom(now)),
    );
    family(
        "claude_pool_account_requests_total",
        "counter",
        "Upstream attempts",
        per!(
            accts(),
            "account",
            |a: &crate::account::Account| a.counters.requests.load(Relaxed) as f64
        ),
    );
    family(
        "claude_pool_account_rate_limited_total",
        "counter",
        "Upstream 429s",
        per!(
            accts(),
            "account",
            |a: &crate::account::Account| a.counters.rate_limited.load(Relaxed) as f64
        ),
    );
    family(
        "claude_pool_account_errors_total",
        "counter",
        "Upstream errors",
        per!(
            accts(),
            "account",
            |a: &crate::account::Account| a.counters.errors.load(Relaxed) as f64
        ),
    );
    family(
        "claude_pool_account_cost_usd_total",
        "counter",
        "Estimated spend",
        per!(
            accts(),
            "account",
            |a: &crate::account::Account| a.counters.cost_micros.load(Relaxed) as f64 / 1e6
        ),
    );
    family(
        "claude_pool_account_month_cost_usd",
        "gauge",
        "Estimated spend this month",
        per!(accts(), "account", |a: &crate::account::Account| a
            .counters
            .month_cost_usd()),
    );
    family(
        "claude_pool_client_requests_total",
        "counter",
        "Requests per team member",
        per!(
            clients(),
            "client",
            |c: &crate::Client| c.counters.requests.load(Relaxed) as f64
        ),
    );
    family(
        "claude_pool_client_cost_usd_total",
        "counter",
        "Estimated spend per team member",
        per!(
            clients(),
            "client",
            |c: &crate::Client| c.counters.cost_micros.load(Relaxed) as f64 / 1e6
        ),
    );
    let g = state.global.snapshot();
    family(
        "claude_pool_tokens_total",
        "counter",
        "Tokens processed",
        vec![
            ("kind=\"input\"".into(), g.input_tokens as f64),
            ("kind=\"output\"".into(), g.output_tokens as f64),
            ("kind=\"cache_read\"".into(), g.cache_read_tokens as f64),
            ("kind=\"cache_write\"".into(), g.cache_write_tokens as f64),
        ],
    );
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out).into_response()
}
