//! Ownership-tracking merge of team JSON config into a person's local config.
//!
//! The rules, in plain words:
//! * Team entries are **added** where the person has nothing.
//! * The person's own values **always win**. A clash is reported, never overwritten.
//! * Entries the team added earlier are **updated or removed** when the team
//!   changes them, but only if the person hasn't edited them since.
//! * Arrays at "deep" paths (permission rules, hook groups) are merged as sets:
//!   team items are added to the person's list and tracked one by one.
//!
//! To make this possible we remember exactly what we inserted (`Owned`).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Owned {
    /// JSON path of the key (for `Key`) or of the array (for `Item`).
    pub path: Vec<String>,
    pub kind: OwnedKind,
    /// The value we wrote (the key's value, or the array item).
    pub value: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnedKind {
    Key,
    Item,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct MergeReport {
    pub owned: Vec<Owned>,
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
    /// The person's value differs from the team's; the person's value was kept.
    pub conflicts: Vec<String>,
}

impl MergeReport {
    pub fn changed(&self) -> bool {
        !(self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty())
    }
}

/// Which paths are merged recursively (objects) or as sets (arrays).
/// Everything else is an atomic value.
pub type DeepFn = fn(&[String]) -> bool;

/// `~/.claude/settings.json`: permissions, env and hooks merge finely;
/// anything else (`model`, `statusLine`, ...) is atomic.
pub fn settings_deep(p: &[String]) -> bool {
    match p {
        [] => true,
        [a] => matches!(a.as_str(), "permissions" | "env" | "hooks"),
        [a, _] => matches!(a.as_str(), "permissions" | "hooks"),
        _ => false,
    }
}

/// `~/.claude.json` `mcpServers`: each server is atomic, so a person's
/// same-named server is never half-overwritten.
pub fn mcp_deep(p: &[String]) -> bool {
    matches!(p, []) || matches!(p, [a] if a == "mcpServers")
}

fn label(path: &[String], item: Option<&Value>) -> String {
    let p = path.join(".");
    match item {
        Some(Value::String(s)) => format!("{p}[{s}]"),
        Some(v) => format!("{p}[{}]", truncate(&v.to_string(), 60)),
        None => p,
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn get_mut<'a>(root: &'a mut Value, path: &[String]) -> Option<&'a mut Value> {
    let mut cur = root;
    for k in path {
        cur = cur.as_object_mut()?.get_mut(k)?;
    }
    Some(cur)
}

struct Ctx<'a> {
    prev: &'a [Owned],
    deep: DeepFn,
    report: MergeReport,
}

impl Ctx<'_> {
    fn prev_key(&self, path: &[String]) -> Option<&Value> {
        self.prev
            .iter()
            .find(|o| o.kind == OwnedKind::Key && o.path == path)
            .map(|o| &o.value)
    }

    fn prev_item(&self, path: &[String], item: &Value) -> bool {
        self.prev
            .iter()
            .any(|o| o.kind == OwnedKind::Item && o.path == path && &o.value == item)
    }

    fn own(&mut self, path: Vec<String>, kind: OwnedKind, value: Value) {
        self.report.owned.push(Owned { path, kind, value });
    }

    fn object(
        &mut self,
        local: &mut Map<String, Value>,
        team: &Map<String, Value>,
        path: &mut Vec<String>,
    ) {
        for (k, tv) in team {
            path.push(k.clone());
            let deep = (self.deep)(path);
            match local.get_mut(k) {
                None if deep && (tv.is_object() || tv.is_array()) => {
                    let mut empty = if tv.is_object() {
                        Value::Object(Map::new())
                    } else {
                        Value::Array(vec![])
                    };
                    self.node(&mut empty, tv, path);
                    local.insert(k.clone(), empty);
                }
                None => {
                    local.insert(k.clone(), tv.clone());
                    self.report.added.push(label(path, None));
                    self.own(path.clone(), OwnedKind::Key, tv.clone());
                }
                Some(lv) => {
                    if let Some(pv) = self.prev_key(path).cloned() {
                        if *lv == pv {
                            // Still exactly what we wrote: the team controls it.
                            if lv != tv {
                                *lv = tv.clone();
                                self.report.updated.push(label(path, None));
                            }
                            self.own(path.clone(), OwnedKind::Key, tv.clone());
                        } else if lv != tv {
                            self.report.conflicts.push(label(path, None));
                        }
                    } else if deep
                        && ((lv.is_object() && tv.is_object()) || (lv.is_array() && tv.is_array()))
                    {
                        self.node(lv, tv, path);
                    } else if lv != tv {
                        self.report.conflicts.push(label(path, None));
                    }
                }
            }
            path.pop();
        }
    }

    fn node(&mut self, local: &mut Value, team: &Value, path: &mut Vec<String>) {
        match (local, team) {
            (Value::Object(l), Value::Object(t)) => self.object(l, t, path),
            (Value::Array(l), Value::Array(t)) => {
                for item in t {
                    if self.prev_item(path, item) {
                        if !l.contains(item) {
                            // The person deleted a team item: respect that, stop tracking it.
                            continue;
                        }
                        self.own(path.clone(), OwnedKind::Item, item.clone());
                    } else if !l.contains(item) {
                        l.push(item.clone());
                        self.report.added.push(label(path, Some(item)));
                        self.own(path.clone(), OwnedKind::Item, item.clone());
                    }
                }
            }
            _ => {}
        }
    }
}

