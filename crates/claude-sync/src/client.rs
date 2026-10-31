//! Client side: pull/apply, publish, and the always-connected agent.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

use crate::apply::{self, LocalState, Paths, Report};
use crate::bundle::{Manifest, Revision, RevisionSummary, scan_dir, scan_secrets, sha256_hex};
use crate::config::ClientConfig;
use crate::proto::*;
use crate::secrets;

#[derive(Clone)]
pub struct HubClient {
    base: String,
    token: String,
    http: reqwest::Client,
}

async fn check(resp: reqwest::Response) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    let mut msg = body
        .get("error")
        .and_then(|e| e.as_str())
        .unwrap_or("request failed")
        .to_string();
    if let Some(p) = body.get("problems").and_then(|p| p.as_array()) {
        for x in p {
            msg.push_str(&format!("\n  - {}", x.as_str().unwrap_or_default()));
        }
    }
    bail!("hub returned {status}: {msg}")
}

impl HubClient {
    pub fn new(cfg: &ClientConfig) -> Result<Self> {
        Ok(Self {
            base: cfg.hub.trim_end_matches('/').to_string(),
            token: cfg.token.clone(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()?,
        })
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
    }

    pub async fn head(&self) -> Result<Head> {
        Ok(check(
            self.get("/api/head")
                .send()
                .await
                .context("contacting hub")?,
        )
        .await?
        .json()
        .await?)
    }

    pub async fn status(&self) -> Result<StatusResponse> {
        Ok(check(self.get("/api/status").send().await?)
            .await?
            .json()
            .await?)
    }

    pub async fn revs(&self) -> Result<Vec<RevisionSummary>> {
        Ok(check(self.get("/api/revs").send().await?)
            .await?
            .json()
            .await?)
    }

    pub async fn rev(&self, n: u64) -> Result<Revision> {
        Ok(check(self.get(&format!("/api/revs/{n}")).send().await?)
            .await?
            .json()
            .await?)
    }

    pub async fn rollback(&self, n: u64) -> Result<PublishResponse> {
        Ok(
            check(self.post(&format!("/api/rollback/{n}")).send().await?)
                .await?
                .json()
                .await?,
        )
    }

    /// Fetch file contents, using (and filling) the local content-addressed cache.
    pub async fn fetch(
        &self,
        paths: &Paths,
        files: &Manifest,
    ) -> Result<BTreeMap<String, Vec<u8>>> {
        let cache = paths.home.join("cache");
        std::fs::create_dir_all(&cache)?;
        let mut out = BTreeMap::new();
        for (path, sha) in files {
            let cached = cache.join(sha);
            let data = match std::fs::read(&cached) {
                Ok(d) if sha256_hex(&d) == *sha => d,
                _ => {
                    let d = check(self.get(&format!("/api/blobs/{sha}")).send().await?)
                        .await?
                        .bytes()
                        .await?
                        .to_vec();
                    if sha256_hex(&d) != *sha {
                        bail!("hub sent corrupted content for {path}");
                    }
                    crate::fsutil::write_atomic(&cached, &d)?;
                    d
                }
            };
            out.insert(path.clone(), data);
        }
        Ok(out)
    }

    /// Upload only the blobs the hub lacks, then publish the manifest.
    pub async fn publish_dir(
        &self,
        dir: &Path,
        message: &str,
        allow_secrets: bool,
    ) -> Result<PublishResponse> {
        let (manifest, blobs) = scan_dir(dir)?;
        if !allow_secrets {
            let mut hits = Vec::new();
            for (path, sha) in &manifest {
                hits.extend(scan_secrets(path, &blobs[sha]));
            }
            if !hits.is_empty() {
                bail!(
                    "refusing to publish what look like credentials:\n  {}\nUse {{{{secret:NAME}}}} placeholders instead (each person sets values with `claude-sync secrets set`).",
                    hits.join("\n  ")
                );
            }
        }
        let head = self.head().await?;
        let shas: Vec<String> = blobs.keys().cloned().collect();
        let missing: MissingResponse = check(
            self.post("/api/blobs/missing")
                .json(&MissingRequest { shas })
                .send()
                .await?,
        )
        .await?
        .json()
        .await?;
        for sha in &missing.missing {
            let data = blobs.get(sha).context("hub asked for an unknown blob")?;
            check(
                self.http
                    .put(format!("{}/api/blobs/{sha}", self.base))
                    .bearer_auth(&self.token)
                    .body(data.clone())
                    .send()
                    .await?,
            )
            .await?;
        }
        let req = PublishRequest {
            base_rev: head.rev,
            message: message.to_string(),
            files: manifest,
        };
        Ok(check(self.post("/api/publish").json(&req).send().await?)
            .await?
            .json()
            .await?)
    }
}

/// Pull the hub's current revision and apply it.
pub async fn pull(paths: &Paths, client: &HubClient, dry_run: bool) -> Result<Report> {
    let head = client.head().await?;
    let files = client.fetch(paths, &head.files).await?;
    let secrets = secrets::load(&paths.secrets_file())?;
    let paths2 = paths.clone();
    // File work is blocking; keep it off the async workers.
    tokio::task::spawn_blocking(move || {
        let mut state = LocalState::load(&paths2)?;
        let report = apply::apply(&paths2, &mut state, head.rev, &files, &secrets, dry_run)?;
        if !dry_run {
            state.save(&paths2)?;
        }
        Ok(report)
    })
    .await?
}

fn ws_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}/ws")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}/ws")
    } else {
        format!("{base}/ws")
    }
}

