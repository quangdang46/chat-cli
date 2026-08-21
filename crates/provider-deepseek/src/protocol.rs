//! DeepSeek web protocol constants — mirrored from ds2api/internal/deepseek/protocol.
//!
//! Source of truth: `ds2api` reference implementation (`constants.go` +
//! `constants_shared.json`) plus its client's `authHeaders()` helper
//! (`authorization: Bearer <token>`, `x-ds-pow-response` for PoW-gated calls).
//! No logic lives here beyond deterministic header builders.

/// Host for all DeepSeek web endpoints.
pub const DEEPSEEK_HOST: &str = "chat.deepseek.com";

// ---------------------------------------------------------------------------
// Endpoints — all under https://chat.deepseek.com/api/v0/
// ---------------------------------------------------------------------------

pub const LOGIN_URL: &str = "https://chat.deepseek.com/api/v0/users/login";
pub const CREATE_SESSION_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/create";
pub const CREATE_POW_URL: &str = "https://chat.deepseek.com/api/v0/chat/create_pow_challenge";
pub const COMPLETION_URL: &str = "https://chat.deepseek.com/api/v0/chat/completion";
pub const CONTINUE_URL: &str = "https://chat.deepseek.com/api/v0/chat/continue";
pub const FETCH_SESSIONS_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/fetch_page";
pub const DELETE_SESSION_URL: &str = "https://chat.deepseek.com/api/v0/chat_session/delete";
pub const DELETE_ALL_SESSIONS_URL: &str =
    "https://chat.deepseek.com/api/v0/chat_session/delete_all";
pub const UPLOAD_FILE_URL: &str = "https://chat.deepseek.com/api/v0/file/upload_file";
pub const FETCH_FILES_URL: &str = "https://chat.deepseek.com/api/v0/file/fetch_files";

/// Target paths used inside PoW payloads (`target_path` field).
pub const COMPLETION_TARGET_PATH: &str = "/api/v0/chat/completion";
pub const UPLOAD_TARGET_PATH: &str = "/api/v0/file/upload_file";

// ---------------------------------------------------------------------------
// Client identity (from constants_shared.json) → base headers
// ---------------------------------------------------------------------------

pub const CLIENT_NAME: &str = "DeepSeek";
pub const CLIENT_PLATFORM: &str = "android";
pub const CLIENT_VERSION: &str = "2.0.4";
pub const CLIENT_ANDROID_API_LEVEL: &str = "35";
pub const CLIENT_LOCALE: &str = "zh_CN";

/// Header name carrying the solved PoW payload (base64 of JSON).
pub const POW_RESPONSE_HEADER: &str = "x-ds-pow-response";

/// Header names that must never be duplicated by callers.
pub const AUTHORIZATION_HEADER: &str = "authorization";

/// Static base headers every request carries (mirrors defaultStaticBaseHeaders).
pub fn base_headers() -> Vec<(&'static str, String)> {
    let user_agent = format!(
        "{}/{} Android/{}",
        CLIENT_NAME, CLIENT_VERSION, CLIENT_ANDROID_API_LEVEL
    );
    vec![
        ("Host", DEEPSEEK_HOST.to_string()),
        ("Accept", "application/json".to_string()),
        ("Content-Type", "application/json".to_string()),
        ("accept-charset", "UTF-8".to_string()),
        ("User-Agent", user_agent),
        ("x-client-platform", CLIENT_PLATFORM.to_string()),
        ("x-client-version", CLIENT_VERSION.to_string()),
        ("x-client-locale", CLIENT_LOCALE.to_string()),
    ]
}

/// Base headers plus `authorization: Bearer <token>` — mirrors client_auth.go.
pub fn auth_headers(token: &str) -> Vec<(&'static str, String)> {
    let mut out = base_headers();
    out.push((AUTHORIZATION_HEADER, format!("Bearer {}", token)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_match_ds2api_reference() {
        assert_eq!(DEEPSEEK_HOST, "chat.deepseek.com");
        assert_eq!(LOGIN_URL, "https://chat.deepseek.com/api/v0/users/login");
        assert_eq!(
            CREATE_SESSION_URL,
            "https://chat.deepseek.com/api/v0/chat_session/create"
        );
        assert_eq!(
            CREATE_POW_URL,
            "https://chat.deepseek.com/api/v0/chat/create_pow_challenge"
        );
        assert_eq!(
            COMPLETION_URL,
            "https://chat.deepseek.com/api/v0/chat/completion"
        );
        assert_eq!(
            CONTINUE_URL,
            "https://chat.deepseek.com/api/v0/chat/continue"
        );
        assert_eq!(
            FETCH_SESSIONS_URL,
            "https://chat.deepseek.com/api/v0/chat_session/fetch_page"
        );
        assert_eq!(
            DELETE_SESSION_URL,
            "https://chat.deepseek.com/api/v0/chat_session/delete"
        );
        assert_eq!(
            DELETE_ALL_SESSIONS_URL,
            "https://chat.deepseek.com/api/v0/chat_session/delete_all"
        );
        assert_eq!(
            UPLOAD_FILE_URL,
            "https://chat.deepseek.com/api/v0/file/upload_file"
        );
        assert_eq!(
            FETCH_FILES_URL,
            "https://chat.deepseek.com/api/v0/file/fetch_files"
        );
        assert_eq!(COMPLETION_TARGET_PATH, "/api/v0/chat/completion");
        assert_eq!(UPLOAD_TARGET_PATH, "/api/v0/file/upload_file");
    }

    #[test]
    fn base_headers_match_reference_defaults() {
        let headers = base_headers();

        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        };

        assert_eq!(get("Host").as_deref(), Some("chat.deepseek.com"));
        assert_eq!(get("Accept").as_deref(), Some("application/json"));
        assert_eq!(get("Content-Type").as_deref(), Some("application/json"));
        assert_eq!(get("accept-charset").as_deref(), Some("UTF-8"));
        // User-Agent built as Name/Version Android/APILevel per buildBaseHeaders().
        assert_eq!(
            get("User-Agent").as_deref(),
            Some("DeepSeek/2.0.4 Android/35")
        );
        assert_eq!(get("x-client-platform").as_deref(), Some("android"));
        assert_eq!(get("x-client-version").as_deref(), Some("2.0.4"));
        assert_eq!(get("x-client-locale").as_deref(), Some("zh_CN"));
    }

    #[test]
    fn auth_headers_add_bearer_and_keep_base() {
        let headers = auth_headers("my-session-token");

        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(
            get("authorization").as_deref(),
            Some("Bearer my-session-token")
        );
        // Base headers survive.
        assert_eq!(get("Host").as_deref(), Some("chat.deepseek.com"));
        assert_eq!(
            get("User-Agent").as_deref(),
            Some("DeepSeek/2.0.4 Android/35")
        );

        // No duplicate Host/Accept entries.
        let host_count = headers.iter().filter(|(k, _)| *k == "Host").count();
        assert_eq!(host_count, 1);
    }
}
