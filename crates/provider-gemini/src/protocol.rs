//! Gemini web wire protocol: init extraction, request building, frame parsing.
//!
//! Reference surface observed on gemini.google.com (kept in sync manually):
//! - `GET /app` HTML embeds the session bootstrap values (`SNlM0e` POST token,
//!   `cfb2h` build label, `FdrFJe` frontend session id) as JSON-ish strings.
//! - `POST StreamGenerate` takes form fields `at` + `f.req` and answers with
//!   length-prefixed JSON frames whose lengths count UTF-16 code units of
//!   `"\n" + payload` (the newline between the digit marker and the payload is
//!   part of the counted span, exactly as the reference parser scans it).
//! - Each frame is a JSON array of "parts"; the interesting part carries the
//!   turn payload as a nested JSON *string* at index 2, whose array positions
//!   `[1]` (conversation metadata), `[4]` (candidates) and `[25]` (final
//!   context) evolve the conversation state. Candidate text at `[1][0]` is a
//!   cumulative snapshot: the last frame for a candidate holds the full text.

use anyhow::{bail, Context};
use regex::Regex;
use serde_json::{json, Value};

pub const INIT_URL: &str = "https://gemini.google.com/app";
pub const GENERATE_URL: &str =
    "https://gemini.google.com/_/BardChatUi/data/assistant.lamda.BardFrontendService/StreamGenerate";

pub const DEFAULT_LANGUAGE: &str = "en";

pub const MODEL_HEADER_KEY: &str = "x-goog-ext-525001261-jspb";
pub const REQUEST_HEADER_KEY: &str = "x-goog-ext-525005358-jspb";

pub const COOKIE_PSID: &str = "__Secure-1PSID";
pub const COOKIE_PSIDTS: &str = "__Secure-1PSIDTS";

/// Chrome-like UA; Gemini serves a different page shape to unknown agents.
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";

/// Baseline headers for both init and generate requests.
pub fn base_headers() -> Vec<(&'static str, String)> {
    vec![
        ("User-Agent", USER_AGENT.to_string()),
        ("Origin", "https://gemini.google.com".to_string()),
        ("Referer", "https://gemini.google.com/".to_string()),
        ("X-Same-Domain", "1".to_string()),
    ]
}

/// Session bootstrap values extracted from the `/app` HTML.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InitInfo {
    /// `SNlM0e` — required for posting; absent on guest pages.
    pub access_token: Option<String>,
    /// `cfb2h` — build label sent as the `bl` query param.
    pub build_label: Option<String>,
    /// `FdrFJe` — frontend session id sent as `f.sid`.
    pub session_id: Option<String>,
}