pub fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-host".into())
}

/// Stay connected to the hub and apply every new revision as soon as it is
/// published. Reconnects forever with backoff. `on_report` sees each apply.
pub async fn connect(
    paths: Paths,
    cfg: ClientConfig,
    on_report: impl Fn(&Report) + Send + Sync + 'static,
) -> Result<()> {
    let client = HubClient::new(&cfg)?;
    let mut backoff = 1u64;
    loop {
        match session(&paths, &cfg, &client, &on_report).await {
            Ok(()) => backoff = 1,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), retry_secs = backoff, "hub connection lost")
            }
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

async fn session(
    paths: &Paths,
    cfg: &ClientConfig,
    client: &HubClient,
    on_report: &(impl Fn(&Report) + Send + Sync),
) -> Result<()> {
    let mut req = ws_url(&cfg.hub).into_client_request()?;
    req.headers_mut()
        .insert("authorization", format!("Bearer {}", cfg.token).parse()?);
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .context("connecting to hub")?;
    let (mut tx, mut rx) = ws.split();
    let applied = LocalState::load(paths)?.applied_rev;
    tx.send(Message::Text(
        serde_json::to_string(&WsMsg::Hello {
            host: hostname(),
            applied_rev: applied,
        })?
        .into(),
    ))
    .await?;
    tracing::info!(hub = %cfg.hub, "connected; waiting for team updates");
    while let Some(msg) = rx.next().await {
        match msg? {
            Message::Text(t) => {
                if let Ok(WsMsg::Head { rev }) = serde_json::from_str::<WsMsg>(&t)
                    && rev != LocalState::load(paths)?.applied_rev
                {
                    let report = pull(paths, client, false).await?;
                    on_report(&report);
                    let msg = WsMsg::Applied {
                        rev: report.rev,
                        conflicts: report.conflicts.len(),
                    };
                    tx.send(Message::Text(serde_json::to_string(&msg)?.into()))
                        .await?;
                }
            }
            Message::Ping(p) => tx.send(Message::Pong(p)).await?,
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}

/// Publish `dir` now and again whenever its contents change (polling).
pub async fn watch_publish(client: &HubClient, dir: &Path, allow_secrets: bool) -> Result<()> {
    let mut last: Option<Manifest> = None;
    loop {
        match scan_dir(dir) {
            Ok((m, _)) if last.as_ref() != Some(&m) => {
                match client
                    .publish_dir(dir, "auto-publish (watch)", allow_secrets)
                    .await
                {
                    Ok(r) => {
                        tracing::info!(rev = r.rev, unchanged = r.unchanged, "published");
                        last = Some(m);
                    }
                    // Retried on the next tick (e.g. after someone else's publish: 409).
                    Err(e) => tracing::error!(error = %format!("{e:#}"), "publish failed"),
                }
            }
            Ok(_) => {}
            Err(e) => tracing::error!(error = %format!("{e:#}"), "cannot read team directory"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
