//! The hub: stores the team bundle as content-addressed blobs plus a
//! revision history, and pushes new revisions to connected members over
//! WebSocket the moment they are published.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use axum::Json;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Deserialize;
use subtle::ConstantTimeEq;
use tokio::sync::watch;

use crate::bundle::{
    MAX_FILE_BYTES, MAX_FILES, Revision, RevisionSummary, is_sha, scan_secrets, sha256_hex,
    validate_path,
};
use crate::proto::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Receives the team setup.
    Member,
    /// Can also publish and roll back.
    Publisher,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberConfig {
    pub name: String,
    pub token: String,
    #[serde(default = "default_role")]
    pub role: Role,
}

fn default_role() -> Role {
    Role::Member
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HubConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_data")]
    pub data_dir: PathBuf,
    /// Reject publishes containing things that look like credentials.
    #[serde(default = "yes")]
    pub block_secrets: bool,
    pub members: Vec<MemberConfig>,
}

fn default_listen() -> String {
    "127.0.0.1:8790".into()
}
fn default_data() -> PathBuf {
    "claude-sync-hub".into()
}
fn yes() -> bool {
    true
}

impl HubConfig {
    pub fn from_toml(text: &str) -> Result<Self> {
        let mut c: HubConfig = toml::from_str(text).context("parsing hub config")?;
        let mut names = std::collections::HashSet::new();
        let mut tokens = std::collections::HashSet::new();
        for m in &mut c.members {
            m.token = crate::config::resolve_secret(&m.token)
                .with_context(|| format!("member `{}`", m.name))?;
            if m.token.len() < 16 {
                bail!(
                    "member `{}` token is too short (min 16 chars; use `claude-sync gen-token`)",
                    m.name
                );
            }
            if !names.insert(m.name.clone()) || !tokens.insert(m.token.clone()) {
                bail!("duplicate member name or token (`{}`)", m.name);
            }
        }
        if c.members.is_empty() {
            bail!("at least one [[members]] entry is required");
        }
        Ok(c)
    }
}

struct Store {
    dir: PathBuf,
    revs: Vec<Revision>,
}

impl Store {
    fn open(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(dir.join("blobs"))?;
        std::fs::create_dir_all(dir.join("revs"))?;
        let mut revs = Vec::new();
        for e in std::fs::read_dir(dir.join("revs"))? {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "json") {
                let r: Revision = serde_json::from_slice(&std::fs::read(&p)?)
                    .with_context(|| format!("reading {}", p.display()))?;
                revs.push(r);
            }
        }
        revs.sort_by_key(|r| r.rev);
        Ok(Self { dir, revs })
    }

    fn head(&self) -> Head {
        self.revs.last().map_or(
            Head {
                rev: 0,
                files: Default::default(),
            },
            |r| Head {
                rev: r.rev,
                files: r.files.clone(),
            },
        )
    }

    fn blob_path(&self, sha: &str) -> PathBuf {
        self.dir.join("blobs").join(sha)
    }

    fn commit(
        &mut self,
        author: &str,
        message: &str,
        files: crate::bundle::Manifest,
    ) -> Result<u64> {
        let rev = self.revs.last().map_or(1, |r| r.rev + 1);
        let r = Revision {
            rev,
            author: author.into(),
            message: message.into(),
            time: chrono::Utc::now(),
            files,
        };
        crate::fsutil::write_atomic(
            &self.dir.join("revs").join(format!("{rev:08}.json")),
            &serde_json::to_vec_pretty(&r)?,
        )?;
        self.revs.push(r);
        Ok(rev)
    }
}

pub struct Hub {
    members: Vec<MemberConfig>,
    by_token: HashMap<Vec<u8>, usize>,
    block_secrets: bool,
    store: Mutex<Store>,
    head_tx: watch::Sender<u64>,
    presence: Mutex<HashMap<u64, Presence>>,
    next_conn: AtomicU64,
}

impl Hub {
    pub fn new(cfg: HubConfig) -> Result<Arc<Self>> {
        let store = Store::open(cfg.data_dir.clone())?;
        let (head_tx, _) = watch::channel(store.head().rev);
        Ok(Arc::new(Self {
            by_token: cfg
                .members
                .iter()
                .enumerate()
                .map(|(i, m)| (m.token.as_bytes().to_vec(), i))
                .collect(),
            members: cfg.members,
            block_secrets: cfg.block_secrets,
            store: Mutex::new(store),
            head_tx,
            presence: Mutex::new(HashMap::new()),
            next_conn: AtomicU64::new(1),
        }))
    }

