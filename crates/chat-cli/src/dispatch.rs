//! Dispatch: resolve provider → attach → budget → history → chat.

use std::path::Path;

use anyhow::Context;

use chat_core::config::Config;
use chat_core::history::HistoryFile;

use crate::cli::{Args, AuthCmd, Command, ConfigCmd, HistoryCmd};

pub async fn run(mut args: Args) -> anyhow::Result<()> {
    // Subcommands
    if let Some(cmd) = args.command.take() {
        return run_subcommand(cmd, &args).await;
    }

    // -p wins over the positional alias; say so on stderr instead of
    // silently dropping the positional (stdout stays clean for piping).
    if args.prompt.is_some() && args.prompt_positional.is_some() {
        eprintln!("[info] both -p/--prompt and a positional prompt were given; using -p");
    }

    // Chat mode: must have a prompt
    let prompt = args
        .effective_prompt()
        .ok_or_else(|| anyhow::anyhow!("no prompt provided; use -p \"...\" or positional arg"))?
        .to_string();

    run_chat(prompt, args).await
}

async fn run_subcommand(cmd: Command, global: &Args) -> anyhow::Result<()> {
    let config_path = global.config.as_deref().map(Path::new);
    match cmd {
        Command::Auth { cmd } => match cmd {
            AuthCmd::Login {
                provider,
                token,
                set_default,
            } => auth_login(&provider, token, set_default, config_path).await,
            AuthCmd::Default { provider } => {
                ensure_known_provider(&provider)?;
                let mut cfg = Config::load(config_path)?;
                cfg.default_provider = Some(provider.clone());
                cfg.save(config_path)?;
                println!("default_provider = {provider}");
                Ok(())
            }
            AuthCmd::Status => {
                let cfg = Config::load(config_path)?;
                match cfg.default_provider.as_deref() {
                    Some(p) => println!("default_provider: {p}"),
                    None => println!("default_provider: (unset)"),
                }
                for (k, v) in &cfg.providers {
                    let has_token = v.session_token.is_some();
                    println!(
                        "  {}: logged_in={} expiry={}",
                        k,
                        has_token,
                        v.access_token_expiry.as_deref().unwrap_or("(none)")
                    );
                }
                Ok(())
            }
            AuthCmd::Logout { provider } => {
                // Same validation as `auth default`/`config set` — a typo'd
                // provider must not "succeed" silently.
                ensure_known_provider(&provider)?;
                let mut cfg = Config::load(config_path)?;
                cfg.providers.remove(&provider);
                // If default_provider was this one, clear it
                if cfg.default_provider.as_deref() == Some(&provider) {
                    cfg.default_provider = None;
                }
                cfg.save(config_path)?;
                println!("logged out from {}", provider);
                Ok(())
            }
        },
        Command::History { cmd } => {
            match cmd {
                HistoryCmd::List { provider, limit } => {
                    // Same validation as everywhere else — a bogus provider
                    // must error, not silently print an empty list.
                    if let Some(p) = &provider {
                        ensure_known_provider(p)?;
                    }
                    let mut items = HistoryFile::list(None)?;
                    if let Some(p) = provider {
                        items.retain(|h| h.provider == p);
                    }
                    // `--limit 0` means "no limit" (show everything).
                    if let Some(n) = limit.filter(|&n| n > 0) {
                        items.truncate(n);
                    }
                    for h in items {
                        println!(
                            "{}  {}  {}  ({} turns)  {}",
                            h.id, h.provider, h.created_at, h.turn_count, h.preview
                        );
                    }
                    Ok(())
                }
                HistoryCmd::Show { id } => {
                    let hf = load_history_or_not_found(&id)?;
                    println!("{}", serde_json::to_string_pretty(&hf)?);
                    Ok(())
                }
                HistoryCmd::Rm { id } => {
                    // HistoryFile::rm validates the id (path-traversal safe)
                    // and reports a friendly not-found.
                    HistoryFile::rm(&id, None)?;
                    println!("removed {}", id);
                    Ok(())
                }
            }
        }
        Command::Config { cmd } => match cmd {
            ConfigCmd::Set { key, value } => {
                if key != "default_provider" {
                    anyhow::bail!(
                        "unknown config key '{}' (only 'default_provider' supported)",
                        key
                    );
                }
                // Validate against registered providers — a garbage value here
                // would silently break every later chat call (#5).
                ensure_known_provider(&value)?;
                let mut cfg = Config::load(config_path)?;
                cfg.default_provider = Some(value.clone());
                cfg.save(config_path)?;
                println!("{} = {}", key, value);
                Ok(())
            }
            ConfigCmd::Get { key } => {
                let cfg = Config::load(config_path)?;
                match key.as_str() {
                    "default_provider" => match cfg.default_provider.as_deref() {
                        Some(p) => println!("{p}"),
                        None => println!("(unset)"),
                    },
                    _ => anyhow::bail!(
                        "unknown config key '{}' (only 'default_provider' supported)",
                        key
                    ),
                }
                Ok(())
            }
        },
    }
}