/// Extract bootstrap values from the init page HTML.
pub fn extract_init(html: &str) -> InitInfo {
    let grab = |key: &str| -> Option<String> {
        let re = Regex::new(&format!(r#""{key}":\s*"(.*?)""#)).ok()?;
        re.captures(html)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
    };
    InitInfo {
        access_token: grab("SNlM0e"),
        build_label: grab("cfb2h"),
        session_id: grab("FdrFJe"),
    }
}

/// Parse the login paste into `(1PSID, Option<1PSIDTS>)`.
///
/// Accepted shapes: a bare `1PSID` value, two bare values separated by `;` or
/// whitespace, or a real cookie paste containing
/// `__Secure-1PSID=…; __Secure-1PSIDTS=…`.
pub fn parse_cookie_credentials(pasted: &str) -> anyhow::Result<(String, Option<String>)> {
    let trimmed = pasted.trim();
    if trimmed.is_empty() {
        bail!(
            "no cookies given — copy {COOKIE_PSID} (+ {COOKIE_PSIDTS}) from \
             DevTools → Application → Cookies on gemini.google.com"
        );
    }

    if trimmed.contains(COOKIE_PSID) {
        let named = |name: &str| -> Option<String> {
            let re = Regex::new(&format!(r"{name}\s*=\s*([^;\s]+)")).ok()?;
            re.captures(trimmed)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string())
        };
        // `__Secure-1PSID=` cannot match inside `__Secure-1PSIDTS=` (the next
        // char is `T`), so both regexes are unambiguous.
        let psid = named(COOKIE_PSID).ok_or_else(|| {
            anyhow::anyhow!("paste contains cookie names but no {COOKIE_PSID} value")
        })?;
        return Ok((psid, named(COOKIE_PSIDTS)));
    }

    // Bare value(s): 1PSID values never contain `;`, `=` or whitespace.
    let chunks: Vec<&str> = trimmed
        .split(|c: char| c == ';' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    match chunks.as_slice() {
        [psid] => Ok((psid.to_string(), None)),
        [psid, psidts] => Ok((psid.to_string(), Some(psidts.to_string()))),
        _ => bail!(
            "could not parse cookie paste — expected {COOKIE_PSID} and optional \
             {COOKIE_PSIDTS} (e.g. `__Secure-1PSID=…; __Secure-1PSIDTS=…`)"
        ),
    }
}

pub fn cookie_header(psid: &str, psidts: Option<&str>) -> String {
    match psidts {
        Some(ts) => format!("{COOKIE_PSID}={psid}; {COOKIE_PSIDTS}={ts}"),
        None => format!("{COOKIE_PSID}={psid}"),
    }
}

/// Statically known models (name → JSPB header ingredients). Google renumbers
/// these occasionally; an unknown name falls back to the account default with
/// a warning instead of failing the turn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StaticModel {
    pub name: &'static str,
    pub model_id: &'static str,
    pub capacity_tail: u8,
    pub model_number: u8,
}

pub const STATIC_MODELS: &[StaticModel] = &[
    StaticModel {
        name: "gemini-flash",
        model_id: "fbb127bbb056c959",
        capacity_tail: 1,
        model_number: 1,
    },
    StaticModel {
        name: "gemini-pro",
        model_id: "9d8ca3786ebdfbea",
        capacity_tail: 1,
        model_number: 3,
    },
    StaticModel {
        name: "gemini-flash-lite",
        model_id: "cf41b0e0dd7d53e5",
        capacity_tail: 1,
        model_number: 6,
    },
];

pub fn resolve_static_model(name: &str) -> Option<&'static StaticModel> {
    let target = name.trim().to_lowercase();
    STATIC_MODELS.iter().find(|m| m.name == target)
}

/// JSPB model-selection headers. The selector array is the reference shape
/// `[1,null,null,null,"<id>",null,null,0,[4,5,6,8],null,null,<tail>,null,null,<num>]`
/// extended with the thinking flag (1) and a per-request session id.
pub fn build_model_headers(model: &StaticModel, session_id: &str) -> Vec<(&'static str, String)> {
    let mut selector = json!([
        1,
        null,
        null,
        null,
        model.model_id,
        null,
        null,
        0,
        [4, 5, 6, 8],
        null,
        null,
        model.capacity_tail,
        null,
        null,
        model.model_number,
    ]);
    if let Some(arr) = selector.as_array_mut() {
        arr.push(json!(1));
        arr.push(json!(session_id));
    }
    vec![
        (MODEL_HEADER_KEY, selector.to_string()),
        ("x-goog-ext-73010989-jspb", "[0]".to_string()),
        ("x-goog-ext-73010990-jspb", "[0,0,0]".to_string()),
    ]
}

/// Conversation metadata slot `[2]`: empty for a fresh chat, `[cid, rid]`
/// overlaid on the baseline for continuation.
pub fn default_metadata() -> Value {
    json!(["", "", "", null, null, null, null, null, null, ""])
}

pub fn continuation_metadata(cid: &str, rid: &str) -> Value {
    json!([cid, rid, "", null, null, null, null, null, null, ""])
}

