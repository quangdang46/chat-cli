//! DeepSeek web provider — session validation, PoW, chat completion.
//!
//! Flow (mirrors the ds2api reference client):
//! 1. `auth(token)` probes `POST /chat_session/fetch_page` with
//!    `authorization: Bearer <token>` — success proves the token works.
//! 2. `chat()`: `--new` first creates a session via `POST /chat_session/create`;
//!    then (if challenged) solves PoW via `/chat/create_pow_challenge` and
//!    attaches `x-ds-pow-response`; finally `POST /chat/completion`.

use std::time::Duration;

use anyhow::{bail, Context};
use chat_core::provider::{ChatReq, ChatResp, Provider, ProviderHandle, Session};

pub mod protocol;
pub use deepseek_pow as pow;

/// DeepSeek access tokens are opaque JWT-ish strings; they do not expire on a
/// schedule we can read, so there is no rolling refresh: validate at login,
/// trust until a 401.
pub struct DeepSeekProvider;

impl DeepSeekProvider {
    fn http_client() -> reqwest::Result<reqwest::blocking::Client> {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
    }

    fn request(
        client: &reqwest::blocking::Client,
        method: reqwest::Method,
        url: &str,
        token: Option<&str>,
        json_body: Option<serde_json::Value>,
    ) -> reqwest::blocking::RequestBuilder {
        let mut rb = client.request(method, url);
        for (k, v) in protocol::base_headers() {
            rb = rb.header(k, v);
        }
        if let Some(t) = token {
            rb = rb.header(protocol::AUTHORIZATION_HEADER, format!("Bearer {t}"));
        }
        if let Some(body) = json_body {
            rb = rb.json(&body);
        }
        rb
    }

    /// Map provider errors to actionable messages.
    fn map_status(e: reqwest::Error) -> anyhow::Error {
        match e.status() {
            Some(reqwest::StatusCode::UNAUTHORIZED) => anyhow::anyhow!(
                "401 from deepseek — token invalid or expired; re-copy it and run 'chat-cli auth login deepseek --token ...'"
            ),
            Some(reqwest::StatusCode::TOO_MANY_REQUESTS) => anyhow::anyhow!(
                "429 rate-limited by deepseek — wait and retry, or use '--provider chatgpt'"
            ),
            Some(reqwest::StatusCode::FORBIDDEN) => anyhow::anyhow!(
                "403 from deepseek — account may be restricted; check the web app"
            ),
            _ => e.into(),
        }
    }

