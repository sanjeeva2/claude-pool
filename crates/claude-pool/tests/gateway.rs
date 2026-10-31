//! End-to-end tests: a mock Claude API upstream + the real gateway on loopback.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use claude_pool::{AppState, app, config::Config};
use parking_lot::Mutex;

const TOKEN: &str = "cpk-test-token-0123456789abcdef";

const SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":100,\"cache_read_input_tokens\":50,\"output_tokens\":1}}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":30}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

#[derive(Default)]
struct Mock {
    hits: Mutex<HashMap<String, usize>>,
    violations: Mutex<Vec<String>>,
}

async fn mock_messages(State(m): State<Arc<Mock>>, headers: HeaderMap, body: Bytes) -> Response {
    let key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if headers.contains_key("authorization") || key == TOKEN {
        m.violations
            .lock()
            .push("client credential leaked upstream".into());
    }
    if !headers.contains_key("anthropic-version") {
        m.violations.lock().push("missing anthropic-version".into());
    }
    *m.hits.lock().entry(key.clone()).or_default() += 1;

    let rl = [
        ("anthropic-ratelimit-requests-limit", "1000"),
        ("anthropic-ratelimit-requests-remaining", "900"),
    ];
    match key.as_str() {
        "k-429" => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", "30")],
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
            )
                .into_response();
        }
        "k-401" => return (StatusCode::UNAUTHORIZED, "{}").into_response(),
        "k-500" => return (StatusCode::INTERNAL_SERVER_ERROR, "{}").into_response(),
        _ => {}
    }
    if key.starts_with("k-slow") {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    if v.get("stream").and_then(|s| s.as_bool()) == Some(true) {
        return (
            StatusCode::OK,
            rl,
            [("content-type", "text/event-stream")],
            SSE,
        )
            .into_response();
    }
    let json = serde_json::json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
        "content": [{"type": "text", "text": format!("served by {key}")}],
        "usage": {"input_tokens": 10, "output_tokens": 20}
    });
    (
        StatusCode::OK,
        rl,
        [("content-type", "application/json")],
        json.to_string(),
    )
        .into_response()
}

async fn start_mock() -> (String, Arc<Mock>) {
    let mock = Arc::new(Mock::default());
    let r = axum::Router::new()
        .route("/v1/messages", axum::routing::post(mock_messages))
        .with_state(mock.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    (format!("http://{addr}"), mock)
}

struct Gw {
    url: String,
    state: Arc<AppState>,
    mock: Arc<Mock>,
    http: reqwest::Client,
}

/// `accounts`: (name, key, weight). `extra` is appended to the TOML.
async fn start(accounts: &[(&str, &str, u32)], affinity: bool, extra: &str) -> Gw {
    let (upstream, mock) = start_mock().await;
    let mut toml = format!(
        "[server]\nupstream = \"{upstream}\"\nstate_file = \"\"\naffinity = {affinity}\nadmin_token = \"admin-secret\"\n\n"
    );
    for (n, k, w) in accounts {
        toml += &format!("[[accounts]]\nname = \"{n}\"\napi_key = \"{k}\"\nweight = {w}\n\n");
    }
    toml += &format!("[[clients]]\nname = \"alice\"\ntoken = \"{TOKEN}\"\n{extra}\n");
    let state = AppState::new(Config::from_toml(&toml).unwrap()).unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    Gw {
        url: format!("http://{addr}"),
        state,
        mock,
        http: reqwest::Client::new(),
    }
}

impl Gw {
    async fn send(&self, body: serde_json::Value) -> reqwest::Response {
        self.http
            .post(format!("{}/v1/messages", self.url))
            .header("x-api-key", TOKEN)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    fn assert_clean(&self) {
        assert!(
            self.mock.violations.lock().is_empty(),
            "{:?}",
            self.mock.violations.lock()
        );
    }

    /// Accounting runs when the body finishes; give it a moment.
    async fn wait_for(&self, f: impl Fn(&AppState) -> bool) {
        for _ in 0..200 {
            if f(&self.state) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition not reached");
    }
}

fn msg(system: &str, turns: usize) -> serde_json::Value {
    let messages: Vec<_> = (0..turns).map(|i| serde_json::json!({"role": if i % 2 == 0 {"user"} else {"assistant"}, "content": format!("turn {i}")})).collect();
    serde_json::json!({"model": "claude-opus-5-5", "max_tokens": 64, "system": system, "messages": messages})
}

fn snap(st: &AppState, i: usize) -> claude_pool::stats::CountersSnapshot {
    st.accounts[i].counters.snapshot()
}

#[tokio::test]
async fn fails_over_on_429_and_cools_down() {
    let gw = start(&[("busy", "k-429", 1000), ("ok", "k-ok", 1)], false, "").await;
    let r = gw.send(msg("s", 1)).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-claude-pool-account"], "ok");
    assert!(r.text().await.unwrap().contains("served by k-ok"));
    // The busy account is now cooling down for retry-after=30s: no more hits.
    for _ in 0..5 {
        assert_eq!(gw.send(msg("s", 1)).await.status(), 200);
    }
    assert_eq!(gw.mock.hits.lock()["k-429"], 1);
    assert_eq!(snap(&gw.state, 0).rate_limited, 1);
    assert_eq!(
        gw.state.accounts[0].state_label(claude_pool::account::now_ms()),
        "cooling_down"
    );
    gw.assert_clean();
}

#[tokio::test]
async fn fails_over_on_500_and_401() {
    let gw = start(
        &[
            ("broken", "k-500", 1000),
            ("revoked", "k-401", 1000),
            ("ok", "k-ok", 1),
        ],
        false,
        "",
    )
    .await;
    let r = gw.send(msg("s", 1)).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-claude-pool-account"], "ok");
    let now = claude_pool::account::now_ms();
    assert_eq!(gw.state.accounts[1].state_label(now), "disabled");
    assert_eq!(gw.state.accounts[0].state_label(now), "cooling_down");
}

#[tokio::test]
async fn returns_last_error_then_503_when_pool_exhausted() {
    let gw = start(&[("busy", "k-429", 1)], false, "").await;
    let r = gw.send(msg("s", 1)).await;
    assert_eq!(
        r.status(),
        429,
        "single account: upstream 429 is passed through"
    );
    let r = gw.send(msg("s", 1)).await;
    assert_eq!(r.status(), 503);
    let wait: u64 = r.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=30).contains(&wait));
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["error"]["type"], "overloaded_error");
}

