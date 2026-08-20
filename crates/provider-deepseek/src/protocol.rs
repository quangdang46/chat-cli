//! DeepSeek web protocol constants (from ds2api/internal/deepseek/protocol).

pub const DEEPSEEK_HOST: &str = "chat.deepseek.com";
pub const LOGIN_URL: &str = "https://chat.deepseek.com/api/v0/users/login";
pub const CREATE_SESSION_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/create";
pub const CREATE_POW_URL: &str = "https://chat.deepseek.com/api/v0/chat/create_pow_challenge";
pub const COMPLETION_URL: &str = "https://chat.deepseek.com/api/v0/chat/completion";
pub const CONTINUE_URL: &str = "https://chat.deepseek.com/api/v0/chat/continue";
pub const FETCH_SESSIONS_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/fetch_page";