    /// Probe an authenticated endpoint; cheap proof that the token works.
    fn probe_token(probe_url: &str, token: &str) -> anyhow::Result<bool> {
        let client = Self::http_client().context("failed to build HTTP client")?;
        // fetch_page with empty body returns the caller's sessions when authed.
        let resp = Self::request(
            &client,
            reqwest::Method::POST,
            probe_url,
            Some(token),
            Some(serde_json::json!({"offset": 0, "limit": 1})),
        )
        .send()
        .with_context(|| format!("POST {probe_url} failed — check network/proxy"))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Ok(false);
        }
        if !status.is_success() {
            bail!("deepseek probe returned {status}: endpoint unavailable, retry later");
        }
        Ok(true)
    }

    /// Create a fresh chat session; returns its id.
    fn create_chat_session(create_url: &str, token: &str) -> anyhow::Result<String> {
        let client = Self::http_client().context("failed to build HTTP client")?;
        let resp = Self::request(
            &client,
            reqwest::Method::POST,
            create_url,
            Some(token),
            Some(serde_json::json!({})),
        )
        .send()
        .with_context(|| format!("POST {create_url} failed"))?
        .error_for_status()
        .map_err(Self::map_status)?;

        let body: serde_json::Value = resp
            .json()
            .context("invalid JSON from chat_session/create")?;
        body["data"]["biz_data"]["id"]
            .as_str()
            .or_else(|| body["data"]["id"].as_str())
            .map(String::from)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "chat_session/create returned no session id — response shape changed"
                )
            })
    }

    /// Ask for a PoW challenge targeting the completion endpoint and solve it.
    fn solve_completion_pow(pow_url: &str, token: &str) -> anyhow::Result<String> {
        let client = Self::http_client().context("failed to build HTTP client")?;
        let resp = Self::request(
            &client,
            reqwest::Method::POST,
            pow_url,
            Some(token),
            Some(serde_json::json!({
                "target_path": protocol::COMPLETION_TARGET_PATH,
            })),
        )
        .send()
        .with_context(|| format!("POST {pow_url} failed"))?
        .error_for_status()
        .map_err(Self::map_status)?;

        let body: serde_json::Value = resp
            .json()
            .context("invalid JSON from create_pow_challenge")?;
        let challenge: deepseek_pow::Challenge =
            serde_json::from_value(body["data"]["biz_data"]["challenge"].clone())
                .context("create_pow_challenge returned no challenge object")?;
        deepseek_pow::solve_and_build_header(&challenge)
    }

    /// One completion POST; returns `(content, message_id)`.
    fn post_completion(
        completion_url: &str,
        token: &str,
        parent_id: Option<&str>,
        chat_session_id: &str,
        prompt: String,
        pow_header: Option<String>,
    ) -> anyhow::Result<(String, String)> {
        let client = Self::http_client().context("failed to build HTTP client")?;
        let mut payload = serde_json::json!({
            "chat_session_id": chat_session_id,
            "parent_id": parent_id.map(|s| s.parse::<i64>().unwrap_or(0)).unwrap_or(0),
            "refs": [],
            "content": prompt,
        });
        if !prompt.is_empty() {
            payload["thinking_enabled"] = serde_json::Value::Bool(false);
        }

        let mut rb = Self::request(
            &client,
            reqwest::Method::POST,
            completion_url,
            Some(token),
            Some(payload),
        );
        if let Some(header) = pow_header {
            rb = rb.header(protocol::POW_RESPONSE_HEADER, header);
        }

        let resp = rb
            .send()
            .with_context(|| format!("POST {completion_url} failed"))?
            .error_for_status()
            .map_err(Self::map_status)?;

        let text = resp.text().context("failed reading completion response")?;
        Self::parse_completion_stream(&text)
    }

    /// Completion bodies arrive as newline-separated JSON fragments:
    /// `{"p":...}` progress chunks and a final `{"v": {"content": ..., "message_id"?}}`.
    fn parse_completion_stream(text: &str) -> anyhow::Result<(String, String)> {
        let mut content = String::new();
        let mut message_id = String::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // fragment mode: append incremental content
            if let Some(frag) = v["v"].as_str() {
                content.push_str(frag);
                continue;
            }
            if let Some(frag) = v["v"]["content"].as_str() {
                content.push_str(frag);
            }
            let mid = v["v"]["message_id"]
                .as_str()
                .map(String::from)
                .or_else(|| v["v"]["message_id"].as_i64().map(|i| i.to_string()));
            if let Some(id) = mid {
                message_id = id;
            }
        }
        if content.is_empty() {
            bail!(
                "no content found in deepseek completion stream — the model may have refused or the session expired"
            );
        }
        Ok((content, message_id))
    }
}

