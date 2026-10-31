//! Applies a team bundle to one person's Claude Code setup.
//!
//! | bundle path             | target                         | strategy                    |
//! |-------------------------|--------------------------------|-----------------------------|
//! | `CLAUDE.md`             | `~/.claude/CLAUDE.md`          | managed block               |
//! | `settings.json`         | `~/.claude/settings.json`      | ownership merge             |
//! | `mcp.json`              | `~/.claude.json` `mcpServers`  | ownership merge, atomic servers |
//! | `commands/ agents/ skills/` | `~/.claude/...`            | managed files               |
//!
//! Nothing the person wrote is ever overwritten; clashes are reported.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::bundle::sha256_hex;
use crate::fsutil::{prune_empty_dirs, read_opt, write_atomic};
use crate::merge::{self, Owned};
use crate::secrets::{self, Secrets};

#[derive(Debug, Clone)]
pub struct Paths {
    /// `~/.claude`
    pub claude_dir: PathBuf,
    /// `~/.claude.json`
    pub claude_json: PathBuf,
    /// `~/.claude-sync` (config, state, secrets)
    pub home: PathBuf,
}

impl Paths {
    pub fn from_home(user_home: &Path) -> Self {
        Self {
            claude_dir: user_home.join(".claude"),
            claude_json: user_home.join(".claude.json"),
            home: user_home.join(".claude-sync"),
        }
    }

    /// Respects `CLAUDE_CONFIG_DIR` (used by Claude Code) and `CLAUDE_SYNC_HOME`.
    pub fn detect() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .context("cannot determine home directory")?;
        let mut p = Self::from_home(&home);
        if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR") {
            p.claude_dir = PathBuf::from(d);
        }
        if let Some(d) = std::env::var_os("CLAUDE_SYNC_HOME") {
            p.home = PathBuf::from(d);
        }
        Ok(p)
    }

    pub fn state_file(&self) -> PathBuf {
        self.home.join("state.json")
    }
    pub fn secrets_file(&self) -> PathBuf {
        self.home.join("secrets.json")
    }
    pub fn config_file(&self) -> PathBuf {
        self.home.join("config.toml")
    }
}

/// Local bookkeeping of what claude-sync wrote (mode 0600: may contain secret values).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct LocalState {
    pub applied_rev: u64,
    /// bundle path → sha256 of the content we wrote
    #[serde(default)]
    pub managed_files: BTreeMap<String, String>,
    #[serde(default)]
    pub settings_owned: Vec<Owned>,
    #[serde(default)]
    pub mcp_owned: Vec<Owned>,
}

