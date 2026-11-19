//! Local history: `~/.local/share/chat-cli/history/<id>.jsonl` append-only.
//!
//! Each line is a `Turn` JSON. The first line is a `role = "__meta__"`
//! header written once at creation. Dynamic metadata (turn_count, preview,
//! provider conversation ids) lives in a sidecar `<id>.meta.json` rewritten
//! atomically on every save, so `save()` appends only the new turn lines
//! instead of rewriting the whole file each turn (O(1) per turn, was O(n)).

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
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
        // Test-only redirect (#1): keeps tests off the real user history —
        // on Windows Known Folders ignore $HOME so env-var tricks don't work.
        if let Some(p) =
            crate::paths::redirected(&dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")))
        {
            return p.join("history");
        }
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("chat-cli")
            .join("history")
    }

    pub fn history_path(&self, override_dir: Option<&Path>) -> anyhow::Result<PathBuf> {
        Self::path_for_id(&self.id, override_dir)
    }

    /// Resolve `<history_dir>/<id>.jsonl` after validating the id.
    ///
    /// Ids are local nanoids; anything outside `[A-Za-z0-9_-]` is rejected so
    /// `load`/`rm`/`--continue` can never escape the history directory via
    /// `../` or absolute-path tricks (path-traversal fix).
    pub fn path_for_id(id: &str, override_dir: Option<&Path>) -> anyhow::Result<PathBuf> {
        validate_history_id(id)?;
        Ok(Self::history_dir(override_dir).join(format!("{id}.jsonl")))
    }

    /// Delete `<id>.jsonl` from the history dir. Validates the id first;
    /// missing entries produce a friendly "not found" instead of a raw OS error.
    pub fn rm(id: &str, override_dir: Option<&Path>) -> anyhow::Result<()> {
        let path = Self::path_for_id(id, override_dir)?;
        if !path.exists() {
            anyhow::bail!(
                "history entry '{id}' not found (run 'chat-cli history list' to see ids)"
            );
        }
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove history file '{}'", path.display()))?;
        // Best-effort: drop the metadata sidecar too (missing is fine).
        let _ = fs::remove_file(Self::meta_sidecar_for_path(&path));
        Ok(())
    }

    /// `<id>.jsonl` → `<id>.meta.json` sibling holding the live meta object.
    fn meta_sidecar_for_path(jsonl_path: &Path) -> PathBuf {
        jsonl_path.with_extension("meta.json")
    }

    /// Serialize the `__meta__` object (same shape as the first JSONL line,
    /// including the `turn_count`/`preview` summary fields `list()` needs).
    fn meta_json(&self) -> anyhow::Result<serde_json::Value> {
        let preview = self
            .turns
            .last()
            .map(|t| truncate(&t.content, 80))
            .unwrap_or_default();
        Ok(serde_json::json!({
            "role": "__meta__",
            "id": self.id,
            "provider": self.provider,
            "provider_conversation_id": self.provider_conversation_id,
            "provider_parent_message_id": self.provider_parent_message_id,
            "created_at": self.created_at,
            "turn_count": self.turns.len(),
            "preview": preview,
        }))
    }

    /// FNV-1a 64 fingerprint of one serialized turn line — lets the append
    /// path verify the on-disk tail still matches `self.turns` without
    /// re-reading the whole file.
    fn turn_fingerprint(line: &str) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in line.as_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// Atomic tmp+rename write with 0600 perms — same scheme as
    /// Config::save; history holds conversation content.
    fn atomic_write(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
        let tmp = path.with_extension(format!(
            "{}.tmp",
            path.extension().and_then(|e| e.to_str()).unwrap_or("tmp")
        ));
        fs::write(&tmp, contents)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Persist the live meta object to `<id>.meta.json` (atomic, 0600).
    /// `file_bytes`/`tail_hash` let the next `save` prove the JSONL on disk
    /// is exactly the prefix of `self.turns` it expects before appending.
    fn write_sidecar(
        &self,
        jsonl_path: &Path,
        meta: &serde_json::Value,
        file_bytes: u64,
        tail_hash: u64,
    ) -> anyhow::Result<()> {
        let mut sidecar = meta.clone();
        if let Some(obj) = sidecar.as_object_mut() {
            obj.insert("file_bytes".to_string(), serde_json::json!(file_bytes));
            obj.insert("tail_hash".to_string(), serde_json::json!(tail_hash));
        }
        let sidecar_path = Self::meta_sidecar_for_path(jsonl_path);
        Self::atomic_write(&sidecar_path, serde_json::to_string(&sidecar)?.as_bytes())
    }

    /// Read the sidecar bookkeeping fields, if present and well-formed.
    fn read_sidecar(path: &Path) -> Option<(usize, u64, u64)> {
        let content = fs::read_to_string(Self::meta_sidecar_for_path(path)).ok()?;
        let v: serde_json::Value = serde_json::from_str(&content).ok()?;
        let turn_count = v.get("turn_count")?.as_u64()? as usize;
        let file_bytes = v.get("file_bytes")?.as_u64()?;
        let tail_hash = v.get("tail_hash")?.as_u64()?;
        Some((turn_count, file_bytes, tail_hash))
    }

    /// Fingerprint of the last persisted turn line (0 when there are none).
    fn tail_fingerprint(turns: &[Turn]) -> anyhow::Result<u64> {
        match turns.last() {
            Some(t) => Ok(Self::turn_fingerprint(&serde_json::to_string(t)?)),
            None => Ok(0),
        }
    }

    pub fn save(&self, override_dir: Option<&Path>) -> anyhow::Result<()> {
        let path = self.history_path(override_dir)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let meta = self.meta_json()?;
        let meta_line = serde_json::to_string(&meta)?;

        if path.exists() {
            // Append path: the sidecar records how many leading turns were
            // persisted, the exact file size, and a fingerprint of the last
            // persisted turn — all three must match or we fall back to a
            // full atomic rewrite (legacy files without a sidecar, or any
            // crash/corruption divergence). Only the NEW suffix is
            // serialized, keeping save() O(new turns), not O(history).
            let file_len = fs::metadata(&path)?.len();
            let append_from = Self::read_sidecar(&path).and_then(|(count, bytes, tail)| {
                if bytes != file_len || count > self.turns.len() {
                    return None;
                }
                if count > 0 {
                    let line = serde_json::to_string(&self.turns[count - 1]).ok()?;
                    if Self::turn_fingerprint(&line) != tail {
                        return None;
                    }
                }
                Some(count)
            });

            if let Some(k) = append_from {
                // Turns[..k] are already on disk as whole lines; append the
                // suffix in one write_all + fsync so a crash can at worst
                // leave a torn FINAL line (detected next save via the
                // file_bytes check → full rewrite), never corrupt earlier
                // lines.
                let mut suffix = String::new();
                for t in &self.turns[k..] {
                    suffix.push_str(&serde_json::to_string(t)?);
                    suffix.push('\n');
                }
                if !suffix.is_empty() {
                    let mut f = fs::OpenOptions::new().append(true).open(&path)?;
                    f.write_all(suffix.as_bytes())?;
                    f.sync_all()?;
                }
                let tail_hash = Self::tail_fingerprint(&self.turns)?;
                self.write_sidecar(&path, &meta, file_len + suffix.len() as u64, tail_hash)?;
                return Ok(());
            }
        }

        // New file, or legacy/diverged file: atomic rewrite — meta line +
        // all turns in one tmp+rename (0600), then refresh the sidecar.
        let mut out = String::with_capacity(
            meta_line.len()
                + 1
                + self
                    .turns
                    .iter()
                    .map(|t| t.content.len() + 64)
                    .sum::<usize>(),
        );
        out.push_str(&meta_line);
        out.push('\n');
        for t in &self.turns {
            out.push_str(&serde_json::to_string(t)?);
            out.push('\n');
        }
        Self::atomic_write(&path, out.as_bytes())?;
        let tail_hash = Self::tail_fingerprint(&self.turns)?;
        self.write_sidecar(&path, &meta, out.len() as u64, tail_hash)?;
        Ok(())
    }

    pub fn load(id: &str, override_dir: Option<&Path>) -> anyhow::Result<Self> {
        let path = Self::path_for_id(id, override_dir)?;
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read history file '{}'", path.display()))?;
        let mut hf = Self::from_jsonl(&content)?;
        // The first-line __meta__ is a creation-time snapshot; the sidecar
        // carries the live provider conversation ids (rewritten every save).
        if let Ok(sidecar) = fs::read_to_string(Self::meta_sidecar_for_path(&path)) {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&sidecar) {
                hf.provider_conversation_id = meta
                    .get("provider_conversation_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                hf.provider_parent_message_id = meta
                    .get("provider_parent_message_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
        }
        Ok(hf)
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
            // Fast path: the `<id>.meta.json` sidecar (new files) or the
            // `turn_count`/`preview` fields duplicated into the __meta__
            // first line (legacy files) let a summary be built from one
            // small read — not a full parse of every turn in every file.
            if let Some(summary) =
                Self::summary_from_sidecar(&path).or_else(|| Self::summary_from_meta_line(&path))
            {
                out.push(summary);
                continue;
            }
            // Fallback for legacy files written before the meta summary
            // fields existed: full parse.
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

    /// Build a `HistorySummary` from the `<id>.meta.json` sidecar written by
    /// `save` — one small file read per history, no JSONL scan at all.
    fn summary_from_sidecar(jsonl_path: &Path) -> Option<HistorySummary> {
        let content = fs::read_to_string(Self::meta_sidecar_for_path(jsonl_path)).ok()?;
        let meta: serde_json::Value = serde_json::from_str(&content).ok()?;
        if meta.get("role").and_then(|v| v.as_str()) != Some("__meta__") {
            return None;
        }
        let turn_count = meta.get("turn_count")?.as_u64()? as usize;
        let preview = meta.get("preview")?.as_str()?.to_string();
        Some(HistorySummary {
            id: meta.get("id")?.as_str()?.to_string(),
            provider: meta.get("provider")?.as_str()?.to_string(),
            created_at: meta.get("created_at")?.as_str()?.to_string(),
            turn_count,
            preview,
        })
    }

    /// Build a `HistorySummary` from only the first (`__meta__`) line of a
    /// history file. Returns `None` for legacy files whose meta line lacks
    /// the `turn_count`/`preview` summary fields.
    fn summary_from_meta_line(path: &Path) -> Option<HistorySummary> {
        let file = fs::File::open(path).ok()?;
        // Meta lines are small; cap the read so a corrupt giant first line
        // can't make `history list` swallow unbounded memory.
        let mut reader = BufReader::new(file.take(MAX_META_LINE_BYTES));
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let meta: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        if meta.get("role").and_then(|v| v.as_str()) != Some("__meta__") {
            return None;
        }
        let turn_count = meta.get("turn_count")?.as_u64()? as usize;
        let preview = meta.get("preview")?.as_str()?.to_string();
        Some(HistorySummary {
            id: meta.get("id")?.as_str()?.to_string(),
            provider: meta.get("provider")?.as_str()?.to_string(),
            created_at: meta.get("created_at")?.as_str()?.to_string(),
            turn_count,
            preview,
        })
    }

    /// Text to inject for `--continue` (all turns concatenated), capped to
    /// the most recent turns that fit `MAX_CONTEXT_TEXT_CHARS` — replaying an
    /// unbounded history can exceed the provider's context window.
    pub fn to_context_text(&self) -> String {
        // Walk newest→oldest, keeping turns until the char budget is spent.
        let mut kept: Vec<&Turn> = Vec::new();
        let mut used = 0usize;
        for t in self.turns.iter().rev() {
            let cost = t.role.len() + t.content.len() + 4; // "role: \n" overhead
            if !kept.is_empty() && used + cost > MAX_CONTEXT_TEXT_CHARS {
                break;
            }
            used += cost;
            kept.push(t);
        }
        kept.reverse();
        let omitted = self.turns.len() - kept.len();
        let mut s = String::new();
        if omitted > 0 {
            s.push_str("[... ");
            s.push_str(&omitted.to_string());
            s.push_str(" earlier turns omitted ...]\n");
        }
        for (i, t) in kept.iter().enumerate() {
            // The newest turn is always kept even when it alone exceeds the
            // budget — replaying it whole would then fail budget_check and
            // kill the whole chat. Truncate it head+tail instead.
            let is_newest = i + 1 == kept.len();
            let overhead = t.role.len() + 4; // "role: \n"
            if is_newest && overhead + t.content.len() > MAX_CONTEXT_TEXT_CHARS {
                let budget = MAX_CONTEXT_TEXT_CHARS.saturating_sub(overhead);
                s.push_str(&t.role);
                s.push_str(": ");
                s.push_str(&truncate_middle(&t.content, budget));
                s.push('\n');
            } else {
                s.push_str(&t.role);
                s.push_str(": ");
                s.push_str(&t.content);
                s.push('\n');
            }
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

/// Char budget for `to_context_text` replay (roughly the text volume that
/// fits a typical provider context window with headroom for the new prompt).
pub const MAX_CONTEXT_TEXT_CHARS: usize = 100_000;

/// Upper bound on bytes read for the `__meta__` line during `list`.
const MAX_META_LINE_BYTES: u64 = 1 << 20; // 1 MiB

/// History ids are nanoid-style tokens; anything outside this alphabet is
/// rejected before it can be joined into a filesystem path.
const MAX_ID_LEN: usize = 64;

fn nanoid() -> String {
    // 12-char id (8 hex + '-' + 3 hex from the UUID prefix): ~48 bits of
    // entropy — the old 8-char ids (32 bits) collided around ~65k histories.
    Uuid::new_v4().to_string()[..12].to_string()
}

/// Validate a user-supplied history id (`history show/rm`, `--continue <id>`).
/// Rejects path separators, `..`, and anything outside the nanoid alphabet so
/// ids can never escape the history directory.
pub fn validate_history_id(id: &str) -> anyhow::Result<()> {
    let ok = !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        anyhow::bail!(
            "invalid history id '{id}' — expected 1-{MAX_ID_LEN} chars of [A-Za-z0-9_-] \
             (run 'chat-cli history list' to see valid ids)"
        );
    }
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    // Char-boundary safe: byte slicing would panic on multibyte UTF-8.
    // Single pass: the byte index of the n-th char boundary is the cut point.
    match s.char_indices().nth(n) {
        Some((byte_idx, _)) => {
            let mut out = String::with_capacity(byte_idx + 3);
            out.push_str(&s[..byte_idx]);
            out.push_str("...");
            out
        }
        None => s.to_string(),
    }
}

/// Truncate `s` to about `max_chars`, keeping the head and tail with an
/// omission marker — better than a head-only cut for code/log-shaped turns
/// where the end carries the answer.
fn truncate_middle(s: &str, max_chars: usize) -> String {
    const MARKER: &str = "\n[... middle omitted ...]\n";
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let budget = max_chars.saturating_sub(MARKER.chars().count());
    if budget == 0 {
        return MARKER.trim().to_string();
    }
    let head = budget / 2;
    let tail = budget - head;
    let total = s.chars().count();
    let head_s: String = s.chars().take(head).collect();
    let tail_s: String = s.chars().skip(total - tail).collect();
    format!("{head_s}{MARKER}{tail_s}")
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
    fn new_history_has_12_char_id_and_no_provider_ids() {
        let hf = HistoryFile::new("chatgpt");
        assert_eq!(hf.id.len(), 12, "local history id must be a 12-char nanoid");
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

    // --- path traversal regression (bead chat-cli-history-id-path-traversal-e5p) ---

    #[test]
    fn validate_history_id_accepts_nanoid_alphabet() {
        for id in ["abcd1234", "a-b_c-9", &"x".repeat(64)] {
            assert!(validate_history_id(id).is_ok(), "{id} must be accepted");
        }
    }

    #[test]
    fn validate_history_id_rejects_traversal_and_separators() {
        for id in [
            "..",
            "../victim",
            "..\\victim",
            "a/b",
            "a\\b",
            "/etc/passwd",
            "C:\\x",
            "",
            "has space",
            "dot.name",
            &"x".repeat(65),
        ] {
            let err = validate_history_id(id).unwrap_err();
            assert!(
                err.to_string().contains("invalid history id"),
                "{id:?} must be rejected: {err}"
            );
        }
    }

    #[test]
    fn load_rejects_traversal_id_before_touching_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        // A real file outside the history dir that a traversal id would hit.
        let victim = dir.path().join("victim.jsonl");
        std::fs::write(&victim, "{\"role\":\"__meta__\"}\n").unwrap();
        let history_dir = dir.path().join("history");
        std::fs::create_dir_all(&history_dir).unwrap();

        for bad in ["../victim", "..\\victim", "../../etc/passwd"] {
            assert!(
                HistoryFile::load(bad, Some(&history_dir)).is_err(),
                "load({bad:?}) must be rejected"
            );
        }
        // The victim file must be untouched.
        assert!(victim.exists());
    }

    #[test]
    fn rm_rejects_traversal_and_never_deletes_outside_dir() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.jsonl");
        std::fs::write(&victim, "data").unwrap();
        let history_dir = dir.path().join("history");
        std::fs::create_dir_all(&history_dir).unwrap();

        for bad in ["../victim", "..\\victim"] {
            assert!(HistoryFile::rm(bad, Some(&history_dir)).is_err());
        }
        assert!(
            victim.exists(),
            "traversal rm must not delete outside files"
        );
    }

    #[test]
    fn save_rejects_hostile_id_instead_of_writing_outside_dir() {
        let dir = tempfile::tempdir().unwrap();
        let history_dir = dir.path().join("history");
        std::fs::create_dir_all(&history_dir).unwrap();
        let mut hf = sample_history("../escape", "chatgpt", "2026-08-21T10:00:00+00:00");
        hf.id = "../escape".to_string();
        assert!(hf.save(Some(&history_dir)).is_err());
        assert!(!dir.path().join("escape.jsonl").exists());
    }

    #[cfg(unix)]
    #[test]
    fn saved_history_file_has_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        sample_history("perm00001", "chatgpt", "2026-08-21T10:00:00+00:00")
            .save(Some(dir.path()))
            .unwrap();
        let mode = std::fs::metadata(dir.path().join("perm00001.jsonl"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "history holds conversation content");
    }

    #[test]
    fn save_is_atomic_and_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        sample_history("atom00001", "chatgpt", "2026-08-21T10:00:00+00:00")
            .save(Some(dir.path()))
            .unwrap();
        assert!(dir.path().join("atom00001.jsonl").exists());
        assert!(
            !dir.path().join("atom00001.jsonl.tmp").exists(),
            "atomic rename must consume the temp file"
        );
    }

    #[test]
    fn list_reads_meta_line_summary_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut hf = sample_history("meta0002", "chatgpt", "2026-08-21T10:00:00+00:00");
        hf.turns.push(turn("user", "extra turn"));
        hf.save(Some(dir.path())).unwrap();

        // Meta line must carry the summary fields list() relies on.
        let content = std::fs::read_to_string(dir.path().join("meta0002.jsonl")).unwrap();
        let meta: serde_json::Value =
            serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(meta["turn_count"], 3);
        assert_eq!(meta["preview"], "extra turn");

        let items = HistoryFile::list(Some(dir.path())).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].turn_count, 3);
        assert_eq!(items[0].preview, "extra turn");
    }

    #[test]
    fn to_context_text_caps_replay_to_recent_turns() {
        let mut hf = sample_history("cap000001", "chatgpt", "2026-08-21T10:00:00+00:00");
        // Fill well past the char budget with old turns, then a fresh tail.
        for i in 0..50 {
            hf.turns.insert(
                0,
                turn("user", &format!("old-turn-{i}-{}", "x".repeat(5_000))),
            );
        }
        hf.turns.push(turn("user", "fresh-tail"));
        let text = hf.to_context_text();
        assert!(
            text.chars().count() <= MAX_CONTEXT_TEXT_CHARS + 200,
            "replay must stay within budget: {} chars",
            text.chars().count()
        );
        assert!(text.contains("fresh-tail"), "newest turn must survive");
        assert!(
            text.contains("earlier turns omitted"),
            "truncation must be announced"
        );
        // old-turn-49 is the oldest (inserted first at index 0 repeatedly);
        // old-turn-0 sits closest to the tail and survives.
        assert!(!text.contains("old-turn-49-"), "oldest turns dropped");
    }

    #[test]
    fn to_context_text_truncates_single_oversized_newest_turn() {
        let mut hf = sample_history("big00001", "chatgpt", "2026-08-21T10:00:00+00:00");
        // One turn larger than the whole replay budget: it must be cut
        // head+tail (with a marker), not replayed whole — a whole replay
        // would blow the provider budget check and fail the chat.
        let huge = format!("HEAD-{}-TAIL", "x".repeat(MAX_CONTEXT_TEXT_CHARS + 10_000));
        hf.turns = vec![turn("user", &huge)];
        let text = hf.to_context_text();
        assert!(
            text.chars().count() <= MAX_CONTEXT_TEXT_CHARS + 200,
            "oversized turn must be truncated: {} chars",
            text.chars().count()
        );
        assert!(
            text.contains("middle omitted"),
            "truncation must be announced"
        );
        assert!(text.contains("HEAD-"), "head must survive");
        assert!(text.contains("-TAIL"), "tail must survive");
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

    #[test]
    fn save_appends_only_new_turn_lines() {
        let dir = tempfile::tempdir().unwrap();
        let mut hf = sample_history("app00002", "chatgpt", "2026-08-21T10:00:00+00:00");
        hf.save(Some(dir.path())).unwrap();
        let path = dir.path().join("app00002.jsonl");
        let after_first = std::fs::read_to_string(&path).unwrap();

        hf.turns.push(turn("user", "second"));
        hf.turns.push(turn("assistant", "third"));
        hf.save(Some(dir.path())).unwrap();
        let after_second = std::fs::read_to_string(&path).unwrap();

        // The original bytes must be a strict prefix — nothing rewritten.
        assert!(
            after_second.starts_with(&after_first),
            "append-only save must not rewrite existing lines"
        );
        assert_eq!(
            after_second.lines().count(),
            after_first.lines().count() + 2
        );

        // Sidecar carries the live summary for list().
        let items = HistoryFile::list(Some(dir.path())).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].turn_count, 4);
        assert_eq!(items[0].preview, "third");

        // And load() sees the appended turns plus live conv ids.
        let loaded = HistoryFile::load("app00002", Some(dir.path())).unwrap();
        assert_eq!(loaded.turns.len(), 4);
        assert_eq!(loaded.turns[3].content, "third");
    }

    #[test]
    fn save_falls_back_to_rewrite_when_sidecar_diverges() {
        let dir = tempfile::tempdir().unwrap();
        let mut hf = sample_history("app00003", "chatgpt", "2026-08-21T10:00:00+00:00");
        hf.save(Some(dir.path())).unwrap();
        let path = dir.path().join("app00003.jsonl");

        // Simulate external corruption: append garbage bytes to the JSONL.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"garbage\n").unwrap();
        drop(f);

        // Next save must detect the size mismatch and rewrite cleanly.
        hf.turns.push(turn("user", "after-corruption"));
        hf.save(Some(dir.path())).unwrap();
        let loaded = HistoryFile::load("app00003", Some(dir.path())).unwrap();
        assert_eq!(loaded.turns.len(), 3);
        assert_eq!(loaded.turns[2].content, "after-corruption");
    }
}
