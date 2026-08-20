//! Local history: `~/.local/share/chat-cli/history/<id>.jsonl` append-only.
//!
//! Each line is a `Turn` JSON. Header metadata is stored as the first line
//! with `role = "__meta__"`.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryFile {
    pub id: String,
    pub provider: String,
    #[serde(default)]
    pub provider_conversation_id: Option<String>,
    #[serde(default)]
    pub provider_parent_message_id: Option<String>,
    pub created_at: String,
    pub turns: Vec<Turn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub role: String, // "user" | "assistant" | "system"
    pub content: String,
    pub timestamp: String,
}

impl HistoryFile {
    pub fn new(provider: &str) -> Self {
        Self {
            id: nanoid(),
            provider: provider.to_string(),
            provider_conversation_id: None,
            provider_parent_message_id: None,
            created_at: Utc::now().to_rfc3339(),
            turns: vec![],
        }
    }

    pub fn history_dir(override_path: Option<&Path>) -> PathBuf {
        if let Some(p) = override_path {
            return p.to_path_buf();
        }
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("chat-cli")
            .join("history")
    }

    pub fn history_path(&self, override_dir: Option<&Path>) -> PathBuf {
        Self::history_dir(override_dir).join(format!("{}.jsonl", self.id))
    }

    pub fn save(&self, override_dir: Option<&Path>) -> anyhow::Result<()> {
        let path = self.history_path(override_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = String::new();
        // First line: meta
        let meta = serde_json::json!({
            "role": "__meta__",
            "id": self.id,
            "provider": self.provider,
            "provider_conversation_id": self.provider_conversation_id,
            "provider_parent_message_id": self.provider_parent_message_id,
            "created_at": self.created_at,
        });
        out.push_str(&serde_json::to_string(&meta)?);
        out.push('\n');
        for t in &self.turns {
            out.push_str(&serde_json::to_string(t)?);
            out.push('\n');
        }
        fs::write(&path, out)?;
        Ok(())
    }

    pub fn load(id: &str, override_dir: Option<&Path>) -> anyhow::Result<Self> {
        let path = Self::history_dir(override_dir).join(format!("{}.jsonl", id));
        let content = fs::read_to_string(&path)?;
        Self::from_jsonl(&content)
    }

    pub fn from_jsonl(content: &str) -> anyhow::Result<Self> {
        let mut lines = content.lines();
        let meta_line = lines.next().ok_or_else(|| anyhow::anyhow!("empty history file"))?;
        let meta: serde_json::Value = serde_json::from_str(meta_line)?;
        if meta.get("role").and_then(|v| v.as_str()) != Some("__meta__") {
            anyhow::bail!("first line must be __meta__");
        }
        let mut hf = HistoryFile {
            id: meta["id"].as_str().unwrap_or("").to_string(),
            provider: meta["provider"].as_str().unwrap_or("").to_string(),
            provider_conversation_id: meta
                .get("provider_conversation_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            provider_parent_message_id: meta
                .get("provider_parent_message_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            created_at: meta["created_at"].as_str().unwrap_or("").to_string(),
            turns: vec![],
        };
        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let turn: Turn = serde_json::from_str(line)?;
            hf.turns.push(turn);
        }
        Ok(hf)
    }

    pub fn list(override_dir: Option<&Path>) -> anyhow::Result<Vec<HistorySummary>> {
        let dir = Self::history_dir(override_dir);
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut out = vec![];
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if let Ok(hf) = Self::from_jsonl(&content) {
                let preview = hf
                    .turns
                    .last()
                    .map(|t| truncate(&t.content, 80))
                    .unwrap_or_default();
                out.push(HistorySummary {
                    id: hf.id,
                    provider: hf.provider,
                    created_at: hf.created_at,
                    turn_count: hf.turns.len(),
                    preview,
                });
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    /// Text to inject for `--continue` (all turns concatenated).
    pub fn to_context_text(&self) -> String {
        let mut s = String::new();
        for t in &self.turns {
            s.push_str(&format!("{}: {}\n", t.role, t.content));
        }
        s
    }
}

#[derive(Debug, Clone)]
pub struct HistorySummary {
    pub id: String,
    pub provider: String,
    pub created_at: String,
    pub turn_count: usize,
    pub preview: String,
}

fn nanoid() -> String {
    // 8-char nanoid for local history id (short, readable)
    Uuid::new_v4().to_string()[..8].to_string()
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    format!("{}...", &s[..n])
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Ensure `--continue <id>` history matches the requested provider.
pub fn validate_continue_provider(
    history: &HistoryFile,
    requested_provider: &str,
) -> anyhow::Result<()> {
    if history.provider != requested_provider {
        anyhow::bail!(
            "history '{}' was created with provider '{}', cannot continue with '--provider {}'",
            history.id,
            history.provider,
            requested_provider
        );
    }
    Ok(())
}
