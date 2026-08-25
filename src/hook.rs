use crate::config::{ConfigStore, RuntimeConfig};
use crate::error::AppError;
use crate::message::build_notification;
use crate::telegram::{HttpTelegramApi, SendMessageRequest, TelegramApi};
use fs2::FileExt;
use serde::{de, Deserialize, Deserializer};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;

#[derive(Debug, Clone, Deserialize)]
pub struct HookPayload {
    #[allow(dead_code)]
    pub session_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub hook_event_name: Option<String>,
    pub model: Option<String>,
    #[serde(alias = "reasoning_effort", alias = "model_reasoning_effort")]
    pub effort: Option<String>,
    pub turn_id: Option<String>,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    #[serde(default, deserialize_with = "deserialize_assistant_message")]
    pub last_assistant_message: Option<String>,
}

fn deserialize_assistant_message<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<Value>::deserialize(deserializer)?
        .map(|value| match value {
            Value::String(value) => Ok(value),
            value => {
                serde_json::to_string(&value).map_err(|error| de::Error::custom(error.to_string()))
            }
        })
        .transpose()
}

pub struct HookFailure {
    pub error: AppError,
    pub always_success: bool,
}

pub async fn run_hook() -> Result<(), HookFailure> {
    let store = ConfigStore::discover().map_err(|error| HookFailure {
        error,
        always_success: true,
    })?;
    let always_success = store.always_success_best_effort();
    let payload = read_payload().await.map_err(|error| HookFailure {
        error,
        always_success,
    })?;

    if payload
        .hook_event_name
        .as_deref()
        .is_some_and(|event| event != "Stop")
    {
        return Ok(());
    }

    let mut payload = payload;
    if payload
        .effort
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
    {
        payload.effort = read_codex_effort();
    }

    let effective = store.effective().map_err(|error| HookFailure {
        error,
        always_success,
    })?;

    if !effective.enabled {
        return Ok(());
    }

    let runtime = effective.runtime().map_err(|error| HookFailure {
        error,
        always_success,
    })?;
    notify_payload(&payload, &runtime)
        .await
        .map_err(|error| HookFailure {
            error,
            always_success,
        })
}

pub async fn run_subagent_probe() -> Result<(), AppError> {
    let payload = read_payload().await?;
    if payload.hook_event_name.as_deref() != Some("SubagentStop") {
        return Ok(());
    }

    let path = subagent_probe_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            AppError::Config(format!(
                "Unable to create hook diagnostics directory: {error}"
            ))
        })?;
    }

    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&path).map_err(|error| {
        AppError::Config(format!("Unable to open hook diagnostics file: {error}"))
    })?;
    restrict_probe_permissions(&file)?;

    let recorded_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let record = serde_json::json!({
        "recorded_at_unix": recorded_at_unix,
        "hook_event_name": payload.hook_event_name,
        "session_id": payload.session_id,
        "turn_id": payload.turn_id,
        "agent_id": payload.agent_id,
        "agent_type": payload.agent_type,
        "cwd": payload.cwd.map(|path| path.to_string_lossy().into_owned()),
        "model": payload.model,
    });

    let mut line = serde_json::to_vec(&record).map_err(|error| {
        AppError::Config(format!("Unable to serialize hook diagnostics: {error}"))
    })?;
    line.push(b'\n');

    file.lock_exclusive().map_err(|error| {
        AppError::Config(format!("Unable to lock hook diagnostics file: {error}"))
    })?;
    let write_result = file.write_all(&line).and_then(|_| file.flush());
    let unlock_result = file.unlock();

    write_result
        .map_err(|error| AppError::Config(format!("Unable to write hook diagnostics: {error}")))?;
    unlock_result.map_err(|error| {
        AppError::Config(format!("Unable to unlock hook diagnostics file: {error}"))
    })?;

    Ok(())
}

#[derive(Debug, Deserialize)]
struct CodexConfig {
    model_reasoning_effort: Option<String>,
}

fn read_codex_effort() -> Option<String> {
    let codex_home = crate::paths::codex_home()?;
    let contents = fs::read_to_string(codex_home.join("config.toml")).ok()?;
    toml::from_str::<CodexConfig>(&contents)
        .ok()?
        .model_reasoning_effort
        .and_then(non_empty_trimmed)
}

fn subagent_probe_path() -> Result<PathBuf, AppError> {
    crate::paths::codex_home()
        .map(|path| path.join("codex-telegram-notify-subagent-events.jsonl"))
        .ok_or_else(|| AppError::Config("Unable to determine Codex home directory".to_string()))
}

fn restrict_probe_permissions(file: &File) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                AppError::Config(format!(
                    "Unable to restrict hook diagnostics permissions: {error}"
                ))
            })?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

