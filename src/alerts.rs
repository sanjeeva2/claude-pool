//! Periodic health/budget checks. Each condition fires once when it starts and
//! once when it clears, so a webhook channel is not spammed.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::AppState;
use crate::account::now_ms;

#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub key: String,
    pub message: String,
}

/// Evaluate all alert conditions right now.
pub fn evaluate(state: &AppState) -> Vec<Alert> {
    let cfg = &state.config.alerts;
    let now = now_ms();
    let mut out = Vec::new();
    for a in &state.accounts {
        match a.state_label(now) {
            "disabled" => out.push(Alert {
                key: format!("disabled:{}", a.name),
                message: format!(
                    "Account `{}` is disabled: its API key was rejected (401).",
                    a.name
                ),
            }),
            "cooling_down" => out.push(Alert {
                key: format!("cooldown:{}", a.name),
                message: format!(
                    "Account `{}` is cooling down after rate limiting or upstream errors.",
                    a.name
                ),
            }),
            _ => {}
        }
        let h = a.headroom(now);
        if h < cfg.headroom_threshold {
            out.push(Alert {
                key: format!("headroom:{}", a.name),
                message: format!(
                    "Account `{}` has only {:.0}% rate-limit headroom left.",
                    a.name,
                    h * 100.0
                ),
            });
        }
        budget_alerts(
            &mut out,
            "account",
            &a.name,
            a.monthly_budget_usd,
            a.counters.month_cost_usd(),
            &cfg.budget_thresholds,
        );
    }
    for c in &state.clients {
        budget_alerts(
            &mut out,
            "client",
            &c.name,
            c.monthly_budget_usd,
            c.counters.month_cost_usd(),
            &cfg.budget_thresholds,
        );
    }
    let available = state.accounts.iter().filter(|a| a.available(now)).count();
    if available == 0 {
        out.push(Alert {
            key: "pool:exhausted".into(),
            message: "No account in the pool can take requests right now.".into(),
        });
    }
    out
}

fn budget_alerts(
    out: &mut Vec<Alert>,
    kind: &str,
    name: &str,
    budget: Option<f64>,
    spent: f64,
    thresholds: &[f64],
) {
    let Some(b) = budget.filter(|b| *b > 0.0) else {
        return;
    };
    // Only the highest crossed threshold is active.
    if let Some(t) = thresholds
        .iter()
        .copied()
        .filter(|t| spent >= b * t)
        .reduce(f64::max)
    {
        out.push(Alert {
            key: format!("budget:{kind}:{name}:{t}"),
            message: format!(
                "{kind} `{name}` has spent ${spent:.2} of its ${b:.2} monthly budget ({:.0}%).",
                spent / b * 100.0
            ),
        });
    }
}

async fn send(state: &AppState, text: &str) {
    tracing::warn!(alert = %text);
    let Some(url) = &state.config.alerts.webhook_url else {
        return;
    };
    let body = serde_json::json!({ "text": format!("[claude-pool] {text}") });
    if let Err(e) = state
        .http
        .post(url)
        .json(&body)
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        tracing::warn!(error = %e, "alert webhook failed");
    }
}

pub fn spawn(state: Arc<AppState>) {
    let every = state.config.alerts.check_interval_secs.max(1);
    tokio::spawn(async move {
        let mut active: HashSet<String> = HashSet::new();
        let mut tick = tokio::time::interval(Duration::from_secs(every));
        loop {
            tick.tick().await;
            let current = evaluate(&state);
            let keys: HashSet<String> = current.iter().map(|a| a.key.clone()).collect();
            for a in &current {
                if !active.contains(&a.key) {
                    send(&state, &a.message).await;
                }
            }
            for k in active.difference(&keys) {
                send(&state, &format!("Resolved: {k}")).await;
            }
            active = keys;
        }
    });
}