impl Provider for DeepSeekProvider {
    fn id(&self) -> &'static str {
        "deepseek"
    }

    fn context_limit(&self) -> usize {
        // ~64k tokens ≈ 256k chars
        256_000
    }

    fn auth(&self, token: &str) -> anyhow::Result<Session> {
        if token.trim().is_empty() {
            bail!(
                "empty session token — copy your DeepSeek userToken and run 'chat-cli auth login deepseek --token ...'"
            );
        }
        let ok = Self::probe_token(protocol::FETCH_SESSIONS_URL, token)?;
        Ok(Session {
            valid: ok,
            expiry: None,
        })
    }

    fn chat(&self, handle: &ProviderHandle, req: ChatReq) -> anyhow::Result<ChatResp> {
        let token = req.auth.session_token.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "no deepseek session_token in config — run 'chat-cli auth login deepseek' first"
            )
        })?;
        Self::chat_at(
            handle,
            req.prompt,
            protocol::CREATE_SESSION_URL,
            protocol::CREATE_POW_URL,
            protocol::COMPLETION_URL,
            token,
        )
    }
}

impl DeepSeekProvider {
    /// Full chat flow at injectable URLs (tests mock all three endpoints).
    fn chat_at(
        handle: &ProviderHandle,
        prompt: String,
        create_session_url: &str,
        pow_url: &str,
        completion_url: &str,
        token: &str,
    ) -> anyhow::Result<ChatResp> {
        // --new: provision a server-side session first
        let chat_session_id = match &handle.conversation_id {
            Some(id) => id.clone(),
            None => Self::create_chat_session(create_session_url, token)?,
        };

        // PoW is challenge-driven; ask once per completion. A missing/failed
        // challenge endpoint must not kill the chat: the server may not
        // require PoW at all.
        let pow_header = Self::solve_completion_pow(pow_url, token)
            .map(Some)
            .unwrap_or_else(|e| {
                eprintln!("note: skipping deepseek PoW ({e})");
                None
            });

        let parent_id = handle.parent_message_id.as_deref();
        let (content, message_id) = Self::post_completion(
            completion_url,
            token,
            parent_id,
            &chat_session_id,
            prompt,
            pow_header,
        )?;

        Ok(ChatResp {
            content,
            conversation_id: chat_session_id,
            message_id: if message_id.is_empty() {
                format!("ds-{}", uuid::Uuid::new_v4())
            } else {
                message_id
            },
        })
    }
}

#[cfg(test)]
impl DeepSeekProvider {
    fn chat_at_for_test(
        &self,
        handle: &ProviderHandle,
        prompt: &str,
        base: &str,
        token: &str,
    ) -> anyhow::Result<ChatResp> {
        Self::chat_at(
            handle,
            prompt.to_string(),
            &format!("{base}/api/v0/chat_session/create"),
            &format!("{base}/api/v0/chat/create_pow_challenge"),
            &format!("{base}/api/v0/chat/completion"),
            token,
        )
    }

    fn probe_token_for_test(&self, base: &str, token: &str) -> anyhow::Result<bool> {
        Self::probe_token(&format!("{base}/api/v0/chat_session/fetch_page"), token)
    }