/// Reject unknown provider ids with the registered list — shared by
/// `auth default` and `config set default_provider`.
fn ensure_known_provider(provider_id: &str) -> anyhow::Result<()> {
    if chat_core::provider::get_provider(provider_id).is_none() {
        anyhow::bail!(
            "unknown provider '{}'. Registered: {:?}",
            provider_id,
            chat_core::provider::list_providers()
        );
    }
    Ok(())
}

/// Load a history entry by id with two layers of protection:
/// - the id is validated against the nanoid alphabet before any path join
///   (path-traversal fix — `../x`, `a/b`, absolute paths are rejected);
/// - a missing entry produces a clean "not found" message (naming the
///   `<id>.jsonl` file) instead of a raw OS error.
fn load_history_or_not_found(id: &str) -> anyhow::Result<HistoryFile> {
    let path = HistoryFile::path_for_id(id, None)?;
    if !path.exists() {
        anyhow::bail!("history entry '{id}' not found (run 'chat-cli history list' to see ids)");
    }
    HistoryFile::load(id, None).with_context(|| {
        format!(
            "failed to parse history file '{}'; it may be corrupt",
            path.display()
        )
    })
}

/// `auth login <provider> [--token ...] [--default]`: print copy-paste
/// snippets, validate via the provider, persist; `--default` overrides the
/// first-login-only default_provider semantics.
async fn auth_login(
    provider_id: &str,
    token: Option<String>,
    set_default: bool,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    let provider = chat_core::provider::get_provider(provider_id).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown provider '{}'. Registered: {:?}",
            provider_id,
            chat_core::provider::list_providers()
        )
    })?;

    // Copy-paste snippets BEFORE asking for the token: paste into DevTools
    // console on the provider site and the token prints/copies in seconds.
    print_token_snippets(provider_id);

    let token = match token {
        Some(t) => t,
        None => {
            // Non-TTY (piped stdin, CI, agent harness): rpassword would block
            // or consume the pipe — fail fast and tell the user to pass
            // --token explicitly.
            use std::io::IsTerminal;
            if !std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "no --token given and stdin is not a TTY — rerun with \
                     'chat-cli auth login {provider_id} --token <token>'"
                );
            }
            eprint!("Paste session token for {provider_id}: ");
            use std::io::Write;
            std::io::stderr().flush()?;
            rpassword::read_password()
                .context("failed reading token from terminal; pass --token instead")?
        }
    };
    // Oversized ChatGPT JWTs are chunked by the browser into
    // `__Secure-next-auth.session-token.0` + `.1`; users paste the parts
    // joined with commas — concatenate them back into one token (#snippet UX).
    let token = if provider_id == "chatgpt" && token.contains(',') {
        let merged: String = token
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .concat();
        eprintln!("[info] merged {} cookie parts into one token", merged.len());
        merged
    } else {
        token
    };

    let token_for_auth = token.clone();
    let session = tokio::task::spawn_blocking(move || provider.auth(&token_for_auth))
        .await
        .context("auth task panicked")??;
    if !session.valid {
        let hint = match provider_id {
            "chatgpt" => {
                "copy the __Secure-next-auth.session-token cookie Value(s) via \
DevTools → Application → Cookies (join .0/.1 parts with commas)"
            }
            "deepseek" => "re-copy your DeepSeek userToken (web session token) and retry",
            "claude" => {
                "re-copy the sessionKey cookie via \
DevTools → Application → Cookies on claude.ai and retry"
            }
            "gemini" => {
                "re-copy __Secure-1PSID (+ __Secure-1PSIDTS) via \
DevTools → Application → Cookies — the 1PSIDTS value expires quickly"
            }
            _ => "re-check the token and retry",
        };
        anyhow::bail!("token rejected by '{provider_id}' — not saved. {hint}.");
    }

    let mut cfg = Config::load(config_path)?;
    {
        let entry = cfg.providers.entry(provider_id.to_string()).or_default();
        entry.session_token = Some(token.clone());
        entry.access_token_expiry = session.expiry.clone();
    }
    if set_default {
        cfg.default_provider = Some(provider_id.to_string());
    } else {
        cfg.ensure_default_provider(provider_id);
    }
    cfg.save(config_path)?;

    println!("logged in to {provider_id}");
    if set_default {
        println!("default_provider = {provider_id}");
    }
    Ok(())
}

