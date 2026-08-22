//! Dispatch: resolve provider → attach → budget → history → chat.

use std::path::Path;

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
            AuthCmd::Login { provider, token } => {
                // TODO: interactive hidden input if token is None, validate via Provider::auth
                let _ = provider;
                let _ = token;
                anyhow::bail!("auth login not yet implemented")
            }
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
    let req = chat_core::provider::ChatReq {
        prompt: prompt.clone(),
        system: args.system.clone(),
        attachments_text,
    };

    // Combine history_text + req into final prompt if continuing
    let final_req = if history_text.is_empty() {
        req
    } else {
        chat_core::provider::ChatReq {
            prompt: format!("{}{}", history_text, prompt),
            system: args.system.clone(),
            attachments_text: req.attachments_text,
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
