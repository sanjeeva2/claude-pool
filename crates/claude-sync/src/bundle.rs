//! The team bundle: a set of files (path → content hash) in a fixed layout.
//!
//! ```text
//! CLAUDE.md                 team instructions (merged into ~/.claude/CLAUDE.md)
//! settings.json             permissions, hooks, env, ... (merged into ~/.claude/settings.json)
//! mcp.json                  {"mcpServers": {...}} (merged into ~/.claude.json)
//! commands/**  agents/**  skills/**   copied into ~/.claude/
//! ```
//! Blobs are content-addressed (SHA-256), so unchanged files are never re-sent.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

pub type Manifest = BTreeMap<String, String>;

pub const MAX_FILE_BYTES: usize = 2 << 20;
pub const MAX_FILES: usize = 2000;

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn is_sha(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Accept only the known layout, relative, no `..`, no hidden path tricks.
pub fn validate_path(p: &str) -> Result<()> {
    if p.is_empty() || p.len() > 300 || p.starts_with('/') || p.contains('\\') || p.contains('\0') {
        bail!("invalid path `{p}`");
    }
    let parts: Vec<&str> = p.split('/').collect();
    if parts
        .iter()
        .any(|c| c.is_empty() || *c == "." || *c == "..")
    {
        bail!("invalid path `{p}`");
    }
    match parts.as_slice() {
        ["CLAUDE.md"] | ["settings.json"] | ["mcp.json"] => Ok(()),
        [top, _, ..] if matches!(*top, "commands" | "agents" | "skills") => Ok(()),
        _ => bail!(
            "`{p}` is outside the bundle layout (CLAUDE.md, settings.json, mcp.json, commands/, agents/, skills/)"
        ),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Revision {
    pub rev: u64,
    pub author: String,
    pub message: String,
    pub time: chrono::DateTime<chrono::Utc>,
    pub files: Manifest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevisionSummary {
    pub rev: u64,
    pub author: String,
    pub message: String,
    pub time: chrono::DateTime<chrono::Utc>,
    pub files: usize,
}

impl From<&Revision> for RevisionSummary {
    fn from(r: &Revision) -> Self {
        Self {
            rev: r.rev,
            author: r.author.clone(),
            message: r.message.clone(),
            time: r.time,
            files: r.files.len(),
        }
    }
}

/// Read a team directory into (manifest, blobs). Ignores dotfiles (`.git`, ...).
pub fn scan_dir(root: &Path) -> Result<(Manifest, BTreeMap<String, Vec<u8>>)> {
    let mut manifest = Manifest::new();
    let mut blobs = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let rel = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            if rel == "README.md" {
                continue; // allowed for documenting the team repo; not synced
            }
            validate_path(&rel)?;
            let data = std::fs::read(&path)?;
            if data.len() > MAX_FILE_BYTES {
                bail!("`{rel}` is larger than {} bytes", MAX_FILE_BYTES);
            }
            if rel.ends_with(".json") {
                serde_json::from_slice::<serde_json::Value>(&data)
                    .map_err(|e| anyhow::anyhow!("`{rel}` is not valid JSON: {e}"))?;
            }
            let sha = sha256_hex(&data);
            manifest.insert(rel, sha.clone());
            blobs.insert(sha, data);
        }
    }
    if manifest.len() > MAX_FILES {
        bail!("too many files ({} > {MAX_FILES})", manifest.len());
    }
    Ok((manifest, blobs))
}

/// Patterns that look like real credentials. Team files should use
/// `{{secret:NAME}}` placeholders instead; each person stores values locally.
const SECRET_PATTERNS: &[(&str, &str)] = &[
    ("sk-ant-", "Anthropic key"),
    ("ghp_", "GitHub token"),
    ("github_pat_", "GitHub token"),
    ("gho_", "GitHub token"),
    ("xoxb-", "Slack token"),
    ("xoxp-", "Slack token"),
    ("AKIA", "AWS access key"),
    ("-----BEGIN", "private key"),
    ("sk_live_", "Stripe key"),
    ("glpat-", "GitLab token"),
    ("AIza", "Google API key"),
];

pub fn scan_secrets(path: &str, data: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(data);
    let mut hits = Vec::new();
    for (pat, what) in SECRET_PATTERNS {
        for (i, _) in text.match_indices(pat) {
            // Require a credential-like tail so prose mentioning "AKIA" etc. passes.
            let tail: String = text[i + pat.len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            if tail.len() >= 12
                || *pat == "-----BEGIN"
                    && text[i..].starts_with("-----BEGIN")
                    && text[i..].contains("PRIVATE KEY")
            {
                let line = text[..i].matches('\n').count() + 1;
                hits.push(format!("{path}:{line}: looks like a {what}"));
                break;
            }
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        for ok in [
            "CLAUDE.md",
            "settings.json",
            "mcp.json",
            "commands/review.md",
            "skills/pdf/SKILL.md",
            "agents/a/b.md",
        ] {
            validate_path(ok).unwrap();
        }
        for bad in [
            "../x",
            "/etc/passwd",
            "commands/../../x",
            "foo.md",
            "commands",
            "skills//x",
            "commands/./x",
            "settings.local.json",
        ] {
            assert!(validate_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn secrets() {
        assert_eq!(
            scan_secrets("mcp.json", br#"{"env":{"T":"ghp_abcdefghijklmnop1234"}}"#).len(),
            1
        );
        assert!(scan_secrets("mcp.json", br#"{"env":{"T":"{{secret:GITHUB_TOKEN}}"}}"#).is_empty());
        assert!(
            scan_secrets(
                "CLAUDE.md",
                b"Never commit keys starting with sk-ant- or AKIA."
            )
            .is_empty()
        );
        assert_eq!(
            scan_secrets("x", b"-----BEGIN OPENSSH PRIVATE KEY-----\nabc").len(),
            1
        );
    }
}
