use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "codex-telegram-notify",
    version,
    about = "Send Codex turn and review notifications to Telegram"
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
    /// Install and control the background review watcher.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Install and start the per-user background watcher.
    Install,
    /// Stop and remove the per-user background watcher.
    Uninstall,
    /// Show the per-user background watcher status.
    Status,
    /// Run the watcher process. Used by the installed service.
    #[command(hide = true)]
    Run {
        /// Explicit Codex home captured by `daemon install`.
        #[arg(long, value_name = "PATH")]
        codex_home: Option<PathBuf>,
    },
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
