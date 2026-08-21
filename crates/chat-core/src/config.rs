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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> Config {
        let mut cfg = Config {
            default_provider: Some("chatgpt".to_string()),
            ..Default::default()
        };
        cfg.providers.insert(
            "chatgpt".to_string(),
            ProviderConfig {
                session_token: Some("session-token-1".to_string()),
                access_token: Some("access-token-1".to_string()),
                access_token_expiry: Some("2026-09-01T00:00:00Z".to_string()),
            },
        );
        cfg.providers.insert(
            "deepseek".to_string(),
            ProviderConfig {
                session_token: Some("ds-token".to_string()),
                access_token: None,
                access_token_expiry: None,
            },
        );
        cfg
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config::load(Some(&path)).unwrap();
        assert!(cfg.default_provider.is_none());
        assert!(cfg.providers.is_empty());
    }

    #[test]
    fn save_and_reload_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = sample_config();
        cfg.save(Some(&path)).unwrap();

        let loaded = Config::load(Some(&path)).unwrap();
        assert_eq!(loaded.default_provider.as_deref(), Some("chatgpt"));
        assert_eq!(loaded.providers.len(), 2);

        let chatgpt = loaded.providers.get("chatgpt").unwrap();
        assert_eq!(chatgpt.session_token.as_deref(), Some("session-token-1"));
        assert_eq!(chatgpt.access_token.as_deref(), Some("access-token-1"));
        assert_eq!(
            chatgpt.access_token_expiry.as_deref(),
            Some("2026-09-01T00:00:00Z")
        );

        let deepseek = loaded.providers.get("deepseek").unwrap();
        assert_eq!(deepseek.session_token.as_deref(), Some("ds-token"));
        assert!(deepseek.access_token.is_none());
    }

    #[test]
    fn ensure_default_provider_first_login_sets_it() {
        let mut cfg = Config::default();
        cfg.ensure_default_provider("deepseek");
        assert_eq!(cfg.default_provider.as_deref(), Some("deepseek"));
    }

    #[test]
    fn ensure_default_provider_second_login_does_not_override() {
        let mut cfg = Config::default();
        cfg.ensure_default_provider("deepseek");
        // A later login to another provider must not steal the default.
        cfg.ensure_default_provider("chatgpt");
        cfg.ensure_default_provider("chatgpt");
        assert_eq!(
            cfg.default_provider.as_deref(),
            Some("deepseek"),
            "first-login-only semantics violated"
        );
    }

    #[cfg(unix)]
    #[test]
    fn saved_config_has_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        sample_config().save(Some(&path)).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "config holds secrets and must be 0600");
    }

    #[test]
    fn save_creates_missing_parent_dirs_and_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deeper").join("config.toml");
        sample_config().save(Some(&path)).unwrap();

        assert!(path.exists());
        let tmp = path.with_extension("toml.tmp");
        assert!(!tmp.exists(), "atomic rename must consume the temp file");

        let loaded = Config::load(Some(&path)).unwrap();
        assert_eq!(loaded.default_provider.as_deref(), Some("chatgpt"));
    }

    #[test]
    fn override_path_is_used_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("ci").join("custom.toml");

        let cfg = Config {
            default_provider: Some("deepseek".to_string()),
            ..Default::default()
        };
        cfg.save(Some(&custom)).unwrap();

        assert_eq!(
            Config::load(Some(&custom))
                .unwrap()
                .default_provider
                .as_deref(),
            Some("deepseek")
        );
        // The real user config location is untouched.
        assert!(!dir.path().join("config.toml").exists());
    }

    #[test]
    fn reload_overwrites_previous_values_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut cfg = sample_config();
        cfg.save(Some(&path)).unwrap();

        cfg.default_provider = Some("deepseek".to_string());
        cfg.save(Some(&path)).unwrap();

        let loaded = Config::load(Some(&path)).unwrap();
        assert_eq!(loaded.default_provider.as_deref(), Some("deepseek"));
    }
}