/// Build the `f.req` form value: an outer envelope wrapping the 81-slot
/// sparse request array as a JSON string. `metadata` is slot `[2]`; the model
/// number (or the baseline 1) lands in slot `[79]`.
pub fn build_f_req(
    prompt: &str,
    metadata: &Value,
    model: Option<&StaticModel>,
    request_uuid: &str,
) -> String {
    let mut inner = vec![Value::Null; 81];
    inner[0] = json!([prompt, 0, null, null, null, null, 0]);
    inner[1] = json!([DEFAULT_LANGUAGE]);
    inner[2] = metadata.clone();
    inner[6] = json!([1]);
    inner[7] = json!(1); // streaming flag
    inner[10] = json!(1);
    inner[11] = json!(0);
    inner[17] = json!([[0]]);
    inner[18] = json!(0);
    inner[27] = json!(1);
    inner[30] = json!([4]);
    inner[41] = json!([1]);
    inner[53] = json!(0);
    inner[59] = json!(request_uuid);
    inner[61] = json!([]);
    inner[68] = json!(1);
    inner[79] = json!(model.map(|m| m.model_number).unwrap_or(1));
    inner[80] = json!(1);
    json!([[
        null,
        serde_json::to_string(&inner).expect("array always serializes")
    ]])
    .to_string()
}

#[cfg(test)]
fn utf16_units(s: &str) -> usize {
    s.chars()
        .map(|c| if (c as u32) > 0xFFFF { 2 } else { 1 })
        .sum()
}

/// Parse a whole StreamGenerate/batchexecute body into the flat list of
/// response parts: strip the XSSI prefix, then repeatedly read a digit
/// length marker (UTF-16 units of `"\n" + payload`), the newline, and that
/// many units of JSON payload. Malformed frames are skipped, mirroring the
/// reference client's tolerance of trailing junk.
pub fn parse_frames(body: &str) -> anyhow::Result<Vec<Value>> {
    let mut rest = body;
    if let Some(stripped) = rest.strip_prefix(")]}'") {
        rest = stripped.trim_start();
    }

    let mut parts = Vec::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return Ok(parts);
        }

        let digits_end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits_end == 0 {
            bail!("frame length marker expected — response is not a Gemini stream");
        }
        let units: usize = rest[..digits_end]
            .parse()
            .context("frame length marker is not a number")?;
        let after_marker = &rest[digits_end..];
        let Some(after_newline) = after_marker.strip_prefix('\n') else {
            bail!("newline missing after frame length marker — response shape changed");
        };

        // The counted span includes the newline we just consumed.
        let want = units.saturating_sub(1);
        let mut consumed = 0usize;
        let mut end_byte = 0usize;
        for c in after_newline.chars() {
            if consumed >= want {
                break;
            }
            consumed += if (c as u32) > 0xFFFF { 2 } else { 1 };
            end_byte += c.len_utf8();
        }
        if consumed != want {
            bail!(
                "truncated frame: marker says {want} units, body has {consumed} — stream cut off"
            );
        }

        rest = &after_newline[end_byte..];
        if let Ok(frame) = serde_json::from_str::<Value>(&after_newline[..end_byte]) {
            if let Some(arr) = frame.as_array() {
                parts.extend(arr.iter().cloned());
            } else {
                parts.push(frame);
            }
        }
    }
}

/// Server-side error code carried at `part[5][2][0][1][0]`, if any.
pub fn part_error_code(part: &Value) -> Option<i64> {
    part.get(5)?.get(2)?.get(0)?.get(1)?.get(0)?.as_i64()
}

/// Human-readable message for a known Gemini error code.
pub fn describe_error_code(code: i64) -> Option<&'static str> {
    Some(match code {
        1013 => "temporary Gemini error (1013) — retry shortly",
        1037 => "usage limit exceeded for this account/model — switch model (e.g. gemini-flash) or wait for the quota window",
        1050 => "model inconsistent with the conversation history — start a new conversation (--new) or keep the same model",
        1052 => "model header invalid — the static model table is likely stale; drop the model setting or update chat-cli",
        1060 => "IP temporarily flagged by Google — try a proxy/different network",
        _ => return None,
    })
}

