# COMPREHENSIVE PLAN FOR CHAT-CLI — ChatGPT & DeepSeek Without The Browser

> **Goal:** a single `chat-cli -p "..." -a ./PLAN.md --provider chatgpt` that an agent can call headlessly, while the human pastes a web session token once and never touches the browser again.
>
> **Status:** scaffold done (`cargo check` clean), POC plan below is the build order.

---

## 1. Why This Repo Exists

| Today (browser) | With `chat-cli` |
|---|---|
| Open tab, paste `PLAN.md`, wait, copy answer, repeat | `chat-cli -p "tear this apart" -a ./PLAN.md --provider chatgpt` |
| Agent can't drive the browser without hacks | Agent calls a plain CLI, `stdout` is the answer |
| One tab per provider, one upload flow per site | `crates/provider-*` — one interface, many backends |
| History lives in the browser sidebar | `history list/show/rm` on `~/.local/share/chat-cli/history` |

Codex / the OpenAI API don't replace this: they are single-backend, API-key only, and don't reuse the ChatGPT web session you already pay for. `chat-cli` reuses that session and is multi-provider by construction.

---

## 2. POC Scope

### In

- **Auth:** ChatGPT + DeepSeek web, manual token paste (hidden input), immediate `GET /api/auth/session` validation, rolling refresh before each chat, `config.toml` (`0600`) as source of truth including `default_provider`.
- **Chat:** `chat-cli -p "prompt"` (or positional alias), `-s` system prompt, `-a` file attachments (repeat / comma / glob / `@list` / implicit `stdin`), `--new` / `--continue[<id>]`, `--provider` override.
- **Budget:** fail-fast check of `system + history + attachments + prompt` vs `context_limit()` with per-segment breakdown.
- **History (local):** `~/.local/share/chat-cli/history/<id>.jsonl` append-only, `--new` vs `--continue` mapped to real `ProviderHandle { conversation_id, parent_message_id }`, cross-provider guard.
- **Provider trait:** `crates/chat-core` knows no concrete provider; each backend is `crates/provider-*` self-registered via `inventory`, so adding Gemini/Claude is `cargo new crates/provider-gemini` with no core edit.

### Out (POC)

- `--json` machine output, `--model` switching, MCP wrapper — deferred.
- Server-side `GET /backend-api/conversations` listing and websocket — POC's `history list` is local only.
- Token-aware tokenizer — POC uses byte/char count against `context_limit()`.

---

## 3. CLI Surface (Frozen For POC)

```
# Auth
chat-cli auth login <chatgpt|deepseek>              # interactive, hidden input
chat-cli auth login <chatgpt|deepseek> --token "…" # non-interactive (agent)
chat-cli auth status                                # providers + expiry
chat-cli auth logout <chatgpt|deepseek>

# Chat
chat-cli -p "prompt"                                # uses default_provider, auto --continue most recent
chat-cli "prompt"                                   # positional alias for -p
chat-cli -p "prompt" --provider deepseek
chat-cli -p "prompt" --new                          # force fresh history
chat-cli -p "prompt" --continue                     # most recent for this provider
chat-cli -p "prompt" --continue <id>                # explicit history id
chat-cli -p "prompt" -s "you are a harsh reviewer"

# Attachments
-a a.md -a b.md                # repeat
-a a.md,b.md                   # comma-separated
-a "src/**/*.rs"               # glob
-a @filelist.txt               # line-delimited list file
cat PLAN.md | chat-cli -p "…" # stdin auto-attachment when -a is empty

# History (all local)
chat-cli history list [--provider x] [--limit N]
chat-cli history show <id>
chat-cli history rm <id>

# Config + globals
chat-cli config set default_provider deepseek
chat-cli config get default_provider
chat-cli --provider chatgpt --config /tmp/ci.toml -v -p "debug"
```

Provider resolution on every chat: `--provider` flag → `config.toml:default_provider` → hard error `No provider specified and no default_provider set. Run 'chat-cli auth login <provider>' first.`

