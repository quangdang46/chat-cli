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

#[derive(Debug, Clone, Default)]
pub struct ChatReq {
    /// The user prompt for this turn.
    pub prompt: String,
    /// Optional system prompt (`-s`).
    pub system: Option<String>,
    /// Already-prepared attachments text (via `prepare_attachments`).
    pub attachments_text: String,
    /// Authenticated session material + persistence target for rolling refresh.
    /// Populated by the dispatcher from `config.toml`; providers that need
    /// tokens (all web providers) read it here and may write refreshed values
    /// back through `persist` before returning.
    pub auth: AuthContext,
}

/// Everything a web provider needs to authenticate one chat turn, plus where
/// to persist a rolled access_token. `session_token` is the long-lived paste;
/// `access_token`/`access_token_expiry` are the rolling cache.
#[derive(Clone, Default)]
pub struct AuthContext {
    pub session_token: Option<String>,
    pub access_token: Option<String>,
    /// RFC 3339 timestamp after which `access_token` must be refreshed.
    pub access_token_expiry: Option<String>,
    /// Called by providers after refreshing credentials so the new
    /// access_token/expiry survive the process. No-op closure when there is
    /// nothing to persist (e.g. tests).
    #[allow(clippy::type_complexity)]
    pub persist: Option<std::sync::Arc<dyn Fn(&str, &str) -> anyhow::Result<()> + Send + Sync>>,
}

impl std::fmt::Debug for AuthContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthContext")
            .field("session_token", &self.session_token.as_ref().map(|_| "…"))
            .field("access_token", &self.access_token.as_ref().map(|_| "…"))
            .field("access_token_expiry", &self.access_token_expiry)
            .field("persist", &self.persist.is_some())
            .finish()
    }
}

impl AuthContext {
    /// Persist a refreshed access token + expiry via the dispatcher-provided
    /// callback. Errors are returned; callers decide whether they are fatal.
    pub fn persist_access_token(&self, token: &str, expiry: &str) -> anyhow::Result<()> {
        match &self.persist {
            Some(save) => save(token, expiry),
            None => Ok(()),
        }
    }

    fn expiry_passed(&self) -> bool {
        match (&self.access_token_expiry, self.access_token.is_some()) {
            (Some(exp), true) => chrono::DateTime::parse_from_rfc3339(exp)
                .map(|t| t < chrono::Utc::now())
                .unwrap_or(true),
            _ => true,
        }
    }

    /// True when the cached access_token exists and its expiry has not passed.
    pub fn has_fresh_access_token(&self) -> bool {
        self.access_token.is_some() && !self.expiry_passed()
    }
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
    inventory::iter::<ProviderEntry>
        .into_iter()
        .map(|e| e.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A mock provider that self-registers via inventory — proves the
    // open-closed contract without importing any concrete provider crate.
    struct MockProvider;

    impl Provider for MockProvider {
        fn id(&self) -> &'static str {
            "mock"
        }

        fn context_limit(&self) -> usize {
            123_456
        }

        fn auth(&self, token: &str) -> anyhow::Result<Session> {
            Ok(Session {
                valid: !token.is_empty(),
                expiry: Some("2026-12-31T00:00:00Z".to_string()),
            })
        }

        fn chat(&self, _handle: &ProviderHandle, req: ChatReq) -> anyhow::Result<ChatResp> {
            Ok(ChatResp {
                content: format!("echo: {}", req.prompt),
                conversation_id: "conv-mock".to_string(),
                message_id: "msg-mock".to_string(),
            })
        }
    }

    inventory::submit! {
        ProviderEntry {
            id: "mock",
            factory: || Box::new(MockProvider),
        }
    }

    #[test]
    fn get_provider_returns_registered_mock() {
        let p = get_provider("mock").expect("mock provider must be discoverable via inventory");
        assert_eq!(p.id(), "mock");
        assert_eq!(p.context_limit(), 123_456);
    }

    #[test]
    fn get_provider_unknown_id_returns_none() {
        assert!(get_provider("no-such-provider").is_none());
    }

    #[test]
    fn list_providers_contains_mock() {
        let ids = list_providers();
        assert!(
            ids.contains(&"mock"),
            "list_providers() should include registered mock, got {:?}",
            ids
        );
    }

    #[test]
    fn mock_auth_and_chat_round_trip() {
        let p = get_provider("mock").unwrap();

        let session = p.auth("token-abc").unwrap();
        assert!(session.valid);
        assert_eq!(session.expiry.as_deref(), Some("2026-12-31T00:00:00Z"));

        let resp = p
            .chat(
                &ProviderHandle::default(),
                ChatReq {
                    prompt: "hi".to_string(),
                    system: None,
                    attachments_text: String::new(),
                    auth: AuthContext::default(),
                },
            )
            .unwrap();

        assert_eq!(resp.content, "echo: hi");
        assert_eq!(resp.conversation_id, "conv-mock");
        assert_eq!(resp.message_id, "msg-mock");
    }

    #[test]
    fn default_prepare_attachments_empty_input_is_empty() {
        let p = get_provider("mock").unwrap();
        let out = p.prepare_attachments(&[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn handle_default_is_new_conversation() {
        let h = ProviderHandle::default();
        assert!(h.conversation_id.is_none());
        assert!(h.parent_message_id.is_none());
    }
}
