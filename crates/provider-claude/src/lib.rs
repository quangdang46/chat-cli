//! Claude.ai web provider — direct reverse-engineered client, no gateway.
//!
//! Same approach as `provider-chatgpt`: chat-cli holds the web session
//! (`sessionKey` cookie, pasted once via `auth login claude`) and speaks the
//! same endpoints the browser does. Protocol mirrored from the MIT-licensed
//! claude2api gateway (its `internal/service` is the upstream client this
//! crate reimplements in Rust; no code copied).
//!
//! Turn flow:
//! 1. `GET /api/account` → first organization uuid (fetched per turn; the CLI
//!    process is short-lived so there is nothing to cache into).
//! 2. `-thinking` models first `PUT /api/account` with `paprika_mode:
//!    "extended"` to switch the account's thinking mode on.
//! 3. `--new` creates a conversation (`POST .../chat_conversations`);
//!    `--continue` reuses the stored conversation uuid.
//! 4. `POST .../completion` streams the answer; text/thinking/tool code are
//!    folded into one markdown text.
//!
//! Conversation continuation replays history as text (dispatcher), with the
//! server-side conversation uuid kept for history bookkeeping.

use std::time::Duration;

use anyhow::{bail, Context};
use chat_core::provider::{ChatReq, ChatResp, Provider, ProviderHandle, Session};
use serde_json::Value;

pub mod protocol;

pub struct ClaudeProvider;

/// claude.ai web can stream for minutes with thinking on, so this provider
/// gets a roomier timeout than the default web clients.
const HTTP_TIMEOUT: Duration = Duration::from_secs(300);

impl ClaudeProvider {
    fn http_client() -> reqwest::Result<reqwest::blocking::Client> {
        reqwest::blocking::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
    }

    fn request(
        client: &reqwest::blocking::Client,
        method: reqwest::Method,
        url: &str,
        session_key: &str,
    ) -> reqwest::blocking::RequestBuilder {
        let mut rb = client
            .request(method, url)
            .header("Cookie", protocol::cookie_header(session_key));
        for (k, v) in protocol::build_headers(session_key) {
            rb = rb.header(k, v);
        }
        rb
    }

    fn map_status(e: reqwest::Error) -> anyhow::Error {
        match e.status() {
            Some(reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN) => {
                anyhow::anyhow!(
                    "claude.ai rejected the session — re-run 'chat-cli auth login claude' with a fresh sessionKey cookie"
                )
            }
            Some(reqwest::StatusCode::TOO_MANY_REQUESTS) => anyhow::anyhow!(
                "429 from claude.ai — web usage limit hit; wait for the quota window or switch provider ('--provider gemini')"
            ),
            _ => e.into(),
        }
    }

    /// `GET /api/account` — validates the cookie (auth) and yields the org uuid.
    fn fetch_org(account_url: &str, session_key: &str) -> anyhow::Result<String> {
        let client = Self::http_client().context("failed to build HTTP client")?;
        let resp = Self::request(&client, reqwest::Method::GET, account_url, session_key)
            .send()
            .with_context(|| format!("GET {account_url} failed — check network/proxy"))?;

        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            bail!("sessionKey rejected by claude.ai — copy it again via DevTools → Application → Cookies");
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            bail!("429 from claude.ai — IP temporarily flagged; try a proxy or wait");
        }
        let body = resp
            .error_for_status()
            .map_err(Self::map_status)?
            .text()
            .context("failed reading /api/account")?;
        protocol::extract_org_uuid(&body)
    }

    /// `PUT /api/account` — flip the account's extended-thinking switch.
    fn set_thinking(account_url: &str, session_key: &str, enabled: bool) -> anyhow::Result<()> {
        let client = Self::http_client().context("failed to build HTTP client")?;
        let payload = protocol::build_account_settings(if enabled {
            Value::String("extended".into())
        } else {
            Value::Null
        });
        let resp = Self::request(
            &client,
            reqwest::Method::PUT,
            &format!("{account_url}/api/account"),
            session_key,
        )
        .header("content-type", "application/json")
        .json(&payload)
        .send()
        .with_context(|| "PUT /api/account failed")?
        .error_for_status()
        .map_err(Self::map_status)?;
        let _ = resp.text()?;
        Ok(())
    }

    /// `POST .../chat_conversations` — create a conversation, return its uuid.
    fn create_conversation(
        create_url: &str,
        session_key: &str,
        model: &str,
    ) -> anyhow::Result<String> {
        let client = Self::http_client().context("failed to build HTTP client")?;
        let resp = Self::request(&client, reqwest::Method::POST, create_url, session_key)
            .header("content-type", "application/json")
            .header("referer", format!("{}/new", protocol::BASE_URL))
            .json(&protocol::build_create_conversation(model))
            .send()
            .with_context(|| format!("POST {create_url} failed"))?
            .error_for_status()
            .map_err(Self::map_status)?;

        let body = resp
            .text()
            .context("failed reading conversation response")?;
        let v: Value = serde_json::from_str(&body)
            .with_context(|| format!("invalid JSON from create: {:.200}", body.trim()))?;
        v["uuid"].as_str().map(str::to_string).ok_or_else(|| {
            anyhow::anyhow!("conversation response missing uuid — response shape changed")
        })
    }

    /// `POST .../completion` — stream one turn, return the folded text.
    fn post_completion(
        completion_url: &str,
        session_key: &str,
        conversation: &str,
        payload: Value,
    ) -> anyhow::Result<String> {
        let client = Self::http_client().context("failed to build HTTP client")?;
        let resp = Self::request(&client, reqwest::Method::POST, completion_url, session_key)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream, text/event-stream")
            .header("cache-control", "no-cache")
            .header(
                "referer",
                format!("{}/chat/{conversation}", protocol::BASE_URL),
            )
            .json(&payload)
            .send()
            .with_context(|| format!("POST {completion_url} failed"))?;

        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let body = resp.text().unwrap_or_default();
            bail!(
                "429 from claude.ai — usage limit: {}",
                body.chars().take(300).collect::<String>()
            );
        }
        let body = resp
            .error_for_status()
            .map_err(Self::map_status)?
            .text()
            .context("failed reading completion stream")?;
        protocol::parse_completion_sse(&body)
    }
}

