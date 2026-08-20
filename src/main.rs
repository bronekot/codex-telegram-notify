mod cli;
mod config;
mod error;
mod hook;
mod message;
mod setup;
mod telegram;

use clap::Parser;
use cli::{Cli, Command, ConfigCommand};
use config::{format_get, format_show, set_value, ConfigStore};
use error::AppError;
use std::process::ExitCode;
use telegram::TelegramApi;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None => run_hook().await,
        Some(Command::Setup) => match ConfigStore::discover() {
            Ok(store) => setup::run_setup(&store).await,
            Err(error) => Err(error),
        },
        Some(Command::Test) => run_test().await,
        Some(Command::Config { command }) => run_config(command),
        Some(Command::ProbeSubagent) => run_subagent_probe().await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(error.exit_code() as u8)
        }
    }
}

async fn run_hook() -> Result<(), AppError> {
    match hook::run_hook().await {
        Ok(()) => Ok(()),
        Err(failure) => {
            if failure.always_success {
                eprintln!("{}", failure.error);
                Ok(())
            } else {
                Err(failure.error)
            }
        }
    }
}

async fn run_subagent_probe() -> Result<(), AppError> {
    match hook::run_subagent_probe().await {
        Ok(()) => Ok(()),
        Err(error) => {
            let always_success = ConfigStore::discover()
                .map(|store| store.always_success_best_effort())
                .unwrap_or(true);
            if always_success {
                eprintln!("{error}");
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

async fn run_test() -> Result<(), AppError> {
    let store = ConfigStore::discover()?;
    let effective = store.effective()?;
    if effective.bot_token.is_none() || effective.chat_id.is_none() {
        return Err(AppError::Config(
            "Configuration not found.\n\nRun:\ncodex-telegram-notify setup".to_string(),
        ));
    }
    let runtime = effective.runtime()?;
    let api = telegram::HttpTelegramApi::new(runtime.bot_token.clone(), runtime.timeout)
        .map_err(|error| AppError::Telegram(error.user_message()))?;
    api.send_message(telegram::SendMessageRequest {
        chat_id: runtime.chat_id,
        text: "🧪 Codex Telegram Notify\n\nТестовое уведомление успешно отправлено.".to_string(),
        disable_notification: runtime.silent,
    })
    .await
    .map_err(|error| AppError::Telegram(error.user_message()))
}

fn run_config(command: ConfigCommand) -> Result<(), AppError> {
    let store = ConfigStore::discover()?;
    match command {
        ConfigCommand::Show => {
            println!("{}", format_show(&store.effective()?));
            Ok(())
        }
        ConfigCommand::Get { key } => {
            println!("{}", format_get(&store.effective()?, &key)?);
            Ok(())
        }
        ConfigCommand::Set { key, value } => {
            let mut config = store.load_file()?;
            set_value(&mut config, &key, &value)?;
            store.save(&config)?;
            println!("Configuration saved.");
            Ok(())
        }
        ConfigCommand::Reset => {
            store.reset()?;
            println!("Configuration reset.");
            Ok(())
        }
    }
}