    #[allow(clippy::result_large_err)] // the error is the ready-made HTTP response
    fn auth(&self, headers: &HeaderMap) -> Result<&MemberConfig, Response> {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.as_bytes().strip_prefix(b"Bearer "))
            .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "missing token"))?;
        match self.by_token.get(token) {
            Some(&i) if bool::from(self.members[i].token.as_bytes().ct_eq(token)) => {
                Ok(&self.members[i])
            }
            _ => Err(err(StatusCode::UNAUTHORIZED, "invalid token")),
        }
    }

    #[allow(clippy::result_large_err)]
    fn publisher(&self, headers: &HeaderMap) -> Result<&MemberConfig, Response> {
        let m = self.auth(headers)?;
        if m.role != Role::Publisher {
            return Err(err(StatusCode::FORBIDDEN, "your role cannot publish"));
        }
        Ok(m)
    }
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(serde_json::json!({ "error": msg }))).into_response()
}

pub fn app(hub: Arc<Hub>) -> axum::Router {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/head", get(head))
        .route("/api/status", get(status))
        .route("/api/revs", get(revs))
        .route("/api/revs/{rev}", get(rev))
        .route("/api/blobs/missing", post(missing))
        .route("/api/blobs/{sha}", get(get_blob).put(put_blob))
        .route("/api/publish", post(publish))
        .route("/api/rollback/{rev}", post(rollback))
        .route("/ws", get(ws))
        .route("/health", get(|| async { "ok" }))
        .layer(axum::extract::DefaultBodyLimit::max(
            MAX_FILE_BYTES + 1024 * 1024,
        ))
        .with_state(hub)
}

async fn head(State(h): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Err(r) = h.auth(&headers) {
        return r;
    }
    Json(h.store.lock().head()).into_response()
}

async fn status(State(h): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Err(r) = h.auth(&headers) {
        return r;
    }
    let mut connected: Vec<Presence> = h.presence.lock().values().cloned().collect();
    connected.sort_by(|a, b| a.member.cmp(&b.member).then(a.host.cmp(&b.host)));
    Json(StatusResponse {
        rev: *h.head_tx.borrow(),
        connected,
    })
    .into_response()
}

async fn revs(State(h): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if let Err(r) = h.auth(&headers) {
        return r;
    }
    let list: Vec<RevisionSummary> = h
        .store
        .lock()
        .revs
        .iter()
        .rev()
        .map(RevisionSummary::from)
        .collect();
    Json(list).into_response()
}

async fn rev(State(h): State<Arc<Hub>>, headers: HeaderMap, Path(n): Path<u64>) -> Response {
    if let Err(r) = h.auth(&headers) {
        return r;
    }
    match h.store.lock().revs.iter().find(|r| r.rev == n) {
        Some(r) => Json(r.clone()).into_response(),
        None => err(StatusCode::NOT_FOUND, "no such revision"),
    }
}

async fn missing(
    State(h): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(req): Json<MissingRequest>,
) -> Response {
    if let Err(r) = h.publisher(&headers) {
        return r;
    }
    let store = h.store.lock();
    let missing = req
        .shas
        .into_iter()
        .filter(|s| is_sha(s) && !store.blob_path(s).exists())
        .collect();
    Json(MissingResponse { missing }).into_response()
}

async fn get_blob(
    State(h): State<Arc<Hub>>,
    headers: HeaderMap,
    Path(sha): Path<String>,
) -> Response {
    if let Err(r) = h.auth(&headers) {
        return r;
    }
    if !is_sha(&sha) {
        return err(StatusCode::BAD_REQUEST, "bad hash");
    }
    let path = h.store.lock().blob_path(&sha);
    match tokio::fs::read(path).await {
        Ok(d) => ([(header::CONTENT_TYPE, "application/octet-stream")], d).into_response(),
        Err(_) => err(StatusCode::NOT_FOUND, "no such blob"),
    }
}

