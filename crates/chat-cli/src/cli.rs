//! CLI surface — clap definitions.
//!
//! ```text
//! chat-cli auth login <chatgpt|deepseek> [--token ...]
//! chat-cli auth status | logout <provider>
//! chat-cli -p "prompt" [--provider x] [-s sys] [-a ...] [--new|--continue[<id>]]
//! chat-cli history list/show/rm
//! ```

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