`default_provider` semantics: first `auth login` sets it if absent; later logins don't override it; user edits `config.toml` or `config set` to change.

---

## 4. Workspace (`crates/*/src` — Open-Closed)

```
Cargo.toml  # [workspace] resolver="2" members = ["crates/*"]
            # workspace.dependencies declares chat-core, provider-*, anyhow, clap, ...

crates/
  chat-core/                 # knows no concrete provider
    src/
      provider.rs            # trait Provider + inventory registry
      config.rs              # config.toml load/save, 0600, default_provider logic
      history.rs             # HistoryFile + CRUD jsonl
      attach.rs              # -a resolution + size guards + default injection
      budget.rs              # fail-fast with breakdown
      lib.rs

  provider-chatgpt/          # ChatGPT web only
    src/
      lib.rs                 # ChatGptProvider impl
      pow.rs                 # Keccak-f[1600] rounds 1..23 (sentinel PoW)
      protocol.rs            # /api/auth/session, sentinel, /backend-api/conversation

  provider-deepseek/         # DeepSeek web only
    src/
      lib.rs                 # DeepSeekProvider impl
      protocol.rs            # chat.deepseek.com/api/v0/*

  chat-cli/                  # thin binary: wiring + clap only
    src/
      cli.rs                 # Args / subcommands
      dispatch.rs            # provider resolve → attach → budget → history → chat
      main.rs
```

Why `crates`, not modules: each provider is a **dependency boundary**. Adding `provider-gemini` later is `cargo new crates/provider-gemini --lib`, `impl Provider`, `inventory::submit!` — no change to `chat-core`, `provider-chatgpt`, or `provider-deepseek`. This is Strategy + Registry + Dependency Inversion (high-level `chat-core` + low-level providers both depend on the `Provider` abstraction).

---

## 5. Core Design

### 5.1 Provider trait (`chat-core/src/provider.rs`)

```rust
pub struct ProviderHandle {
    pub conversation_id: Option<String>,
    pub parent_message_id: Option<String>,
}
pub struct ChatReq {
    pub prompt: String,
    pub system: Option<String>,
    pub attachments_text: String, // from prepare_attachments()
}
pub struct ChatResp {
    pub content: String,
    pub conversation_id: String,
    pub message_id: String,
}
pub struct Session { pub valid: bool, pub expiry: Option<String> }

pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    fn context_limit(&self) -> usize;  // chatgpt ~512k chars, deepseek ~256k
    fn auth(&self, token: &str) -> Result<Session>;
    fn chat(&self, handle: &ProviderHandle, req: ChatReq) -> Result<ChatResp>;
    fn prepare_attachments(&self, files: &[PathBuf]) -> Result<String> {
        default_prepare_attachments(files) // <<<FILE: path>>> fences
    }
}

pub struct ProviderEntry { pub id: &'static str, pub factory: fn() -> Box<dyn Provider> }
inventory::collect!(ProviderEntry);
```

Registration is pull-free — each provider crate does `inventory::submit!(ProviderEntry { id: "gemini", factory: … })`; `dispatch.rs` discovers providers with `inventory::iter::<ProviderEntry>`.

### 5.2 Config (`chat-core/src/config.rs`)

```toml
# ~/.config/chat-cli/config.toml  (0600, atomic write+rename)
default_provider = "chatgpt"

[providers.chatgpt]
session_token = "paste-once"
access_token = "cached, refreshed via GET /api/auth/session"
access_token_expiry = "2026-08-21T00:00:00Z"

[providers.deepseek]
session_token = "..."
```

`Config::load(override)` / `save(override)` (+ `--config <path>` for tests/CI), `ensure_default_provider()` with first-login-only semantics.

### 5.3 History (`chat-core/src/history.rs`)

