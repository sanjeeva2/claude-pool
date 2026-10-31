//! The hot path: authenticate the team member, pick an account, forward the
//! request, fail over on 429/5xx/connection errors, and stream the response
//! back while counting tokens.

use std::borrow::Cow;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::Stream;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use serde::Deserialize;
use serde_json::value::RawValue;
use subtle::ConstantTimeEq;
use xxhash_rust::xxh3::Xxh3;

use crate::AppState;
use crate::account::now_ms;
use crate::router::Router;
use crate::tap::UsageTap;

const AFFINITY_HEADER: &str = "x-claude-pool-affinity";

pub fn api_error(status: StatusCode, kind: &str, msg: &str) -> Response {
    let body = serde_json::json!({ "type": "error", "error": { "type": kind, "message": msg } });
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// Returns the client index, `Ok(None)` in open mode (no clients configured).
fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Option<usize>, &'static str> {
    if state.clients.is_empty() {
        return Ok(None);
    }
    let token = headers
        .get("x-api-key")
        .map(|v| v.as_bytes())
        .or_else(|| {
            headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.as_bytes().strip_prefix(b"Bearer "))
        })
        .ok_or("missing pool token")?;
    match state.client_by_token.get(token) {
        Some(&i) if bool::from(state.clients[i].token.ct_eq(token)) => Ok(Some(i)),
        _ => Err("invalid pool token"),
    }
}

#[derive(Deserialize)]
struct RequestMeta<'a> {
    #[serde(borrow)]
    model: Option<Cow<'a, str>>,
    #[serde(borrow)]
    tools: Option<&'a RawValue>,
    #[serde(borrow)]
    system: Option<&'a RawValue>,
    #[serde(borrow)]
    messages: Option<Vec<&'a RawValue>>,
}