/// One turn's outcome: full assistant text plus conversation ids.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GenerateOutcome {
    pub text: String,
    pub cid: String,
    pub rid: String,
}

const CARD_CONTENT_PREFIX: &str = "https://googleusercontent.com/card_content/";

fn strip_artifacts(text: &str) -> String {
    // Placeholder links where rendered attachments belong, possibly multi
    // segment paths, optionally followed by newlines.
    let re = Regex::new(r"https?://googleusercontent\.com/(?:\w+/)+\d+\n*").expect("valid regex");
    re.replace_all(text, "").to_string()
}

/// Candidate text: `[1][0]`, with the card-placeholder fallback at `[22][0]`.
fn candidate_text(candidate: &Value) -> String {
    let raw = candidate
        .get(1)
        .and_then(|t| t.get(0))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if raw.starts_with(CARD_CONTENT_PREFIX) {
        let alt = candidate
            .get(22)
            .and_then(|t| t.get(0))
            .and_then(Value::as_str)
            .unwrap_or("");
        return strip_artifacts(alt);
    }
    strip_artifacts(&raw)
}

/// Walk every part in stream order, tracking conversation metadata and the
/// latest (cumulative) text snapshot per candidate; the finished candidate —
/// or failing that, the last one seen — is the answer.
pub fn parse_generate_response(body: &str) -> anyhow::Result<GenerateOutcome> {
    let parts = parse_frames(body)?;
    let mut outcome = GenerateOutcome::default();
    let mut last: Option<(String, String, bool)> = None; // (rcid, text, completed)
    let mut completed: Option<(String, String)> = None;

    for part in &parts {
        if let Some(code) = part_error_code(part) {
            match describe_error_code(code) {
                Some(msg) => bail!("{msg}"),
                None => bail!("Gemini stream error code {code} — temporary service issue, retry"),
            }
        }

        let Some(inner_str) = part.get(2).and_then(Value::as_str) else {
            continue;
        };
        let Ok(inner) = serde_json::from_str::<Value>(inner_str) else {
            continue;
        };

        if let Some(m) = inner.get(1).filter(|m| m.is_array()) {
            if let Some(c) = m.get(0).and_then(Value::as_str) {
                if !c.is_empty() {
                    outcome.cid = c.to_string();
                }
            }
            if let Some(r) = m.get(1).and_then(Value::as_str) {
                if !r.is_empty() {
                    outcome.rid = r.to_string();
                }
            }
        }

        let Some(candidates) = inner.get(4).and_then(Value::as_array) else {
            continue;
        };
        for candidate in candidates {
            let Some(rcid) = candidate.get(0).and_then(Value::as_str) else {
                continue;
            };
            if rcid.is_empty() {
                continue;
            }
            let text = candidate_text(candidate);
            let is_completed = candidate
                .get(8)
                .and_then(|i| i.get(0))
                .and_then(Value::as_i64)
                == Some(2);
            last = Some((rcid.to_string(), text, is_completed));
            if is_completed {
                completed = Some((rcid.to_string(), last.as_ref().unwrap().1.clone()));
            }
        }
    }

    let chosen = match completed {
        Some((_, text)) => text,
        None => match last {
            Some((_, text, _)) => text,
            None => bail!(
                "no candidates in Gemini response — the model may have refused, \
                 the cookies may have expired, or the response shape changed"
            ),
        },
    };
    if chosen.trim().is_empty() {
        bail!("Gemini returned an empty answer — retry or re-run 'chat-cli auth login gemini'");
    }
    outcome.text = chosen;
    Ok(outcome)
}