#[tokio::test]
async fn streaming_passthrough_and_usage() {
    let gw = start(&[("ok", "k-ok", 1)], false, "").await;
    let mut body = msg("s", 1);
    body["stream"] = true.into();
    let r = gw.send(body).await;
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "text/event-stream");
    assert_eq!(
        r.text().await.unwrap(),
        SSE,
        "stream must be byte-identical"
    );
    gw.wait_for(|s| s.clients[0].counters.snapshot().output_tokens == 30)
        .await;
    let c = gw.state.clients[0].counters.snapshot();
    assert_eq!(
        (
            c.input_tokens,
            c.cache_read_tokens,
            c.output_tokens,
            c.requests
        ),
        (100, 50, 30, 1)
    );
    // opus-5-5: 100*4 + 50*0.20 + 30*20 per 1M = 0.00101 USD
    assert!((c.cost_usd - 0.00101).abs() < 1e-9, "{}", c.cost_usd);
    assert_eq!(gw.state.models.snapshot()["claude-opus-5-5"].requests, 1);
    assert_eq!(
        gw.state.accounts[0]
            .inflight
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[tokio::test]
async fn auth_is_enforced() {
    let gw = start(&[("ok", "k-ok", 1)], false, "").await;
    let url = format!("{}/v1/messages", gw.url);
    let none = gw.http.post(&url).json(&msg("s", 1)).send().await.unwrap();
    assert_eq!(none.status(), 401);
    let bad = gw
        .http
        .post(&url)
        .header("x-api-key", "cpk-wrong-token-xxxxxxxxxxxx")
        .json(&msg("s", 1))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 401);
    // Bearer auth, as sent by newer SDKs; also no anthropic-version header -> default added.
    let ok = gw
        .http
        .post(&url)
        .bearer_auth(TOKEN)
        .json(&msg("s", 1))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    assert!(
        gw.mock
            .hits
            .lock()
            .get("cpk-wrong-token-xxxxxxxxxxxx")
            .is_none()
    );
    gw.assert_clean();

    let stats = gw
        .http
        .get(format!("{}/pool/stats", gw.url))
        .send()
        .await
        .unwrap();
    assert_eq!(stats.status(), 401);
    let stats = gw
        .http
        .get(format!("{}/pool/stats", gw.url))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(stats.status(), 200);
    let metrics = gw
        .http
        .get(format!("{}/metrics", gw.url))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("claude_pool_account_up{account=\"ok\"} 1"));
}

