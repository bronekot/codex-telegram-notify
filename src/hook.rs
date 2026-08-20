use crate::config::{ConfigStore, RuntimeConfig};
use crate::error::AppError;
use crate::message::build_notification;
use crate::telegram::{HttpTelegramApi, SendMessageRequest, TelegramApi};
use directories::BaseDirs;
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;
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
    #[allow(dead_code)]
    pub turn_id: Option<String>,
    pub last_assistant_message: Option<String>,
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

#[derive(Debug, Deserialize)]
struct CodexConfig {
    model_reasoning_effort: Option<String>,
}

fn read_codex_effort() -> Option<String> {
    let codex_home = env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| BaseDirs::new().map(|dirs| dirs.home_dir().join(".codex")))?;
    let contents = fs::read_to_string(codex_home.join("config.toml")).ok()?;
    toml::from_str::<CodexConfig>(&contents)
        .ok()?
        .model_reasoning_effort
        .and_then(non_empty_trimmed)
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
