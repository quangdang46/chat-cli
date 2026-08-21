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
        let meta_line = lines
            .next()
            .ok_or_else(|| anyhow::anyhow!("empty history file"))?;
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
    // Char-boundary safe: byte slicing would panic on multibyte UTF-8.
    if s.chars().count() <= n {
        return s.to_string();
    }
    let cut: String = s.chars().take(n).collect();
    format!("{}...", cut)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: &str, content: &str) -> Turn {
        Turn {
            role: role.to_string(),
            content: content.to_string(),
            timestamp: "2026-08-21T00:00:00Z".to_string(),
        }
    }

    fn sample_history(id: &str, provider: &str, created_at: &str) -> HistoryFile {
        HistoryFile {
            id: id.to_string(),
            provider: provider.to_string(),
            provider_conversation_id: Some("conv-123".to_string()),
            provider_parent_message_id: Some("msg-456".to_string()),
            created_at: created_at.to_string(),
            turns: vec![turn("user", "hello"), turn("assistant", "hi there")],
        }
    }

    #[test]
    fn new_history_has_8_char_id_and_no_provider_ids() {
        let hf = HistoryFile::new("chatgpt");
        assert_eq!(hf.id.len(), 8, "local history id must be an 8-char nanoid");
        assert!(hf.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        assert_eq!(hf.provider, "chatgpt");
        assert!(hf.provider_conversation_id.is_none());
        assert!(hf.provider_parent_message_id.is_none());
        assert!(hf.turns.is_empty());
    }

    #[test]
    fn new_history_ids_are_unique() {
        let a = HistoryFile::new("chatgpt").id;
        let b = HistoryFile::new("chatgpt").id;
        assert_ne!(a, b);
    }

    #[test]
    fn save_then_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let hf = sample_history("abcd1234", "chatgpt", "2026-08-21T10:00:00+00:00");
        hf.save(Some(dir.path())).unwrap();

        let loaded = HistoryFile::load("abcd1234", Some(dir.path())).unwrap();
        assert_eq!(loaded.id, "abcd1234");
        assert_eq!(loaded.provider, "chatgpt");
        assert_eq!(loaded.provider_conversation_id.as_deref(), Some("conv-123"));
        assert_eq!(
            loaded.provider_parent_message_id.as_deref(),
            Some("msg-456")
        );
        assert_eq!(loaded.created_at, "2026-08-21T10:00:00+00:00");
        assert_eq!(loaded.turns.len(), 2);
        assert_eq!(loaded.turns[0].role, "user");
        assert_eq!(loaded.turns[0].content, "hello");
        assert_eq!(loaded.turns[1].role, "assistant");
    }

    #[test]
    fn first_jsonl_line_is_meta_followed_by_turn_lines() {
        let dir = tempfile::tempdir().unwrap();
        let hf = sample_history("meta0001", "deepseek", "2026-08-21T10:00:00+00:00");
        hf.save(Some(dir.path())).unwrap();

        let content = std::fs::read_to_string(dir.path().join("meta0001.jsonl")).unwrap();
        let mut lines = content.lines();
        let meta: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(meta["role"], "__meta__");
        assert_eq!(meta["provider"], "deepseek");
        for line in lines {
            let t: Turn = serde_json::from_str(line).unwrap();
            assert_ne!(t.role, "__meta__");
        }
    }

    #[test]
    fn from_jsonl_rejects_file_without_meta_header() {
        let content = "{\"role\":\"user\",\"content\":\"x\",\"timestamp\":\"t\"}\n";
        let err = HistoryFile::from_jsonl(content).unwrap_err();
        assert!(err.to_string().contains("__meta__"));
    }

    #[test]
    fn load_missing_id_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(HistoryFile::load("nope0000", Some(dir.path())).is_err());
    }

    #[test]
    fn list_orders_by_created_at_desc_and_skips_non_history_files() {
        let dir = tempfile::tempdir().unwrap();

        // Intentionally saved out of order; list must sort newest first.
        sample_history("old00001", "chatgpt", "2026-08-20T10:00:00+00:00")
            .save(Some(dir.path()))
            .unwrap();
        sample_history("new00001", "chatgpt", "2026-08-21T12:00:00+00:00")
            .save(Some(dir.path()))
            .unwrap();
        sample_history("mid00001", "deepseek", "2026-08-21T11:00:00+00:00")
            .save(Some(dir.path()))
            .unwrap();

        // Noise that must be ignored.
        std::fs::write(dir.path().join("notes.txt"), "not a history").unwrap();
        std::fs::write(dir.path().join("broken.jsonl"), "not json").unwrap();

        let items = HistoryFile::list(Some(dir.path())).unwrap();
        let ids: Vec<&str> = items.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, vec!["new00001", "mid00001", "old00001"]);
        assert_eq!(items[0].turn_count, 2);
    }

    #[test]
    fn list_on_missing_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent");
        let items = HistoryFile::list(Some(&absent)).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn list_preview_truncates_to_80_chars_including_multibyte() {
        let dir = tempfile::tempdir().unwrap();

        let long = "x".repeat(200);
        let mut hf = sample_history("prev0001", "chatgpt", "2026-08-21T10:00:00+00:00");
        hf.turns.push(turn("assistant", &long));
        hf.save(Some(dir.path())).unwrap();

        let viet = "Tiếng Việt có dấu — thử truncate đa byte 🎉".repeat(5);
        let mut hf2 = sample_history("prev0002", "chatgpt", "2026-08-21T11:00:00+00:00");
        hf2.turns.push(turn("assistant", &viet));
        hf2.save(Some(dir.path())).unwrap();

        let items = HistoryFile::list(Some(dir.path())).unwrap();
        let by_id: std::collections::HashMap<_, _> =
            items.iter().map(|h| (h.id.as_str(), h)).collect();

        let ascii_preview = &by_id["prev0001"].preview;
        assert!(ascii_preview.starts_with("xxx"));
        assert!(ascii_preview.ends_with("..."));
        assert!(ascii_preview.chars().count() <= 80 + 3);

        // Must not panic and must stay within bound on multibyte text.
        let uni_preview = &by_id["prev0002"].preview;
        assert!(uni_preview.chars().count() <= 80 + 3);
    }

    #[test]
    fn to_context_text_formats_role_colon_content_per_line() {
        let hf = sample_history("ctx00001", "chatgpt", "2026-08-21T10:00:00+00:00");
        let text = hf.to_context_text();
        assert_eq!(text, "user: hello\nassistant: hi there\n");
    }

    #[test]
    fn validate_continue_provider_same_provider_ok() {
        let hf = sample_history("ok000001", "chatgpt", "2026-08-21T10:00:00+00:00");
        assert!(validate_continue_provider(&hf, "chatgpt").is_ok());
    }

    #[test]
    fn validate_continue_provider_cross_provider_bails_with_actionable_error() {
        let hf = sample_history("bad00001", "chatgpt", "2026-08-21T10:00:00+00:00");
        let err = validate_continue_provider(&hf, "deepseek").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bad00001"),
            "error should name the history id: {msg}"
        );
        assert!(
            msg.contains("chatgpt"),
            "error should name the original provider: {msg}"
        );
        assert!(
            msg.contains("deepseek"),
            "error should name the requested provider: {msg}"
        );
    }

    #[test]
    fn append_only_save_preserves_earlier_turns() {
        let dir = tempfile::tempdir().unwrap();
        let mut hf = sample_history("app00001", "chatgpt", "2026-08-21T10:00:00+00:00");
        hf.save(Some(dir.path())).unwrap();

        // A later turn appends and re-saves; earlier turns must survive.
        hf.provider_conversation_id = Some("conv-new".to_string());
        hf.turns.push(turn("user", "follow-up"));
        hf.save(Some(dir.path())).unwrap();

        let loaded = HistoryFile::load("app00001", Some(dir.path())).unwrap();
        assert_eq!(loaded.turns.len(), 3);
        assert_eq!(loaded.turns[2].content, "follow-up");
        assert_eq!(loaded.provider_conversation_id.as_deref(), Some("conv-new"));
    }
}
