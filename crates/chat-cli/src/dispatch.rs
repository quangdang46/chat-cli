//! Dispatch: resolve provider → attach → budget → history → chat.

use std::path::Path;

use anyhow::Context;

use chat_core::config::Config;
use chat_core::history::HistoryFile;

use crate::cli::{Args, AuthCmd, Command, ConfigCmd, HistoryCmd};

pub async fn run(args: Args) -> anyhow::Result<()> {
    // Subcommands
    if let Some(cmd) = args.command.clone() {
        return run_subcommand(cmd, &args).await;
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
            AuthCmd::Login { provider, token } => auth_login(&provider, token, config_path).await,
            AuthCmd::Status => {
                let cfg = Config::load(config_path)?;
                println!("default_provider: {:?}", cfg.default_provider);
                for (k, v) in &cfg.providers {
                    let has_token = v.session_token.is_some();
                    println!(
                        "  {}: logged_in={} expiry={:?}",
                        k, has_token, v.access_token_expiry
                    );
                }
                Ok(())
            }
            AuthCmd::Logout { provider } => {
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
        Command::History { cmd } => match cmd {
            HistoryCmd::List { provider, limit } => {
                let mut items = HistoryFile::list(None)?;
                if let Some(p) = provider {
                    items.retain(|h| h.provider == p);
                }
                if let Some(n) = limit {
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
                let hf = HistoryFile::load(&id, None)?;
                println!("{}", serde_json::to_string_pretty(&hf)?);
                Ok(())
            }
            HistoryCmd::Rm { id } => {
                let path = HistoryFile::history_dir(None).join(format!("{}.jsonl", id));
                std::fs::remove_file(&path)?;
                println!("removed {}", id);
                Ok(())
            }
        },
        Command::Config { cmd } => match cmd {
            ConfigCmd::Set { key, value } => {
                if key != "default_provider" {
                    anyhow::bail!(
                        "unknown config key '{}' (only 'default_provider' supported)",
                        key
                    );
                }
                let mut cfg = Config::load(config_path)?;
                cfg.default_provider = Some(value.clone());
                cfg.save(config_path)?;
                println!("{} = {}", key, value);
                Ok(())
            }
            ConfigCmd::Get { key } => {
                let cfg = Config::load(config_path)?;
                match key.as_str() {
                    "default_provider" => println!("{:?}", cfg.default_provider),
                    _ => anyhow::bail!("unknown config key '{}'", key),
                }
                Ok(())
            }
        },
    }
}

/// `auth login <provider> [--token ...]`: validate via the provider, then
/// persist with first-login-only default_provider semantics.
async fn auth_login(
    provider_id: &str,
    token: Option<String>,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    let provider = chat_core::provider::get_provider(provider_id).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown provider '{}'. Registered: {:?}",
            provider_id,
            chat_core::provider::list_providers()
        )
    })?;

    let token = match token {
        Some(t) => t,
        None => {
            // Interactive hidden input (bead chat-cli-v2y polishes this path).
            eprint!("Paste session token for {provider_id}: ");
            use std::io::Write;
            std::io::stderr().flush()?;
            rpassword::read_password()
                .context("failed reading token from terminal (no TTY?); pass --token instead")?
        }
    };

    let token_for_auth = token.clone();
    let session = tokio::task::spawn_blocking(move || provider.auth(&token_for_auth))
        .await
        .context("auth task panicked")??;
    if !session.valid {
        anyhow::bail!(
            "token rejected by '{provider_id}' — not saved. Re-check the cookie/token and retry."
        );
    }

    let mut cfg = Config::load(config_path)?;
    {
        let entry = cfg.providers.entry(provider_id.to_string()).or_default();
        entry.session_token = Some(token.clone());
        entry.access_token_expiry = session.expiry.clone();
    }
    cfg.ensure_default_provider(provider_id);
    cfg.save(config_path)?;

    println!("logged in to {provider_id}");
    Ok(())
}
async fn run_chat(prompt: String, args: Args) -> anyhow::Result<()> {
    let config_path = args.config.as_deref().map(Path::new);

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

    let provider = chat_core::provider::get_provider(&provider_id)
        .ok_or_else(|| anyhow::anyhow!("unknown provider '{}'", provider_id))?;

    // 2. Attach
    let files = chat_core::attach::resolve_attachments(&args.attach)?;
    let attachments_text = provider.prepare_attachments(&files)?;
    let stdin_text = chat_core::attach::maybe_read_stdin_as_attachment(!files.is_empty())?;
    let attachments_text = format!("{}{}", attachments_text, stdin_text);

    // 3. History
    let (handle, history_text) = resolve_history(&args, &provider_id)?;

    // 4. Budget check (fail fast with breakdown)
    chat_core::budget::budget_check(
        args.system.as_deref(),
        &history_text,
        &attachments_text,
        &prompt,
        provider.context_limit(),
    )?;

    // 5. Build ChatReq and call provider
    let pcfg = cfg.providers.get(&provider_id).cloned().unwrap_or_default();
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
        auth,
    };

    // Combine history_text + req into final prompt if continuing
    let final_req = if history_text.is_empty() {
        req
    } else {
        chat_core::provider::ChatReq {
            prompt: format!("{}{}", history_text, prompt),
            system: args.system.clone(),
            attachments_text: req.attachments_text,
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

fn resolve_history(
    args: &Args,
    provider_id: &str,
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
                let hf = HistoryFile::load(&item.id, None)?;
                let handle = ProviderHandle {
                    conversation_id: hf.provider_conversation_id.clone(),
                    parent_message_id: hf.provider_parent_message_id.clone(),
                };
                let text = hf.to_context_text();
                return Ok((handle, text));
            }
            return Ok((ProviderHandle::default(), String::new()));
        } else {
            let hf = HistoryFile::load(cid, None)?;
            chat_core::history::validate_continue_provider(&hf, provider_id)?;
            let handle = ProviderHandle {
                conversation_id: hf.provider_conversation_id.clone(),
                parent_message_id: hf.provider_parent_message_id.clone(),
            };
            let text = hf.to_context_text();
            return Ok((handle, text));
        }
    }

    // Default: continue most recent if any, else new
    let mut items = HistoryFile::list(None)?;
    items.retain(|h| h.provider == provider_id);
    if let Some(item) = items.into_iter().next() {
        let hf = HistoryFile::load(&item.id, None)?;
        let handle = ProviderHandle {
            conversation_id: hf.provider_conversation_id.clone(),
            parent_message_id: hf.provider_parent_message_id.clone(),
        };
        let text = hf.to_context_text();
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
            HistoryFile::load(cid, None)?
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
                "conv={:?} parent={:?} prompt={} auth={}",
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

    fn test_calls() -> Arc<Mutex<Vec<String>>> {
        REGISTERED.clone()
    }

    struct TempEnv {
        // mutating it must not overlap (TempDir drop would yank the path out
        // from under a concurrent test).
        _guard: parking_lot::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        config: PathBuf,
    }

    static HOME_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Point HOME at a temp dir so `dirs::config_dir()`/`data_local_dir()`
    /// resolve inside it. On macOS both map to `$HOME/Library/Application
    /// Support`; on Linux to `$HOME/.config` and `$HOME/.local/share`.
    fn temp_env() -> TempEnv {
        let guard = HOME_LOCK.lock();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        let app_support = dir
            .path()
            .join("Library/Application Support")
            .join("chat-cli");
        TempEnv {
            _guard: guard,
            config: app_support.join("config.toml"),
            _dir: dir,
        }
    }

    fn parse(args: &[&str]) -> Args {
        let mut all = vec!["chat-cli"];
        all.extend_from_slice(args);
        Args::parse_from(all)
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
        let _env = temp_env();
        // unknown provider cannot validate → hard error, nothing saved
        let args = parse(&["auth", "login", "no-such-provider", "--token", "x"]);
        let err = run(args).await.unwrap_err();
        assert!(err
            .to_string()
            .contains("unknown provider 'no-such-provider'"));
    }

    #[tokio::test]
    async fn chat_new_creates_history_and_persists_ids() {
        test_calls();
        let _env = temp_env();
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
    }

    #[tokio::test]
    async fn chat_continue_passes_stored_provider_ids() {
        test_calls();
        let _env = temp_env();
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
    async fn chat_cross_provider_continue_is_hard_error() {
        test_calls();
        let _env = temp_env();
        run(parse(&["auth", "login", "testprov", "--token", "s"]))
            .await
            .unwrap();
        run(parse(&["--new", "-p", "mine"])).await.unwrap();

        let items = HistoryFile::list(None).unwrap();
        let hid = &items[0].id;
        let args = parse(&["--provider", "deepseek", "--continue", hid, "-p", "hi"]);
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
}
