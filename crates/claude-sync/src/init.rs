//! `claude-sync init-team DIR`: bootstrap a team directory from your own
//! Claude Code setup, replacing credentials with `{{secret:NAME}}` placeholders
//! (and saving the real values in your local secret store).

use anyhow::{Result, bail};
use serde_json::{Map, Value};
use std::path::Path;

use crate::apply::Paths;
use crate::fsutil::{read_opt, write_atomic};
use crate::secrets::{self, Secrets};

#[derive(Debug, Default)]
pub struct InitReport {
    pub files: Vec<String>,
    pub secrets: Vec<String>,
}

fn looks_secret(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    [
        "TOKEN",
        "KEY",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "AUTH",
        "CREDENTIAL",
        "COOKIE",
    ]
    .iter()
    .any(|w| k.contains(w))
}

fn secret_name(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| {
            p.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_uppercase()
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("_")
}

/// Replace secret-looking values under `obj` with placeholders.
fn extract(
    obj: &mut Map<String, Value>,
    prefix: &[&str],
    force: bool,
    out: &mut Secrets,
    names: &mut Vec<String>,
) {
    for (k, v) in obj.iter_mut() {
        if let Value::String(s) = v
            && (force || looks_secret(k))
            && !s.is_empty()
            && !s.contains("{{secret:")
            && !s.starts_with("${")
        {
            let mut parts = prefix.to_vec();
            parts.push(k);
            let name = secret_name(&parts);
            out.insert(name.clone(), s.clone());
            names.push(name.clone());
            *s = format!("{{{{secret:{name}}}}}");
        }
    }
}

fn copy_tree(src: &Path, dst: &Path, rel: &str, report: &mut InitReport) -> Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let r = format!("{rel}/{name}");
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &dst.join(&name), &r, report)?;
        } else if e.file_type()?.is_file() {
            write_atomic(&dst.join(&name), &std::fs::read(e.path())?)?;
            report.files.push(r);
        }
    }
    Ok(())
}

pub fn init_team(paths: &Paths, dir: &Path) -> Result<InitReport> {
    if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
        bail!("{} is not empty", dir.display());
    }
    std::fs::create_dir_all(dir)?;
    let mut report = InitReport::default();
    let mut found = Secrets::new();

    if let Some(md) = read_opt(&paths.claude_dir.join("CLAUDE.md"))? {
        // Never re-publish a team block we wrote ourselves.
        let text = crate::merge::merge_markdown(&String::from_utf8_lossy(&md), None);
        if !text.trim().is_empty() {
            write_atomic(&dir.join("CLAUDE.md"), text.as_bytes())?;
            report.files.push("CLAUDE.md".into());
        }
    }
    for sub in ["commands", "agents", "skills"] {
        copy_tree(
            &paths.claude_dir.join(sub),
            &dir.join(sub),
            sub,
            &mut report,
        )?;
    }

    if let Some(d) = read_opt(&paths.claude_dir.join("settings.json"))?
        && let Ok(Value::Object(s)) = serde_json::from_slice::<Value>(&d)
    {
        let mut team = Map::new();
        for key in ["permissions", "hooks", "env"] {
            if let Some(v) = s.get(key) {
                team.insert(key.into(), v.clone());
            }
        }
        if let Some(Value::Object(env)) = team.get_mut("env") {
            extract(env, &["ENV"], false, &mut found, &mut report.secrets);
        }
        if !team.is_empty() {
            write_atomic(
                &dir.join("settings.json"),
                (serde_json::to_string_pretty(&team)? + "\n").as_bytes(),
            )?;
            report.files.push("settings.json".into());
        }
    }

    if let Some(d) = read_opt(&paths.claude_json)?
        && let Ok(v) = serde_json::from_slice::<Value>(&d)
        && let Some(Value::Object(servers)) = v.get("mcpServers")
    {
        let mut servers = servers.clone();
        for (name, cfg) in servers.iter_mut() {
            let Value::Object(cfg) = cfg else { continue };
            if let Some(Value::Object(env)) = cfg.get_mut("env") {
                // Every env value of an MCP server is treated as personal.
                extract(env, &[name], true, &mut found, &mut report.secrets);
            }
            if let Some(Value::Object(h)) = cfg.get_mut("headers") {
                extract(h, &[name, "HEADER"], false, &mut found, &mut report.secrets);
            }
        }
        if !servers.is_empty() {
            let doc = serde_json::json!({ "mcpServers": servers });
            write_atomic(
                &dir.join("mcp.json"),
                (serde_json::to_string_pretty(&doc)? + "\n").as_bytes(),
            )?;
            report.files.push("mcp.json".into());
        }
    }

    if !found.is_empty() {
        let mut mine = secrets::load(&paths.secrets_file())?;
        mine.extend(found);
        secrets::save(&paths.secrets_file(), &mine)?;
    }
    write_atomic(
        &dir.join("README.md"),
        b"# Team Claude Code setup\n\nPublished with `claude-sync publish .`. Values like `{{secret:NAME}}` are filled in on each\nperson's machine from `claude-sync secrets set NAME`.\n",
    )?;
    report.files.sort();
    Ok(report)
}
