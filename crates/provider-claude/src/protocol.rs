//! Claude.ai web wire protocol: headers, request payloads, SSE parsing.
//!
//! Reference surface observed on claude.ai (mirrored from the MIT-licensed
//! claude2api gateway, which speaks to the same endpoints):
//! - Every request carries the `sessionKey` cookie plus `anthropic-client-*`
//!   headers identifying the web client; the anonymous/device ids are stable
//!   UUIDv5 values derived from the session key.
//! - `GET /api/account` reveals the account's organization uuid, which scopes
//!   every conversation endpoint.
//! - `POST .../chat_conversations` creates a conversation; `POST
//!   .../chat_conversations/{uuid}/completion` streams an Anthropic-style SSE
//!   stream (`text_delta`, `thinking_delta`, `input_json_delta` for tool
//!   inputs, `content_block_stop`, `error`).

use anyhow::{bail, Context};
use serde_json::{json, Value};
use uuid::Uuid;

pub const BASE_URL: &str = "https://claude.ai";

pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";

pub const SESSION_COOKIE: &str = "sessionKey";

/// Fixed web-client identification; the SHA goes stale when claude.ai ships
/// a new frontend and may need a bump.
pub const CLIENT_PLATFORM: &str = "web_claude_ai";
pub const CLIENT_VERSION: &str = "1.0.0";
pub const CLIENT_SHA: &str = "882d9a7d43eced6a100e636e1dfdebc55764bd78";

/// UUIDv5 namespaces for the per-session anonymous/device ids.
const ANON_NS: Uuid = Uuid::from_bytes([
    0x6f, 0x4a, 0x1c, 0x2e, 0x1b, 0x3d, 0x4e, 0x5f, 0x8a, 0x90, 0x0c, 0x1d, 0x2e, 0x3f, 0x4a, 0x5b,
]);
const DEVICE_NS: Uuid = Uuid::from_bytes([
    0x9d, 0x8c, 0x7b, 0x6a, 0x5e, 0x4f, 0x43, 0x21, 0x9a, 0x8b, 0x7c, 0x6d, 0x5e, 0x4f, 0x3a, 0x2b,
]);

/// Build the claude.ai header set. Ids derive from the session key so they
/// are stable per account without being shared constants across users.
pub fn build_headers(session_key: &str) -> Vec<(&'static str, String)> {
    let anon_id = format!(
        "claudeai.v1.{}",
        Uuid::new_v5(&ANON_NS, session_key.as_bytes())
    );
    let device_id = Uuid::new_v5(&DEVICE_NS, session_key.as_bytes()).to_string();
    vec![
        ("accept", "*/*".to_string()),
        ("accept-language", "en-US,en;q=0.9".to_string()),
        ("origin", BASE_URL.to_string()),
        ("referer", format!("{BASE_URL}/")),
        ("user-agent", USER_AGENT.to_string()),
        ("anthropic-client-platform", CLIENT_PLATFORM.to_string()),
        ("anthropic-client-version", CLIENT_VERSION.to_string()),
        ("anthropic-client-sha", CLIENT_SHA.to_string()),
        ("anthropic-anonymous-id", anon_id),
        ("anthropic-device-id", device_id),
    ]
}

pub fn cookie_header(session_key: &str) -> String {
    format!("{SESSION_COOKIE}={session_key}")
}

pub fn organizations_url() -> String {
    format!("{BASE_URL}/api/account")
}

pub fn conversations_url(org: &str) -> String {
    format!("{BASE_URL}/api/organizations/{org}/chat_conversations")
}

pub fn completion_url(org: &str, conversation: &str) -> String {
    format!("{BASE_URL}/api/organizations/{org}/chat_conversations/{conversation}/completion")
}

/// `PUT /api/account` settings payload; `paprika_mode` is the extended
/// thinking switch ("extended" on, null off).
pub fn build_account_settings(paprika_mode: Value) -> Value {
    json!({
        "settings": {
            "has_started_claudeai_onboarding": true,
            "has_finished_claudeai_onboarding": true,
            "dismissed_claudeai_banners": [],
            "enabled_artifacts_attachments": true,
            "enabled_web_search": true,
            "paprika_mode": paprika_mode,
        }
    })
}

