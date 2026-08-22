pub use deepseek_pow as pow;
pub mod protocol;

use std::time::Duration;

use anyhow::{bail, Context};
use chat_core::provider::{AuthContext, ChatReq, ChatResp, Provider, ProviderHandle, Session};
use uuid::Uuid;

use crate::protocol::{
    SessionResponse, CONVERSATION_URL, SENTINEL_REQUIREMENTS_URL, SESSION_COOKIE_NAME, SESSION_URL,
};

pub struct ChatGptProvider;

/// Result of parsing a `/backend-api/conversation` stream body.
#[derive(Debug, Default)]
struct StreamParsed {
    content: String,
    conversation_id: Option<String>,
    message_id: Option<String>,
}

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

    /// Full chat flow at injectable URLs (tests mock both endpoints):
    /// 1. `POST <requirements_url>` → token + optional PoW challenge
    /// 2. Solve PoW locally when `required`
    /// 3. `POST <conversation_url>` with the message; parse the streamed body
    fn chat_at(
        handle: &ProviderHandle,
        req: ChatReq,
        access_token: &str,
        requirements_url: &str,
        conversation_url: &str,
    ) -> anyhow::Result<ChatResp> {
        let client = Self::http_client().context("failed to build HTTP client")?;
        let device_id = protocol::device_id();

        // --- Sentinel: ask whether this turn needs a proof-of-work ---
        let requirements: crate::protocol::ChatRequirements = client
            .post(requirements_url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Oai-Device-Id", &device_id)
            .header("Oai-Language", "en-US")
            .header("User-Agent", protocol::USER_AGENT)
            .json(&serde_json::json!({"p": pow::chat_requirements_proof()}))
            .send()
            .with_context(|| format!("POST {requirements_url} failed"))?
            .error_for_status()
            .map_err(|e| match e.status() {
                Some(reqwest::StatusCode::UNAUTHORIZED) => anyhow::anyhow!(
                    "401 from sentinel — session expired; run 'chat-cli auth login chatgpt --token ...' again"
                ),
                Some(reqwest::StatusCode::TOO_MANY_REQUESTS) => anyhow::anyhow!(
                    "429 rate-limited by chatgpt — wait, or use '--provider deepseek'"
                ),
                _ => e.into(),
            })?
            .json()
            .context("invalid JSON from sentinel chat-requirements")?;

        let openai_sentinel_token = requirements
            .token
            .ok_or_else(|| anyhow::anyhow!("sentinel returned no requirements token"))?;

        // --- Conversation POST (new vs continue decided by the handle) ---
        let parent_message_id = handle
            .parent_message_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let user_message_id = Uuid::new_v4().to_string();
        // The web payload has no separate system field; the browser injects
        // platform instructions into the first user part. Same convention here.
        let mut full_prompt = String::new();
        if let Some(sys) = &req.system {
            full_prompt.push_str(&format!("[System instruction]: {sys}\n\n"));
        }
        if !req.attachments_text.is_empty() {
            full_prompt.push_str(&req.attachments_text);
            full_prompt.push_str("\n\n");
        }
        full_prompt.push_str(&req.prompt);

        let payload = protocol::ConversationPayload {
            action: "next",
            messages: vec![protocol::ConversationMessage {
                id: user_message_id.clone(),
                role: "user",
                content: protocol::MessageContent {
                    content_type: "text",
                    parts: vec![full_prompt],
                },
                metadata: None,
            }],
            conversation_id: handle.conversation_id.clone(),
            parent_message_id: parent_message_id.clone(),
            model: "auto",
            history_and_training_disabled: false,
        };

        let mut post = client
            .post(conversation_url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header(
                "OpenAI-Sentinel-Chat-Requirements-Token",
                &openai_sentinel_token,
            )
            .header("Oai-Device-Id", &device_id)
            .header("Oai-Language", "en-US")
            .header("User-Agent", protocol::USER_AGENT)
            .header("Accept", "text/event-stream")
            .json(&payload);
        if let Some(pow) = &requirements.proof_of_work {
            if pow.required {
                let header = Self::solve_sentinel_pow(pow)?;
                post = post.header("OpenAI-Sentinel-Proof-Token", header);
            }
        }

        let resp = post
            .send()
            .with_context(|| format!("POST {conversation_url} failed"))?
            .error_for_status()
            .map_err(|e| match e.status() {
                Some(reqwest::StatusCode::UNAUTHORIZED) => anyhow::anyhow!(
                    "401 from conversation endpoint — session expired; run 'chat-cli auth login chatgpt --token ...' again"
                ),
                Some(reqwest::StatusCode::TOO_MANY_REQUESTS) => anyhow::anyhow!(
                    "429 rate-limited by chatgpt — wait, or use '--provider deepseek'"
                ),
                _ => e.into(),
            })?;

        let body = resp.text().context("failed reading conversation stream")?;
        let parsed = Self::parse_conversation_stream(&body)?;

        Ok(ChatResp {
            content: parsed.content,
            conversation_id: parsed.conversation_id.unwrap_or_else(|| {
                handle
                    .conversation_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string())
            }),
            message_id: parsed.message_id.unwrap_or(user_message_id),
        })
    }

    /// ChatGPT sentinel proof-of-work: brute-force `sha3_512(seed + base64)`
    /// until the hex prefix meets `difficulty`, wrapped in the `gAAAAAB`
    /// marker (browser `_generateAnswer` scheme).
    fn solve_sentinel_pow(pow: &protocol::PowRequired) -> anyhow::Result<String> {
        Ok(protocol::solve_proof_token(&pow.seed, &pow.difficulty))
    }

    /// Extract `(content, conversation_id, message_id)` from the SSE/JSONL
    /// body of `/backend-api/conversation`: one JSON object per line; the
    /// last line with a completed assistant message wins.
    fn parse_conversation_stream(body: &str) -> anyhow::Result<StreamParsed> {
        let mut out = StreamParsed::default();
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() || line == "data: [DONE]" {
                continue;
            }
            let json_str = line.strip_prefix("data: ").unwrap_or(line); // some responses omit the SSE prefix
            let v: serde_json::Value = match serde_json::from_str(json_str) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("message").is_none() {
                continue;
            }
            let status = v["message"]["status"].as_str().unwrap_or("");
            let author_role = v["message"]["author"]["role"].as_str().unwrap_or("");
            if author_role != "assistant"
                || !(status == "finished_successfully" || status.is_empty())
            {
                continue;
            }
            if let Some(part) = v["message"]["content"]["parts"]
                .as_array()
                .and_then(|p| p.first())
                .and_then(|p| p.as_str())
            {
                out.content = part.to_string();
                out.conversation_id = v["conversation_id"].as_str().map(String::from);
                out.message_id = v["message"]["id"].as_str().map(String::from);
            }
        }
        if out.content.is_empty() && out.conversation_id.is_none() {
            bail!(
                "no assistant response found in conversation stream — the account may be flagged or the model refused"
            );
        }
        Ok(out)
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
        let access_token = Self::ensure_fresh_access_token_at(&req.auth, SESSION_URL)?;
        Self::chat_at(
            handle,
            req,
            &access_token,
            SENTINEL_REQUIREMENTS_URL,
            CONVERSATION_URL,
        )
    }
}

