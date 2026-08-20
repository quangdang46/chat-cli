//! `~/.config/chat-cli/config.toml` — source of truth for `default_provider`.
//!
//! ```toml
//! default_provider = "chatgpt"
//! [providers.chatgpt]
//! session_token = "..."
//! access_token = "..."
//! access_token_expiry = "2026-08-21T00:00:00Z"
//! [providers.deepseek]
//! session_token = "..."
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    #[serde(default)]
    pub session_token: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub access_token_expiry: Option<String>,
}

impl Config {
    pub fn config_path(override_path: Option<&Path>) -> PathBuf {
        if let Some(p) = override_path {
            return p.to_path_buf();
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("chat-cli")
            .join("config.toml")
    }

    pub fn load(override_path: Option<&Path>) -> anyhow::Result<Self> {
        let path = Self::config_path(override_path);
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(&path)?;
        let cfg: Self = toml::from_str(&content)?;
        Ok(cfg)
    }

    /// Atomic save: write to temp + rename, chmod 0600.
    pub fn save(&self, override_path: Option<&Path>) -> anyhow::Result<()> {
        let path = Self::config_path(override_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, &content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Called on `auth login <provider>` — sets default_provider only if not yet set.
    pub fn ensure_default_provider(&mut self, provider_id: &str) {
        if self.default_provider.is_none() {
            self.default_provider = Some(provider_id.to_string());
        }
    }
}
