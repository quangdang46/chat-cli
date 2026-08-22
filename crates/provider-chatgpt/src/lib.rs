pub mod pow;
pub mod protocol;

use std::time::Duration;

use anyhow::{bail, Context};
use chat_core::provider::{AuthContext, ChatReq, ChatResp, Provider, ProviderHandle, Session};

use crate::protocol::{SessionResponse, SESSION_COOKIE_NAME, SESSION_URL};

pub struct ChatGptProvider;

impl ChatGptProvider {
    fn http_client() -> reqwest::Result<reqwest::blocking::Client> {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
    }

    /// `GET <session_url>` with the pasted session cookie.
    ///
    /// The URL is a parameter (production passes `SESSION_URL`; tests point at
    /// a mock server) so mocked tests exercise this exact code path.
    /// Returns `(valid, access_token, expires)`; an expired/anonymous session
    /// is HTTP 200 with null accessToken and must read as invalid, not as a
    /// JSON panic.
    fn fetch_session(
        session_url: &str,
        token: &str,
    ) -> anyhow::Result<(bool, Option<String>, Option<String>)> {
        let client = Self::http_client().context("failed to build HTTP client")?;
        let resp = client
            .get(session_url)
            .header("Cookie", format!("{}={}", SESSION_COOKIE_NAME, token))
            .send()
            .with_context(|| format!("GET {session_url} failed — check network/proxy"))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Ok((false, None, None));
        }
        if !status.is_success() {
            bail!("GET {session_url} returned {status}: session endpoint unavailable, retry later");
        }
        let body: SessionResponse = resp.json().context("invalid JSON from session endpoint")?;
        match body.access_token {
            Some(at) => Ok((true, Some(at), body.expires)),

            None => Ok((false, None, body.expires)),
        }
    }

    /// Refresh via the session endpoint and persist through `auth.persist`.
    /// Returns the fresh access_token.
    fn refresh_access_token_at(auth: &AuthContext, session_url: &str) -> anyhow::Result<String> {
        let session_token = auth.session_token.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "no chatgpt session_token in config — run 'chat-cli auth login chatgpt' first"
            )
        })?;
        let (valid, access_token, expiry) = Self::fetch_session(session_url, session_token)?;
        if !valid {
            bail!(
                "chatgpt session token is invalid or expired — re-copy {} from \
                 DevTools → Application → Cookies and run 'chat-cli auth login chatgpt --token ...'",
                SESSION_COOKIE_NAME
            );
        }
        let access_token = access_token.expect("valid session must carry accessToken");
        let expiry = expiry.unwrap_or_default();
        // Best-effort persistence: a failed config write must not kill the chat.
        if let Err(e) = auth.persist_access_token(&access_token, &expiry) {
            eprintln!("warning: could not persist refreshed access_token: {e}");
        }
        Ok(access_token)
    }

    /// Ensure a usable access_token: cached-and-fresh wins, else refresh.
    fn ensure_fresh_access_token_at(
        auth: &AuthContext,
        session_url: &str,
    ) -> anyhow::Result<String> {
        if auth.has_fresh_access_token() {
            Ok(auth.access_token.clone().expect("checked fresh"))
        } else {
            Self::refresh_access_token_at(auth, session_url)
        }
    }
    // Test-only seams: production paths go through the `Provider` impl with
    // the real SESSION_URL; these expose the same logic at an injected URL.
    #[cfg(test)]
    fn auth_with_url(&self, session_url: &str, token: &str) -> anyhow::Result<Session> {
        if token.trim().is_empty() {
            bail!(
                "empty session token — copy {} exactly from DevTools → Application → Cookies",
                SESSION_COOKIE_NAME
            );
        }
        let (valid, _access, expires) = Self::fetch_session(session_url, token)?;
        Ok(Session {
            valid,
            expiry: if valid { expires } else { None },
        })
    }

    #[cfg(test)]
    fn refresh_access_token_for_test(
        &self,
        auth: &AuthContext,
        session_url: &str,
    ) -> anyhow::Result<String> {
        Self::refresh_access_token_at(auth, session_url)
    }

    #[cfg(test)]
    fn ensure_fresh_access_token_for_test(
        &self,
        auth: &AuthContext,
        session_url: &str,
    ) -> anyhow::Result<String> {
        Self::ensure_fresh_access_token_at(auth, session_url)
    }
}

impl Provider for ChatGptProvider {
    fn id(&self) -> &'static str {
        "chatgpt"
    }

    fn context_limit(&self) -> usize {
        // ~128k tokens ≈ 512k chars
        512_000
    }

    fn auth(&self, token: &str) -> anyhow::Result<Session> {
        if token.trim().is_empty() {
            bail!(
                "empty session token — copy {} exactly from DevTools → Application → Cookies",
                SESSION_COOKIE_NAME
            );
        }
        let (valid, _access, expires) = Self::fetch_session(SESSION_URL, token)?;
        Ok(Session {
            valid,
            expiry: if valid { expires } else { None },
        })
    }

    fn chat(&self, handle: &ProviderHandle, req: ChatReq) -> anyhow::Result<ChatResp> {
        // Rolling refresh happens here even though the full conversation flow
        // lands in the next bead: credentials must be validated before any
        // conversation POST is attempted.
        let _access_token = Self::ensure_fresh_access_token_at(&req.auth, SESSION_URL)?;
        let _ = handle;
        anyhow::bail!("chatgpt chat not yet implemented")
    }
}