fn non_empty_trimmed(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

pub async fn notify_payload(payload: &HookPayload, config: &RuntimeConfig) -> Result<(), AppError> {
    let api = HttpTelegramApi::new(config.bot_token.clone(), config.timeout)
        .map_err(|error| AppError::Telegram(error.user_message()))?;
    notify_payload_with_api(payload, config, &api).await
}

pub async fn notify_payload_with_api(
    payload: &HookPayload,
    config: &RuntimeConfig,
    api: &dyn TelegramApi,
) -> Result<(), AppError> {
    let text = build_notification(payload, config.max_length);
    api.send_message(SendMessageRequest {
        chat_id: config.chat_id,
        text,
        disable_notification: config.silent,
    })
    .await
    .map_err(|error| AppError::Telegram(error.user_message()))
}

async fn read_payload() -> Result<HookPayload, AppError> {
    let mut input = Vec::new();
    tokio::io::stdin()
        .read_to_end(&mut input)
        .await
        .map_err(|_| AppError::Payload("Unable to read hook payload from stdin".to_string()))?;
    serde_json::from_slice::<HookPayload>(&input)
        .map_err(|_| AppError::Payload("Hook payload is not valid JSON".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BotToken, RuntimeConfig};
    use crate::telegram::TelegramError;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct FakeApi {
        request: Arc<Mutex<Option<SendMessageRequest>>>,
    }

    #[async_trait]
    impl TelegramApi for FakeApi {
        async fn get_me(&self) -> Result<crate::telegram::BotUser, TelegramError> {
            unreachable!()
        }

        async fn get_updates(
            &self,
            _request: crate::telegram::GetUpdatesRequest,
        ) -> Result<Vec<crate::telegram::Update>, TelegramError> {
            unreachable!()
        }

        async fn send_message(&self, request: SendMessageRequest) -> Result<(), TelegramError> {
            *self.request.lock().expect("lock") = Some(request);
            Ok(())
        }
    }

    fn config() -> RuntimeConfig {
        RuntimeConfig {
            bot_token: BotToken::new("123:secret"),
            chat_id: -100,
            max_length: 3500,
            timeout: Duration::from_secs(5),
            silent: true,
        }
    }

    #[tokio::test]
    async fn sends_only_stop_payload_content_to_the_api() {
        let request = Arc::new(Mutex::new(None));
        let api = FakeApi {
            request: request.clone(),
        };
        let payload = HookPayload {
            session_id: Some("session".to_string()),
            cwd: Some(PathBuf::from("/tmp/my-project")),
            hook_event_name: Some("Stop".to_string()),
            model: None,
            effort: None,
            turn_id: Some("turn".to_string()),
            agent_id: None,
            agent_type: None,
            last_assistant_message: Some("Готово ✅".to_string()),
        };

        notify_payload_with_api(&payload, &config(), &api)
            .await
            .expect("notify");
        let request = request.lock().expect("lock").clone().expect("request");
        assert_eq!(request.chat_id, -100);
        assert!(request.text.contains("my-project"));
        assert!(request.text.contains("Готово ✅"));
        assert!(request.disable_notification);
    }

    #[test]
    fn ignores_unknown_payload_fields() {
        let payload: HookPayload = serde_json::from_str(
            r#"{
                "hook_event_name": "Stop",
                "last_assistant_message": "done",
                "effort": "high",
                "future_codex_field": true
            }"#,
        )
        .expect("payload");
        assert_eq!(payload.hook_event_name.as_deref(), Some("Stop"));
        assert_eq!(payload.last_assistant_message.as_deref(), Some("done"));
        assert_eq!(payload.effort.as_deref(), Some("high"));
    }

    #[test]
    fn accepts_object_as_last_assistant_message() {
        let payload: HookPayload = serde_json::from_str(
            r#"{
                "hook_event_name": "Stop",
                "last_assistant_message": {"answer": "done"}
            }"#,
        )
        .expect("payload");
        assert_eq!(
            payload.last_assistant_message.as_deref(),
            Some(r#"{"answer":"done"}"#)
        );
    }

    #[test]
    fn accepts_subagent_metadata() {
        let payload: HookPayload = serde_json::from_str(
            r#"{
                "hook_event_name": "SubagentStop",
                "agent_id": "agent-123",
                "agent_type": "review"
            }"#,
        )
        .expect("payload");
        assert_eq!(payload.agent_id.as_deref(), Some("agent-123"));
        assert_eq!(payload.agent_type.as_deref(), Some("review"));
    }

    #[test]
    fn accepts_effort_aliases() {
        let payload: HookPayload = serde_json::from_str(
            r#"{
                "model": "gpt-5.6-luna",
                "reasoning_effort": "medium"
            }"#,
        )
        .expect("payload");
        assert_eq!(payload.effort.as_deref(), Some("medium"));
    }

    #[test]
    fn reads_model_reasoning_effort_from_codex_config() {
        let config = r#"
            model = "gpt-5.6-luna"
            model_reasoning_effort = " max "
        "#;
        let parsed: CodexConfig = toml::from_str(config).expect("config");
        assert_eq!(
            parsed.model_reasoning_effort.and_then(non_empty_trimmed),
            Some("max".to_string())
        );
    }
}
