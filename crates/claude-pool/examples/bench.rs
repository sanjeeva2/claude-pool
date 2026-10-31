//! Measures the gateway's added overhead against a zero-latency mock upstream.
//!
//!     cargo run --release --example bench -- [requests] [concurrency]

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::response::IntoResponse;
use claude_pool::{AppState, app, config::Config};

const TOKEN: &str = "cpk-bench-token-0123456789abcdef";

async fn mock() -> impl IntoResponse {
    (
        [("content-type", "application/json")],
        r#"{"id":"m","type":"message","model":"claude-opus-5-5","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":10,"output_tokens":20}}"#,
    )
}

async fn serve(router: axum::Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    use axum::serve::ListenerExt;
    let l = l.tap_io(|t| {
        let _ = t.set_nodelay(true);
    });
    tokio::spawn(async move { axum::serve(l, router).await.unwrap() });
    format!("http://{addr}")
}

async fn run(label: &str, url: String, n: usize, conc: usize, body: Arc<String>) {
    let http = reqwest::Client::builder()
        .pool_max_idle_per_host(conc)
        .build()
        .unwrap();
    // Warm up connections.
    for _ in 0..conc.min(64) {
        let _ = http
            .post(&url)
            .header("x-api-key", TOKEN)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await;
    }
    let per = n / conc;
    let start = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..conc {
        let (http, url, body) = (http.clone(), url.clone(), body.clone());
        set.spawn(async move {
            let mut lat = Vec::with_capacity(per);
            for _ in 0..per {
                let t = Instant::now();
                let r = http
                    .post(&url)
                    .header("x-api-key", TOKEN)
                    .header("content-type", "application/json")
                    .body(body.to_string())
                    .send()
                    .await
                    .unwrap();
                assert!(r.status().is_success());
                r.bytes().await.unwrap();
                lat.push(t.elapsed());
            }
            lat
        });
    }
    let mut all: Vec<Duration> = Vec::with_capacity(n);
    while let Some(r) = set.join_next().await {
        all.extend(r.unwrap());
    }
    let total = start.elapsed();
    all.sort();
    let p = |q: f64| all[((all.len() as f64 * q) as usize).min(all.len() - 1)];
    println!(
        "{label:<10} {:>9.0} req/s   p50 {:>7.1?}   p99 {:>7.1?}",
        all.len() as f64 / total.as_secs_f64(),
        p(0.50),
        p(0.99)
    );
}

#[tokio::main]
async fn main() {
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let n = *args.first().unwrap_or(&20_000);
    let conc = *args.get(1).unwrap_or(&64);

    let upstream =
        serve(axum::Router::new().route("/v1/messages", axum::routing::post(mock))).await;
    let toml = format!(
        "[server]\nupstream = \"{upstream}\"\nstate_file = \"\"\n\n\
         [[accounts]]\nname = \"a\"\napi_key = \"k1\"\n\n[[accounts]]\nname = \"b\"\napi_key = \"k2\"\n\n\
         [[accounts]]\nname = \"c\"\napi_key = \"k3\"\n\n[[clients]]\nname = \"bench\"\ntoken = \"{TOKEN}\"\n"
    );
    let state = AppState::new(Config::from_toml(&toml).unwrap()).unwrap();
    let gateway = serve(app(state.clone())).await;

    // A realistic ~8 KB request with a system prompt (exercises affinity hashing).
    let body = Arc::new(
        serde_json::json!({
            "model": "claude-opus-5-5", "max_tokens": 1024,
            "system": "You are a helpful assistant. ".repeat(250),
            "messages": [{"role": "user", "content": "Hello there"}]
        })
        .to_string(),
    );
    println!(
        "{n} requests, concurrency {conc}, body {} bytes",
        body.len()
    );
    run(
        "direct",
        format!("{upstream}/v1/messages"),
        n,
        conc,
        body.clone(),
    )
    .await;
    run("gateway", format!("{gateway}/v1/messages"), n, conc, body).await;
    let g = state.global.snapshot();
    println!(
        "gateway accounted {} requests, {} output tokens",
        g.requests, g.output_tokens
    );
}