impl Provider for ClaudeProvider {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn context_limit(&self) -> usize {
        // claude.ai web: ~200k-token window on current models.
        200_000
    }

    fn auth(&self, token: &str) -> anyhow::Result<Session> {
        if token.trim().is_empty() {
            bail!(
                "empty sessionKey — copy it from claude.ai (DevTools → Application → \
                 Cookies) and run 'chat-cli auth login claude'"
            );
        }
        let org = Self::fetch_org(protocol::organizations_url().as_str(), token)?;
        Ok(Session {
            valid: !org.is_empty(),
            expiry: None,
        })
    }

    fn chat(&self, handle: &ProviderHandle, req: ChatReq) -> anyhow::Result<ChatResp> {
        let session_key = req.auth.session_token.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "no claude sessionKey in config — run 'chat-cli auth login claude' first"
            )
        })?;

        // -thinking suffix → extended mode + base model for the payloads.
        let raw_model = req
            .model
            .clone()
            .unwrap_or_else(|| "claude-sonnet-4-6".to_string());
        let (thinking, model) = protocol::split_thinking_suffix(&raw_model);

        let org = Self::fetch_org(protocol::organizations_url().as_str(), session_key)?;

        if thinking {
            Self::set_thinking(protocol::BASE_URL, session_key, true)
                .context("failed to enable extended thinking on the account")?;
        }

        // --continue reuses the server-side conversation; --new creates one.
        let conversation = match &handle.conversation_id {
            Some(id) => id.clone(),
            None => Self::create_conversation(
                protocol::conversations_url(&org).as_str(),
                session_key,
                model,
            )?,
        };

        // Attachments arrive as fenced text — ship them as a text attachment
        // so long context rides the same path the web app uses.
        let attachments = if req.attachments_text.is_empty() {
            Vec::new()
        } else {
            vec![protocol::text_attachment(&req.attachments_text)]
        };

        let content = Self::post_completion(
            protocol::completion_url(&org, &conversation).as_str(),
            session_key,
            &conversation,
            protocol::build_completion_payload(&req.prompt, model, attachments),
        )?;

        Ok(ChatResp {
            content,
            conversation_id: conversation,
            message_id: format!("cl-{}", uuid::Uuid::new_v4()),
        })
    }
}

inventory::submit!(chat_core::provider::ProviderEntry {
    id: "claude",
    factory: || Box::new(ClaudeProvider),
});

#[cfg(test)]
impl ClaudeProvider {
    fn fetch_org_for_test(&self, base: &str, session_key: &str) -> anyhow::Result<String> {
        Self::fetch_org(&format!("{base}/api/account"), session_key)
    }