    fn parse_completion_for_test(&self, text: &str) -> anyhow::Result<(String, String)> {
        Self::parse_completion_stream(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> mockito::ServerGuard {
        mockito::Server::new()
    }

    #[test]
    fn auth_valid_token_probes_fetch_page() {
        let mut s = server();
        s.mock("POST", "/api/v0/chat_session/fetch_page")
            .match_header("authorization", "Bearer good-token")
            .with_status(200)
            .with_body(r#"{"data":{"biz_data":{"chat_sessions":[]}}}"#)
            .create();
        let url = s.url();

        let session = DeepSeekProvider
            .probe_token_for_test(&url, "good-token")
            .unwrap();
        assert!(session);
    }

    #[test]
    fn auth_rejected_token_maps_401_to_invalid() {
        let mut s = server();
        s.mock("POST", "/api/v0/chat_session/fetch_page")
            .with_status(401)
            .create();
        let url = s.url();

        let ok = DeepSeekProvider
            .probe_token_for_test(&url, "stale")
            .unwrap();
        assert!(!ok);
    }

    #[test]
    fn auth_empty_token_fails_fast_without_http() {
        let err = DeepSeekProvider.auth("").unwrap_err();
        assert!(err.to_string().contains("empty session token"));
    }

    #[test]
    fn chat_new_creates_session_then_completes() {
        let mut s = server();
        s.mock("POST", "/api/v0/chat_session/create")
            .match_header("authorization", "Bearer tok")
            .with_status(200)
            .with_body(r#"{"data":{"biz_data":{"id":"sess-77"}}}"#)
            .create();
        // no PoW mock: the provider must tolerate a challenge endpoint that
        // answers 404 by skipping PoW
        s.mock("POST", "/api/v0/chat/create_pow_challenge")
            .with_status(404)
            .create();
        let completion = s
            .mock("POST", "/api/v0/chat/completion")
            .match_header("authorization", "Bearer tok")
            .with_status(200)
            .with_body(concat!(
                "{\"p\":\"progress\"}\n",
                "{\"v\":{\"content\":\"deep answer\",\"message_id\":501}}\n"
            ))
            .create();
        let url = s.url();

        let resp = DeepSeekProvider
            .chat_at_for_test(&ProviderHandle::default(), "hi", &url, "tok")
            .unwrap();

        assert_eq!(resp.content, "deep answer");
        assert_eq!(resp.conversation_id, "sess-77");
        assert_eq!(resp.message_id, "501");
        let _ = completion;
    }

    #[test]
    fn chat_continue_skips_session_create_and_carries_parent() {
        let mut s = server();
        // --continue must NOT hit create; a stray call would 500 this mock
        s.mock("POST", "/api/v0/chat_session/create")
            .with_status(500)
            .create();
        s.mock("POST", "/api/v0/chat/create_pow_challenge")
            .with_status(404)
            .create();
        s.mock("POST", "/api/v0/chat/completion")
            .with_status(200)
            .with_body("{\"v\":\"chunk \"}\n{\"v\":{\"content\":\"two\",\"message_id\":\"9\"}}\n")
            .create();
        let url = s.url();

        let handle = ProviderHandle {
            conversation_id: Some("sess-existing".to_string()),
            parent_message_id: Some("42".to_string()),
        };
        let resp = DeepSeekProvider
            .chat_at_for_test(&handle, "next", &url, "tok")
            .unwrap();

        assert_eq!(resp.content, "chunk two");
        assert_eq!(resp.conversation_id, "sess-existing");
        assert_eq!(resp.message_id, "9");
    }

    #[test]
    fn chat_maps_401_completion_to_relogin_hint() {
        let mut s = server();
        s.mock("POST", "/api/v0/chat_session/create")
            .with_status(200)
            .with_body(r#"{"data":{"biz_data":{"id":"s1"}}}"#)
            .create();
        s.mock("POST", "/api/v0/chat/create_pow_challenge")
            .with_status(404)
            .create();
        s.mock("POST", "/api/v0/chat/completion")
            .with_status(401)
            .create();
        let url = s.url();

        let err = DeepSeekProvider
            .chat_at_for_test(&ProviderHandle::default(), "hi", &url, "dead-token")
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("401") && msg.contains("auth login deepseek"),
            "{msg}"
        );
    }

    #[test]
    fn parse_completion_accumulates_fragments_and_final_ids() {
        let (content, mid) = DeepSeekProvider
            .parse_completion_for_test(
                "{\"v\":\"a\"}\n{\"v\":\"b\"}\n{\"v\":{\"content\":\"c\",\"message_id\":7}}\n",
            )
            .unwrap();
        assert_eq!(content, "abc");
        assert_eq!(mid, "7");
    }

    #[test]
    fn parse_completion_empty_stream_is_clear_error() {
        let err = DeepSeekProvider
            .parse_completion_for_test("")
            .expect_err("empty stream must fail");
        assert!(err.to_string().contains("no content"));
    }
}

inventory::submit!(chat_core::provider::ProviderEntry {
    id: "deepseek",
    factory: || Box::new(DeepSeekProvider),
});
