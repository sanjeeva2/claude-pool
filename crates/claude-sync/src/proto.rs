//! Wire types shared by hub and client.

use serde::{Deserialize, Serialize};

use crate::bundle::Manifest;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Head {
    pub rev: u64,
    pub files: Manifest,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MissingRequest {
    pub shas: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MissingResponse {
    pub missing: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PublishRequest {
    /// Must equal the hub's current revision (optimistic concurrency).
    pub base_rev: u64,
    pub message: String,
    pub files: Manifest,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PublishResponse {
    pub rev: u64,
    pub unchanged: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Presence {
    pub member: String,
    pub host: String,
    pub applied_rev: u64,
    pub conflicts: usize,
    pub connected_since: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusResponse {
    pub rev: u64,
    pub connected: Vec<Presence>,
}

/// WebSocket messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsMsg {
    /// hub → client: the current team revision (sent on connect and on every publish).
    Head { rev: u64 },
    /// client → hub: introduce yourself.
    Hello { host: String, applied_rev: u64 },
    /// client → hub: finished applying a revision.
    Applied { rev: u64, conflicts: usize },
}
