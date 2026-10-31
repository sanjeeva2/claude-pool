//! `claude-pool usage`: pulls the Admin API usage and cost reports from every
//! account that has an `admin_key` and prints combined totals.
//!
//! Endpoints: `GET /v1/organizations/usage_report/messages` and
//! `GET /v1/organizations/cost_report` (raw HTTP; not in the SDKs).

use anyhow::{Context, Result, bail};
use chrono::{Duration, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

use crate::config::Config;

const ADMIN_API_VERSION: &str = "2023-06-01";

#[derive(Debug, Default, Clone, Serialize)]
pub struct ModelTokens {
    pub uncached_input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct OrgReport {
    pub account: String,
    pub models: BTreeMap<String, ModelTokens>,
    /// Currency → amount in major units (e.g. USD dollars).
    pub cost: BTreeMap<String, f64>,
    pub error: Option<String>,
}

fn num(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Sum token fields from one usage-report result row.
pub fn absorb_usage_row(models: &mut BTreeMap<String, ModelTokens>, row: &Value) {
    let model = row
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let e = models.entry(model).or_default();
    e.uncached_input += num(row, "uncached_input_tokens");
    e.cache_read += num(row, "cache_read_input_tokens");
    if let Some(cc) = row.get("cache_creation") {
        e.cache_write +=
            num(cc, "ephemeral_5m_input_tokens") + num(cc, "ephemeral_1h_input_tokens");
    }
    e.output += num(row, "output_tokens");
}

/// Cost report amounts are decimal strings in the currency's lowest unit (cents for USD).
pub fn absorb_cost_row(cost: &mut BTreeMap<String, f64>, row: &Value) {
    let currency = row
        .get("currency")
        .and_then(Value::as_str)
        .unwrap_or("USD")
        .to_string();
    let amount = match row.get("amount") {
        Some(Value::String(s)) => s.parse::<f64>().unwrap_or(0.0),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    };
    *cost.entry(currency).or_default() += amount / 100.0;
}

async fn paged(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    path: &str,
    query: &[(&str, String)],
) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    let mut page: Option<String> = None;
    for _ in 0..100 {
        let mut q: Vec<(&str, String)> = query.to_vec();
        if let Some(p) = &page {
            q.push(("page", p.clone()));
        }
        let resp = http
            .get(format!("{base}{path}"))
            .header("x-api-key", key)
            .header("anthropic-version", ADMIN_API_VERSION)
            .query(&q)
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let msg = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("request failed");
            bail!("{path}: HTTP {status}: {msg}");
        }
        for bucket in body
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            rows.extend(
                bucket
                    .get("results")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
        }
        page = body
            .get("next_page")
            .and_then(Value::as_str)
            .map(str::to_string);
        if !body
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || page.is_none()
        {
            break;
        }
    }
    Ok(rows)
}

async fn org_report(
    http: &reqwest::Client,
    base: &str,
    account: &str,
    key: &str,
    days: u32,
) -> OrgReport {
    let start = (Utc::now() - Duration::days(days as i64))
        .format("%Y-%m-%dT00:00:00Z")
        .to_string();
    let mut r = OrgReport {
        account: account.to_string(),
        ..Default::default()
    };
    let usage_q = [
        ("starting_at", start.clone()),
        ("bucket_width", "1d".to_string()),
        ("group_by[]", "model".to_string()),
        ("limit", "31".to_string()),
    ];
    let cost_q = [("starting_at", start), ("limit", "31".to_string())];
    let (usage, cost) = tokio::join!(
        paged(
            http,
            base,
            key,
            "/v1/organizations/usage_report/messages",
            &usage_q
        ),
        paged(http, base, key, "/v1/organizations/cost_report", &cost_q),
    );
    match usage {
        Ok(rows) => rows
            .iter()
            .for_each(|row| absorb_usage_row(&mut r.models, row)),
        Err(e) => r.error = Some(e.to_string()),
    }
    match cost {
        Ok(rows) => rows
            .iter()
            .for_each(|row| absorb_cost_row(&mut r.cost, row)),
        Err(e) => {
            r.error = Some(
                r.error
                    .take()
                    .map_or(e.to_string(), |p| format!("{p}; {e}")),
            )
        }
    }
    r
}

/// Fetch reports for every account with an admin key. Accounts sharing an
/// admin key belong to the same organization and are only counted once.
pub async fn collect(cfg: &Config, days: u32) -> Result<Vec<OrgReport>> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let base = cfg.server.upstream.trim_end_matches('/');
    let mut seen = HashSet::new();
    let mut jobs = Vec::new();
    for a in &cfg.accounts {
        let Some(key) = &a.admin_key else { continue };
        if !seen.insert(key.clone()) {
            continue;
        }
        jobs.push(org_report(&http, base, &a.name, key, days));
    }
    if jobs.is_empty() {
        bail!("no account has an `admin_key`; add one (sk-ant-admin...) to use `usage`");
    }
    Ok(futures_util::future::join_all(jobs).await)
}

pub fn render(reports: &[OrgReport]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let mut total_models: BTreeMap<String, ModelTokens> = BTreeMap::new();
    let mut total_cost: BTreeMap<String, f64> = BTreeMap::new();
    let row = |out: &mut String, name: &str, t: &ModelTokens| {
        let _ = writeln!(
            out,
            "  {name:<28} {:>14} {:>14} {:>14} {:>14}",
            t.uncached_input, t.cache_read, t.cache_write, t.output
        );
    };
    for r in reports {
        let _ = writeln!(out, "== {} ==", r.account);
        if let Some(e) = &r.error {
            let _ = writeln!(out, "  error: {e}");
        }
        let _ = writeln!(
            out,
            "  {:<28} {:>14} {:>14} {:>14} {:>14}",
            "model", "input", "cache_read", "cache_write", "output"
        );
        for (m, t) in &r.models {
            row(&mut out, m, t);
            let e = total_models.entry(m.clone()).or_default();
            e.uncached_input += t.uncached_input;
            e.cache_read += t.cache_read;
            e.cache_write += t.cache_write;
            e.output += t.output;
        }
        for (c, v) in &r.cost {
            let _ = writeln!(out, "  cost: {v:.2} {c}");
            *total_cost.entry(c.clone()).or_default() += v;
        }
        out.push('\n');
    }
    let _ = writeln!(out, "== ALL ACCOUNTS ==");
    for (m, t) in &total_models {
        row(&mut out, m, t);
    }
    for (c, v) in &total_cost {
        let _ = writeln!(out, "  total cost: {v:.2} {c}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sums_rows() {
        let mut m = BTreeMap::new();
        absorb_usage_row(
            &mut m,
            &json!({"model":"claude-opus-5-5","uncached_input_tokens":10,"cache_read_input_tokens":5,"cache_creation":{"ephemeral_5m_input_tokens":1,"ephemeral_1h_input_tokens":2},"output_tokens":7}),
        );
        absorb_usage_row(
            &mut m,
            &json!({"model":"claude-opus-5-5","uncached_input_tokens":1,"output_tokens":1}),
        );
        let t = &m["claude-opus-5-5"];
        assert_eq!(
            (t.uncached_input, t.cache_read, t.cache_write, t.output),
            (11, 5, 3, 8)
        );
        let mut c = BTreeMap::new();
        absorb_cost_row(&mut c, &json!({"currency":"USD","amount":"1234.5"}));
        assert!((c["USD"] - 12.345).abs() < 1e-9);
    }
}
