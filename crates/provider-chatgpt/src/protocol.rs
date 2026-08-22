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
pub const SENTINEL_FINALIZE_URL: &str =
    "https://chatgpt.com/backend-api/sentinel/chat-requirements/finalize";
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_https_chatgpt_com() {
        for (name, url) in [
            ("SESSION_URL", SESSION_URL),
            ("SENTINEL_PREPARE_URL", SENTINEL_PREPARE_URL),
            ("SENTINEL_FINALIZE_URL", SENTINEL_FINALIZE_URL),
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
    fn sentinel_urls_are_prepare_and_finalize_pair() {
        assert!(SENTINEL_PREPARE_URL.ends_with("/sentinel/chat-requirements/prepare"));
        assert!(SENTINEL_FINALIZE_URL.ends_with("/sentinel/chat-requirements/finalize"));
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
}
