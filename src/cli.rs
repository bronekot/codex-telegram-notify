use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "codex-telegram-notify",
    version,
    about = "Send Codex Stop hook notifications to Telegram"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Interactively configure the Telegram bot and chat.
    Setup,
    /// Send a test notification using the saved configuration.
    Test,
    /// Inspect or change the saved configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Record sanitized SubagentStop metadata for hook diagnostics.
    #[command(hide = true)]
    ProbeSubagent,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Show all effective configuration values.
    Show,
    /// Show one effective configuration value.
    Get { key: String },
    /// Set one persisted configuration value.
    Set {
        key: String,
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Remove the persisted configuration file.
    Reset,
}
