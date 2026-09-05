//! Gemini web provider — reverse-engineered client for gemini.google.com.
//!
//! Protocol facts were ported from the public `gemini_webapi` (Python)
//! reference; this is an independent Rust implementation, no code copied
//! (the reference is AGPL-3.0, this crate stays MIT).
//!
//! Flow per turn:
//! 1. Cookies `__Secure-1PSID` (stored as `session_token`) + rolling
//!    `__Secure-1PSIDTS` (stored as `access_token`) authenticate the account.
//! 2. `GET /app` yields the `SNlM0e` POST token, build label and session id.
//! 3. `POST StreamGenerate` (whole body) carries the turn; frames are parsed
//!    into the final candidate text plus conversation ids.
//! 4. A rotated `1PSIDTS` from the response headers is persisted back through
//!    `AuthContext::persist_access_token`.
//!
//! Conversation continuation reuses `[cid, rid]` from the previous turn's
//! response as the request's metadata slot; history text is replayed by the
//! dispatcher like every other provider.

use std::time::Duration;

use anyhow::{bail, Context};
use chat_core::provider::{ChatReq, ChatResp, Provider, ProviderHandle, Session};

pub mod protocol;

pub struct GeminiProvider;

impl GeminiProvider {
    fn http_client() -> reqwest::Result<reqwest::blocking::Client> {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
    }