impl LocalState {
    pub fn load(paths: &Paths) -> Result<Self> {
        match read_opt(&paths.state_file())? {
            Some(d) => serde_json::from_slice(&d).context("reading claude-sync state"),
            None => Ok(Self::default()),
        }
    }
    pub fn save(&self, paths: &Paths) -> Result<()> {
        std::fs::create_dir_all(&paths.home)?;
        crate::fsutil::write_private(&paths.state_file(), &serde_json::to_vec_pretty(self)?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Create,
    Update,
    Delete,
}

#[derive(Debug, Clone, Serialize)]
pub struct Change {
    pub target: String,
    pub action: Action,
    /// What changed inside merged files (e.g. `permissions.allow[Bash(git:*)]`).
    pub details: Vec<String>,
    #[serde(skip)]
    pub old: Option<String>,
    #[serde(skip)]
    pub new: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Report {
    pub rev: u64,
    pub changes: Vec<Change>,
    /// Places where the person's version was kept instead of the team's.
    pub conflicts: Vec<String>,
    pub warnings: Vec<String>,
}

fn text(d: &[u8]) -> String {
    String::from_utf8_lossy(d).into_owned()
}

fn rel(paths: &Paths, p: &Path) -> String {
    let home = paths.claude_dir.parent().unwrap_or(Path::new("/"));
    match p.strip_prefix(home) {
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => p.display().to_string(),
    }
}

fn pretty(v: &Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap_or_default();
    s.push('\n');
    s
}

fn parse_obj(data: Option<&Vec<u8>>, what: &str, warnings: &mut Vec<String>) -> Option<Value> {
    let d = data?;
    match serde_json::from_slice::<Value>(d) {
        Ok(v) if v.is_object() => Some(v),
        _ => {
            warnings.push(format!("team {what} is not a JSON object; ignored"));
            None
        }
    }
}

/// Compute and (unless `dry_run`) apply the bundle. `files` maps bundle path → content.
pub fn apply(
    paths: &Paths,
    state: &mut LocalState,
    rev: u64,
    files: &BTreeMap<String, Vec<u8>>,
    secrets: &Secrets,
    dry_run: bool,
) -> Result<Report> {
    let mut r = Report {
        rev,
        ..Default::default()
    };

    // 1. CLAUDE.md managed block.
    {
        let target = paths.claude_dir.join("CLAUDE.md");
        let old = read_opt(&target)?.map(|d| text(&d));
        let team = files.get("CLAUDE.md").map(|d| text(d));
        let base = old.clone().unwrap_or_default();
        let new = merge::merge_markdown(&base, team.as_deref());
        let unchanged = new == base || (old.is_none() && new.trim().is_empty());
        if !unchanged {
            if !dry_run {
                write_atomic(&target, new.as_bytes())?;
            }
            r.changes.push(Change {
                target: rel(paths, &target),
                action: if old.is_none() {
                    Action::Create
                } else {
                    Action::Update
                },
                details: vec!["team block".into()],
                old,
                new: Some(new),
            });
        }
    }

    // 2. settings.json.
    {
        let target = paths.claude_dir.join("settings.json");
        let mut team = parse_obj(files.get("settings.json"), "settings.json", &mut r.warnings)
            .unwrap_or_else(|| Value::Object(Map::new()));
        for name in secrets::substitute_or_drop(&mut team, secrets) {
            r.warnings.push(format!("settings.json needs secret `{name}` (claude-sync secrets set {name}); those entries were skipped"));
        }
        let old = read_opt(&target)?;
        let local_parsed = match &old {
            Some(d) => serde_json::from_slice::<Value>(d).ok(),
            None => Some(Value::Object(Map::new())),
        };
        match local_parsed {
            None => r.warnings.push(format!(
                "{} is not valid JSON; left untouched",
                rel(paths, &target)
            )),
            Some(mut local) => {
                let rep = merge::merge(
                    &mut local,
                    &state.settings_owned,
                    &team,
                    merge::settings_deep,
                );
                record_merge(
                    &mut r,
                    paths,
                    &target,
                    old.as_deref(),
                    &local,
                    &rep,
                    dry_run,
                )?;
                if !dry_run {
                    state.settings_owned = rep.owned;
                }
            }
        }
    }

    // 3. MCP servers in ~/.claude.json (only the `mcpServers` key is ever touched).
    {
        let target = &paths.claude_json;
        let mut servers = Map::new();
        if let Some(v) = parse_obj(files.get("mcp.json"), "mcp.json", &mut r.warnings)
            && let Some(Value::Object(m)) = v.get("mcpServers")
        {
            for (name, cfg) in m {
                let mut cfg = cfg.clone();
                let missing = secrets::substitute_all(&mut cfg, secrets);
                if missing.is_empty() {
                    servers.insert(name.clone(), cfg);
                } else {
                    let list: Vec<_> = missing.into_iter().collect();
                    r.warnings.push(format!(
                            "MCP server `{name}` skipped: missing secret(s) {} (claude-sync secrets set NAME)",
                            list.join(", ")
                        ));
                }
            }
        }
        let team = serde_json::json!({ "mcpServers": servers });
        // ~/.claude.json is also written by Claude Code: re-check right before replacing it.
        for attempt in 0..3 {
            let old = read_opt(target)?;
            let Some(mut local) = (match &old {
                Some(d) => serde_json::from_slice::<Value>(d).ok(),
                None => Some(Value::Object(Map::new())),
            }) else {
                r.warnings.push(format!(
                    "{} is not valid JSON; left untouched",
                    rel(paths, target)
                ));
                break;
            };
            let rep = merge::merge(&mut local, &state.mcp_owned, &team, merge::mcp_deep);
            if !dry_run && rep.changed() && read_opt(target)? != old && attempt < 2 {
                continue; // changed under us; merge again on the fresh copy
            }
            if old.is_none() && !rep.changed() {
                if !dry_run {
                    state.mcp_owned = rep.owned;
                }
                break;
            }
            record_merge(&mut r, paths, target, old.as_deref(), &local, &rep, dry_run)?;
            if !dry_run {
                state.mcp_owned = rep.owned;
            }
            break;
        }
    }

    // 4. Managed files: commands/, agents/, skills/.
    let mut managed = state.managed_files.clone();
    for (path, data) in files.iter().filter(|(p, _)| is_managed_path(p)) {
        let target = paths.claude_dir.join(path);
        let new_sha = sha256_hex(data);
        let cur = read_opt(&target)?;
        let cur_sha = cur.as_deref().map(sha256_hex);
        let ours = managed.get(path).cloned();
        match cur_sha {
            None => {
                if !dry_run {
                    write_atomic(&target, data)?;
                }
                r.changes.push(file_change(
                    paths,
                    &target,
                    Action::Create,
                    None,
                    Some(data),
                ));
                managed.insert(path.clone(), new_sha);
            }
            Some(c) if c == new_sha => {
                managed.insert(path.clone(), new_sha);
            }
            Some(c) if ours.as_deref() == Some(c.as_str()) => {
                if !dry_run {
                    write_atomic(&target, data)?;
                }
                r.changes.push(file_change(
                    paths,
                    &target,
                    Action::Update,
                    cur.as_deref(),
                    Some(data),
                ));
                managed.insert(path.clone(), new_sha);
            }
            Some(_) => {
                let why = if ours.is_some() {
                    "you edited it"
                } else {
                    "you already have your own file there"
                };
                r.conflicts
                    .push(format!("{}: kept yours ({why})", rel(paths, &target)));
                managed.remove(path);
            }
        }
    }
    let stale: Vec<String> = managed
        .keys()
        .filter(|p| !files.contains_key(*p))
        .cloned()
        .collect();
    for path in stale {
        let target = paths.claude_dir.join(&path);
        let ours = managed.remove(&path).unwrap_or_default();
        if let Some(cur) = read_opt(&target)? {
            if sha256_hex(&cur) == ours {
                if !dry_run {
                    std::fs::remove_file(&target)
                        .with_context(|| format!("removing {}", target.display()))?;
                    if let Some(parent) = target.parent() {
                        prune_empty_dirs(parent, &paths.claude_dir);
                    }
                }
                r.changes.push(file_change(
                    paths,
                    &target,
                    Action::Delete,
                    Some(&cur),
                    None,
                ));
            } else {
                r.conflicts.push(format!(
                    "{}: removed from the team setup, kept your edited copy",
                    rel(paths, &target)
                ));
            }
        }
    }

    if !dry_run {
        state.managed_files = managed;
        state.applied_rev = rev;
    }
    Ok(r)
}

pub fn is_managed_path(p: &str) -> bool {
    p.starts_with("commands/") || p.starts_with("agents/") || p.starts_with("skills/")
}

fn file_change(
    paths: &Paths,
    target: &Path,
    action: Action,
    old: Option<&[u8]>,
    new: Option<&[u8]>,
) -> Change {
    Change {
        target: rel(paths, target),
        action,
        details: vec![],
        old: old.map(text),
        new: new.map(text),
    }
}

fn record_merge(
    r: &mut Report,
    paths: &Paths,
    target: &Path,
    old: Option<&[u8]>,
    merged: &Value,
    rep: &merge::MergeReport,
    dry_run: bool,
) -> Result<()> {
    let label = rel(paths, target);
    r.conflicts.extend(
        rep.conflicts
            .iter()
            .map(|c| format!("{label}: `{c}` kept yours")),
    );
    if !rep.changed() {
        return Ok(());
    }
    let new = pretty(merged);
    if !dry_run {
        write_atomic(target, new.as_bytes())?;
    }
    let mut details = Vec::new();
    details.extend(rep.added.iter().map(|x| format!("+ {x}")));
    details.extend(rep.updated.iter().map(|x| format!("~ {x}")));
    details.extend(rep.removed.iter().map(|x| format!("- {x}")));
    r.changes.push(Change {
        target: label,
        action: if old.is_none() {
            Action::Create
        } else {
            Action::Update
        },
        details,
        old: old.map(text),
        new: Some(new),
    });
    Ok(())
}

/// Human-readable summary; with `diffs`, unified diffs of each change.
pub fn render(r: &Report, diffs: bool) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    if r.changes.is_empty() {
        let _ = writeln!(out, "Already up to date with team revision {}.", r.rev);
    }
    for c in &r.changes {
        let sym = match c.action {
            Action::Create => "+",
            Action::Update => "~",
            Action::Delete => "-",
        };
        let _ = writeln!(out, "{sym} {}", c.target);
        for d in &c.details {
            let _ = writeln!(out, "    {d}");
        }
        if diffs {
            let old = c.old.as_deref().unwrap_or("");
            let new = c.new.as_deref().unwrap_or("");
            let diff = similar::TextDiff::from_lines(old, new);
            let _ = write!(
                out,
                "{}",
                diff.unified_diff()
                    .context_radius(2)
                    .header("current", "after sync")
            );
        }
    }
    for c in &r.conflicts {
        let _ = writeln!(out, "! {c}");
    }
    for w in &r.warnings {
        let _ = writeln!(out, "warning: {w}");
    }
    out
}
