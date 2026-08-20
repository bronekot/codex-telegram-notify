use crate::error::AppError;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const APP_NAME: &str = "codex-telegram-notify";
const DEFAULT_MAX_LENGTH: usize = 3500;
const MAX_TELEGRAM_LENGTH: usize = 4096;
const DEFAULT_TIMEOUT_SECONDS: u64 = 5;

#[derive(Clone)]
pub struct BotToken(String);

impl BotToken {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn masked(&self) -> String {
        mask_token(&self.0)
    }
}

impl fmt::Debug for BotToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("BotToken")
            .field(&"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<i64>,
    pub enabled: bool,
    pub max_length: usize,
    pub timeout_seconds: u64,
    pub silent: bool,
    pub always_success: bool,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            bot_token: None,
            chat_id: None,
            enabled: true,
            max_length: DEFAULT_MAX_LENGTH,
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            silent: false,
            always_success: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub bot_token: Option<BotToken>,
    pub chat_id: Option<i64>,
    pub enabled: bool,
    pub max_length: usize,
    pub timeout_seconds: u64,
    pub silent: bool,
    pub always_success: bool,
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub bot_token: BotToken,
    pub chat_id: i64,
    pub max_length: usize,
    pub timeout: Duration,
    pub silent: bool,
}