    /// Fetch the init page and extract the bootstrap session values.
    fn init_session(init_url: &str, cookie_value: &str) -> anyhow::Result<protocol::InitInfo> {
        let client = Self::http_client().context("failed to build HTTP client")?;
        let resp = client
            .get(init_url)
            .header("Cookie", cookie_value)
            .header("User-Agent", protocol::USER_AGENT)
            .send()
            .with_context(|| format!("GET {init_url} failed — check network/proxy"))?;

        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            bail!("429 from gemini.google.com — your IP is temporarily flagged by Google; try a proxy or another network");
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            bail!("{} from gemini.google.com — cookies rejected; re-run 'chat-cli auth login gemini' with fresh cookies", status.as_u16());
        }
        let html = resp.text().context("failed reading the Gemini init page")?;
        Ok(protocol::extract_init(&html))
    }

    fn map_status(e: reqwest::Error) -> anyhow::Error {
        match e.status() {
            Some(reqwest::StatusCode::TOO_MANY_REQUESTS) => anyhow::anyhow!(
                "429 from gemini.google.com — IP temporarily flagged; try a proxy or '--provider chatgpt'"
            ),
            Some(reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN) => {
                anyhow::anyhow!(
                    "gemini.google.com rejected the session — re-run 'chat-cli auth login gemini' with fresh __Secure-1PSID cookies"
                )
            }
            _ => e.into(),
        }
    }

    /// The full turn, parameterized over the two endpoints (test seam).
    fn chat_flow(
        handle: &ProviderHandle,
        req: &ChatReq,
        init_url: &str,
        generate_url: &str,
    ) -> anyhow::Result<ChatResp> {
        let psid = req.auth.session_token.as_deref().ok_or_else(|| {
            anyhow::anyhow!("no gemini cookies in config — run 'chat-cli auth login gemini' first")
        })?;
        let psidts = req.auth.access_token.clone();
        let cookie_value = protocol::cookie_header(psid, psidts.as_deref());

        // Model: static table; unknown names fall back to the account default.
        let model = match req.model.as_deref().map(protocol::resolve_static_model) {
            Some(Some(m)) => Some(*m),
            Some(None) => {
                eprintln!(
                    "note: unknown gemini model '{}' — using the account default (known: {})",
                    req.model.as_deref().unwrap_or_default(),
                    protocol::STATIC_MODELS
                        .iter()
                        .map(|m| m.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                None
            }
            None => None,
        };

        let info = Self::init_session(init_url, &cookie_value)?;
        let access_token = info.access_token.ok_or_else(|| {
            anyhow::anyhow!(
                "init page carried no SNlM0e token — cookies expired or region-blocked; \
                 re-run 'chat-cli auth login gemini' with fresh __Secure-1PSID cookies"
            )
        })?;

        // Attachments arrive as fenced text — same inline convention.
        let prompt = if req.attachments_text.is_empty() {
            req.prompt.clone()
        } else {
            format!("{}\n\n{}", req.attachments_text, req.prompt)
        };

        let metadata = match &handle.conversation_id {
            Some(cid) => protocol::continuation_metadata(
                cid,
                handle.parent_message_id.as_deref().unwrap_or(""),
            ),
            None => protocol::default_metadata(),
        };

        let request_session_id = uuid::Uuid::new_v4().to_string().to_uppercase();
        let request_uuid = uuid::Uuid::new_v4().to_string().to_uppercase();
        let reqid: u32 = 10_000
            + (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos())
                % 90_000;

        let client = Self::http_client().context("failed to build HTTP client")?;
        let f_req = protocol::build_f_req(&prompt, &metadata, model.as_ref(), &request_uuid);

        let mut rb = client
            .post(generate_url)
            .header("Cookie", &cookie_value)
            .header("User-Agent", protocol::USER_AGENT);
        for (k, v) in protocol::base_headers() {
            rb = rb.header(k, v);
        }
        if let Some(m) = model {
            for (k, v) in protocol::build_model_headers(&m, &request_session_id) {
                rb = rb.header(k, v);
            }
        }
        rb = rb.header(
            protocol::REQUEST_HEADER_KEY,
            format!("[\"{request_uuid}\",1]"),
        );

        let mut query = vec![
            ("hl", protocol::DEFAULT_LANGUAGE.to_string()),
            ("_reqid", reqid.to_string()),
            ("rt", "c".to_string()),
        ];
        if let Some(bl) = &info.build_label {
            query.push(("bl", bl.clone()));
        }
        if let Some(sid) = &info.session_id {
            query.push(("f.sid", sid.clone()));
        }
        rb = rb.query(&query);
        rb = rb.form(&[("at", access_token.as_str()), ("f.req", f_req.as_str())]);

        let resp = rb
            .send()
            .with_context(|| format!("POST {generate_url} failed"))?
            .error_for_status()
            .map_err(Self::map_status)?;

        // Rotate the rolling cookie when Google re-issues it.
        if let Some(rotated) = protocol::rotated_psidts_from_headers(
            resp.headers()
                .get_all("set-cookie")
                .iter()
                .filter_map(|v| v.to_str().ok()),
        ) {
            if Some(&rotated) != psidts.as_ref() {
                let expiry = (chrono::Utc::now() + chrono::Duration::minutes(45)).to_rfc3339();
                let _ = req.auth.persist_access_token(&rotated, &expiry);
            }
        }

        let body = resp
            .text()
            .context("failed reading the Gemini stream response")?;
        let outcome = protocol::parse_generate_response(&body)?;

        Ok(ChatResp {
            content: outcome.text,
            conversation_id: if outcome.cid.is_empty() {
                format!("gm-{}", uuid::Uuid::new_v4())
            } else {
                outcome.cid
            },
            message_id: if outcome.rid.is_empty() {
                format!("gm-{}", uuid::Uuid::new_v4())
            } else {
                outcome.rid
            },
        })
    }
}

impl Provider for GeminiProvider {
    fn id(&self) -> &'static str {
        "gemini"
    }

    fn context_limit(&self) -> usize {
        // Generous upper bound; the budget check only fails fast on overflow.
        1_000_000
    }

    fn auth(&self, token: &str) -> anyhow::Result<Session> {
        let (psid, psidts) = protocol::parse_cookie_credentials(token)?;
        let info = Self::init_session(
            protocol::INIT_URL,
            &protocol::cookie_header(&psid, psidts.as_deref()),
        )?;
        // A guest page carries no SNlM0e: the cookies were not accepted.
        Ok(Session {
            valid: info.access_token.is_some(),
            expiry: None,
        })
    }

    fn chat(&self, handle: &ProviderHandle, req: ChatReq) -> anyhow::Result<ChatResp> {
        Self::chat_flow(handle, &req, protocol::INIT_URL, protocol::GENERATE_URL)
    }
}

