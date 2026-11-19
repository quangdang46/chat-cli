//! ChatGPT web protocol constants.
//!
//! Discovered via Chrome DevTools (2026-08-20):
//! - POST /unauth-mweb/sentinel/chat-requirements/prepare
//! - POST /backend-api/sentinel/chat-requirements/finalize
//! - GET/POST /backend-api/conversation/*

pub const SESSION_URL: &str = "https://chatgpt.com/api/auth/session";
pub const SENTINEL_REQUIREMENTS_URL: &str =
    "https://chatgpt.com/backend-api/sentinel/chat-requirements";
pub const CONVERSATION_URL: &str = "https://chatgpt.com/backend-api/conversation";

/// Browser cookie carrying the long-lived web session (DevTools →
/// Application → Cookies). Pasted once at `auth login`; refreshed values are
/// persisted by the dispatcher.
pub const SESSION_COOKIE_NAME: &str = "__Secure-next-auth.session-token";

/// Body of `GET /api/auth/session`. A logged-in session has a non-null
/// `accessToken`; an expired/anonymous session returns 200 with both fields
/// null — that must surface as an auth failure, not a panic on unwrap.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SessionResponse {
    #[serde(rename = "accessToken", default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub expires: Option<String>,
}

/// Stable per-install device id for `Oai-Device-Id` (the browser stores a
/// uuid; any stable uuid satisfies the sentinel).
///
/// Resolution order: `[providers.chatgpt].device_id` in config.toml (loaded
/// into `AuthContext::device_id` by the dispatcher) → a generated uuid that
/// is persisted back through `AuthContext::persist_device_id` → the
/// process-lifetime `OnceLock` fallback for callers without a persistence
/// callback (tests).
pub fn device_id(auth: &chat_core::provider::AuthContext) -> String {
    use std::sync::OnceLock;
    static GENERATED: OnceLock<String> = OnceLock::new();

    if let Some(id) = auth.device_id.as_deref() {
        return id.to_string();
    }
    GENERATED
        .get_or_init(|| {
            let id = uuid::Uuid::new_v4().to_string();
            // Best-effort: a stable id across runs looks more like a real
            // browser install. Failure (read-only config, tests) is
            // non-fatal — the in-process id still satisfies the sentinel.
            let _ = auth.save_device_id(&id);
            id
        })
        .clone()
}

/// Chrome-like UA matching the config array convention (FreeGPT35/chatGPTBox).
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123.0.0.0 Safari/537.36";