#[cfg(test)]
impl ChatGptProvider {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn chat_at_for_test(
        &self,
        handle: &ProviderHandle,
        req: ChatReq,
        access_token: &str,
        requirements_url: &str,
        conversation_url: &str,
    ) -> anyhow::Result<ChatResp> {
        Self::chat_at(
            handle,
            req,
            access_token,
            requirements_url,
            conversation_url,
        )
    }

    pub(crate) fn parse_conversation_stream_for_test(
        &self,
        body: &str,
    ) -> anyhow::Result<StreamParsed> {
        Self::parse_conversation_stream(body)
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

    // ---- chat: sentinel + conversation (mocked) ----

    fn requirements_body(required_pow: bool) -> String {
        serde_json::json!({
            "token": "req-token-1",
            "proofofwork": {"required": required_pow, "seed": "s", "difficulty": "0.9"}
        })
        .to_string()
    }

    /// Two-line SSE stream: user echo + finished assistant message.
    fn conversation_body() -> String {
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n",
            serde_json::json!({
                "message": {
                    "id": "user-msg-1",
                    "author": {"role": "user"},
                    "status": "finished_successfully",
                    "content": {"content_type": "text", "parts": ["hi"]}
                },
                "conversation_id": "conv-9",
            }),
            serde_json::json!({
                "message": {
                    "id": "assistant-msg-7",
                    "author": {"role": "assistant"},
                    "status": "finished_successfully",
                    "content": {"content_type": "text", "parts": ["hello there"]}
                },
                "conversation_id": "conv-9",
            })
        )
    }