```rust
pub struct HistoryFile {
    pub id: String,                              // 8-char nanoid, local
    pub provider: String,
    pub provider_conversation_id: Option<String>, // chatgpt uuid | deepseek chat_session_id
    pub provider_parent_message_id: Option<String>,
    pub created_at: String,
    pub turns: Vec<Turn>,
}
pub struct Turn { pub role: String, pub content: String, pub timestamp: String }
```

- Path: `~/.local/share/chat-cli/history/<id>.jsonl` (override in tests), line 1 is `__meta__`, then one JSON `Turn` per line (append-only).
- `--new` creates fresh file, `ProviderHandle::default()`.
- `--continue[ <id>]` loads file, validates `history.provider == --provider` via `validate_continue_provider()` (cross-provider is a hard error), passes stored IDs in the handle; after chat, updates `provider_*` + appends two turns and `save()`.
- Default (no `--new` / `--continue` flag): continue most-recent history for the resolved provider, else new.
- `history list` is local scan, not `GET /backend-api/conversations`.

### 5.4 Attach (`chat-core/src/attach.rs`)

```rust
pub fn resolve_attachments(raw: &[String]) -> Result<Vec<PathBuf>>;
// comma split inside each -a, @file indirection, glob expansion, dedup preserving order

pub fn default_prepare_attachments(files: &[PathBuf]) -> Result<String>;
// guards: 1 MB/file, 5 MB total, UTF-8 only; injects <<<FILE: path>>>\n```\ncontent\n```\n

pub fn maybe_read_stdin_as_attachment(has_attachments: bool) -> Result<String>;
// when -a is empty and stdin is piped, read stdin as one virtual file
```

Provider override: `provider-chatgpt` can later replace default with real `/backend-api/files` upload without touching core.

### 5.5 Budget (`chat-core/src/budget.rs`)

```rust
pub fn budget_check(system: Option<&str>, history_text: &str,
                    attachments_text: &str, prompt: &str, limit: usize)
    -> Result<(), BudgetError>
// total = system.len + history_text.len + attachments_text.len + prompt.len
// on exceed: "context limit 512000 exceeded: system=.. history=.. attachments=.. prompt=.. total=.."
```

Checked at the **last point** before `Provider::chat()` — so the breakdown is truthful. No silent truncation; the agent decides what to cut.

### 5.6 PoW (`provider-chatgpt/src/pow.rs`)

Keccak-f[1600] **rounds 1..23** (skip round 0) — same variant as DeepSeek's `DeepSeekHashV1`. API:

```rust
pub fn solve_pow(challenge_hex: &str, salt: &str, expire_at: i64, difficulty: i64) -> Result<i64>
```

Preserve the reference optimizations: pre-absorb `prefix = "{salt}_{expire_at}_"` into the Keccak state, zero-alloc nonce suffix per iteration, check `ctx` cancellation every 1024 nonces. Carried over from the `ds2api` reference implementation; split into `pow.rs` (hash + solver) so `provider-deepseek` can reuse it if its sentinel shares the same hash.

### 5.7 Protocol (`provider-*/src/protocol.rs`)

- ChatGPT: `https://chatgpt.com/api/auth/session`, `.../sentinel/chat-requirements/prepare|finalize`, `.../backend-api/conversation`, `.../backend-api/conversations`.
- DeepSeek: `https://chat.deepseek.com/api/v0/users/login`, `.../chat_session/create`, `.../chat/create_pow_challenge`, `.../chat/completion`, `.../chat_session/fetch_page`.
- Verified against a live Chrome DevTools capture (2026-08-20) for ChatGPT sentinel + conversation flows; DeepSeek constants mirror `ds2api/internal/deepseek/protocol`.

---

## 6. Conversation Semantics (Grounded In Real APIs)