async fn put_blob(
    State(h): State<Arc<Hub>>,
    headers: HeaderMap,
    Path(sha): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(r) = h.publisher(&headers) {
        return r;
    }
    if !is_sha(&sha) || sha256_hex(&body) != sha {
        return err(StatusCode::BAD_REQUEST, "content does not match hash");
    }
    if body.len() > MAX_FILE_BYTES {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "file too large");
    }
    let path = h.store.lock().blob_path(&sha);
    match crate::fsutil::write_atomic(&path, &body) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn publish(
    State(h): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(req): Json<PublishRequest>,
) -> Response {
    let author = match h.publisher(&headers) {
        Ok(m) => m.name.clone(),
        Err(r) => return r,
    };
    if req.files.len() > MAX_FILES {
        return err(StatusCode::BAD_REQUEST, "too many files");
    }
    let mut problems = Vec::new();
    {
        let store = h.store.lock();
        for (path, sha) in &req.files {
            if let Err(e) = validate_path(path) {
                problems.push(e.to_string());
                continue;
            }
            if !is_sha(sha) {
                problems.push(format!("{path}: bad hash"));
                continue;
            }
            match std::fs::read(store.blob_path(sha)) {
                Err(_) => problems.push(format!("{path}: content not uploaded")),
                Ok(data) if h.block_secrets => problems.extend(scan_secrets(path, &data)),
                Ok(_) => {}
            }
        }
    }
    if !problems.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": "publish rejected", "problems": problems })),
        )
            .into_response();
    }
    let rev = {
        let mut store = h.store.lock();
        let head = store.head();
        if req.base_rev != head.rev {
            return err(
                StatusCode::CONFLICT,
                &format!(
                    "team setup changed (now revision {}); pull and retry",
                    head.rev
                ),
            );
        }
        if head.files == req.files {
            return Json(PublishResponse {
                rev: head.rev,
                unchanged: true,
            })
            .into_response();
        }
        match store.commit(&author, &req.message, req.files) {
            Ok(r) => r,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    };
    tracing::info!(rev, %author, message = %req.message, "published");
    h.head_tx.send_replace(rev);
    Json(PublishResponse {
        rev,
        unchanged: false,
    })
    .into_response()
}

async fn rollback(State(h): State<Arc<Hub>>, headers: HeaderMap, Path(n): Path<u64>) -> Response {
    let author = match h.publisher(&headers) {
        Ok(m) => m.name.clone(),
        Err(r) => return r,
    };
    let rev = {
        let mut store = h.store.lock();
        let Some(old) = store.revs.iter().find(|r| r.rev == n).cloned() else {
            return err(StatusCode::NOT_FOUND, "no such revision");
        };
        match store.commit(&author, &format!("rollback to revision {n}"), old.files) {
            Ok(r) => r,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    };
    h.head_tx.send_replace(rev);
    Json(PublishResponse {
        rev,
        unchanged: false,
    })
    .into_response()
}

async fn ws(State(h): State<Arc<Hub>>, headers: HeaderMap, up: WebSocketUpgrade) -> Response {
    let member = match h.auth(&headers) {
        Ok(m) => m.name.clone(),
        Err(r) => return r,
    };
    up.on_upgrade(move |sock| session(h, member, sock))
}

async fn session(h: Arc<Hub>, member: String, sock: WebSocket) {
    let id = h.next_conn.fetch_add(1, Ordering::Relaxed);
    let (mut tx, mut rx) = sock.split();
    let mut head_rx = h.head_tx.subscribe();
    let initial = *head_rx.borrow_and_update();
    let send = |rev| {
        Message::Text(
            serde_json::to_string(&WsMsg::Head { rev })
                .unwrap_or_default()
                .into(),
        )
    };
    if tx.send(send(initial)).await.is_err() {
        return;
    }
    let mut ping = tokio::time::interval(std::time::Duration::from_secs(25));
    loop {
        tokio::select! {
            changed = head_rx.changed() => {
                if changed.is_err() { break; }
                let rev = *head_rx.borrow_and_update();
                if tx.send(send(rev)).await.is_err() { break; }
            }
            msg = rx.next() => {
                let Some(Ok(msg)) = msg else { break };
                let Message::Text(t) = msg else { continue };
                match serde_json::from_str::<WsMsg>(&t) {
                    Ok(WsMsg::Hello { host, applied_rev }) => {
                        h.presence.lock().insert(id, Presence {
                            member: member.clone(),
                            host: host.chars().take(100).collect(),
                            applied_rev,
                            conflicts: 0,
                            connected_since: chrono::Utc::now(),
                        });
                    }
                    Ok(WsMsg::Applied { rev, conflicts }) => {
                        if let Some(p) = h.presence.lock().get_mut(&id) {
                            p.applied_rev = rev;
                            p.conflicts = conflicts;
                        }
                    }
                    _ => {}
                }
            }
            _ = ping.tick() => {
                if tx.send(Message::Ping(Bytes::new())).await.is_err() { break; }
            }
        }
    }
    h.presence.lock().remove(&id);
}