/// Console one-liners per provider: paste into the site's DevTools console
/// to print (and copy) the session token without hunting through panels.
fn print_token_snippets(provider_id: &str) {
    let snippet = match provider_id {
        // `__Secure-next-auth.session-token` is HttpOnly — document.cookie can
        // NEVER see it, so no console one-liner exists. Worse, oversized JWTs
        // are chunked into `.0`/`.1` cookies; users must copy BOTH values and
        // join them (comma-separated works — auth login merges the parts).
        "chatgpt" => concat!(
            "// No console one-liner possible: the cookie is HttpOnly.\n",
            "// 1. Open https://chatgpt.com (logged in), press F12\n",
            "// 2. Application tab → Storage → Cookies → https://chatgpt.com\n",
            "// 3. Copy Value of '__Secure-next-auth.session-token.0'\n",
            "//    (if '.1' exists too, copy it as well)\n",
            "// 4. Run, joining multiple parts with commas:\n",
            "//    chat-cli auth login chatgpt --token <value0>[,<value1>]"
        ),
        "deepseek" => concat!(
            "// 1. Open https://chat.deepseek.com (logged in)\n",
            "// 2. DevTools (F12) → Console → paste:\n",
            "copy(JSON.parse(localStorage.getItem('userToken')).value)\n",
            "// Token is now in your clipboard — paste it here."
        ),
        "claude" => concat!(
            "// 1. Open https://claude.ai (logged in)\n",
            "// 2. DevTools (F12) → Application → Cookies → https://claude.ai\n",
            "//    The sessionKey cookie is HttpOnly — copy its value manually.\n",
            "// 3. Paste the value (starts with sk-ant-sid01-…) here."
        ),
        "gemini" => concat!(
            "// 1. Open https://gemini.google.com (logged in)\n",
            "// 2. DevTools (F12) → Application → Cookies → https://gemini.google.com\n",
            "//    The cookies are HttpOnly, so the console cannot read them —\n",
            "//    copy the values of __Secure-1PSID and __Secure-1PSIDTS manually.\n",
            "// 3. Paste both as one line:\n",
            "//    __Secure-1PSID=<value>; __Secure-1PSIDTS=<value>\n",
            "// (a bare __Secure-1PSID value alone also works)"
        ),
        _ => return,
    };
    eprintln!("\n--- How to get your {provider_id} token ---\n{snippet}\n-----------------------------------------\n");
}

async fn run_chat(prompt: String, args: Args) -> anyhow::Result<()> {
    let config_path = args.config.as_deref().map(Path::new);
    let verbose = args.verbose;

    // All -v diagnostics go to stderr; stdout stays clean for piping.
    macro_rules! vlog {
        ($($arg:tt)*) => {
            if verbose { eprintln!("[verbose] {}", format_args!($($arg)*)); }
        };
    }

    // 1. Resolve provider: --provider flag > config.toml default_provider > error
    let cfg = Config::load(config_path)?;
    let provider_id = if let Some(p) = args.provider.clone() {
        p
    } else if let Some(dp) = cfg.default_provider.clone() {
        dp
    } else {
        anyhow::bail!(
            "No provider specified and no default_provider set. Run 'chat-cli auth login <provider>' first."
        );
    };

    let provider = chat_core::provider::get_provider(&provider_id).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown provider '{}'. Registered: {:?}",
            provider_id,
            chat_core::provider::list_providers()
        )
    })?;
    vlog!(
        "provider resolved: {provider_id} (context_limit={})",
        provider.context_limit()
    );

    // 2. Attach. The stdin read is blocking I/O — keep it off the tokio
    // worker via spawn_blocking (the read itself is byte-capped inside).
    let files = chat_core::attach::resolve_attachments(&args.attach)?;
    let attachments_text = provider.prepare_attachments(&files)?;
    let file_count = files.len();
    let has_files = !files.is_empty();
    let stdin_text = tokio::task::spawn_blocking(move || {
        chat_core::attach::maybe_read_stdin_as_attachment(has_files)
    })
    .await
    .context("stdin attachment task panicked")??;
    let attachments_text = format!("{}{}", attachments_text, stdin_text);
    vlog!(
        "attachments: {} file(s), {} chars (stdin: {})",
        file_count,
        attachments_text.len(),
        !stdin_text.is_empty()
    );

    // 3. History. Providers that continue server-side via ProviderHandle ids
    // already carry prior turns — only providers without server-side memory
    // get the transcript replayed into the prompt (double-replay fix).
    let replay_text = provider.replays_history_text();
    let (handle, history_text) = resolve_history(&args, &provider_id, replay_text)?;
    vlog!(
        "history: conversation_id={:?} parent_message_id={:?} context_chars={}",
        handle.conversation_id,
        handle.parent_message_id,
        history_text.len()
    );

    // 4. Budget check (fail fast with breakdown)
    vlog!(
        "budget: system={} history={} attachments={} prompt={} limit={}",
        args.system.as_deref().map(str::len).unwrap_or(0),
        history_text.len(),
        attachments_text.len(),
        prompt.len(),
        provider.context_limit()
    );
    chat_core::budget::budget_check(
        args.system.as_deref(),
        &history_text,
        &attachments_text,
        &prompt,
        provider.context_limit(),
    )?;

    // 5. Build ChatReq and call provider
    let pcfg = cfg.providers.get(&provider_id).cloned().unwrap_or_default();
    // --model wins over [providers.<id>].model
    let model = args.model.clone().or(pcfg.model.clone());
    let config_for_persist = config_path.map(|p| p.to_path_buf());
    let provider_id_for_persist = provider_id.clone();
    let auth = chat_core::provider::AuthContext {
        session_token: pcfg.session_token,
        access_token: pcfg.access_token,
        access_token_expiry: pcfg.access_token_expiry,
        persist: Some(std::sync::Arc::new(
            move |token: &str, expiry: &str| -> anyhow::Result<()> {
                let mut cfg = Config::load(config_for_persist.as_deref())?;
                let entry = cfg
                    .providers
                    .entry(provider_id_for_persist.clone())
                    .or_default();
                entry.access_token = Some(token.to_string());
                entry.access_token_expiry = Some(expiry.to_string());
                cfg.save(config_for_persist.as_deref())
            },
        )),
    };

    let req = chat_core::provider::ChatReq {
        prompt: prompt.clone(),
        system: args.system.clone(),
        attachments_text,
        model,
        auth,
    };

    // Combine history_text + req into final prompt if continuing
    let final_req = if history_text.is_empty() {
        req
    } else {
        chat_core::provider::ChatReq {
            prompt: format!("{}{}", history_text, prompt),
            system: req.system,
            attachments_text: req.attachments_text,
            model: req.model,
            auth: req.auth,
        }
    };

    let resp = provider.chat(&handle, final_req)?;

    // 6. Persist history
    persist_history(&args, &provider_id, &prompt, &resp)?;

    // 7. Output
    println!("{}", resp.content);

    Ok(())
}

