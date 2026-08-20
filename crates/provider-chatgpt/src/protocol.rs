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