inventory::submit!(chat_core::provider::ProviderEntry {
    id: "gemini",
    factory: || Box::new(GeminiProvider),
});

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    fn server() -> mockito::ServerGuard {
        mockito::Server::new()
    }

    /// Init page fixture: bootstrap values embedded as WIZ_global_data.
    fn init_page(access_token: &str) -> String {
        format!(
            r#"<html><script>window.WIZ_global_data = {{"SNlM0e": "{access_token}", "cfb2h": "boq_test_build", "FdrFJe": "-99"}};</script></html>"#
        )
    }

    fn frame(parts: serde_json::Value) -> String {
        let payload = serde_json::to_string(&parts).unwrap();
        let units: usize = payload
            .chars()
            .map(|c| if (c as u32) > 0xFFFF { 2 } else { 1 })
            .sum();
        format!("{}\n{}", units + 1, payload)
    }

    fn answer_body(text: &str, cid: &str, rid: &str) -> String {
        let candidate =
            serde_json::json!(["rc-1", [text], null, null, null, null, null, null, [2],]);
        let inner = serde_json::json!([null, [cid, rid], null, null, [candidate]]);
        let part = serde_json::json!(["wrb.fr", null, inner.to_string(), null, null, null]);
        format!(")]}}'\n{}", frame(serde_json::json!([part])))
    }

    fn gen_req(auth: chat_core::provider::AuthContext) -> ChatReq {
        ChatReq {
            prompt: "hello gemini".to_string(),
            system: None,
            attachments_text: String::new(),
            model: None,
            auth,
        }
    }

    /// Minimal form-urlencoded parser for asserting captured bodies.
    fn form_pairs(raw: &str) -> Vec<(String, String)> {
        raw.split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (urldecode(k), urldecode(v)))
            .collect()
    }

    fn urldecode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'%' if i + 3 <= bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz");
                    out.push(u8::from_str_radix(hex, 16).unwrap_or(b'%'));
                    i += 3;
                }
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                b => {
                    out.push(b);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).to_string()
    }

    #[test]
    fn registered_provider_is_discoverable() {
        assert!(chat_core::provider::get_provider("gemini").is_some());
    }

    #[test]
    fn init_sends_cookies_and_extracts_bootstrap() {
        let mut s = server();
        let _m = s
            .mock("GET", "/app")
            .match_header("cookie", "__Secure-1PSID=psid-v; __Secure-1PSIDTS=psidts-v")
            .with_status(200)
            .with_body(init_page("at-1"))
            .create();
        let url = s.url();

        let info = GeminiProvider::init_session(
            &format!("{url}/app"),
            "__Secure-1PSID=psid-v; __Secure-1PSIDTS=psidts-v",
        )
        .unwrap();
        assert_eq!(info.access_token.as_deref(), Some("at-1"));
        assert_eq!(info.build_label.as_deref(), Some("boq_test_build"));
        assert_eq!(info.session_id.as_deref(), Some("-99"));
    }

    #[test]
    fn auth_credentials_parse_and_gate_without_http() {
        // auth() = credential parse + init probe (covered by init tests);
        // the parse gate is what we can assert without hitting the network.
        let (psid, psidts) =
            protocol::parse_cookie_credentials("__Secure-1PSID=g.psid; __Secure-1PSIDTS=g.ts")
                .unwrap();
        assert_eq!(psid, "g.psid");
        assert_eq!(psidts.as_deref(), Some("g.ts"));
    }

    #[test]
    fn auth_empty_paste_fails_fast_without_http() {
        let err = GeminiProvider.auth("").unwrap_err();
        assert!(err.to_string().contains("no cookies"), "{err}");
    }

    #[test]
    fn init_429_maps_to_proxy_hint() {
        let mut s = server();
        let _m = s.mock("GET", "/app").with_status(429).create();
        let url = s.url();
        let err = GeminiProvider::init_session(&format!("{url}/app"), "__Secure-1PSID=g.psid")
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("429") && msg.contains("proxy"), "{msg}");
    }

    #[test]
    fn chat_new_turn_posts_at_and_default_metadata() {
        let mut s = server();
        let _init = s
            .mock("GET", "/app")
            .with_status(200)
            .with_body(init_page("at-9"))
            .create();
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sink = captured.clone();
        let _gen = s
            .mock(
                "POST",
                mockito::Matcher::Regex(r"^/StreamGenerate.*$".to_string()),
            )
            .match_header("cookie", "__Secure-1PSID=psid-v")
            .with_body_from_request(move |req| {
                *sink.lock() = Some(String::from_utf8_lossy(req.body().unwrap()).to_string());
                answer_body("the answer", "cid-77", "rid-88").into_bytes()
            })
            .with_status(200)
            .create();
        let url = s.url();

        let resp = GeminiProvider::chat_flow(
            &ProviderHandle::default(),
            &gen_req(chat_core::provider::AuthContext {
                session_token: Some("psid-v".to_string()),
                ..Default::default()
            }),
            &format!("{url}/app"),
            &format!("{url}/StreamGenerate"),
        )
        .unwrap();

        assert_eq!(resp.content, "the answer");
        assert_eq!(resp.conversation_id, "cid-77");
        assert_eq!(resp.message_id, "rid-88");

        let raw = captured.lock().take().expect("form body captured");
        let pairs = form_pairs(&raw);
        assert_eq!(
            pairs
                .iter()
                .find(|(k, _)| k == "at")
                .map(|(_, v)| v.as_str()),
            Some("at-9")
        );
        let f_req = &pairs.iter().find(|(k, _)| k == "f.req").unwrap().1;
        let outer: serde_json::Value = serde_json::from_str(f_req).unwrap();
        let inner: serde_json::Value = serde_json::from_str(outer[0][1].as_str().unwrap()).unwrap();
        assert_eq!(inner[0][0], "hello gemini");
        assert_eq!(inner[2][0], "", "fresh chat: empty metadata cid");
        assert_eq!(inner[2][1], "");
    }

    #[test]
    fn chat_continue_carries_cid_rid_in_metadata() {
        let mut s = server();
        let _init = s
            .mock("GET", "/app")
            .with_status(200)
            .with_body(init_page("at-9"))
            .create();
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sink = captured.clone();
        let _gen = s
            .mock(
                "POST",
                mockito::Matcher::Regex(r"^/StreamGenerate.*$".to_string()),
            )
            .with_body_from_request(move |req| {
                *sink.lock() = Some(String::from_utf8_lossy(req.body().unwrap()).to_string());
                answer_body("second", "cid-77", "rid-89").into_bytes()
            })
            .with_status(200)
            .create();
        let url = s.url();

        let handle = ProviderHandle {
            conversation_id: Some("cid-77".to_string()),
            parent_message_id: Some("rid-88".to_string()),
        };
        let resp = GeminiProvider::chat_flow(
            &handle,
            &gen_req(chat_core::provider::AuthContext {
                session_token: Some("psid-v".to_string()),
                ..Default::default()
            }),
            &format!("{url}/app"),
            &format!("{url}/StreamGenerate"),
        )
        .unwrap();
        assert_eq!(resp.message_id, "rid-89");

        let raw = captured.lock().take().unwrap();
        let pairs = form_pairs(&raw);
        let f_req = &pairs.iter().find(|(k, _)| k == "f.req").unwrap().1;
        let outer: serde_json::Value = serde_json::from_str(f_req).unwrap();
        let inner: serde_json::Value = serde_json::from_str(outer[0][1].as_str().unwrap()).unwrap();
        assert_eq!(
            inner[2][0], "cid-77",
            "continuation metadata must carry cid"
        );
        assert_eq!(
            inner[2][1], "rid-88",
            "continuation metadata must carry rid"
        );
    }

    #[test]
    fn chat_with_known_model_sends_model_header() {
        let mut s = server();
        let _init = s
            .mock("GET", "/app")
            .with_status(200)
            .with_body(init_page("at-9"))
            .create();
        let _gen = s
            .mock(
                "POST",
                mockito::Matcher::Regex(r"^/StreamGenerate.*$".to_string()),
            )
            .match_header(
                protocol::MODEL_HEADER_KEY,
                mockito::Matcher::Regex("fbb127bbb056c959".to_string()),
            )
            .with_status(200)
            .with_body(answer_body("flash!", "c", "r"))
            .create();
        let url = s.url();

        let resp = GeminiProvider::chat_flow(
            &ProviderHandle::default(),
            &ChatReq {
                model: Some("gemini-flash".to_string()),
                ..gen_req(chat_core::provider::AuthContext {
                    session_token: Some("psid-v".to_string()),
                    ..Default::default()
                })
            },
            &format!("{url}/app"),
            &format!("{url}/StreamGenerate"),
        )
        .unwrap();
        assert_eq!(resp.content, "flash!");
    }

    #[test]
    fn chat_persists_rotated_psidts_through_persist_callback() {
        let mut s = server();
        let _init = s
            .mock("GET", "/app")
            .with_status(200)
            .with_body(init_page("at-9"))
            .create();
        let _gen = s
            .mock(
                "POST",
                mockito::Matcher::Regex(r"^/StreamGenerate.*$".to_string()),
            )
            .with_status(200)
            .with_header("set-cookie", "__Secure-1PSIDTS=rotated-99; Path=/; Secure")
            .with_body(answer_body("ok", "c", "r"))
            .create();
        let url = s.url();

        let rotated: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
        let sink = rotated.clone();
        let mut auth = chat_core::provider::AuthContext {
            session_token: Some("psid-v".to_string()),
            access_token: Some("old-ts".to_string()),
            ..Default::default()
        };
        auth.persist = Some(Arc::new(move |token: &str, expiry: &str| {
            *sink.lock() = Some((token.to_string(), expiry.to_string()));
            Ok(())
        }));

        let resp = GeminiProvider::chat_flow(
            &ProviderHandle::default(),
            &gen_req(auth),
            &format!("{url}/app"),
            &format!("{url}/StreamGenerate"),
        )
        .unwrap();
        assert_eq!(resp.content, "ok");

        let got = rotated.lock().take().expect("rotation must be persisted");
        assert_eq!(got.0, "rotated-99");
        assert!(!got.1.is_empty(), "expiry must be set alongside the cookie");
    }

    #[test]
    fn chat_requires_session_token() {
        let err = GeminiProvider::chat_flow(
            &ProviderHandle::default(),
            &gen_req(chat_core::provider::AuthContext::default()),
            "http://localhost:1/app",
            "http://localhost:1/StreamGenerate",
        )
        .unwrap_err();
        assert!(err.to_string().contains("auth login gemini"), "{err}");
    }
}
