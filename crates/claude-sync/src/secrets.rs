//! Personal secrets. Team files reference `{{secret:NAME}}`; each person keeps
//! the values in a local file (mode 0600) that is never uploaded.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub type Secrets = BTreeMap<String, String>;

pub fn load(path: &Path) -> Result<Secrets> {
    match std::fs::read_to_string(path) {
        Ok(t) => serde_json::from_str(&t).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Secrets::new()),
        Err(e) => Err(e.into()),
    }
}

pub fn save(path: &Path, s: &Secrets) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::fsutil::write_private(path, serde_json::to_string_pretty(s)?.as_bytes())
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 100 && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Replace placeholders in one string. Missing names are collected.
pub fn substitute_str(s: &str, secrets: &Secrets, missing: &mut BTreeSet<String>) -> String {
    const OPEN: &str = "{{secret:";
    if !s.contains(OPEN) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(OPEN) {
        out.push_str(&rest[..i]);
        let after = &rest[i + OPEN.len()..];
        match after.find("}}") {
            Some(j) if valid_name(&after[..j]) => {
                let name = &after[..j];
                match secrets.get(name) {
                    Some(v) => out.push_str(v),
                    None => {
                        missing.insert(name.to_string());
                    }
                }
                rest = &after[j + 2..];
            }
            _ => {
                out.push_str(OPEN);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Substitute everywhere in `v`. Entries (object members / array items)
/// whose subtree references a missing secret are dropped. Returns the names
/// that were missing.
pub fn substitute_or_drop(v: &mut Value, secrets: &Secrets) -> BTreeSet<String> {
    fn walk(v: &mut Value, secrets: &Secrets, all: &mut BTreeSet<String>) -> bool {
        match v {
            Value::String(s) => {
                let mut missing = BTreeSet::new();
                *s = substitute_str(s, secrets, &mut missing);
                let ok = missing.is_empty();
                all.extend(missing);
                ok
            }
            Value::Object(m) => {
                m.retain(|_, x| walk(x, secrets, all));
                true
            }
            Value::Array(a) => {
                a.retain_mut(|x| walk(x, secrets, all));
                true
            }
            _ => true,
        }
    }
    let mut all = BTreeSet::new();
    walk(v, secrets, &mut all);
    all
}

/// Substitute within `v` as one unit: on any missing secret, returns the names and leaves `v` unusable.
pub fn substitute_all(v: &mut Value, secrets: &Secrets) -> BTreeSet<String> {
    fn walk(v: &mut Value, secrets: &Secrets, missing: &mut BTreeSet<String>) {
        match v {
            Value::String(s) => *s = substitute_str(s, secrets, missing),
            Value::Object(m) => m.values_mut().for_each(|x| walk(x, secrets, missing)),
            Value::Array(a) => a.iter_mut().for_each(|x| walk(x, secrets, missing)),
            _ => {}
        }
    }
    let mut missing = BTreeSet::new();
    walk(v, secrets, &mut missing);
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn substitution() {
        let mut s = Secrets::new();
        s.insert("TOKEN".into(), "abc".into());
        let mut m = BTreeSet::new();
        assert_eq!(
            substitute_str("Bearer {{secret:TOKEN}}!", &s, &mut m),
            "Bearer abc!"
        );
        assert_eq!(
            substitute_str("{{secret:NOPE}} {{secret:bad name}}", &s, &mut m),
            " {{secret:bad name}}"
        );
        assert_eq!(m.into_iter().collect::<Vec<_>>(), vec!["NOPE"]);

        let mut v = json!({"env": {"A": "{{secret:TOKEN}}", "B": "{{secret:MISSING}}"}, "keep": 1});
        let missing = substitute_or_drop(&mut v, &s);
        assert_eq!(v, json!({"env": {"A": "abc"}, "keep": 1}));
        assert!(missing.contains("MISSING"));
    }
}
