//! Client config (`~/.claude-sync/config.toml`) and secret references.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::apply::Paths;

/// Resolve `env:NAME`, `file:/path`, or a literal value.
pub fn resolve_secret(raw: &str) -> Result<String> {
    if let Some(var) = raw.strip_prefix("env:") {
        std::env::var(var).with_context(|| format!("environment variable `{var}` is not set"))
    } else if let Some(path) = raw.strip_prefix("file:") {
        Ok(std::fs::read_to_string(path)
            .with_context(|| format!("reading `{path}`"))?
            .trim()
            .to_string())
    } else {
        Ok(raw.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub hub: String,
    pub token: String,
}

impl ClientConfig {
    pub fn load(paths: &Paths) -> Result<Self> {
        let p = paths.config_file();
        let text = std::fs::read_to_string(&p).with_context(|| {
            format!(
                "not logged in (no {}); run `claude-sync login`",
                p.display()
            )
        })?;
        let mut c: Self = toml::from_str(&text).context("parsing claude-sync config")?;
        c.token = resolve_secret(&c.token)?;
        Ok(c)
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        std::fs::create_dir_all(&paths.home)?;
        crate::fsutil::write_private(&paths.config_file(), toml::to_string(self)?.as_bytes())
    }
}