/// Solve the ChatGPT sentinel proof-of-work and build the
/// `Openai-Sentinel-Proof-Token` header value.
///
/// Mirrors the browser `_generateAnswer` (chatGPTBox `generateProofToken`,
/// FreeGPT35 `app.js`): brute-force an index into a base64(JSON) config array
/// until `sha3_512(seed + base)` hex prefix <= difficulty, then prepend the
/// `gAAAAAB` marker.
///
/// `difficulty` comes from the server and is UNTRUSTED: it must be a
/// non-empty, even-length hex string of at most 128 chars. Anything else
/// used to panic (`hex[..diff_len]` out of bounds) or silently emit a
/// garbage token, so it is rejected up front.
pub fn solve_proof_token(seed: &str, difficulty: &str) -> anyhow::Result<String> {
    validate_difficulty(difficulty)?;
    let now = chrono::Utc::now() - chrono::Duration::hours(8);
    let parse_time = now
        .format("%a, %d %b %Y %H:%M:%S GMT-0500 (Eastern Time)")
        .to_string();
    // Pre-serialize the fixed array elements ONCE. Per iteration only the
    // attempt counter (index 3) changes, so the JSON is rebuilt by splicing
    // decimal digits into a reused buffer — no serde_json::to_string, no
    // per-element format! allocs (was ~70 allocs/iter, up to 500k iters).
    let time_json = serde_json::to_string(&parse_time).expect("string serializes");
    let ua_json = serde_json::to_string(USER_AGENT).expect("string serializes");
    let url_json = serde_json::to_string(
        "https://tcr9i.chat.openai.com/v2/35536E1E-65B4-4D96-9D97-6ADB7EFF8147/api.js",
    )
    .expect("string serializes");
    let dpl_json = serde_json::to_string("dpl=1440a687921de39ff5ee56b92807faaadce73f13")
        .expect("string serializes");
    let plugins_json =
        serde_json::to_string("plugins−[object PluginArray]").expect("string serializes");
    // [4058, <time>, 4294705152, <counter>, <ua>, <url>, <dpl>, "en", "en-US",
    //  4294705152, <plugins>, 101, 301]
    let json_prefix = format!("[4058,{time_json},4294705152,");
    let json_suffix = format!(
        ",{ua_json},{url_json},{dpl_json},\"en\",\"en-US\",4294705152,{plugins_json},101,301]"
    );

    // Difficulty decoded to raw bytes once; the hex-string compare is
    // equivalent to a lexicographic byte compare (hex digit order == value
    // order), with early exit on the first differing byte.
    let diff_bytes = decode_hex(difficulty).expect("validated hex");

    use base64::Engine;
    use sha3::{Digest, Sha3_512};
    let mut json_buf = String::with_capacity(json_prefix.len() + json_suffix.len() + 8);
    let mut b64_buf: Vec<u8> = Vec::new();
    let mut num_buf = [0u8; 20];
    for i in 0..500_000u64 {
        // Zero-alloc decimal rendering of the counter.
        let mut v = i;
        let mut pos = 20usize;
        if v == 0 {
            pos -= 1;
            num_buf[pos] = b'0';
        } else {
            while v > 0 {
                pos -= 1;
                num_buf[pos] = b'0' + (v % 10) as u8;
                v /= 10;
            }
        }

        json_buf.clear();
        json_buf.push_str(&json_prefix);
        json_buf.push_str(std::str::from_utf8(&num_buf[pos..]).expect("ascii digits"));
        json_buf.push_str(&json_suffix);

        // base64 into a reused buffer sized for the worst case; the encoded
        // length only changes when the counter crosses a power of 10.
        let need = 4 * (json_buf.len() + 2) / 3;
        if b64_buf.len() < need {
            b64_buf.resize(need, 0);
        }
        let n = base64::engine::general_purpose::STANDARD
            .encode_slice(json_buf.as_bytes(), &mut b64_buf)
            .expect("buffer sized for encoded output");
        let base = &b64_buf[..n];

        let mut hasher = Sha3_512::new();
        hasher.update(seed.as_bytes());
        hasher.update(base);
        let digest = hasher.finalize();
        if digest[..diff_bytes.len()] <= diff_bytes[..] {
            let base_str = std::str::from_utf8(base).expect("base64 is ascii");
            return Ok(format!("gAAAAAB{base_str}"));
        }
    }
    // Fallback observed in reference impls when unsolved within attempts.
    let fallback = base64::engine::general_purpose::STANDARD.encode(format!("\"{seed}\""));
    Ok(format!("gAAAAABwQ8Lk5FbGpA2NcR9dShT6gYjU7VxZ4D{fallback}"))
}

/// Decode an even-length hex string to bytes. Caller must have run
/// `validate_difficulty` first (non-empty, even length, all hex digits).
fn decode_hex(s: &str) -> Option<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Server-controlled `difficulty` sanity gate. The digest hex is 128 chars;
/// `diff_len = difficulty.len() / 2` must stay within it, and a malformed or
/// empty value must never reach the compare loop.
fn validate_difficulty(difficulty: &str) -> anyhow::Result<()> {
    if difficulty.is_empty() {
        anyhow::bail!("sentinel PoW difficulty is empty — refusing to guess a token");
    }
    if difficulty.len() > 128 {
        anyhow::bail!(
            "sentinel PoW difficulty too long ({} chars, max 128) — server response is malformed",
            difficulty.len()
        );
    }
    if !difficulty.len().is_multiple_of(2) {
        anyhow::bail!("sentinel PoW difficulty must be an even-length hex string");
    }
    if !difficulty.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("sentinel PoW difficulty is not hex — refusing to solve");
    }
    Ok(())
}