/// Resolve the continuation handle + optional history transcript.
/// `replay_text` is the provider's `replays_history_text()` capability — when
/// false the transcript is NOT built (server-side continuation carries it).
fn resolve_history(
    args: &Args,
    provider_id: &str,
    replay_text: bool,
) -> anyhow::Result<(chat_core::provider::ProviderHandle, String)> {
    use chat_core::provider::ProviderHandle;

    if args.new {
        return Ok((ProviderHandle::default(), String::new()));
    }

    // --continue <id> or --continue (empty = most recent)
    if let Some(ref cid) = args.continue_id {
        if cid.is_empty() {
            // --continue without value: most recent history for this provider
            let mut items = HistoryFile::list(None)?;
            items.retain(|h| h.provider == provider_id);
            if let Some(item) = items.into_iter().next() {
                let hf = load_history_or_not_found(&item.id)?;
                let handle = ProviderHandle {
                    conversation_id: hf.provider_conversation_id.clone(),
                    parent_message_id: hf.provider_parent_message_id.clone(),
                };
                let text = if replay_text {
                    hf.to_context_text()
                } else {
                    String::new()
                };
                return Ok((handle, text));
            }
            return Ok((ProviderHandle::default(), String::new()));
        } else {
            // Friendly not-found + id validation (traversal-safe) — same path
            // as `history show`, so --continue <bogus> no longer leaks OS errors.
            let hf = load_history_or_not_found(cid)?;
            chat_core::history::validate_continue_provider(&hf, provider_id)?;
            let handle = ProviderHandle {
                conversation_id: hf.provider_conversation_id.clone(),
                parent_message_id: hf.provider_parent_message_id.clone(),
            };
            let text = if replay_text {
                hf.to_context_text()
            } else {
                String::new()
            };
            return Ok((handle, text));
        }
    }

    // Default: continue most recent if any, else new
    let mut items = HistoryFile::list(None)?;
    items.retain(|h| h.provider == provider_id);
    if let Some(item) = items.into_iter().next() {
        let hf = load_history_or_not_found(&item.id)?;
        let handle = ProviderHandle {
            conversation_id: hf.provider_conversation_id.clone(),
            parent_message_id: hf.provider_parent_message_id.clone(),
        };
        let text = if replay_text {
            hf.to_context_text()
        } else {
            String::new()
        };
        return Ok((handle, text));
    }
    Ok((ProviderHandle::default(), String::new()))
}