inventory::submit!(chat_core::provider::ProviderEntry {
    id: "chatgpt",
    factory: || Box::new(ChatGptProvider),
});

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;
    /// Create an isolated mock server; the guard must stay bound in each
    /// test (mockito mocks die with the guard).
    fn server() -> mockito::ServerGuard {
        mockito::Server::new()
    }

    fn valid_session_body() -> String {
        // expires ~30 days out, like the real endpoint
        serde_json::json!({
            "accessToken": "access-token-xyz",
            "expires": (chrono::Utc::now() + chrono::Duration::days(30)).to_rfc3339(),
        })
        .to_string()
    }

    #[test]
    fn auth_valid_token_returns_valid_session_with_expiry() {
        let mut s = server();
        s.mock("GET", "/api/auth/session")
            .match_header(
                "cookie",
                format!("{}=paste-token", SESSION_COOKIE_NAME).as_str(),
            )
            .with_status(200)
            .with_body(valid_session_body())
            .create();
        let url = format!("{}/api/auth/session", s.url());

        let p = ChatGptProvider;
        let session = p.auth_with_url(&url, "paste-token").unwrap();
        assert!(session.valid);
        assert!(session.expiry.is_some());
    }

    #[test]
    fn auth_invalid_token_maps_401_to_invalid_session() {
        let mut s = server();
        s.mock("GET", "/api/auth/session").with_status(401).create();
        let url = format!("{}/api/auth/session", s.url());

        let p = ChatGptProvider;
        let session = p.auth_with_url(&url, "stale-token").unwrap();
        assert!(!session.valid);
        assert!(session.expiry.is_none());
    }

    #[test]
    fn auth_anonymous_session_null_access_token_is_invalid() {
        let mut s = server();
        s.mock("GET", "/api/auth/session")
            .with_status(200)
            .with_body(r#"{"accessToken":null,"expires":null}"#)
            .create();
        let url = format!("{}/api/auth/session", s.url());

        let p = ChatGptProvider;
        let session = p.auth_with_url(&url, "whatever").unwrap();
        assert!(!session.valid);
    }

    #[test]
    fn auth_empty_token_fails_fast_without_http() {
        let err = ChatGptProvider.auth("").unwrap_err();
        assert!(err.to_string().contains("empty session token"));
    }

    #[test]
    fn refresh_persists_new_access_token_via_callback() {
        let mut s = server();
        s.mock("GET", "/api/auth/session")
            .with_status(200)
            .with_body(valid_session_body())
            .create();
        let url = format!("{}/api/auth/session", s.url());

        let persisted = Arc::new(Mutex::new(None::<(String, String)>));
        let sink = persisted.clone();
        let auth = AuthContext {
            session_token: Some("rolling-token".to_string()),
            access_token: None,
            access_token_expiry: None,
            persist: Some(Arc::new(move |t: &str, e: &str| {
                *sink.lock() = Some((t.to_string(), e.to_string()));
                Ok(())
            })),
        };

        let fresh = ChatGptProvider
            .refresh_access_token_for_test(&auth, &url)
            .unwrap();
        assert_eq!(fresh, "access-token-xyz");
        let saved = persisted.lock().take().expect("persist must fire");
        assert_eq!(saved.0, "access-token-xyz");
        assert!(!saved.1.is_empty());
    }

    #[test]
    fn refresh_without_session_token_names_the_login_command() {
        let auth = AuthContext::default();
        let err = ChatGptProvider
            .refresh_access_token_for_test(&auth, "http://127.0.0.1:9/unreachable")
            .unwrap_err();
        assert!(err.to_string().contains("auth login chatgpt"));
    }

    #[test]
    fn refresh_rejected_session_bubbles_cookie_hint() {
        let mut s = server();
        s.mock("GET", "/api/auth/session").with_status(401).create();
        let url = format!("{}/api/auth/session", s.url());

        let auth = AuthContext {
            session_token: Some("dead".to_string()),
            ..AuthContext::default()
        };
        let err = ChatGptProvider
            .refresh_access_token_for_test(&auth, &url)
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(SESSION_COOKIE_NAME),
            "hint missing cookie name: {msg}"
        );
        assert!(msg.contains("--token"), "hint missing --token flag: {msg}");
    }

    #[test]
    fn ensure_fresh_skips_refresh_when_cached_token_unexpired() {
        let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let auth = AuthContext {
            session_token: None,
            access_token: Some("cached".to_string()),
            access_token_expiry: Some(future),
            persist: None,
        };
        let got = ChatGptProvider
            .ensure_fresh_access_token_for_test(&auth, "http://127.0.0.1:9/unreachable")
            .unwrap();
        assert_eq!(got, "cached");
    }

    #[test]
    fn ensure_fresh_refreshes_when_expiry_passed() {
        let mut s = server();
        s.mock("GET", "/api/auth/session")
            .with_status(200)
            .with_body(valid_session_body())
            .create();
        let url = format!("{}/api/auth/session", s.url());

        let past = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let auth = AuthContext {
            session_token: Some("rolling-token".to_string()),
            access_token: Some("stale".to_string()),
            access_token_expiry: Some(past),
            persist: None,
        };
        let got = ChatGptProvider
            .ensure_fresh_access_token_for_test(&auth, &url)
            .unwrap();
        assert_eq!(got, "access-token-xyz");
    }
}