    fn chat_at_for_test(
        &self,
        handle: &ProviderHandle,
        base: &str,
        session_key: &str,
        model: Option<String>,
        prompt: &str,
    ) -> anyhow::Result<ChatResp> {
        let raw_model = model.unwrap_or_else(|| "claude-sonnet-4-6".to_string());
        let (thinking, model) = protocol::split_thinking_suffix(&raw_model);
        let org = Self::fetch_org(&format!("{base}/api/account"), session_key)?;
        if thinking {
            Self::set_thinking(base, session_key, true)?;
        }
        let conversation = match &handle.conversation_id {
            Some(id) => id.clone(),
            None => Self::create_conversation(
                &format!("{base}/api/organizations/{org}/chat_conversations"),
                session_key,
                model,
            )?,
        };
        let content = Self::post_completion(
            &format!("{base}/api/organizations/{org}/chat_conversations/{conversation}/completion"),
            session_key,
            &conversation,
            protocol::build_completion_payload(prompt, model, Vec::new()),
        )?;
        Ok(ChatResp {
            content,
            conversation_id: conversation,
            message_id: format!("cl-{}", uuid::Uuid::new_v4()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    fn server() -> mockito::ServerGuard {
        mockito::Server::new()
    }

    fn account_page(org: &str) -> String {
        format!(
            r#"{{"email_address":"me@example.com","memberships":[{{"organization":{{"uuid":"{org}"}}}}]}}"#
        )
    }

    fn sse_text(text: &str) -> String {
        let mut body = String::from(
            "data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"text\"}}\n",
        );
        body.push_str(&format!(
            "data: {{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":{:?}}}}}\n",
            text
        ));
        body
    }

    #[test]
    fn registered_provider_is_discoverable() {
        assert!(chat_core::provider::get_provider("claude").is_some());
    }

    #[test]
    fn auth_valid_session_probes_account() {
        let mut s = server();
        let _m = s
            .mock("GET", "/api/account")
            .match_header("cookie", "sessionKey=sk-ant-sid01-ok")
            .with_status(200)
            .with_body(account_page("org-1"))
            .create();
        let url = s.url();

        let session = ClaudeProvider
            .fetch_org_for_test(&url, "sk-ant-sid01-ok")
            .unwrap();
        assert_eq!(session, "org-1");
    }