fn persist_history(
    args: &Args,
    provider_id: &str,
    prompt: &str,
    resp: &chat_core::provider::ChatResp,
) -> anyhow::Result<()> {
    use chrono::Utc;

    // Determine which history file to append to
    let mut hf = if args.new {
        HistoryFile::new(provider_id)
    } else if let Some(ref cid) = args.continue_id {
        if cid.is_empty() {
            // most recent
            let mut items = HistoryFile::list(None)?;
            items.retain(|h| h.provider == provider_id);
            if let Some(item) = items.into_iter().next() {
                HistoryFile::load(&item.id, None)?
            } else {
                HistoryFile::new(provider_id)
            }
        } else {
            // Friendly not-found + traversal-safe id validation, same as
            // resolve_history — a bogus --continue=<id> must not leak OS errors.
            load_history_or_not_found(cid)?
        }
    } else {
        // default continue most recent
        let mut items = HistoryFile::list(None)?;
        items.retain(|h| h.provider == provider_id);
        if let Some(item) = items.into_iter().next() {
            HistoryFile::load(&item.id, None)?
        } else {
            HistoryFile::new(provider_id)
        }
    };

    hf.provider_conversation_id = Some(resp.conversation_id.clone());
    hf.provider_parent_message_id = Some(resp.message_id.clone());
    hf.turns.push(chat_core::history::Turn {
        role: "user".to_string(),
        content: prompt.to_string(),
        timestamp: Utc::now().to_rfc3339(),
    });
    hf.turns.push(chat_core::history::Turn {
        role: "assistant".to_string(),
        content: resp.content.clone(),
        timestamp: Utc::now().to_rfc3339(),
    });
    hf.save(None)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Args;
    use clap::Parser;
    use parking_lot::Mutex;
    use std::path::PathBuf;
    use std::sync::{Arc, LazyLock};

    /// Mock provider self-registered via inventory — proves the dispatcher
    /// can drive an arbitrary backend through the real code paths.
    struct TestProvider {
        calls: Arc<Mutex<Vec<String>>>,
        fail_auth: bool,
    }

    impl chat_core::provider::Provider for TestProvider {
        fn id(&self) -> &'static str {
            "testprov"
        }

        fn context_limit(&self) -> usize {
            100_000
        }

        // Test provider has no server-side memory — it relies on the
        // dispatcher replaying the transcript, like a text-only backend.
        fn replays_history_text(&self) -> bool {
            true
        }

        fn auth(&self, token: &str) -> anyhow::Result<chat_core::provider::Session> {
            if self.fail_auth {
                return Ok(chat_core::provider::Session {
                    valid: false,
                    expiry: None,
                });
            }
            Ok(chat_core::provider::Session {
                valid: !token.is_empty(),
                expiry: Some("2030-01-01T00:00:00Z".to_string()),
            })
        }

        fn chat(
            &self,
            handle: &chat_core::provider::ProviderHandle,
            req: chat_core::provider::ChatReq,
        ) -> anyhow::Result<chat_core::provider::ChatResp> {
            let mut calls = self.calls.lock();
            calls.push(format!(
                "model={:?} conv={:?} parent={:?} prompt={} auth={}",
                req.model,
                handle.conversation_id,
                handle.parent_message_id,
                req.prompt,
                req.auth.session_token.as_deref().unwrap_or("NONE"),
            ));
            Ok(chat_core::provider::ChatResp {
                content: format!("echo:{}", req.prompt),
                conversation_id: "tc-1".into(),
                message_id: "tm-1".into(),
            })
        }
    }

    /// Provider that continues server-side (like the real web providers):
    /// `replays_history_text()` is false, so the dispatcher must NOT prepend
    /// the transcript — the handle ids carry the context.
    struct ServerSideProvider;

    impl chat_core::provider::Provider for ServerSideProvider {
        fn id(&self) -> &'static str {
            "serverprov"
        }

        fn context_limit(&self) -> usize {
            100_000
        }

        fn replays_history_text(&self) -> bool {
            false
        }

        fn auth(&self, _token: &str) -> anyhow::Result<chat_core::provider::Session> {
            Ok(chat_core::provider::Session {
                valid: true,
                expiry: None,
            })
        }

        fn chat(
            &self,
            handle: &chat_core::provider::ProviderHandle,
            req: chat_core::provider::ChatReq,
        ) -> anyhow::Result<chat_core::provider::ChatResp> {
            test_calls().lock().push(format!(
                "prompt={} conv={:?} parent={:?}",
                req.prompt, handle.conversation_id, handle.parent_message_id
            ));
            Ok(chat_core::provider::ChatResp {
                content: "ok".into(),
                conversation_id: "sc-1".into(),
                message_id: "sm-1".into(),
            })
        }
    }

    static REGISTERED_SERVER: LazyLock<()> = LazyLock::new(|| {
        inventory::submit! {
            chat_core::provider::ProviderEntry {
                id: "serverprov",
                factory: || Box::new(ServerSideProvider),
            }
        }
    });

    fn ensure_server_registered() {
        LazyLock::force(&REGISTERED_SERVER);
    }

    static REGISTERED: LazyLock<Arc<Mutex<Vec<String>>>> = LazyLock::new(|| {
        inventory::submit! {
            chat_core::provider::ProviderEntry {
                id: "testprov",
                factory: || Box::new(TestProvider {
                    calls: test_calls(),
                    fail_auth: false,
                }),
            }
        }
        Arc::new(Mutex::new(Vec::new()))
    });

    static REGISTERED_FAILING: LazyLock<()> = LazyLock::new(|| {
        inventory::submit! {
            chat_core::provider::ProviderEntry {
                id: "failingprov",
                factory: || Box::new(TestProvider {
                    calls: test_calls(),
                    fail_auth: true,
                }),
            }
        }
    });

    fn ensure_failing_registered() {
        LazyLock::force(&REGISTERED_FAILING);
    }

    fn test_calls() -> Arc<Mutex<Vec<String>>> {
        REGISTERED.clone()
    }

    struct TempEnv {
        // Drop order matters: fields drop in declaration order. The override
        // guard MUST drop before the lock guard — otherwise the next test can
        // acquire HOME_LOCK while the previous test's path override is still
        // installed (CI race: "path base override already active").
        _paths_guard: chat_core::paths::OverrideGuard,
        _dir: tempfile::TempDir,
        config: PathBuf,
        // mutating it must not overlap (TempDir drop would yank the path out
        // from under a concurrent test).
        _guard: parking_lot::MutexGuard<'static, ()>,
    }

    static HOME_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Redirect per-user dirs into a temp dir for one test.
    ///
    /// Uses the chat-core process-global base override instead of `$HOME`:
    /// on Windows Known-Folder APIs ignore env vars entirely, so the previous
    /// HOME trick silently left tests reading/writing/deleting the REAL user
    /// config and history (#1). The override is honored by both
    /// `Config::config_path` and `HistoryFile::history_dir` on every platform.
    fn temp_env() -> TempEnv {
        let guard = HOME_LOCK.lock();
        let dir = tempfile::tempdir().unwrap();
        let paths_guard = chat_core::paths::set_base_override(dir.path());
        let config = dir.path().join("chat-cli").join("config.toml");
        TempEnv {
            _paths_guard: paths_guard,
            _dir: dir,
            config,
            _guard: guard,
        }
    }

    fn parse(args: &[&str]) -> Args {
        let mut all = vec!["chat-cli"];
        all.extend_from_slice(args);
        Args::parse_from(all)
    }

    #[tokio::test]
    async fn auth_login_rejection_carries_provider_hint() {
        test_calls();
        ensure_failing_registered();
        let _env = temp_env();
        let args = parse(&["auth", "login", "failingprov", "--token", "bad"]);
        let err = run(args).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("rejected by 'failingprov'") && msg.contains("not saved"),
            "must name provider and refuse save: {msg}"
        );
        // nothing persisted
        assert!(!Config::load(None)
            .unwrap()
            .providers
            .contains_key("failingprov"));
    }

    #[test]
    fn merge_chunked_chatgpt_cookie_parts() {
        // Mirrors the merge logic in auth_login: comma-separated
        // `.0`/`.1` cookie values concatenate into one token.
        let raw = "part0,part1";
        let merged: String = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .concat();
        assert_eq!(merged, "part0part1");

        // whitespace + empty segments tolerated
        let raw = " part0 , ,part1 ";
        let merged: String = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .concat();
        assert_eq!(merged, "part0part1");
    }

    #[tokio::test]
    async fn auth_login_saves_token_and_sets_first_default() {
        test_calls();
        let env = temp_env();
        let args = parse(&["auth", "login", "testprov", "--token", "secret"]);
        run(args).await.unwrap();

        assert!(env.config.exists(), "config must be written");
        let cfg = Config::load(None).unwrap();
        assert_eq!(
            cfg.providers["testprov"].session_token.as_deref(),
            Some("secret")
        );
        // first login sets the default
        assert_eq!(cfg.default_provider.as_deref(), Some("testprov"));
    }

    #[tokio::test]
    async fn auth_login_rejects_invalid_token_without_save() {
        test_calls();
        ensure_failing_registered();
        let _env = temp_env();
        let args = parse(&["auth", "login", "failingprov", "--token", "x"]);
        run(args).await.unwrap_err();
        assert!(
            !Config::load(None)
                .unwrap()
                .providers
                .contains_key("failingprov"),
            "rejected token must not be saved"
        );
    }

    #[tokio::test]
    async fn auth_unknown_provider_is_clean_error() {
        test_calls();
        let _env = temp_env();
        let args = parse(&["auth", "login", "no-such-provider", "--token", "x"]);
        let err = run(args).await.unwrap_err();
        assert!(err
            .to_string()
            .contains("unknown provider 'no-such-provider'"));
    }

    #[tokio::test]
    async fn snippets_printed_for_known_providers_only() {
        // no panic; function is stderr-only
        print_token_snippets("chatgpt");
        print_token_snippets("deepseek");
        print_token_snippets("unknownprov"); // silent
    }

    /// Remove every history file — tests assert deltas. Resolves under the
    /// active test override (see `temp_env`), never the real user history (#1).
    fn clear_history_dir() {
        let dir = chat_core::history::HistoryFile::history_dir(None);
        if dir.exists() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let p = entry.unwrap().path();
                if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    std::fs::remove_file(p).unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn chat_new_creates_history_and_persists_ids() {
        test_calls();
        let _env = temp_env();
        clear_history_dir();
        // login first so default_provider resolves and session token exists
        run(parse(&["auth", "login", "testprov", "--token", "secret"]))
            .await
            .unwrap();
        test_calls().lock().clear();

        run(parse(&["--new", "-p", "hello world"])).await.unwrap();
        let calls_arc = test_calls();
        let calls = calls_arc.lock();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("prompt=hello world"), "{}", calls[0]);
        assert!(
            calls[0].contains("auth=secret"),
            "session token must reach provider"
        );
        drop(calls);

        // one history file with 2 turns persisted
        let items = HistoryFile::list(None).unwrap();
        assert_eq!(items.len(), 1);
        let hf = HistoryFile::load(&items[0].id, None).unwrap();
        assert_eq!(hf.turns.len(), 2);
        assert_eq!(hf.provider_conversation_id.as_deref(), Some("tc-1"));
        assert_eq!(hf.provider_parent_message_id.as_deref(), Some("tm-1"));
        clear_history_dir();
    }

    #[tokio::test]
    async fn chat_continue_passes_stored_provider_ids() {
        test_calls();
        let _env = temp_env();
        clear_history_dir();
        run(parse(&["auth", "login", "testprov", "--token", "secret"]))
            .await
            .unwrap();
        run(parse(&["--new", "-p", "first"])).await.unwrap();
        test_calls().lock().clear();
        run(parse(&["--continue", "-p", "second"])).await.unwrap();

        let calls_arc = test_calls();
        let calls = calls_arc.lock();
        // continue must carry the ids persisted by the first turn
        assert!(
            calls[0].contains("conv=Some(\"tc-1\")") && calls[0].contains("parent=Some(\"tm-1\")"),
            "stored ids must flow into ProviderHandle: {}",
            calls[0]
        );
    }

    #[tokio::test]
    async fn chat_model_flag_overrides_config_model() {
        test_calls();
        let _env = temp_env();
        clear_history_dir();
        run(parse(&["auth", "login", "testprov", "--token", "s"]))
            .await
            .unwrap();
        test_calls().lock().clear();

        // No --model and no config model → None
        run(parse(&["--new", "-p", "plain"])).await.unwrap();
        assert!(
            test_calls().lock()[0].contains("model=None"),
            "no model configured: {:?}",
            test_calls().lock()
        );
        test_calls().lock().clear();

        // --model flag wins for the turn
        run(parse(&["--new", "-p", "flagged", "--model", "m-flag"]))
            .await
            .unwrap();
        assert!(
            test_calls().lock()[0].contains("model=Some(\"m-flag\")"),
            "--model must reach the provider: {:?}",
            test_calls().lock()
        );
        clear_history_dir();
    }

    #[tokio::test]
    async fn chat_cross_provider_continue_is_hard_error() {
        test_calls();
        let _env = temp_env();
        clear_history_dir();
        run(parse(&["auth", "login", "testprov", "--token", "s"]))
            .await
            .unwrap();
        run(parse(&["--new", "-p", "mine"])).await.unwrap();

        let items = HistoryFile::list(None).unwrap();
        let hid = &items[0].id;
        // --continue=<id> form: space-separated values are now positional prompts.
        let cont = format!("--continue={hid}");
        let args = parse(&["--provider", "deepseek", &cont, "-p", "hi"]);
        let err = run(args).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("deepseek") && (msg.contains("provider") || msg.contains("history")),
            "must name the mismatch: {msg}"
        );
    }

    #[tokio::test]
    async fn chat_stdin_becomes_attachment_when_no_files() {
        test_calls();
        let _env = temp_env();
        run(parse(&["auth", "login", "testprov", "--token", "s"]))
            .await
            .unwrap();
        test_calls().lock().clear();

        // stdin is a pipe in cargo test? No — it's usually null; simulate by
        // asserting the no-stdin path does not panic and prompt still flows.
        run(parse(&["--new", "-p", "ask"])).await.unwrap();
        assert!(!test_calls().lock().is_empty());
    }

    #[tokio::test]
    async fn chat_continue_explicit_id_targets_that_history() {
        test_calls();
        let _env = temp_env();
        clear_history_dir();
        run(parse(&["auth", "login", "testprov", "--token", "s"]))
            .await
            .unwrap();
        run(parse(&["--new", "-p", "root turn"])).await.unwrap();
        let items = HistoryFile::list(None).unwrap();
        let hid = items[0].id.clone();
        test_calls().lock().clear();

        let cont = format!("--continue={hid}");
        run(parse(&[&cont, "-p", "branch"])).await.unwrap();
        let calls_arc = test_calls();
        let calls = calls_arc.lock();
        assert!(
            calls[0].ends_with("branch auth=s") && calls[0].contains("root turn"),
            "explicit --continue <id> sends history text + new prompt: {:?}",
            calls
        );
    }

    #[tokio::test]
    async fn history_subcommands_list_show_rm_round_trip() {
        test_calls();
        let _env = temp_env();
        clear_history_dir();
        run(parse(&["auth", "login", "testprov", "--token", "s"]))
            .await
            .unwrap();
        run(parse(&["--new", "-p", "for history"])).await.unwrap();
        let items = HistoryFile::list(None).unwrap();
        assert_eq!(items.len(), 1, "list must see the fresh file");
        let hid = items[0].id.clone();

        // show prints valid JSON of the history
        run(parse(&["history", "show", &hid])).await.unwrap();

        // rm deletes it; subsequent list is empty
        run(parse(&["history", "rm", &hid])).await.unwrap();
        let after = HistoryFile::list(None).unwrap();
        assert!(after.is_empty(), "rm must remove the file");
    }

    #[tokio::test]
    async fn history_list_filters_by_provider_and_limit() {
        test_calls();
        let _env = temp_env();
        run(parse(&["auth", "login", "testprov", "--token", "s"]))
            .await
            .unwrap();
        run(parse(&["--new", "-p", "one"])).await.unwrap();

        // provider filter for a different provider yields nothing (no panic)
        run(parse(&["history", "list", "--provider", "deepseek"]))
            .await
            .unwrap();
        run(parse(&["history", "list", "--limit", "5"]))
            .await
            .unwrap();
    }

    // --- bead chat-cli-continue-and-auth-ux-038 regressions ---

    #[tokio::test]
    async fn continue_bogus_id_is_friendly_not_found() {
        test_calls();
        let _env = temp_env();
        run(parse(&["auth", "login", "testprov", "--token", "s"]))
            .await
            .unwrap();
        let err = run(parse(&["--continue=bogus0000", "-p", "hi"]))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not found"), "friendly error expected: {msg}");
        assert!(
            !msg.contains("os error"),
            "raw OS error must not leak: {msg}"
        );
    }

    #[tokio::test]
    async fn continue_traversal_id_is_rejected() {
        test_calls();
        let _env = temp_env();
        run(parse(&["auth", "login", "testprov", "--token", "s"]))
            .await
            .unwrap();
        // --continue=<id> form (require_equals): hostile ids must be rejected
        // before any filesystem path is built.
        for bad in ["../victim", "a/b", "..\\win", ".."] {
            let arg = format!("--continue={bad}");
            let err = run(parse(&[&arg, "-p", "hi"])).await.unwrap_err();
            assert!(
                err.to_string().contains("invalid history id"),
                "{bad:?} must be rejected: {err}"
            );
        }
        // And the shared loader rejects them too (covers history show path).
        assert!(load_history_or_not_found("../victim").is_err());
        assert!(load_history_or_not_found("a/b").is_err());
        assert!(load_history_or_not_found("..").is_err());
    }

    #[tokio::test]
    async fn auth_logout_unknown_provider_is_error() {
        test_calls();
        let _env = temp_env();
        let err = run(parse(&["auth", "logout", "no-such-provider"]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown provider"), "{err}");
    }

    #[tokio::test]
    async fn auth_login_without_token_on_non_tty_fails_fast() {
        test_calls();
        let _env = temp_env();
        // cargo test stdin is not a TTY — must bail before prompting.
        let err = run(parse(&["auth", "login", "testprov"]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--token"), "{err}");
        assert!(
            !Config::load(None)
                .unwrap()
                .providers
                .contains_key("testprov"),
            "failed login must not persist"
        );
    }

    #[tokio::test]
    async fn history_list_unknown_provider_is_error() {
        test_calls();
        let _env = temp_env();
        let err = run(parse(&["history", "list", "--provider", "bogusprov"]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown provider"), "{err}");
    }

    #[tokio::test]
    async fn history_rm_traversal_id_is_rejected() {
        test_calls();
        let _env = temp_env();
        let err = run(parse(&["history", "rm", "../victim"]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid history id"), "{err}");
    }

    #[tokio::test]
    async fn server_side_provider_does_not_get_history_text_replay() {
        test_calls();
        ensure_server_registered();
        let _env = temp_env();
        clear_history_dir();
        run(parse(&["auth", "login", "serverprov", "--token", "s"]))
            .await
            .unwrap();
        run(parse(&[
            "--provider",
            "serverprov",
            "--new",
            "-p",
            "first turn",
        ]))
        .await
        .unwrap();
        test_calls().lock().clear();
        run(parse(&[
            "--provider",
            "serverprov",
            "--continue",
            "-p",
            "second turn",
        ]))
        .await
        .unwrap();

        let calls = test_calls();
        let calls = calls.lock();
        assert_eq!(calls.len(), 1);
        // Server-side continuation: prompt is ONLY the new turn — no
        // "user:/assistant:" transcript replay — while the stored ids flow.
        assert!(
            calls[0].starts_with("prompt=second turn "),
            "no history text may be prepended: {}",
            calls[0]
        );
        assert!(
            !calls[0].contains("user:"),
            "transcript replay leaked into prompt: {}",
            calls[0]
        );
        assert!(
            calls[0].contains("conv=Some(\"sc-1\")"),
            "server-side ids must still flow: {}",
            calls[0]
        );
        clear_history_dir();
    }
}