/// Parses just enough of the request body to learn the model and derive a
/// prompt-prefix affinity key (tools → system → first message, the same order
/// the prompt cache uses). Unneeded fields are skipped without allocation.
fn inspect_body(body: &[u8], want_affinity: bool) -> (Option<String>, Option<u64>) {
    let Ok(meta) = serde_json::from_slice::<RequestMeta>(body) else {
        return (None, None);
    };
    let model = meta.model.map(Cow::into_owned);
    if !want_affinity {
        return (model, None);
    }
    let first = meta.messages.as_ref().and_then(|m| m.first());
    if meta.system.is_none() && first.is_none() {
        return (model, None);
    }
    let mut h = Xxh3::new();
    for part in [meta.tools, meta.system, first.copied()] {
        h.update(part.map(|r| r.get()).unwrap_or("").as_bytes());
        h.update(&[0xff]);
    }
    (model, Some(h.digest()))
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn upstream_headers(state: &AppState, incoming: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(incoming.len() + 2);
    for (name, value) in incoming {
        let n = name.as_str();
        if is_hop_by_hop(name)
            || matches!(
                n,
                "host" | "content-length" | "x-api-key" | "authorization" | "accept-encoding"
            )
            || n.starts_with("x-claude-pool-")
        {
            continue;
        }
        if n == "anthropic-beta" {
            // OAuth betas only make sense with the caller's own token; we swap in an API key.
            if let Ok(v) = value.to_str() {
                let kept: Vec<&str> = v
                    .split(',')
                    .map(str::trim)
                    .filter(|b| !b.is_empty() && !b.starts_with("oauth-"))
                    .collect();
                if !kept.is_empty()
                    && let Ok(hv) = HeaderValue::from_str(&kept.join(","))
                {
                    out.append(name.clone(), hv);
                }
            }
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    if !out.contains_key("anthropic-version")
        && let Ok(v) = HeaderValue::from_str(&state.config.server.default_anthropic_version)
    {
        out.insert("anthropic-version", v);
    }
    out
}

fn retry_after_secs(h: &HeaderMap) -> Option<u64> {
    h.get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .map(|s| s.ceil() as u64)
}

fn is_retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

pub async fn handle(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let started = Instant::now();
    let (parts, body) = req.into_parts();

    let client = match authenticate(&state, &parts.headers) {
        Ok(c) => c,
        Err(msg) => return api_error(StatusCode::UNAUTHORIZED, "authentication_error", msg),
    };
    if let Some(ci) = client {
        let c = &state.clients[ci];
        if c.over_budget() {
            return api_error(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                &format!("claude-pool: monthly budget for `{}` is exhausted", c.name),
            );
        }
    }

    let limit = state.config.server.max_body_mb << 20;
    let body = match to_bytes(body, limit).await {
        Ok(b) => b,
        Err(_) => {
            return api_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_too_large",
                "request body too large",
            );
        }
    };

    let is_json = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"));
    let (req_model, mut affinity) = if is_json && !body.is_empty() {
        inspect_body(&body, state.config.server.affinity)
    } else {
        (None, None)
    };
    if let Some(v) = parts.headers.get(AFFINITY_HEADER) {
        affinity = Some(xxhash_rust::xxh3::xxh3_64(v.as_bytes()));
    }

    let path_q = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let url = format!("{}{}", state.upstream, path_q);
    let headers = upstream_headers(&state, &parts.headers);
    let srv = &state.config.server;
    let router = Router {
        accounts: &state.accounts,
        min_affinity_headroom: srv.affinity_min_headroom,
    };

    let mut excluded: u64 = 0;
    let mut next = router.pick(affinity, excluded, now_ms());
    let mut attempt = 0usize;

    while let Some(idx) = next {
        attempt += 1;
        let acct = &state.accounts[idx];
        excluded |= 1 << idx;
        acct.inflight.fetch_add(1, Relaxed);
        let inflight = OwnedInflight {
            state: state.clone(),
            idx,
        };

        let mut h = headers.clone();
        h.insert("x-api-key", acct.api_key.clone());
        let result = state
            .http
            .request(parts.method.clone(), &url)
            .headers(h)
            .body(body.clone())
            .send()
            .await;
        acct.counters.requests.fetch_add(1, Relaxed);

        let resp = match result {
            Ok(r) => r,
            Err(e) => {
                acct.counters.errors.fetch_add(1, Relaxed);
                let secs = acct.mark_failure(None, srv.cooldown_base_secs, srv.cooldown_max_secs);
                tracing::warn!(account = %acct.name, error = %e, cooldown_secs = secs, "upstream connection failed");
                drop(inflight);
                next = if attempt < srv.max_attempts {
                    router.pick(affinity, excluded, now_ms())
                } else {
                    None
                };
                if next.is_none() {
                    state.global.errors.fetch_add(1, Relaxed);
                    if let Some(ci) = client {
                        state.clients[ci].counters.errors.fetch_add(1, Relaxed);
                    }
                    return api_error(
                        StatusCode::BAD_GATEWAY,
                        "api_error",
                        "claude-pool: upstream unreachable",
                    );
                }
                continue;
            }
        };

        let status = resp.status();
        acct.update_limits(resp.headers());

        let failover = if status == StatusCode::UNAUTHORIZED {
            acct.counters.errors.fetch_add(1, Relaxed);
            acct.disable_for(srv.auth_failure_disable_secs);
            tracing::error!(account = %acct.name, "upstream rejected API key (401); account disabled");
            true
        } else if is_retryable(status) {
            if status == StatusCode::TOO_MANY_REQUESTS {
                acct.counters.rate_limited.fetch_add(1, Relaxed);
            } else {
                acct.counters.errors.fetch_add(1, Relaxed);
            }
            let secs = acct.mark_failure(
                retry_after_secs(resp.headers()),
                srv.cooldown_base_secs,
                srv.cooldown_max_secs,
            );
            tracing::warn!(account = %acct.name, %status, cooldown_secs = secs, "upstream busy; failing over");
            true
        } else {
            if status.is_client_error() {
                acct.counters.errors.fetch_add(1, Relaxed);
            } else {
                acct.mark_success();
            }
            false
        };

        if failover && attempt < srv.max_attempts {
            next = router.pick(affinity, excluded, now_ms());
            if next.is_some() {
                drop(resp);
                drop(inflight);
                continue;
            }
        }

        // Forward this response (success, non-retryable error, or the last failure).
        let mut out_headers = HeaderMap::with_capacity(resp.headers().len() + 1);
        for (n, v) in resp.headers() {
            // Drop content-length so the body is always polled to its end,
            // which is where usage is recorded.
            if !is_hop_by_hop(n) && n != header::CONTENT_LENGTH {
                out_headers.append(n.clone(), v.clone());
            }
        }
        if srv.expose_account_header
            && let Ok(v) = HeaderValue::from_str(&acct.name)
        {
            out_headers.insert("x-claude-pool-account", v);
        }
        let mode = if status.is_success() {
            UsageTap::mode_for(
                resp.headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
            )
        } else {
            crate::tap::Mode::Off
        };
        let stream = TappedStream {
            inner: Box::pin(resp.bytes_stream()),
            tap: UsageTap::new(mode),
            done: false,
            ctx: Some(Finish {
                state: state.clone(),
                account: idx,
                client,
                req_model,
                status,
                started,
                _inflight: inflight,
            }),
        };
        let mut response = Response::new(Body::from_stream(stream));
        *response.status_mut() = status;
        *response.headers_mut() = out_headers;
        return response;
    }

    // Every account is cooling down, disabled, saturated or over budget.
    state.global.errors.fetch_add(1, Relaxed);
    if let Some(ci) = client {
        state.clients[ci].counters.errors.fetch_add(1, Relaxed);
    }
    let now = now_ms();
    let wait = state
        .accounts
        .iter()
        .map(|a| a.cooldown_until_ms.load(Relaxed).saturating_sub(now))
        .filter(|&w| w > 0)
        .min()
        .map(|ms| ms.div_ceil(1000))
        .unwrap_or(1);
    let mut r = api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "overloaded_error",
        "claude-pool: no account currently available (all cooling down, saturated or over budget)",
    );
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(wait));
    r
}