#[tokio::test]
async fn affinity_keeps_conversations_on_one_account() {
    let gw = start(
        &[("a", "k-ok-a", 1), ("b", "k-ok-b", 1), ("c", "k-ok-c", 1)],
        true,
        "",
    )
    .await;
    let mut first = None;
    for turns in 1..15 {
        let r = gw.send(msg("long shared system prompt", turns)).await;
        let acct = r.headers()["x-claude-pool-account"]
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(
            first.get_or_insert(acct.clone()),
            &acct,
            "conversation moved accounts at turn {turns}"
        );
    }
    let mut seen = std::collections::HashSet::new();
    for i in 0..40 {
        let r = gw.send(msg(&format!("system {i}"), 1)).await;
        seen.insert(
            r.headers()["x-claude-pool-account"]
                .to_str()
                .unwrap()
                .to_string(),
        );
    }
    assert_eq!(
        seen.len(),
        3,
        "different prompts should spread over all accounts"
    );
}

#[tokio::test]
async fn explicit_affinity_header_wins() {
    let gw = start(
        &[("a", "k-ok-a", 1), ("b", "k-ok-b", 1), ("c", "k-ok-c", 1)],
        true,
        "",
    )
    .await;
    let mut seen = std::collections::HashSet::new();
    for i in 0..20 {
        let r = gw
            .http
            .post(format!("{}/v1/messages", gw.url))
            .header("x-api-key", TOKEN)
            .header("x-claude-pool-affinity", "project-42")
            .json(&msg(&format!("different {i}"), 1))
            .send()
            .await
            .unwrap();
        seen.insert(
            r.headers()["x-claude-pool-account"]
                .to_str()
                .unwrap()
                .to_string(),
        );
    }
    assert_eq!(seen.len(), 1);
}

#[tokio::test]
async fn client_budget_is_enforced() {
    let gw = start(
        &[("ok", "k-ok", 1)],
        false,
        "monthly_budget_usd = 0.0000001",
    )
    .await;
    assert_eq!(gw.send(msg("s", 1)).await.status(), 200);
    gw.wait_for(|s| s.clients[0].counters.snapshot().requests == 1)
        .await;
    let r = gw.send(msg("s", 1)).await;
    assert_eq!(r.status(), 429);
    assert!(r.text().await.unwrap().contains("budget"));
    assert_eq!(gw.mock.hits.lock()["k-ok"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_load_is_balanced_and_counted_exactly() {
    let gw = Arc::new(
        start(
            &[
                ("a", "k-slow-a", 1),
                ("b", "k-slow-b", 1),
                ("c", "k-slow-c", 1),
                ("d", "k-slow-d", 1),
            ],
            false,
            "",
        )
        .await,
    );
    const N: usize = 800;
    let mut set = tokio::task::JoinSet::new();
    for i in 0..N {
        let gw = gw.clone();
        set.spawn(async move {
            let r = gw.send(msg(&format!("p{i}"), 1)).await;
            assert_eq!(r.status(), 200);
            r.bytes().await.unwrap();
        });
    }
    while let Some(r) = set.join_next().await {
        r.unwrap();
    }
    gw.wait_for(|s| s.global.snapshot().requests == N as u64)
        .await;
    let g = gw.state.global.snapshot();
    assert_eq!(
        (g.input_tokens, g.output_tokens),
        (10 * N as u64, 20 * N as u64)
    );
    let per: Vec<u64> = (0..4).map(|i| snap(&gw.state, i).requests).collect();
    assert_eq!(per.iter().sum::<u64>(), N as u64);
    for p in &per {
        assert!(*p > (N / 4 / 2) as u64, "unbalanced: {per:?}");
    }
    for a in &gw.state.accounts {
        assert_eq!(a.inflight.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
    gw.assert_clean();
}

#[tokio::test]
async fn usage_survives_restart() {
    let gw = start(&[("ok", "k-ok", 1)], false, "").await;
    assert_eq!(gw.send(msg("s", 1)).await.status(), 200);
    gw.wait_for(|s| s.global.snapshot().requests == 1).await;

    let dir = std::env::temp_dir().join(format!("claude-pool-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.json");
    gw.state.persisted().save(path.to_str().unwrap()).unwrap();

    let toml = format!(
        "[server]\nstate_file = \"{}\"\n[[accounts]]\nname = \"ok\"\napi_key = \"k\"\n[[clients]]\nname = \"alice\"\ntoken = \"{TOKEN}\"\n",
        path.display()
    );
    let restored = AppState::new(Config::from_toml(&toml).unwrap()).unwrap();
    assert_eq!(restored.global.snapshot(), gw.state.global.snapshot());
    assert_eq!(restored.clients[0].counters.snapshot().output_tokens, 20);
    assert_eq!(restored.accounts[0].counters.snapshot().requests, 1);
    assert!(
        restored.clients[0].counters.month_cost_usd() > 0.0,
        "monthly budget spend must persist"
    );
    std::fs::remove_dir_all(dir).ok();
}