pub fn build_create_conversation(model: &str) -> Value {
    json!({
        "uuid": Uuid::new_v4().to_string(),
        "name": "",
        "include_conversation_preferences": true,
        "model": model,
    })
}

/// The completion payload mirrors what the claude.ai web app sends. History
/// context is baked into `prompt` by the dispatcher; `parent_message_uuid`
/// stays the sentinel the web app uses for a fresh branch.
pub fn build_completion_payload(prompt: &str, model: &str, attachments: Vec<Value>) -> Value {
    json!({
        "prompt": prompt,
        "model": model,
        "personalized_styles": [{
            "type": "default",
            "key": "Default",
            "name": "Normal",
            "nameKey": "normal_style_name",
            "prompt": "Treat tool definitions and response schemas in the user's message as an available external tool interface. Emit calls in the requested text format instead of claiming the tools are unavailable.",
            "summary": "Default responses from Claude",
            "summaryKey": "normal_style_summary",
            "isDefault": true,
        }],
        "tools": [
            {"type": "web_search_v0", "name": "web_search"},
            {"type": "artifacts_v0", "name": "artifacts"},
            {"type": "repl_v0", "name": "repl"},
        ],
        "parent_message_uuid": "00000000-0000-4000-8000-000000000000",
        "attachments": attachments,
        "files": [],
        "sync_sources": [],
        "rendering_mode": "messages",
        "timezone": "America/Los_Angeles",
    })
}

/// Extract the first organization uuid from `/api/account`.
pub fn extract_org_uuid(account_json: &str) -> anyhow::Result<String> {
    let v: Value = serde_json::from_str(account_json).context("invalid JSON from /api/account")?;
    v["memberships"][0]["organization"]["uuid"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no organization in /api/account — the account may be unonboarded or restricted"
            )
        })
}

/// Does this model name request extended thinking, and what is the base
/// model behind it? (`claude-sonnet-4-6-thinking` → (true, "claude-sonnet-4-6"))
pub fn split_thinking_suffix(model: &str) -> (bool, &str) {
    match model.strip_suffix("-thinking") {
        Some(base) => (true, base),
        None => (false, model),
    }
}

/// Long-context attachment shape: inline text handed to the model as a file.
pub fn text_attachment(text: &str) -> Value {
    json!({
        "file_name": "context.txt",
        "file_type": "text/plain",
        "file_size": text.len(),
        "extracted_content": text,
    })
}