| Flag | ChatGPT web | DeepSeek web | Local |
|---|---:|---:|---:|
| `--new` | No `conversation_id`, fresh `parent_message_id` (uuid) | `POST /chat_session/create` → new `chat_session_id` | New `<id>.jsonl`, `provider_* = None` |
| `--continue` | Send stored `conversation_id + parent_message_id` | Send `chat_session_id + message_id` (int, stored as string) | Load file, build `ProviderHandle` |
| `--continue <id>` | Same, with explicit id | Same | Load `id`, validate `history.provider == --provider` |
| `history list` | (server) `GET /backend-api/conversations?limit=28` — not used in POC | (server) `GET /chat_session/fetch_page` — not used in POC | Scan `~/.local/share/.../*.jsonl` |
| `history show <id>` | (server) `GET /conversation/{id}` — not used in POC | (server) fetch session — not used in POC | `HistoryFile::load(id)` |

POC does **not** call server-side list/websocket. Only two server calls matter per chat: auth/refresh + chat (with optional PoW).

---

## 7. Build Order

| Phase | Deliverable | Est. | Done When |
|---|---:|---:|---|
| 0 | **Scaffold** — `Cargo.toml` + 4 crates + `cargo check` clean | 0.5d | ✅ scaffold done, `cargo check` passes (this plan) |
| 1 | **chat-core** — `config + history + attach + budget` unit-tested, no network | 1d | `cargo test -p chat-core` green, mocked provider |
| 2 | **provider-deepseek** — `auth + chat` (PoW if needed) | 1d | `chat-cli -p "hi" --provider deepseek` streams |
| 3 | **provider-chatgpt** — `auth` rolling refresh + sentinel PoW + `/conversation` | 1.5–2d | `chat-cli -p "hi" --provider chatgpt` + `-a` works |
| 4 | **chat-cli wiring** — `cli.rs + dispatch.rs` e2e + `cargo run -- --help` | 0.5d | All `history` subcommands + `--new/--continue` e2e |
| 5 | **Polish** — hidden input for `auth login`, `auth status` expiry, `-v`, error copy | 0.5d | Agent can `auth login --token` non-interactively, `cargo check --warnings` clean |

Total ~5d. DeepSeek (phase 2) unblocks a runnable demo before ChatGPT PoW.

---

## 8. Risks & Mitigations

| Risk | Mitigation |
|---|---:|
| PoW port off by one bit | Bring test vectors from the reference impl into `provider-chatgpt/src/pow.rs` `#[test]`; `cargo test -p provider-chatgpt` must pass same vectors |
| Cloudflare Turnstile beyond PoW | Out of scope for POC; if triggered, surface a clear error and suggest `--provider deepseek` fallback |
| Session rotation invalidates token | `auth login` re-validates via `GET /api/auth/session`; each chat refreshes before calling conversation — a bad paste fails fast |
| Adding a provider breaks core | `inventory` registry + `Provider` trait guarantee open-closed; `cargo test` in `chat-core` never imports a provider crate |
| History file races | Single-writer CLI (no daemon in POC); `history.rs` uses `write + rename` semantics already |

---

## 9. Repo Layout (Actual)

```
chat-cli/
  Cargo.toml
  README.md                          # English, hero + why-not-web/Codex + comparison table
  chat-cli_illustration.webp         # hero at repo root (per user pref)
  COMPREHENSIVE_PLAN_FOR_CHAT_CLI.md # this file
  crates/
    chat-core/src/{lib,provider,config,history,attach,budget}.rs
    provider-chatgpt/src/{lib,pow,protocol}.rs
    provider-deepseek/src/{lib,protocol}.rs
    chat-cli/src/{main,cli,dispatch}.rs
```

---

## 10. Not In This POC

`--json` output, `--model` switching, MCP wrapper, server-side conversation listing, token-aware tokenizer, background daemon — all deferred. Each is additive on top of this plan (no redesign needed: `--json` is a new `dispatch` output layer, MCP is a new `crates/chat-mcp` that reuses `chat-core`).

---

*Plan authored 2026-08-20. Scaffold is `cargo check` clean; implementation proceeds in phase order above.*