/// Extract a rotated `__Secure-1PSIDTS` value from Set-Cookie headers.
pub fn rotated_psidts_from_headers<'a, I: Iterator<Item = &'a str>>(
    mut set_cookies: I,
) -> Option<String> {
    let re = Regex::new(r"(?i)__Secure-1PSIDTS=([^;\s]+)").expect("valid regex");
    set_cookies.find_map(|c| {
        re.captures(c)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_init_finds_bootstrap_values() {
        let html = r#"
            <html><body>
            window.WIZ_global_data = {"SNlM0e": "at-token-1", "cfb2h":"boq_build_01", "FdrFJe": "-12345"};
            </body></html>"#;
        let info = extract_init(html);
        assert_eq!(info.access_token.as_deref(), Some("at-token-1"));
        assert_eq!(info.build_label.as_deref(), Some("boq_build_01"));
        assert_eq!(info.session_id.as_deref(), Some("-12345"));
    }

    #[test]
    fn extract_init_guest_page_has_no_token() {
        let html = r#"<html>{"cfb2h":"boq_build_02"}</html>"#;
        let info = extract_init(html);
        assert!(info.access_token.is_none());
        assert_eq!(info.build_label.as_deref(), Some("boq_build_02"));
    }

    #[test]
    fn cookie_credentials_accept_all_paste_shapes() {
        let (psid, ts) = parse_cookie_credentials("g.a000value").unwrap();
        assert_eq!(psid, "g.a000value");
        assert!(ts.is_none());

        let (psid, ts) = parse_cookie_credentials("g.a000value; g.b111value").unwrap();
        assert_eq!(psid, "g.a000value");
        assert_eq!(ts.as_deref(), Some("g.b111value"));

        let (psid, ts) =
            parse_cookie_credentials("__Secure-1PSID=g.a000value; __Secure-1PSIDTS=g.b111value")
                .unwrap();
        assert_eq!(psid, "g.a000value");
        assert_eq!(ts.as_deref(), Some("g.b111value"));

        let (psid, ts) = parse_cookie_credentials("__Secure-1PSID = g.a000value;").unwrap();
        assert_eq!(psid, "g.a000value");
        assert!(ts.is_none());
    }

    #[test]
    fn cookie_credentials_reject_bad_pastes() {
        assert!(parse_cookie_credentials("").is_err());
        assert!(parse_cookie_credentials("   ").is_err());
        // 1PSIDTS alone is unusable
        assert!(parse_cookie_credentials("__Secure-1PSIDTS=g.b111value").is_err());
        // three bare chunks is not a shape we can trust
        assert!(parse_cookie_credentials("a b c d").is_err());
    }

    #[test]
    fn cookie_header_shapes() {
        assert_eq!(cookie_header("p", None), "__Secure-1PSID=p");
        assert_eq!(
            cookie_header("p", Some("t")),
            "__Secure-1PSID=p; __Secure-1PSIDTS=t"
        );
    }

    #[test]
    fn static_model_table_resolves_exact_names() {
        let m = resolve_static_model("gemini-flash").expect("flash must resolve");
        assert_eq!(m.model_id, "fbb127bbb056c959");
        assert_eq!(m.model_number, 1);
        assert!(resolve_static_model("gemini-ultra").is_none());
    }

    #[test]
    fn model_headers_extend_selector_with_flag_and_session() {
        let m = resolve_static_model("gemini-pro").unwrap();
        let headers = build_model_headers(m, "ABCD-1234");
        let selector = &headers
            .iter()
            .find(|(k, _)| *k == MODEL_HEADER_KEY)
            .unwrap()
            .1;
        let parsed: Value = serde_json::from_str(selector).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 17, "15 base fields + thinking flag + session id");
        assert_eq!(arr[4], "9d8ca3786ebdfbea");
        assert_eq!(arr[14], 3, "model number");
        assert_eq!(arr[15], 1, "thinking flag slot");
        assert_eq!(arr[16], "ABCD-1234");
    }

    #[test]
    fn metadata_shapes_match_reference_layout() {
        let base = default_metadata();
        assert_eq!(base.as_array().unwrap().len(), 10);
        let cont = continuation_metadata("c1", "r1");
        let arr = cont.as_array().unwrap();
        assert_eq!(arr[0], "c1");
        assert_eq!(arr[1], "r1");
        assert_eq!(arr[2], "");
        assert!(arr[3].is_null());
        assert_eq!(arr[9], "");
    }

    #[test]
    fn f_req_wraps_sparse_inner_array_as_string() {
        let f_req = build_f_req("hi", &default_metadata(), None, "UUID-1");
        let outer: Value = serde_json::from_str(&f_req).unwrap();
        let inner_str = outer[0][1].as_str().unwrap();
        let inner: Value = serde_json::from_str(inner_str).unwrap();
        let arr = inner.as_array().unwrap();
        assert_eq!(arr.len(), 81);
        assert_eq!(arr[0][0], "hi");
        assert_eq!(arr[1][0], "en");
        assert!(arr[7] == 1, "streaming flag");
        assert_eq!(arr[59], "UUID-1");
        assert_eq!(arr[79], 1, "no model → baseline number");
        // untouched slots stay null
        assert!(arr[5].is_null());

        let m = resolve_static_model("gemini-flash-lite").unwrap();
        let f_req = build_f_req("hi", &default_metadata(), Some(m), "U");
        let inner: Value = serde_json::from_str(
            serde_json::from_str::<Value>(&f_req).unwrap()[0][1]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(inner[79], 6, "model number overrides baseline");
    }

    /// Frame wire format: `<utf16 units of "\n"+payload>\n<payload>`.
    fn frame(parts: Value) -> String {
        let payload = serde_json::to_string(&parts).unwrap();
        format!("{}\n{}", utf16_units(&payload) + 1, payload)
    }

    fn stream_body(frames: &[Value]) -> String {
        let mut body = String::from(")]}'\n");
        for f in frames {
            body.push_str(&frame(f.clone()));
        }
        body
    }

    fn turn_parts(inner: Value) -> Value {
        json!([["wrb.fr", null, inner.to_string(), null, null, null]])
    }

    fn candidate(rcid: &str, text: &str, completed: bool) -> Value {
        let mut cand = vec![json!(rcid), json!([text])];
        for _ in 2..8 {
            cand.push(Value::Null);
        }
        cand.push(if completed { json!([2]) } else { json!([]) });
        json!(cand)
    }

    #[test]
    fn frames_parse_to_flat_parts() {
        let p1 = turn_parts(json!([null, ["c", "r"]]));
        let p2 = json!(["erh", 1]);
        let body = stream_body(&[p1, json!([p2])]);

        let parts = parse_frames(&body).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0][0], "wrb.fr");
        assert_eq!(parts[1][0], "erh");
    }

    #[test]
    fn frames_without_xssi_prefix_parse_too() {
        let p1 = turn_parts(json!([null, ["c", "r"]]));
        let raw = frame(json!([p1]));
        let parts = parse_frames(raw.trim_start()).unwrap();
        assert_eq!(parts.len(), 1);
    }

    #[test]
    fn generate_response_extracts_text_and_ids() {
        let inner = json!([
            null,
            ["cid-1", "rid-1"],
            null,
            null,
            [candidate("rc-1", "Hello world", true)]
        ]);
        let body = stream_body(&[turn_parts(inner)]);
        let out = parse_generate_response(&body).unwrap();
        assert_eq!(out.text, "Hello world");
        assert_eq!(out.cid, "cid-1");
        assert_eq!(out.rid, "rid-1");
    }

    #[test]
    fn generate_response_last_snapshot_wins_over_partials() {
        let inner1 = json!([
            null,
            ["cid", "rid"],
            null,
            null,
            [candidate("rc", "Hello ", false)]
        ]);
        let inner2 = json!([
            null,
            ["cid", "rid"],
            null,
            null,
            [candidate("rc", "Hello world", true)]
        ]);
        let body = stream_body(&[turn_parts(inner1), turn_parts(inner2)]);
        let out = parse_generate_response(&body).unwrap();
        assert_eq!(out.text, "Hello world");
    }

    #[test]
    fn generate_response_prefers_completed_candidate() {
        let inner = json!([
            null,
            ["cid", "rid"],
            null,
            null,
            [
                candidate("rc-partial", "draft", false),
                candidate("rc-done", "final answer", true),
            ]
        ]);
        let body = stream_body(&[turn_parts(inner)]);
        let out = parse_generate_response(&body).unwrap();
        assert_eq!(out.text, "final answer");
    }

    #[test]
    fn generate_response_strips_artifact_links() {
        let inner = json!([
            null,
            ["c", "r"],
            null,
            null,
            [candidate(
                "rc",
                "look https://googleusercontent.com/image_collection/image_retrieval/9\nhere",
                true,
            )]
        ]);
        let out = parse_generate_response(&stream_body(&[turn_parts(inner)])).unwrap();
        assert_eq!(out.text, "look here");
    }

    #[test]
    fn generate_response_falls_back_from_card_placeholder() {
        // candidate[1][0] is a card placeholder; real text sits at [22][0]
        let mut cand = vec![
            json!("rc"),
            json!(["https://googleusercontent.com/card_content/1"]),
        ];
        for _ in 2..22 {
            cand.push(Value::Null);
        }
        cand.push(json!(["the real card text"]));
        cand.push(json!([2]));
        let inner = json!([null, ["c", "r"], null, null, [json!(cand)]]);
        let out = parse_generate_response(&stream_body(&[turn_parts(inner)])).unwrap();
        assert_eq!(out.text, "the real card text");
    }

    #[test]
    fn generate_response_surfaces_error_codes() {
        for (code, expected) in [
            (1037, "usage limit"),
            (1060, "IP temporarily flagged"),
            (1052, "model header invalid"),
        ] {
            let part = json!([
                "wrb.fr",
                null,
                Value::Null,
                null,
                null,
                [null, null, [[null, [code]]]]
            ]);
            let body = stream_body(&[json!([part])]);
            let err = parse_generate_response(&body).unwrap_err();
            assert!(err.to_string().contains(expected), "{code}: {err}");
        }
    }

    #[test]
    fn generate_response_unknown_code_is_reported() {
        let part = json!([
            "wrb.fr",
            null,
            Value::Null,
            null,
            null,
            [null, null, [[null, [4242]]]]
        ]);
        let body = stream_body(&[json!([part])]);
        let err = parse_generate_response(&body).unwrap_err();
        assert!(err.to_string().contains("4242"), "{err}");
    }

    #[test]
    fn generate_response_without_candidates_is_actionable() {
        let inner = json!([null, ["c", "r"]]);
        let err = parse_generate_response(&stream_body(&[turn_parts(inner)])).unwrap_err();
        assert!(err.to_string().contains("no candidates"), "{err}");
    }

    #[test]
    fn generate_response_non_stream_body_is_clear_error() {
        let err = parse_generate_response("<html>consent page</html>").unwrap_err();
        assert!(
            err.to_string().contains("length marker"),
            "must point at the shape mismatch: {err}"
        );
    }

    #[test]
    fn multibyte_payload_lengths_count_utf16_units() {
        // payload with an astral char (2 UTF-16 units, 4 UTF-8 bytes)
        let inner = json!([
            null,
            ["c", "r"],
            null,
            null,
            [candidate("rc", "ok 🎉 done", true)]
        ]);
        let out = parse_generate_response(&stream_body(&[turn_parts(inner)])).unwrap();
        assert_eq!(out.text, "ok 🎉 done");
    }

    #[test]
    fn rotated_psidts_found_in_set_cookie() {
        let got = rotated_psidts_from_headers(
            ["__Secure-1PSIDTS=rotated-1; Path=/; Secure", "other=x"].into_iter(),
        );
        assert_eq!(got.as_deref(), Some("rotated-1"));

        assert!(rotated_psidts_from_headers(["nothing"].into_iter()).is_none());
    }
}