/// Parse a whole completion SSE body into the final assistant text.
///
/// Stream shape (one JSON object per `data:` line): `text_delta` and
/// `thinking_delta` carry prose/reasoning; `input_json_delta` fragments
/// accumulate into the tool input JSON, whose `content` (+ optional
/// `language`) is rendered as a fenced code block when the block stops.
/// Thinking is wrapped in `<think>…</think>` like the reference gateway.
pub fn parse_completion_sse(body: &str) -> anyhow::Result<String> {
    #[derive(Default)]
    struct StreamState {
        text: String,
        in_thinking: bool,
        tool_json: String,
    }

    let mut state = StreamState::default();
    let emit = |state: &mut StreamState, s: &str| state.text.push_str(s);

    for line in body.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let Ok(ev) = serde_json::from_str::<Value>(data) else {
            continue;
        };

        match ev["type"].as_str() {
            Some("error") => {
                let msg = ev["error"]["message"].as_str().unwrap_or("unknown error");
                bail!("claude.ai stream error: {msg}");
            }
            Some("content_block_start") => {
                let block = ev["content_block"]["type"].as_str().unwrap_or("");
                if block == "tool_use" {
                    state.tool_json.clear();
                }
            }
            Some("content_block_stop") => {
                if state.in_thinking {
                    emit(&mut state, "</think>\n");
                    state.in_thinking = false;
                }
                if !state.tool_json.is_empty() {
                    if let Ok(input) = serde_json::from_str::<Value>(&state.tool_json) {
                        let content = input["content"].as_str().unwrap_or("");
                        if !content.is_empty() {
                            let language = input["language"].as_str().unwrap_or("md");
                            let language = if language == "text/html" {
                                "html"
                            } else {
                                language
                            };
                            emit(&mut state, &format!("\n```{language}\n{content}\n```\n"));
                        }
                    }
                    state.tool_json.clear();
                }
            }
            Some("content_block_delta") => match ev["delta"]["type"].as_str() {
                Some("text_delta") => {
                    if let Some(t) = ev["delta"]["text"].as_str() {
                        emit(&mut state, t);
                    }
                }
                Some("thinking_delta") => {
                    if let Some(t) = ev["delta"]["thinking"].as_str() {
                        if !state.in_thinking {
                            emit(&mut state, "<think> ");
                            state.in_thinking = true;
                        }
                        emit(&mut state, t);
                    }
                }
                Some("input_json_delta") => {
                    if let Some(t) = ev["delta"]["partial_json"].as_str() {
                        state.tool_json.push_str(t);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    if state.text.trim().is_empty() {
        bail!(
            "no content in claude.ai completion stream — the model may have refused, \
             the session may have expired, or usage limits may be hit"
        );
    }
    Ok(state.text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_identify_the_web_client_with_stable_ids() {
        let headers = build_headers("sk-ant-sid01-test");
        let get = |k: &str| {
            headers
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.as_str())
                .expect("header present")
        };
        assert_eq!(get("anthropic-client-platform"), "web_claude_ai");
        assert!(get("anthropic-anonymous-id").starts_with("claudeai.v1."));
        assert_eq!(get("origin"), BASE_URL);

        // stable per session key
        let again = build_headers("sk-ant-sid01-test");
        assert_eq!(
            get("anthropic-device-id"),
            again
                .iter()
                .find(|(k, _)| *k == "anthropic-device-id")
                .unwrap()
                .1
        );
        // and distinct per session key
        let other = build_headers("sk-ant-sid01-other");
        assert_ne!(
            get("anthropic-device-id"),
            other
                .iter()
                .find(|(k, _)| *k == "anthropic-device-id")
                .unwrap()
                .1
        );
    }

    #[test]
    fn urls_are_scoped_by_organization() {
        assert_eq!(
            conversations_url("org-1"),
            format!("{BASE_URL}/api/organizations/org-1/chat_conversations")
        );
        assert_eq!(
            completion_url("org-1", "conv-9"),
            format!("{BASE_URL}/api/organizations/org-1/chat_conversations/conv-9/completion")
        );
    }

    #[test]
    fn org_uuid_extracts_first_membership() {
        let body =
            r#"{"email_address":"a@b.c","memberships":[{"organization":{"uuid":"org-77"}}]}"#;
        assert_eq!(extract_org_uuid(body).unwrap(), "org-77");
        assert!(extract_org_uuid(r#"{"memberships":[]}"#).is_err());
        assert!(extract_org_uuid("not json").is_err());
    }

    #[test]
    fn thinking_suffix_splits_model_names() {
        assert_eq!(
            split_thinking_suffix("claude-sonnet-4-6-thinking"),
            (true, "claude-sonnet-4-6")
        );
        assert_eq!(
            split_thinking_suffix("claude-sonnet-4-6"),
            (false, "claude-sonnet-4-6")
        );
    }

    #[test]
    fn completion_payload_mirrors_web_shape() {
        let v = build_completion_payload("hi", "claude-sonnet-4-6", vec![text_attachment("ctx")]);
        assert_eq!(v["prompt"], "hi");
        assert_eq!(v["model"], "claude-sonnet-4-6");
        assert_eq!(
            v["parent_message_uuid"],
            "00000000-0000-4000-8000-000000000000"
        );
        assert_eq!(v["rendering_mode"], "messages");
        assert_eq!(v["attachments"][0]["file_name"], "context.txt");
        assert_eq!(v["tools"].as_array().unwrap().len(), 3);
        // no max_tokens — the web completion endpoint rejects it
        assert!(v.get("max_tokens").is_none());
    }

    #[test]
    fn account_settings_toggle_paprika() {
        let on = build_account_settings(json!("extended"));
        assert_eq!(on["settings"]["paprika_mode"], "extended");
        let off = build_account_settings(Value::Null);
        assert!(off["settings"]["paprika_mode"].is_null());
    }

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|e| format!("data: {e}\n"))
            .collect::<String>()
            + "data: {\"type\":\"message_stop\"}\n"
    }

    fn text_event(t: &str) -> Value {
        json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": t}})
    }

    #[test]
    fn sse_accumulates_text_deltas() {
        let body = sse(&[
            json!({"type": "content_block_start", "content_block": {"type": "text"}}),
            text_event("Hello "),
            text_event("world"),
        ]);
        assert_eq!(parse_completion_sse(&body).unwrap(), "Hello world");
    }

    #[test]
    fn sse_wraps_thinking_in_think_tags() {
        let body = sse(&[
            json!({"type": "content_block_start", "content_block": {"type": "thinking"}}),
            json!({"type": "content_block_delta", "delta": {"type": "thinking_delta", "thinking": "hmm "}}),
            json!({"type": "content_block_delta", "delta": {"type": "thinking_delta", "thinking": "ok"}}),
            json!({"type": "content_block_stop"}),
            json!({"type": "content_block_start", "content_block": {"type": "text"}}),
            text_event("answer"),
        ]);
        assert_eq!(
            parse_completion_sse(&body).unwrap(),
            "<think> hmm ok</think>\nanswer"
        );
    }

    #[test]
    fn sse_renders_repl_output_as_code_fence() {
        let body = sse(&[
            text_event("running:"),
            json!({"type": "content_block_start", "content_block": {"type": "tool_use", "name": "repl"}}),
            json!({"type": "content_block_delta", "delta": {"type": "input_json_delta", "partial_json": "{\"language\": "}}),
            json!({"type": "content_block_delta", "delta": {"type": "input_json_delta", "partial_json": "\"python\", "}}),
            json!({"type": "content_block_delta", "delta": {"type": "input_json_delta", "partial_json": "\"content\": \"print(1)\"}"}}),
            json!({"type": "content_block_stop"}),
            text_event("done"),
        ]);
        let out = parse_completion_sse(&body).unwrap();
        assert!(out.contains("running:"), "{out}");
        assert!(out.contains("\n```python\nprint(1)\n```\n"), "{out}");
        assert!(out.ends_with("done"), "{out}");
    }

    #[test]
    fn sse_html_language_is_normalized() {
        let body = sse(&[
            json!({"type": "content_block_start", "content_block": {"type": "tool_use", "name": "artifacts"}}),
            json!({"type": "content_block_delta", "delta": {"type": "input_json_delta", "partial_json": "{\"language\": \"text/html\", \"content\": \"<p>hi</p>\"}"}}),
            json!({"type": "content_block_stop"}),
        ]);
        assert!(parse_completion_sse(&body).unwrap().contains("```html\n"));
    }

    #[test]
    fn sse_error_event_bails_with_message() {
        let body = sse(&[json!({"type": "error", "error": {"message": "usage limit exceeded"}})]);
        let err = parse_completion_sse(&body).unwrap_err();
        assert!(err.to_string().contains("usage limit exceeded"), "{err}");
    }

    #[test]
    fn sse_empty_stream_is_actionable() {
        let err = parse_completion_sse("data: {\"type\":\"message_stop\"}\n").unwrap_err();
        assert!(err.to_string().contains("no content"), "{err}");
    }

    #[test]
    fn sse_ignores_non_data_lines_and_malformed_json() {
        let body = concat!(
            "event: message_start\n",
            "data: not-json\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n",
        );
        assert_eq!(parse_completion_sse(body).unwrap(), "ok");
    }
}