/// Merge `team` into `local` in place. `prev` is what the last merge owned.
pub fn merge(local: &mut Value, prev: &[Owned], team: &Value, deep: DeepFn) -> MergeReport {
    if !local.is_object() {
        *local = Value::Object(Map::new());
    }
    let mut ctx = Ctx {
        prev,
        deep,
        report: MergeReport::default(),
    };
    let mut path = Vec::new();
    if team.is_object() {
        ctx.node(local, team, &mut path);
    }

    // Entries we owned that the team no longer provides: remove them, unless edited locally.
    let still: Vec<Owned> = ctx.report.owned.clone();
    for o in prev {
        if still
            .iter()
            .any(|s| s.kind == o.kind && s.path == o.path && s.value == o.value)
        {
            continue;
        }
        match o.kind {
            OwnedKind::Key => {
                let (parent, key) = o.path.split_at(o.path.len() - 1);
                if let Some(Value::Object(m)) = get_mut(local, parent)
                    && m.get(&key[0]) == Some(&o.value)
                {
                    m.shift_remove(&key[0]);
                    ctx.report.removed.push(label(&o.path, None));
                }
            }
            OwnedKind::Item => {
                // Only drop it if the team stopped shipping it (not merely re-owned).
                if let Some(Value::Array(a)) = get_mut(local, &o.path)
                    && let Some(pos) = a.iter().position(|v| v == &o.value)
                {
                    a.remove(pos);
                    ctx.report.removed.push(label(&o.path, Some(&o.value)));
                }
            }
        }
    }
    ctx.report
}

pub const BLOCK_BEGIN: &str =
    "<!-- claude-sync:team:begin (managed by claude-sync; edit outside this block) -->";
pub const BLOCK_END: &str = "<!-- claude-sync:team:end -->";

