//! Provider trait + inventory registry.
//!
//! `chat-core` defines the abstraction only — it never imports a concrete provider.
//! Each provider crate calls `inventory::submit!(ProviderEntry { ... })` to self-register.
//! Adding a new provider (e.g. `provider-gemini`) requires zero changes to `chat-core`.

use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Request / response types — shared across all providers
// ---------------------------------------------------------------------------

/// Handle for continuing an existing conversation.
/// `None` means `--new` (create fresh).
#[derive(Debug, Clone, Default)]
pub struct ProviderHandle {
    pub conversation_id: Option<String>,
    pub parent_message_id: Option<String>,
}

/// Input to a chat turn.
#[derive(Debug, Clone)]
pub struct ChatReq {
    /// The user prompt for this turn.
    pub prompt: String,
    /// Optional system prompt (`-s`).
    pub system: Option<String>,
    /// Already-prepared attachments text (via `prepare_attachments`).
    pub attachments_text: String,
}

/// Output from a chat turn, including new IDs to persist in history.
#[derive(Debug, Clone)]
pub struct ChatResp {
    pub content: String,
    pub conversation_id: String,
    pub message_id: String,
}

/// Result of `auth` validation.
#[derive(Debug, Clone)]
pub struct Session {
    pub valid: bool,
    pub expiry: Option<String>,
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    fn context_limit(&self) -> usize;
    fn auth(&self, token: &str) -> anyhow::Result<Session>;
    fn chat(&self, handle: &ProviderHandle, req: ChatReq) -> anyhow::Result<ChatResp>;

    /// Default: read UTF-8 text and inject `<<<FILE: path>>>\n```\ncontent\n```\n`.
    /// Providers that support real file upload (e.g. ChatGPT ` /backend-api/files`)
    /// override this without touching core.
    fn prepare_attachments(&self, files: &[PathBuf]) -> anyhow::Result<String> {
        crate::attach::default_prepare_attachments(files)
    }
}

// ---------------------------------------------------------------------------
// Registry — open-closed: new provider = new crate, no core edit
// ---------------------------------------------------------------------------

pub struct ProviderEntry {
    pub id: &'static str,
    pub factory: fn() -> Box<dyn Provider>,
}

inventory::collect!(ProviderEntry);

/// Look up a provider by id via the inventory registry.
pub fn get_provider(id: &str) -> Option<Box<dyn Provider>> {
    for entry in inventory::iter::<ProviderEntry> {
        if entry.id == id {
            return Some((entry.factory)());
        }
    }
    None
}

/// List all registered provider ids.
pub fn list_providers() -> Vec<&'static str> {
    inventory::iter::<ProviderEntry>.into_iter().map(|e| e.id).collect()
}