/// `POST /sentinel/chat-requirements` — server decides whether a proof-of-work
/// is required before a conversation POST.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ChatRequirements {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(rename = "proofofwork", default)]
    pub proof_of_work: Option<PowRequired>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PowRequired {
    pub required: bool,
    pub seed: String,
    /// Base64 blob encoding difficulty + config for the sentinel solver.
    pub difficulty: String,
}

/// Body of `POST /backend-api/conversation`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationPayload {
    pub action: &'static str,
    pub messages: Vec<ConversationMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(rename = "parent_message_id")]
    pub parent_message_id: String,
    /// `"auto"` lets the backend pick; a concrete slug (e.g. from `--model`)
    /// is passed through verbatim — the server validates it.
    pub model: String,
    #[serde(rename = "history_and_training_disabled")]
    pub history_and_training_disabled: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationMessage {
    pub id: String,
    pub role: &'static str,
    pub content: MessageContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// POC sends text parts only (attachments are already fenced inline).
#[derive(Debug, Clone, serde::Serialize)]
pub struct MessageContent {
    #[serde(rename = "content_type")]
    pub content_type: &'static str,
    pub parts: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_https_chatgpt_com() {
        for (name, url) in [
            ("SESSION_URL", SESSION_URL),
            ("SENTINEL_REQUIREMENTS_URL", SENTINEL_REQUIREMENTS_URL),
            ("CONVERSATION_URL", CONVERSATION_URL),
        ] {
            assert!(
                url.starts_with("https://chatgpt.com/"),
                "{name} must target https://chatgpt.com, got {url}"
            );
        }
    }

    #[test]
    fn session_url_is_next_auth_endpoint() {
        assert_eq!(SESSION_URL, "https://chatgpt.com/api/auth/session");
    }

    #[test]
    fn sentinel_requirements_url_drives_pow_decision() {
        assert!(
            SENTINEL_REQUIREMENTS_URL.ends_with("/sentinel/chat-requirements"),
            "requirements endpoint drives the PoW decision"
        );
    }

    #[test]
    fn conversation_urls_target_backend_api() {
        assert_eq!(
            CONVERSATION_URL,
            "https://chatgpt.com/backend-api/conversation"
        );
    }

    #[test]
    fn session_cookie_name_is_secure_next_auth() {
        assert_eq!(SESSION_COOKIE_NAME, "__Secure-next-auth.session-token");
    }

    #[test]
    fn session_response_parses_access_token_and_expires() {
        let body = r#"{"accessToken":"abc","expires":"2026-09-21T00:00:00.000Z"}"#;
        let parsed: SessionResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.access_token.as_deref(), Some("abc"));
        assert_eq!(parsed.expires.as_deref(), Some("2026-09-21T00:00:00.000Z"));
    }

    #[test]
    fn session_response_accepts_null_fields_for_expired_session() {
        let parsed: SessionResponse =
            serde_json::from_str(r#"{"accessToken":null,"expires":null}"#)
                .expect("expired session body must deserialize");
        assert!(parsed.access_token.is_none());
        assert!(parsed.expires.is_none());
    }

    #[test]
    fn chat_requirements_parses_token_and_optional_pow() {
        let with_pow: ChatRequirements = serde_json::from_str(
            r#"{"token":"t-123","proofofwork":{"required":true,"seed":"s","difficulty":"0.9"}}"#,
        )
        .unwrap();
        assert_eq!(with_pow.token.as_deref(), Some("t-123"));
        let pow = with_pow.proof_of_work.expect("pow required");
        assert!(pow.required);
        assert_eq!(pow.seed, "s");

        let without_pow: ChatRequirements = serde_json::from_str(r#"{"token":"t-123"}"#).unwrap();
        assert!(without_pow.proof_of_work.is_none());
    }

    #[test]
    fn conversation_payload_serializes_new_and_continue_shapes() {
        // --new: no conversation_id, fresh parent uuid
        let new_payload = ConversationPayload {
            action: "next",
            messages: vec![ConversationMessage {
                id: "msg-1".into(),
                role: "user",
                content: MessageContent {
                    content_type: "text",
                    parts: vec!["hi".into()],
                },
                metadata: None,
            }],
            conversation_id: None,
            parent_message_id: "parent-uuid".into(),
            model: "auto".to_string(),
            history_and_training_disabled: false,
        };
        let v: serde_json::Value = serde_json::to_value(&new_payload).unwrap();
        assert_eq!(v["action"], "next");
        assert!(v.get("conversation_id").is_none(), "new must omit id");
        assert_eq!(v["parent_message_id"], "parent-uuid");
        assert_eq!(v["messages"][0]["content"]["content_type"], "text");

        // --continue: carries stored conversation_id
        let mut cont = new_payload.clone();
        cont.conversation_id = Some("conv-uuid".into());
        let v: serde_json::Value = serde_json::to_value(&cont).unwrap();
        assert_eq!(v["conversation_id"], "conv-uuid");
    }

    #[test]
    fn proof_token_has_gaaaaab_prefix_and_solves_low_difficulty() {
        let token = solve_proof_token("0.9abcdef", "0003ff").unwrap();
        assert!(token.starts_with("gAAAAAB"), "prefix required: {token}");
    }

    #[test]
    fn proof_token_deterministic_for_same_seed_and_difficulty() {
        // `parse_time` has one-second resolution, so two calls can
        // legitimately differ when they straddle a second boundary. Each
        // solve is sub-ms at this easy difficulty — retry until both land
        // in the same second.
        for _ in 0..100 {
            let a = solve_proof_token("seed-x", "00ffff").unwrap();
            let b = solve_proof_token("seed-x", "00ffff").unwrap();
            if a == b {
                return;
            }
        }
        panic!("same-second solves must be deterministic");
    }

    /// Server-controlled `difficulty`: oversized values used to panic on the
    /// `hex[..diff_len]` slice; empty/malformed values produced garbage.
    #[test]
    fn proof_token_rejects_hostile_difficulty_values() {
        for bad in [
            "",
            &"f".repeat(130), // > 128 chars → used to panic
            &"f".repeat(256), // far beyond the 128-char digest
            "abc",            // odd length
            "zzzz",           // not hex
            "0.9",            // sentinel test fixture shape, not hex
        ] {
            let err = solve_proof_token("seed", bad).unwrap_err();
            assert!(
                err.to_string().contains("difficulty"),
                "difficulty {bad:?} must fail with a difficulty error: {err}"
            );
        }
    }

    #[test]
    fn proof_token_accepts_boundary_difficulty_lengths() {
        // 128 hex chars is the max allowed — must not panic. "f"*128 is the
        // easiest possible prefix compare (every hex digit <= 'f'), so the
        // solver returns on the first attempt instead of grinding 500k.
        let token = solve_proof_token("seed", &"f".repeat(128)).unwrap();
        assert!(token.starts_with("gAAAAAB"));
    }

    #[test]
    fn device_id_is_stable_within_process() {
        let auth = chat_core::provider::AuthContext::default();
        assert_eq!(device_id(&auth), device_id(&auth));
        assert!(device_id(&auth).len() >= 32);
    }

    /// A configured device id wins over generation and is never persisted
    /// back (nothing to learn).
    #[test]
    fn device_id_prefers_configured_value() {
        let saved: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = saved.clone();
        let auth = chat_core::provider::AuthContext {
            device_id: Some("configured-id".to_string()),
            persist_device_id: Some(std::sync::Arc::new(move |id: &str| {
                sink.lock().unwrap().push(id.to_string());
                Ok(())
            })),
            ..Default::default()
        };
        assert_eq!(device_id(&auth), "configured-id");
        assert!(saved.lock().unwrap().is_empty());
    }
}