/// Put the team's CLAUDE.md inside a managed block; the rest of the file is the person's.
pub fn merge_markdown(local: &str, team: Option<&str>) -> String {
    let (before, after) = match (local.find(BLOCK_BEGIN), local.find(BLOCK_END)) {
        (Some(b), Some(e)) if e > b => (&local[..b], &local[e + BLOCK_END.len()..]),
        _ => (local, ""),
    };
    let before = before.trim_end_matches('\n');
    let after = after.trim_start_matches('\n');
    let mut out = String::with_capacity(local.len() + team.map_or(0, str::len) + 128);
    match team.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => {
            out.push_str(before);
            if !before.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(BLOCK_BEGIN);
            out.push('\n');
            out.push_str(t);
            out.push('\n');
            out.push_str(BLOCK_END);
            out.push('\n');
            if !after.is_empty() {
                out.push('\n');
                out.push_str(after);
            }
        }
        None => {
            out.push_str(before);
            if !after.is_empty() {
                if !before.is_empty() {
                    out.push_str("\n\n");
                }
                out.push_str(after);
            }
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(local: &mut Value, prev: &[Owned], team: Value) -> MergeReport {
        merge(local, prev, &team, settings_deep)
    }

    #[test]
    fn adds_team_entries_and_keeps_local() {
        let mut local = json!({"model": "opus", "permissions": {"allow": ["Bash(ls:*)"]}});
        let r = run(
            &mut local,
            &[],
            json!({
                "model": "sonnet",
                "permissions": {"allow": ["Bash(git status:*)", "Bash(ls:*)"], "deny": ["Read(./.env)"]},
                "env": {"TEAM": "1"}
            }),
        );
        assert_eq!(local["model"], "opus", "local wins");
        assert_eq!(r.conflicts, vec!["model"]);
        assert_eq!(
            local["permissions"]["allow"],
            json!(["Bash(ls:*)", "Bash(git status:*)"])
        );
        assert_eq!(local["permissions"]["deny"], json!(["Read(./.env)"]));
        assert_eq!(local["env"]["TEAM"], "1");
        // The pre-existing "Bash(ls:*)" belongs to the person, not the team.
        assert!(!r.owned.iter().any(|o| o.value == json!("Bash(ls:*)")));
    }

    #[test]
    fn team_updates_and_removals_propagate() {
        let mut local = json!({"permissions": {"allow": ["mine"]}});
        let team1 = json!({"permissions": {"allow": ["a", "b"]}, "env": {"X": "1", "Y": "1"}});
        let r1 = run(&mut local, &[], team1);
        let team2 = json!({"permissions": {"allow": ["b", "c"]}, "env": {"X": "2"}});
        let r2 = run(&mut local, &r1.owned, team2.clone());
        assert_eq!(local["permissions"]["allow"], json!(["mine", "b", "c"]));
        assert_eq!(local["env"], json!({"X": "2"}));
        assert!(r2.removed.contains(&"permissions.allow[a]".to_string()));
        assert!(r2.updated.contains(&"env.X".to_string()));
        // Idempotent.
        let before = local.clone();
        let r3 = run(&mut local, &r2.owned, team2);
        assert_eq!(local, before);
        assert!(!r3.changed());
    }

    #[test]
    fn local_edits_are_respected() {
        let mut local = json!({});
        let r1 = run(
            &mut local,
            &[],
            json!({"env": {"X": "1"}, "permissions": {"allow": ["a"]}}),
        );
        local["env"]["X"] = json!("mine");
        local["permissions"]["allow"] = json!([]); // person removed team rule
        let r2 = run(
            &mut local,
            &r1.owned,
            json!({"env": {"X": "2"}, "permissions": {"allow": ["a"]}}),
        );
        assert_eq!(local["env"]["X"], "mine");
        assert_eq!(r2.conflicts, vec!["env.X"]);
        assert_eq!(
            local["permissions"]["allow"],
            json!([]),
            "deleted team item stays deleted"
        );
        // Team later drops X: the person's edited value must survive.
        run(&mut local, &r2.owned, json!({}));
        assert_eq!(local["env"]["X"], "mine");
    }

    #[test]
    fn hooks_merge_by_group() {
        let g1 = json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "audit.sh"}]});
        let mine = json!({"matcher": "Edit", "hooks": [{"type": "command", "command": "fmt.sh"}]});
        let mut local = json!({"hooks": {"PreToolUse": [mine.clone()]}});
        let r = run(
            &mut local,
            &[],
            json!({"hooks": {"PreToolUse": [g1.clone()], "Stop": [g1.clone()]}}),
        );
        assert_eq!(local["hooks"]["PreToolUse"], json!([mine, g1]));
        assert_eq!(local["hooks"]["Stop"], json!([g1]));
        run(&mut local, &r.owned, json!({}));
        assert_eq!(local["hooks"]["PreToolUse"], json!([mine]));
    }

    #[test]
    fn mcp_servers_are_atomic() {
        let mut local =
            json!({"oauthAccount": {"x": 1}, "mcpServers": {"github": {"command": "mine"}}});
        let team = json!({"mcpServers": {"github": {"command": "npx", "args": ["a"]}, "sentry": {"type": "http", "url": "u"}}});
        let r = merge(&mut local, &[], &team, mcp_deep);
        assert_eq!(local["mcpServers"]["github"], json!({"command": "mine"}));
        assert_eq!(local["mcpServers"]["sentry"]["url"], "u");
        assert_eq!(
            local["oauthAccount"],
            json!({"x": 1}),
            "unrelated state untouched"
        );
        assert_eq!(r.conflicts, vec!["mcpServers.github"]);
    }

    #[test]
    fn markdown_block_roundtrip() {
        let local = "# My notes\nbe terse\n";
        let a = merge_markdown(local, Some("Team rule 1"));
        assert!(a.starts_with("# My notes\nbe terse\n\n<!-- claude-sync:team:begin"));
        let b = merge_markdown(&format!("{a}\nmore of mine\n"), Some("Team rule 2"));
        assert!(b.contains("Team rule 2") && !b.contains("Team rule 1"));
        assert!(b.ends_with("more of mine\n"));
        let c = merge_markdown(&b, None);
        assert_eq!(c, "# My notes\nbe terse\n\nmore of mine\n");
        assert_eq!(
            merge_markdown("", Some("T")),
            format!("{BLOCK_BEGIN}\nT\n{BLOCK_END}\n")
        );
    }
}
