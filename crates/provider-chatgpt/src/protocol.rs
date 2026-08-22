//! ChatGPT web protocol constants.
//!
//! Discovered via Chrome DevTools (2026-08-20):
//! - POST /unauth-mweb/sentinel/chat-requirements/prepare
//! - POST /backend-api/sentinel/chat-requirements/finalize
//! - GET/POST /backend-api/conversation/*

pub const CHATGPT_HOST: &str = "chatgpt.com";
pub const SESSION_URL: &str = "https://chatgpt.com/api/auth/session";
pub const SENTINEL_PREPARE_URL: &str =
    "https://chatgpt.com/unauth-mweb/sentinel/chat-requirements/prepare";
pub const SENTINEL_REQUIREMENTS_URL: &str =
    "https://chatgpt.com/backend-api/sentinel/chat-requirements";
pub const CONVERSATION_URL: &str = "https://chatgpt.com/backend-api/conversation";
pub const CONVERSATIONS_LIST_URL: &str = "https://chatgpt.com/backend-api/conversations";

/// Browser cookie carrying the long-lived web session (DevTools →
/// Application → Cookies). Pasted once at `auth login`; refreshed values are
/// persisted by the dispatcher.
pub const SESSION_COOKIE_NAME: &str = "__Secure-next-auth.session-token";

/// Body of `GET /api/auth/session`. A logged-in session has a non-null
/// `accessToken`; an expired/anonymous session returns 200 with both fields
/// null — that must surface as an auth failure, not a panic on unwrap.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SessionResponse {
    #[serde(rename = "accessToken", default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub expires: Option<String>,
}

/// `POST /sentinel/chat-requirements` — server decides whether a proof-of-work
/// is required before a conversation POST.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChatRequirements {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(rename = "proofofwork", default)]
    pub proof_of_work: Option<PowRequired>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PowRequired {
    pub required: bool,
    pub seed: String,
    /// Base64 blob encoding difficulty + config for the sentinel solver.
    pub difficulty: String,
}

/// Body of `POST /backend-api/conversation`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationPayload {
    pub action: &'static str,
    pub messages: Vec<ConversationMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(rename = "parent_message_id")]
    pub parent_message_id: String,
    pub model: &'static str,
    #[serde(rename = "history_and_training_disabled")]
    pub history_and_training_disabled: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationMessage {
    pub id: String,
    pub role: &'static str,
    pub content: MessageContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// POC sends text parts only (attachments are already fenced inline).
#[derive(Debug, Clone, serde::Serialize)]
pub struct MessageContent {
    #[serde(rename = "content_type")]
    pub content_type: &'static str,
    pub parts: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_https_chatgpt_com() {
        for (name, url) in [
            ("SESSION_URL", SESSION_URL),
            ("SENTINEL_PREPARE_URL", SENTINEL_PREPARE_URL),
            ("SENTINEL_REQUIREMENTS_URL", SENTINEL_REQUIREMENTS_URL),
            ("CONVERSATION_URL", CONVERSATION_URL),
            ("CONVERSATIONS_LIST_URL", CONVERSATIONS_LIST_URL),
        ] {
            assert!(
                url.starts_with("https://chatgpt.com/"),
                "{name} must target https://chatgpt.com, got {url}"
            );
        }
        assert_eq!(CHATGPT_HOST, "chatgpt.com");
    }

    #[test]
    fn session_url_is_next_auth_endpoint() {
        assert_eq!(SESSION_URL, "https://chatgpt.com/api/auth/session");
    }

    #[test]
    fn sentinel_urls_cover_prepare_and_requirements() {
        assert!(SENTINEL_PREPARE_URL.ends_with("/sentinel/chat-requirements/prepare"));
        assert!(
            SENTINEL_REQUIREMENTS_URL.ends_with("/sentinel/chat-requirements"),
            "requirements endpoint drives the PoW decision"
        );
    }

    #[test]
    fn conversation_urls_target_backend_api() {
        assert_eq!(
            CONVERSATION_URL,
            "https://chatgpt.com/backend-api/conversation"
        );
        assert_eq!(
            CONVERSATIONS_LIST_URL,
            "https://chatgpt.com/backend-api/conversations"
        );
    }

    #[test]
    fn session_cookie_name_is_secure_next_auth() {
        assert_eq!(SESSION_COOKIE_NAME, "__Secure-next-auth.session-token");
    }

    #[test]
    fn session_response_parses_access_token_and_expires() {
        let body = r#"{"accessToken":"abc","expires":"2026-09-21T00:00:00.000Z"}"#;
        let parsed: SessionResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.access_token.as_deref(), Some("abc"));
        assert_eq!(parsed.expires.as_deref(), Some("2026-09-21T00:00:00.000Z"));
    }

    #[test]
    fn session_response_accepts_null_fields_for_expired_session() {
        let parsed: SessionResponse =
            serde_json::from_str(r#"{"accessToken":null,"expires":null}"#)
                .expect("expired session body must deserialize");
        assert!(parsed.access_token.is_none());
        assert!(parsed.expires.is_none());
    }

    #[test]
    fn chat_requirements_parses_token_and_optional_pow() {
        let with_pow: ChatRequirements = serde_json::from_str(
            r#"{"token":"t-123","proofofwork":{"required":true,"seed":"s","difficulty":"0.9"}}"#,
        )
        .unwrap();
        assert_eq!(with_pow.token.as_deref(), Some("t-123"));
        let pow = with_pow.proof_of_work.expect("pow required");
        assert!(pow.required);
        assert_eq!(pow.seed, "s");

        let without_pow: ChatRequirements = serde_json::from_str(r#"{"token":"t-123"}"#).unwrap();
        assert!(without_pow.proof_of_work.is_none());
    }

    #[test]
    fn conversation_payload_serializes_new_and_continue_shapes() {
        // --new: no conversation_id, fresh parent uuid
        let new_payload = ConversationPayload {
            action: "next",
            messages: vec![ConversationMessage {
                id: "msg-1".into(),
                role: "user",
                content: MessageContent {
                    content_type: "text",
                    parts: vec!["hi".into()],
                },
                metadata: None,
            }],
            conversation_id: None,
            parent_message_id: "parent-uuid".into(),
            model: "auto",
            history_and_training_disabled: false,
        };
        let v: serde_json::Value = serde_json::to_value(&new_payload).unwrap();
        assert_eq!(v["action"], "next");
        assert!(v.get("conversation_id").is_none(), "new must omit id");
        assert_eq!(v["parent_message_id"], "parent-uuid");
        assert_eq!(v["messages"][0]["content"]["content_type"], "text");

        // --continue: carries stored conversation_id
        let mut cont = new_payload.clone();
        cont.conversation_id = Some("conv-uuid".into());
        let v: serde_json::Value = serde_json::to_value(&cont).unwrap();
        assert_eq!(v["conversation_id"], "conv-uuid");
    }
}