    #[test]
    fn auth_rejected_cookie_names_the_cookie() {
        let mut s = server();
        let _m = s.mock("GET", "/api/account").with_status(401).create();
        let url = s.url();

        let err = ClaudeProvider
            .fetch_org_for_test(&url, "stale-key")
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("sessionKey") && msg.contains("DevTools"),
            "{msg}"
        );
    }

    #[test]
    fn auth_empty_cookie_fails_fast() {
        let err = ClaudeProvider.auth("").unwrap_err();
        assert!(err.to_string().contains("empty sessionKey"), "{err}");
    }

    #[test]
    fn chat_new_creates_conversation_and_streams() {
        let mut s = server();
        let _acct = s
            .mock("GET", "/api/account")
            .with_status(200)
            .with_body(account_page("org-9"))
            .create();
        let create_store: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let create_sink = create_store.clone();
        let _create = s
            .mock("POST", "/api/organizations/org-9/chat_conversations")
            .match_header("cookie", "sessionKey=sk-ok")
            .with_body_from_request(move |req| {
                *create_sink.lock() =
                    Some(String::from_utf8_lossy(req.body().unwrap()).to_string());
                r#"{"uuid":"conv-55","name":""}"#.as_bytes().to_vec()
            })
            .with_status(201)
            .create();
        let completion_store: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let completion_sink = completion_store.clone();
        let _completion = s
            .mock(
                "POST",
                "/api/organizations/org-9/chat_conversations/conv-55/completion",
            )
            .with_body_from_request(move |req| {
                *completion_sink.lock() =
                    Some(String::from_utf8_lossy(req.body().unwrap()).to_string());
                sse_text("hello from claude").into_bytes()
            })
            .with_status(200)
            .create();
        let url = s.url();

        let resp = ClaudeProvider
            .chat_at_for_test(&ProviderHandle::default(), &url, "sk-ok", None, "hi claude")
            .unwrap();

        assert_eq!(resp.content, "hello from claude");
        assert_eq!(resp.conversation_id, "conv-55");
        assert!(resp.message_id.starts_with("cl-"));

        // create payload carries the model; completion payload carries the prompt
        let create_payload: serde_json::Value =
            serde_json::from_str(&create_store.lock().take().unwrap()).unwrap();
        assert_eq!(create_payload["model"], "claude-sonnet-4-6");
        assert_eq!(create_payload["include_conversation_preferences"], true);
        let completion_payload: serde_json::Value =
            serde_json::from_str(&completion_store.lock().take().unwrap()).unwrap();
        assert_eq!(completion_payload["prompt"], "hi claude");
        assert_eq!(completion_payload["model"], "claude-sonnet-4-6");
        assert_eq!(
            completion_payload["parent_message_uuid"],
            "00000000-0000-4000-8000-000000000000"
        );
        assert_eq!(
            completion_payload["attachments"].as_array().unwrap().len(),
            0
        );
    }

    #[test]
    fn chat_continue_reuses_conversation_without_create() {
        let mut s = server();
        let _acct = s
            .mock("GET", "/api/account")
            .with_status(200)
            .with_body(account_page("org-9"))
            .create();
        // create must NOT be hit — an unexpected call returns 501 and fails the turn
        let _completion = s
            .mock(
                "POST",
                "/api/organizations/org-9/chat_conversations/conv-held/completion",
            )
            .with_status(200)
            .with_body(sse_text("second turn"))
            .create();
        let url = s.url();

        let handle = ProviderHandle {
            conversation_id: Some("conv-held".to_string()),
            parent_message_id: Some("cl-prev".to_string()),
        };
        let resp = ClaudeProvider
            .chat_at_for_test(&handle, &url, "sk-ok", None, "next")
            .unwrap();
        assert_eq!(resp.conversation_id, "conv-held");
        assert_eq!(resp.content, "second turn");
    }

    #[test]
    fn chat_thinking_model_toggles_paprika_and_strips_suffix() {
        let mut s = server();
        let paprika_hit: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
        let hit = paprika_hit.clone();
        let _paprika = s
            .mock("PUT", "/api/account")
            .match_header("cookie", "sessionKey=sk-ok")
            .with_body_from_request(move |req| {
                let body = String::from_utf8_lossy(req.body().unwrap()).to_string();
                assert!(body.contains("\"paprika_mode\":\"extended\""), "{body}");
                *hit.lock() = true;
                r#"{"ok":true}"#.as_bytes().to_vec()
            })
            .with_status(200)
            .create();
        let _acct = s
            .mock("GET", "/api/account")
            .with_status(200)
            .with_body(account_page("org-9"))
            .create();
        let _create = s
            .mock("POST", "/api/organizations/org-9/chat_conversations")
            .with_status(201)
            .with_body(r#"{"uuid":"conv-t"}"#)
            .create();
        let completion_store: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let completion_sink = completion_store.clone();
        let _completion = s
            .mock(
                "POST",
                "/api/organizations/org-9/chat_conversations/conv-t/completion",
            )
            .with_body_from_request(move |req| {
                *completion_sink.lock() =
                    Some(String::from_utf8_lossy(req.body().unwrap()).to_string());
                sse_text("thoughtful").into_bytes()
            })
            .with_status(200)
            .create();
        let url = s.url();

        let resp = ClaudeProvider
            .chat_at_for_test(
                &ProviderHandle::default(),
                &url,
                "sk-ok",
                Some("claude-sonnet-4-6-thinking".to_string()),
                "think",
            )
            .unwrap();
        assert_eq!(resp.content, "thoughtful");
        assert!(
            *paprika_hit.lock(),
            "paprika toggle must fire for -thinking models"
        );
        let payload: serde_json::Value =
            serde_json::from_str(&completion_store.lock().take().unwrap()).unwrap();
        assert_eq!(
            payload["model"], "claude-sonnet-4-6",
            "-thinking suffix must not reach the upstream payload"
        );
    }

    #[test]
    fn chat_maps_429_to_usage_limit_hint() {
        let mut s = server();
        let _acct = s
            .mock("GET", "/api/account")
            .with_status(200)
            .with_body(account_page("org-9"))
            .create();
        let _create = s
            .mock("POST", "/api/organizations/org-9/chat_conversations")
            .with_status(201)
            .with_body(r#"{"uuid":"c1"}"#)
            .create();
        let _completion = s
            .mock(
                "POST",
                "/api/organizations/org-9/chat_conversations/c1/completion",
            )
            .with_status(429)
            .with_body("rate limit")
            .create();
        let url = s.url();

        let err = ClaudeProvider
            .chat_at_for_test(&ProviderHandle::default(), &url, "sk-ok", None, "hi")
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("429") && msg.contains("usage limit"), "{msg}");
    }

    #[test]
    fn chat_maps_401_completion_to_relogin_hint() {
        let mut s = server();
        let _acct = s
            .mock("GET", "/api/account")
            .with_status(200)
            .with_body(account_page("org-9"))
            .create();
        let _create = s
            .mock("POST", "/api/organizations/org-9/chat_conversations")
            .with_status(201)
            .with_body(r#"{"uuid":"c2"}"#)
            .create();
        let _completion = s
            .mock(
                "POST",
                "/api/organizations/org-9/chat_conversations/c2/completion",
            )
            .with_status(401)
            .create();
        let url = s.url();

        let err = ClaudeProvider
            .chat_at_for_test(&ProviderHandle::default(), &url, "dead", None, "hi")
            .unwrap_err();
        assert!(err.to_string().contains("auth login claude"), "{err}");
    }

    #[test]
    fn chat_requires_session_token() {
        let err = ClaudeProvider
            .chat(
                &ProviderHandle::default(),
                ChatReq {
                    prompt: "hi".into(),
                    system: None,
                    attachments_text: String::new(),
                    model: None,
                    auth: chat_core::provider::AuthContext::default(),
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("auth login claude"), "{err}");
    }
}