impl EffectiveConfig {
    pub fn validate(&self) -> Result<(), AppError> {
        if self.max_length == 0 || self.max_length > MAX_TELEGRAM_LENGTH {
            return Err(AppError::Config(format!(
                "max_length must be between 1 and {MAX_TELEGRAM_LENGTH}"
            )));
        }
        if self.timeout_seconds == 0 {
            return Err(AppError::Config(
                "timeout_seconds must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }

    pub fn runtime(&self) -> Result<RuntimeConfig, AppError> {
        self.validate()?;

        let bot_token = self.bot_token.clone().ok_or_else(|| {
            AppError::Config("Telegram bot token is not configured. Run setup.".to_string())
        })?;
        if bot_token.is_empty() {
            return Err(AppError::Config(
                "Telegram bot token is empty. Run setup.".to_string(),
            ));
        }
        let chat_id = self.chat_id.ok_or_else(|| {
            AppError::Config("Telegram chat_id is not configured. Run setup.".to_string())
        })?;

        Ok(RuntimeConfig {
            bot_token,
            chat_id,
            max_length: self.max_length,
            timeout: Duration::from_secs(self.timeout_seconds),
            silent: self.silent,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    pub fn discover() -> Result<Self, AppError> {
        let project_dirs = ProjectDirs::from("", "", APP_NAME).ok_or_else(|| {
            AppError::Config("Unable to determine the user configuration directory".to_string())
        })?;
        Ok(Self::from_path(
            project_dirs.config_dir().join("config.toml"),
        ))
    }

    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load_file(&self) -> Result<ConfigFile, AppError> {
        if !self.path.exists() {
            return Ok(ConfigFile::default());
        }

        let contents = fs::read_to_string(&self.path)
            .map_err(|_| AppError::Config("Unable to read the configuration file".to_string()))?;
        toml::from_str(&contents).map_err(|_| {
            AppError::Config("The configuration file contains invalid TOML".to_string())
        })
    }

    pub fn effective(&self) -> Result<EffectiveConfig, AppError> {
        let file = self.load_file()?;
        let mut effective = EffectiveConfig {
            bot_token: file
                .bot_token
                .map(|value| BotToken::new(value.trim().to_string())),
            chat_id: file.chat_id,
            enabled: file.enabled,
            max_length: file.max_length,
            timeout_seconds: file.timeout_seconds,
            silent: file.silent,
            always_success: file.always_success,
        };

        if let Some(value) = env_string("CODEX_TELEGRAM_BOT_TOKEN") {
            effective.bot_token = Some(BotToken::new(value.trim().to_string()));
        }
        if let Some(value) = env_string("CODEX_TELEGRAM_CHAT_ID") {
            effective.chat_id = Some(parse_env::<i64>("CODEX_TELEGRAM_CHAT_ID", &value)?);
        }
        if let Some(value) = env_string("CODEX_TELEGRAM_ENABLED") {
            effective.enabled = parse_bool("CODEX_TELEGRAM_ENABLED", &value)?;
        }
        if let Some(value) = env_string("CODEX_TELEGRAM_MAX_LENGTH") {
            effective.max_length = parse_env::<usize>("CODEX_TELEGRAM_MAX_LENGTH", &value)?;
        }
        if let Some(value) = env_string("CODEX_TELEGRAM_TIMEOUT") {
            effective.timeout_seconds = parse_env::<u64>("CODEX_TELEGRAM_TIMEOUT", &value)?;
        }
        if let Some(value) = env_string("CODEX_TELEGRAM_SILENT") {
            effective.silent = parse_bool("CODEX_TELEGRAM_SILENT", &value)?;
        }
        if let Some(value) = env_string("CODEX_TELEGRAM_ALWAYS_SUCCESS") {
            effective.always_success = parse_bool("CODEX_TELEGRAM_ALWAYS_SUCCESS", &value)?;
        }

        effective.validate()?;
        Ok(effective)
    }

    pub fn always_success_best_effort(&self) -> bool {
        let mut value = self
            .load_file()
            .map(|config| config.always_success)
            .unwrap_or(true);
        if let Some(environment_value) = env_string("CODEX_TELEGRAM_ALWAYS_SUCCESS") {
            if let Ok(parsed) = parse_bool("CODEX_TELEGRAM_ALWAYS_SUCCESS", &environment_value) {
                value = parsed;
            }
        }
        value
    }

    pub fn save(&self, config: &ConfigFile) -> Result<(), AppError> {
        let parent = self.path.parent().ok_or_else(|| {
            AppError::Config("Unable to determine the configuration directory".to_string())
        })?;
        fs::create_dir_all(parent).map_err(|_| {
            AppError::Config("Unable to create the configuration directory".to_string())
        })?;
        restrict_directory_permissions(parent)?;

        let serialized = toml::to_string_pretty(config)
            .map_err(|_| AppError::Config("Unable to serialize the configuration".to_string()))?;
        let temporary_path = self
            .path
            .with_file_name(format!(".config.toml.{}.tmp", std::process::id()));

        let write_result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temporary_path)?;
            restrict_file_permissions(&file)?;
            file.write_all(serialized.as_bytes())?;
            file.sync_all()?;
            drop(file);
            replace_file(&temporary_path, &self.path)
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temporary_path);
            return Err(AppError::Config(
                "Unable to save the configuration file".to_string(),
            ));
        }

        Ok(())
    }

    pub fn reset(&self) -> Result<(), AppError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(AppError::Config(
                "Unable to remove the configuration file".to_string(),
            )),
        }
    }
}

pub fn mask_token(token: &str) -> String {
    if token.is_empty() {
        return "<not set>".to_string();
    }
    match token.split_once(':') {
        Some((prefix, _)) if !prefix.is_empty() => format!("{prefix}:************"),
        _ => "************".to_string(),
    }
}

pub fn normalize_key(key: &str) -> &str {
    match key {
        "bot-token" | "bot_token" => "bot_token",
        "chat-id" | "chat_id" => "chat_id",
        "max-length" | "max_length" => "max_length",
        "timeout" | "timeout-seconds" | "timeout_seconds" => "timeout_seconds",
        "always-success" | "always_success" => "always_success",
        other => other,
    }
}

pub fn set_value(config: &mut ConfigFile, key: &str, value: &str) -> Result<(), AppError> {
    match normalize_key(key) {
        "bot_token" => {
            return Err(AppError::Config(
                "Setting bot-token via CLI is disabled; use setup or CODEX_TELEGRAM_BOT_TOKEN"
                    .to_string(),
            ));
        }
        "chat_id" => {
            config.chat_id = Some(value.parse::<i64>().map_err(|_| {
                AppError::Config("chat-id must be a signed 64-bit integer".to_string())
            })?);
        }
        "enabled" => config.enabled = parse_bool("enabled", value)?,
        "silent" => config.silent = parse_bool("silent", value)?,
        "max_length" => {
            config.max_length = value
                .parse::<usize>()
                .map_err(|_| AppError::Config("max-length must be an integer".to_string()))?;
        }
        "timeout_seconds" => {
            config.timeout_seconds = value
                .parse::<u64>()
                .map_err(|_| AppError::Config("timeout must be an integer".to_string()))?;
        }
        "always_success" => config.always_success = parse_bool("always-success", value)?,
        _ => {
            return Err(AppError::Config(format!(
                "Unknown configuration key: {key}"
            )))
        }
    }

    validate_file_values(config)
}

pub fn validate_file_values(config: &ConfigFile) -> Result<(), AppError> {
    if config.max_length == 0 || config.max_length > MAX_TELEGRAM_LENGTH {
        return Err(AppError::Config(format!(
            "max-length must be between 1 and {MAX_TELEGRAM_LENGTH}"
        )));
    }
    if config.timeout_seconds == 0 {
        return Err(AppError::Config(
            "timeout must be greater than zero".to_string(),
        ));
    }
    Ok(())
}

pub fn format_show(config: &EffectiveConfig) -> String {
    let bot_token = config
        .bot_token
        .as_ref()
        .map(|token| token.masked())
        .unwrap_or_else(|| "<not set>".to_string());
    let chat_id = config
        .chat_id
        .map(|value| value.to_string())
        .unwrap_or_else(|| "<not set>".to_string());

    format!(
        "bot_token = \"{bot_token}\"\nchat_id = {chat_id}\nenabled = {}\nsilent = {}\nmax_length = {}\ntimeout_seconds = {}\nalways_success = {}",
        config.enabled,
        config.silent,
        config.max_length,
        config.timeout_seconds,
        config.always_success
    )
}

pub fn format_get(config: &EffectiveConfig, key: &str) -> Result<String, AppError> {
    match normalize_key(key) {
        "bot_token" => Ok(config
            .bot_token
            .as_ref()
            .map(|token| token.masked())
            .unwrap_or_else(|| "<not set>".to_string())),
        "chat_id" => Ok(config
            .chat_id
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<not set>".to_string())),
        "enabled" => Ok(config.enabled.to_string()),
        "silent" => Ok(config.silent.to_string()),
        "max_length" => Ok(config.max_length.to_string()),
        "timeout_seconds" => Ok(config.timeout_seconds.to_string()),
        "always_success" => Ok(config.always_success.to_string()),
        _ => Err(AppError::Config(format!(
            "Unknown configuration key: {key}"
        ))),
    }
}

fn env_string(name: &str) -> Option<String> {
    std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
}

fn parse_env<T>(name: &str, value: &str) -> Result<T, AppError>
where
    T: std::str::FromStr,
{
    value
        .parse::<T>()
        .map_err(|_| AppError::Config(format!("Environment variable {name} has an invalid value")))
}

fn parse_bool(name: &str, value: &str) -> Result<bool, AppError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(AppError::Config(format!("{name} must be true or false"))),
    }
}

fn restrict_directory_permissions(path: &Path) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| {
            AppError::Config("Unable to secure the configuration directory".to_string())
        })?;
    }
    Ok(())
}