/// Decrements the account's in-flight counter when dropped.
struct OwnedInflight {
    state: Arc<AppState>,
    idx: usize,
}

impl Drop for OwnedInflight {
    fn drop(&mut self) {
        self.state.accounts[self.idx].inflight.fetch_sub(1, Relaxed);
    }
}

struct Finish {
    state: Arc<AppState>,
    account: usize,
    client: Option<usize>,
    req_model: Option<String>,
    status: StatusCode,
    started: Instant,
    _inflight: OwnedInflight,
}

impl Finish {
    fn record(self, tap: &mut UsageTap, completed: bool) {
        tap.finish();
        let st = &self.state;
        let acct = &st.accounts[self.account];
        let model = tap
            .model
            .take()
            .or(self.req_model)
            .unwrap_or_else(|| "unknown".into());
        let u = tap.usage;
        let cost = if tap.saw_usage {
            st.prices.cost_micros(&model, &u)
        } else {
            0
        };

        st.global.requests.fetch_add(1, Relaxed);
        let client = self.client.map(|i| &st.clients[i]);
        if let Some(c) = client {
            c.counters.requests.fetch_add(1, Relaxed);
        }
        if !self.status.is_success() {
            let counter = |c: &crate::stats::Counters| {
                if self.status == StatusCode::TOO_MANY_REQUESTS {
                    c.rate_limited.fetch_add(1, Relaxed);
                } else {
                    c.errors.fetch_add(1, Relaxed);
                }
            };
            counter(&st.global);
            if let Some(c) = client {
                counter(&c.counters);
            }
        }
        if tap.saw_usage {
            acct.counters.record_usage(&u, cost);
            st.global.record_usage(&u, cost);
            if let Some(c) = client {
                c.counters.record_usage(&u, cost);
            }
            st.models.record(&model, &u, cost);
        }
        tracing::info!(
            account = %acct.name,
            client = client.map(|c| c.name.as_str()).unwrap_or("-"),
            %model,
            status = self.status.as_u16(),
            ms = self.started.elapsed().as_millis() as u64,
            input = u.input,
            output = u.output,
            cache_read = u.cache_read,
            cost_usd = cost as f64 / 1e6,
            completed,
            "request"
        );
    }
}

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// Passes bytes straight through while feeding them to the usage tap.
/// Accounting happens exactly once: at end of stream, or on drop if the
/// client disconnects mid-stream.
struct TappedStream {
    inner: ByteStream,
    tap: UsageTap,
    done: bool,
    ctx: Option<Finish>,
}

impl Stream for TappedStream {
    type Item = Result<Bytes, reqwest::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        let this = &mut *self;
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.tap.feed(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.done = true;
                if let Some(f) = this.ctx.take() {
                    f.record(&mut this.tap, false);
                }
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.done = true;
                if let Some(f) = this.ctx.take() {
                    f.record(&mut this.tap, true);
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for TappedStream {
    fn drop(&mut self) {
        if let Some(f) = self.ctx.take() {
            f.record(&mut self.tap, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_key_ignores_later_turns() {
        let a = br#"{"model":"m","system":"S","messages":[{"role":"user","content":"hi"}],"max_tokens":5}"#;
        let b = br#"{"model":"m","max_tokens":99,"system":"S","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"yo"},{"role":"user","content":"more"}]}"#;
        let c = br#"{"model":"m","system":"OTHER","messages":[{"role":"user","content":"hi"}]}"#;
        let (m, ka) = inspect_body(a, true);
        let (_, kb) = inspect_body(b, true);
        let (_, kc) = inspect_body(c, true);
        assert_eq!(m.as_deref(), Some("m"));
        assert_eq!(ka, kb);
        assert_ne!(ka, kc);
    }

    #[test]
    fn strips_oauth_beta() {
        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-beta",
            "oauth-2025-04-20, files-api-2025-04-14".parse().unwrap(),
        );
        h.insert("x-api-key", "client-token".parse().unwrap());
        h.insert("x-claude-pool-affinity", "abc".parse().unwrap());
        let cfg =
            crate::config::Config::from_toml("[[accounts]]\nname='a'\napi_key='k'\n").unwrap();
        let st = AppState::new(cfg).unwrap();
        let out = upstream_headers(&st, &h);
        assert_eq!(out.get("anthropic-beta").unwrap(), "files-api-2025-04-14");
        assert!(out.get("x-api-key").is_none());
        assert!(out.get("x-claude-pool-affinity").is_none());
        assert_eq!(out.get("anthropic-version").unwrap(), "2023-06-01");
    }
}
