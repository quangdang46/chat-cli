//! CLI surface — clap definitions.
//!
//! ```text
//! chat-cli auth login <chatgpt|deepseek> [--token ...]
//! chat-cli auth status | logout <provider>
//! chat-cli -p "prompt" [--provider x] [-s sys] [-a ...] [--new|--continue[<id>]]
//! chat-cli history list/show/rm
//! ```

#[cfg(test)]
use clap::CommandFactory;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "chat-cli", about = "Chat with ChatGPT/DeepSeek web via CLI")]
pub struct Args {
    /// Provider override (chatgpt | deepseek)
    #[arg(long, global = true)]
    pub provider: Option<String>,

    /// Override config path
    #[arg(long, global = true)]
    pub config: Option<String>,

    /// Verbose output
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Option<Command>,

    /// Prompt text (positional alias for -p)
    #[arg(value_name = "PROMPT")]
    pub prompt_positional: Option<String>,

    /// Prompt text
    #[arg(short = 'p', long)]
    pub prompt: Option<String>,

    /// System prompt
    #[arg(short = 's', long)]
    pub system: Option<String>,

    /// Attach files: repeatable, comma-separated, glob, @list.txt
    #[arg(short = 'a', long = "attach", value_name = "FILE")]
    pub attach: Vec<String>,

    /// Force new conversation
    #[arg(long = "new", conflicts_with = "continue_id")]
    pub new: bool,

    /// Continue conversation (optionally by id)
    #[arg(long = "continue", value_name = "ID", num_args = 0..=1, default_missing_value = "")]
    pub continue_id: Option<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    History {
        #[command(subcommand)]
        cmd: HistoryCmd,
    },
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum AuthCmd {
    /// Login to a provider (interactive or --token)
    Login {
        provider: String,
        #[arg(long)]
        token: Option<String>,
    },
    /// Show auth status
    Status,
    /// Logout from a provider
    Logout { provider: String },
}

#[derive(Subcommand, Debug, Clone)]
pub enum HistoryCmd {
    /// List local history
    List {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Show a history entry
    Show { id: String },
    /// Remove a history entry
    Rm { id: String },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ConfigCmd {
    /// Set a config value (currently: default_provider)
    Set { key: String, value: String },
    /// Get a config value
    Get { key: String },
}

impl Args {
    pub fn parse_args() -> Self {
        Self::parse()
    }

    /// Effective prompt: -p takes precedence over positional.
    pub fn effective_prompt(&self) -> Option<&str> {
        self.prompt.as_deref().or(self.prompt_positional.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        let mut all = vec!["chat-cli"];
        all.extend_from_slice(args);
        Args::parse_from(all)
    }

    #[test]
    fn effective_prompt_p_flag_wins_over_positional() {
        let args = parse(&["-p", "from-flag", "from-positional"]);
        assert_eq!(args.effective_prompt(), Some("from-flag"));

        let args = parse(&["only-positional"]);
        assert_eq!(args.effective_prompt(), Some("only-positional"));

        let args = parse(&["--new"]);
        assert_eq!(args.effective_prompt(), None);
    }

    #[test]
    fn new_and_continue_conflict() {
        let result = Args::try_parse_from(["chat-cli", "--new", "--continue", "abc", "-p", "x"]);
        assert!(result.is_err(), "--new + --continue must be rejected");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("cannot be used with") || err.contains("conflict"),
            "clap conflict error expected: {err}"
        );
    }

    #[test]
    fn continue_without_value_defaults_to_empty_string() {
        let args = parse(&["--continue", "-p", "x"]);
        assert_eq!(args.continue_id.as_deref(), Some(""));

        let args = parse(&["--continue", "explicit-id", "-p", "x"]);
        assert_eq!(args.continue_id.as_deref(), Some("explicit-id"));
    }

    /// Help snapshot — frozen for POC; README examples depend on these flags.
    #[test]
    fn help_snapshot_lists_frozen_surface() {
        let help = Args::command().render_help().to_string();
        for flag in [
            "--provider",
            "--config",
            "--verbose",
            "--prompt",
            "--system",
            "--attach",
            "--new",
            "--continue",
        ] {
            assert!(help.contains(flag), "help must document {flag}:\n{help}");
        }
        for sub in ["auth", "history", "config"] {
            assert!(
                help.contains(sub),
                "help must list '{sub}' subcommand:\n{help}"
            );
        }
    }
}
