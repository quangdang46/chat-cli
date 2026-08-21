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
}