fn restrict_file_permissions(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(source, destination)
    }

    #[cfg(windows)]
    {
        let backup = destination.with_extension("toml.bak");
        let had_destination = destination.exists();
        if had_destination {
            fs::rename(destination, &backup)?;
        }
        match fs::rename(source, destination) {
            Ok(()) => {
                if had_destination {
                    let _ = fs::remove_file(backup);
                }
                Ok(())
            }
            Err(error) => {
                if had_destination {
                    let _ = fs::rename(backup, destination);
                }
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

    #[test]
    fn masks_only_the_secret_part_of_a_token() {
        assert_eq!(mask_token("123456:ABC"), "123456:************");
        assert_eq!(mask_token("ABC"), "************");
        assert_eq!(mask_token(""), "<not set>");
    }

    #[test]
    fn reads_and_writes_signed_chat_ids() {
        let directory = tempdir().expect("tempdir");
        let store = ConfigStore::from_path(directory.path().join("config.toml"));
        let config = ConfigFile {
            chat_id: Some(-1001234567890),
            ..ConfigFile::default()
        };
        store.save(&config).expect("save");
        let loaded = store.load_file().expect("load");
        assert_eq!(loaded.chat_id, Some(-1001234567890));
        assert!(fs::metadata(store.path()).is_ok());
    }

    #[test]
    fn set_value_rejects_bot_tokens() {
        let mut config = ConfigFile::default();
        let error = set_value(&mut config, "bot-token", "123456:secret").unwrap_err();
        assert!(error.to_string().contains("setup"));
        assert!(config.bot_token.is_none());
    }

    #[test]
    fn validates_length_and_timeout() {
        let mut config = ConfigFile::default();
        assert!(set_value(&mut config, "max-length", "4096").is_ok());
        assert!(set_value(&mut config, "max-length", "4097").is_err());
        assert!(set_value(&mut config, "timeout", "0").is_err());
    }

    #[test]
    fn environment_values_override_file_values() {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _lock = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("lock");
        let mut environment = EnvironmentGuard::new([
            "CODEX_TELEGRAM_BOT_TOKEN",
            "CODEX_TELEGRAM_CHAT_ID",
            "CODEX_TELEGRAM_ENABLED",
            "CODEX_TELEGRAM_MAX_LENGTH",
            "CODEX_TELEGRAM_TIMEOUT",
            "CODEX_TELEGRAM_SILENT",
            "CODEX_TELEGRAM_ALWAYS_SUCCESS",
        ]);
        environment.set("CODEX_TELEGRAM_BOT_TOKEN", "999:environment");
        environment.set("CODEX_TELEGRAM_CHAT_ID", "-100");
        environment.set("CODEX_TELEGRAM_ENABLED", "false");
        environment.set("CODEX_TELEGRAM_MAX_LENGTH", "4000");
        environment.set("CODEX_TELEGRAM_TIMEOUT", "9");
        environment.set("CODEX_TELEGRAM_SILENT", "true");
        environment.set("CODEX_TELEGRAM_ALWAYS_SUCCESS", "false");

        let directory = tempdir().expect("tempdir");
        let store = ConfigStore::from_path(directory.path().join("config.toml"));
        store
            .save(&ConfigFile {
                bot_token: Some("111:file".to_string()),
                chat_id: Some(1),
                ..ConfigFile::default()
            })
            .expect("save");

        let effective = store.effective().expect("effective config");
        assert_eq!(
            effective.bot_token.as_ref().map(BotToken::as_str),
            Some("999:environment")
        );
        assert_eq!(effective.chat_id, Some(-100));
        assert!(!effective.enabled);
        assert_eq!(effective.max_length, 4000);
        assert_eq!(effective.timeout_seconds, 9);
        assert!(effective.silent);
        assert!(!effective.always_success);
    }

    struct EnvironmentGuard {
        previous: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvironmentGuard {
        fn new<const N: usize>(names: [&'static str; N]) -> Self {
            Self {
                previous: names
                    .into_iter()
                    .map(|name| (name, std::env::var_os(name)))
                    .collect(),
            }
        }

        fn set(&mut self, name: &'static str, value: &str) {
            std::env::set_var(name, value);
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            for (name, value) in &self.previous {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}