    fn chat_req() -> ChatReq {
        ChatReq {
            prompt: "hi".to_string(),
            system: None,
            attachments_text: String::new(),
            auth: AuthContext::default(),
        }
    }

    #[test]
    fn chat_new_conversation_returns_parsed_content_and_ids() {
        let mut s = server();
        s.mock("POST", "/sentinel/req")
            .match_header("authorization", "Bearer access-1")
            .with_status(200)
            .with_body(requirements_body(false))
            .create();
        let _conv = s
            .mock("POST", "/backend-api/conversation")
            .match_header("OpenAI-Sentinel-Chat-Requirements-Token", "req-token-1")
            .match_header("Accept", "text/event-stream")
            .with_status(200)
            .with_body(conversation_body())
            .create();
        let base = s.url();

        let resp = ChatGptProvider::chat_at_for_test(
            &ChatGptProvider,
            &ProviderHandle::default(),
            chat_req(),
            "access-1",
            &format!("{base}/sentinel/req"),
            &format!("{base}/backend-api/conversation"),
        )
        .unwrap();

        assert_eq!(resp.content, "hello there");
        assert_eq!(resp.conversation_id, "conv-9");
        assert_eq!(resp.message_id, "assistant-msg-7");
    }

    #[test]
    fn chat_continue_carries_stored_conversation_id() {
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sink = captured.clone();

        let mut s = server();
        s.mock("POST", "/sentinel/req")
            .with_status(200)
            .with_body(requirements_body(false))
            .create();
        s.mock("POST", "/backend-api/conversation")
            .with_body_from_request(move |req| {
                // capture the raw payload for the assertion below, still
                // answer with the normal conversation stream
                *sink.lock() = Some(String::from_utf8_lossy(req.body().unwrap()).to_string());
                conversation_body().into_bytes()
            })
            .create();
        let base = s.url();

        let handle = ProviderHandle {
            conversation_id: Some("stored-conv-42".to_string()),
            parent_message_id: Some("stored-parent-1".to_string()),
        };
        let resp = ChatGptProvider::chat_at_for_test(
            &ChatGptProvider,
            &handle,
            chat_req(),
            "access-1",
            &format!("{base}/sentinel/req"),
            &format!("{base}/backend-api/conversation"),
        )
        .unwrap();

        assert_eq!(resp.content, "hello there");
        let payload_raw = captured.lock().take().expect("request body captured");
        assert!(
            payload_raw.contains("stored-conv-42") && payload_raw.contains("stored-parent-1"),
            "continue payload must carry stored ids"
        );
    }

    #[test]
    fn chat_maps_401_to_relogin_hint() {
        let mut s = server();
        s.mock("POST", "/sentinel/req").with_status(401).create();
        let base = s.url();

        let err = ChatGptProvider::chat_at_for_test(
            &ChatGptProvider,
            &ProviderHandle::default(),
            chat_req(),
            "access-stale",
            &format!("{base}/sentinel/req"),
            &format!("{base}/backend-api/conversation"),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("401"), "must name the status: {msg}");
        assert!(msg.contains("auth login"), "must hint re-login: {msg}");
    }

    #[test]
    fn chat_maps_429_to_rate_limit_hint_with_deepseek_fallback() {
        let mut s = server();
        s.mock("POST", "/sentinel/req").with_status(429).create();
        let base = s.url();

        let err = ChatGptProvider::chat_at_for_test(
            &ChatGptProvider,
            &ProviderHandle::default(),
            chat_req(),
            "access-1",
            &format!("{base}/sentinel/req"),
            &format!("{base}/backend-api/conversation"),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("429") && msg.contains("--provider deepseek"),
            "{msg}"
        );
    }

    #[test]
    fn parse_stream_picks_last_finished_assistant_message() {
        let parsed = ChatGptProvider
            .parse_conversation_stream_for_test(&conversation_body())
            .unwrap();
        assert_eq!(parsed.content, "hello there");
        assert_eq!(parsed.conversation_id.as_deref(), Some("conv-9"));
        assert_eq!(parsed.message_id.as_deref(), Some("assistant-msg-7"));
    }

    #[test]
    fn parse_stream_empty_body_is_a_clear_error() {
        let err = ChatGptProvider
            .parse_conversation_stream_for_test("")
            .expect_err("empty stream must fail");
        assert!(
            err.to_string().contains("no assistant response"),
            "error must be actionable: {err}"
        );
    }
}
