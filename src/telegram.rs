use crate::config::BotToken;
use async_trait::async_trait;
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;
use tokio::time::{sleep, timeout, Instant};

const DEFAULT_API_BASE_URL: &str = "https://api.telegram.org";
const RETRY_DELAY: Duration = Duration::from_millis(100);

#[derive(Debug, Error)]
pub enum TelegramError {
    #[error("Telegram API error")]
    Api {
        code: Option<i64>,
        description: String,
    },
    #[error("network error while contacting Telegram")]
    Network,
    #[error("request to Telegram timed out")]
    Timeout,
    #[error("Telegram returned an invalid response")]
    InvalidResponse,
}

impl TelegramError {
    pub fn user_message(&self) -> String {
        match self {
            Self::Api { code, description } => {
                let description = sanitize_error_text(description);
                match code {
                    Some(code) => format!("Telegram API error ({code}): {description}"),
                    None => format!("Telegram API error: {description}"),
                }
            }
            Self::Network => self.to_string(),
            Self::Timeout => self.to_string(),
            Self::InvalidResponse => self.to_string(),
        }
    }

    pub fn is_authentication_error(&self) -> bool {
        matches!(
            self,
            Self::Api {
                code: Some(401),
                ..
            }
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BotUser {
    #[allow(dead_code)]
    pub id: i64,
    #[allow(dead_code)]
    pub is_bot: bool,
    pub first_name: String,
    #[serde(default)]
    pub last_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub chat: Chat,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub chat_type: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub last_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GetUpdatesRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendMessageRequest {
    pub chat_id: i64,
    pub text: String,
    pub disable_notification: bool,
}

#[async_trait]
pub trait TelegramApi: Send + Sync {
    async fn get_me(&self) -> Result<BotUser, TelegramError>;
    async fn get_updates(&self, request: GetUpdatesRequest) -> Result<Vec<Update>, TelegramError>;
    async fn send_message(&self, request: SendMessageRequest) -> Result<(), TelegramError>;
}

#[derive(Clone)]
pub struct HttpTelegramApi {
    client: reqwest::Client,
    token: BotToken,
    base_url: String,
    request_timeout: Duration,
}

impl HttpTelegramApi {
    pub fn new(token: BotToken, request_timeout: Duration) -> Result<Self, TelegramError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|_| TelegramError::Network)?;
        Ok(Self {
            client,
            token,
            base_url: DEFAULT_API_BASE_URL.to_string(),
            request_timeout,
        })
    }

    #[cfg(test)]
    fn with_base_url(
        token: BotToken,
        request_timeout: Duration,
        base_url: String,
    ) -> Result<Self, TelegramError> {
        let mut api = Self::new(token, request_timeout)?;
        api.base_url = base_url;
        Ok(api)
    }

    fn endpoint(&self, method: &str) -> String {
        format!(
            "{}/bot{}/{}",
            self.base_url.trim_end_matches('/'),
            self.token.as_str(),
            method
        )
    }

    async fn post<T, B>(&self, method: &str, body: &B) -> Result<T, TelegramError>
    where
        T: DeserializeOwned,
        B: Serialize + Sync,
    {
        let endpoint = self.endpoint(method);
        let deadline = Instant::now() + self.request_timeout;
        let mut retry_used = false;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(TelegramError::Timeout);
            }

            let request = self.client.post(&endpoint).json(body).timeout(remaining);
            let response = match timeout(remaining, request.send()).await {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    if !retry_used && is_retryable_transport(&error) {
                        retry_used = true;
                        if !wait_for_retry(deadline).await {
                            return Err(if error.is_timeout() {
                                TelegramError::Timeout
                            } else {
                                TelegramError::Network
                            });
                        }
                        continue;
                    }
                    return Err(if error.is_timeout() {
                        TelegramError::Timeout
                    } else {
                        TelegramError::Network
                    });
                }
                Err(_) => return Err(TelegramError::Timeout),
            };

            let status = response.status();
            let body_remaining = deadline.saturating_duration_since(Instant::now());
            let bytes = match timeout(body_remaining, response.bytes()).await {
                Ok(Ok(bytes)) => bytes,
                Ok(Err(_)) => {
                    if !retry_used {
                        retry_used = true;
                        if wait_for_retry(deadline).await {
                            continue;
                        }
                    }
                    return Err(TelegramError::Network);
                }
                Err(_) => return Err(TelegramError::Timeout),
            };

            if status.is_server_error() && !retry_used {
                retry_used = true;
                if wait_for_retry(deadline).await {
                    continue;
                }
            }

            let parsed: ApiResponse<T> =
                serde_json::from_slice(&bytes).map_err(|_| TelegramError::InvalidResponse)?;
            if parsed.ok {
                return parsed.result.ok_or(TelegramError::InvalidResponse);
            }

            return Err(TelegramError::Api {
                code: parsed.error_code.or_else(|| status_code(status)),
                description: redact_secret(
                    &parsed
                        .description
                        .unwrap_or_else(|| "request rejected".to_string()),
                    self.token.as_str(),
                ),
            });
        }
    }
}

#[async_trait]
impl TelegramApi for HttpTelegramApi {
    async fn get_me(&self) -> Result<BotUser, TelegramError> {
        self.post("getMe", &EmptyRequest {}).await
    }

    async fn get_updates(&self, request: GetUpdatesRequest) -> Result<Vec<Update>, TelegramError> {
        self.post("getUpdates", &request).await
    }

    async fn send_message(&self, request: SendMessageRequest) -> Result<(), TelegramError> {
        let _: SentMessage = self.post("sendMessage", &request).await?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct EmptyRequest {}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
    error_code: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct SentMessage {
    #[allow(dead_code)]
    message_id: i64,
}

fn is_retryable_transport(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

async fn wait_for_retry(deadline: Instant) -> bool {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining <= RETRY_DELAY {
        return false;
    }
    sleep(RETRY_DELAY).await;
    true
}

fn status_code(status: StatusCode) -> Option<i64> {
    if status.is_success() {
        None
    } else {
        Some(i64::from(status.as_u16()))
    }
}

fn sanitize_error_text(value: &str) -> String {
    let sanitized = value
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
    let sanitized = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    let sanitized = sanitized.chars().take(200).collect::<String>();
    if sanitized.trim().is_empty() {
        "request rejected".to_string()
    } else {
        sanitized
    }
}

fn redact_secret(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        value.to_string()
    } else {
        value.replace(secret, "[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BotToken;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn sends_get_me_request_and_parses_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:secret/getMe"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": {
                    "id": 123,
                    "is_bot": true,
                    "first_name": "Codex",
                    "username": "codex_bot"
                }
            })))
            .mount(&server)
            .await;

        let api = HttpTelegramApi::with_base_url(
            BotToken::new("123:secret"),
            Duration::from_secs(2),
            server.uri(),
        )
        .expect("client");
        let bot = api.get_me().await.expect("getMe");
        assert_eq!(bot.username.as_deref(), Some("codex_bot"));
    }

    #[tokio::test]
    async fn sends_message_fields_without_parse_mode() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:secret/sendMessage"))
            .and(body_json(serde_json::json!({
                "chat_id": -100,
                "text": "hello",
                "disable_notification": true
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "message_id": 1 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let api = HttpTelegramApi::with_base_url(
            BotToken::new("123:secret"),
            Duration::from_secs(2),
            server.uri(),
        )
        .expect("client");
        api.send_message(SendMessageRequest {
            chat_id: -100,
            text: "hello".to_string(),
            disable_notification: true,
        })
        .await
        .expect("sendMessage");
    }

    #[tokio::test]
    async fn parses_an_empty_get_updates_result() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:secret/getUpdates"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": []
            })))
            .mount(&server)
            .await;

        let api = HttpTelegramApi::with_base_url(
            BotToken::new("123:secret"),
            Duration::from_secs(2),
            server.uri(),
        )
        .expect("client");
        let updates = api
            .get_updates(GetUpdatesRequest {
                offset: Some(10),
                limit: Some(100),
                timeout: Some(15),
            })
            .await
            .expect("getUpdates");
        assert!(updates.is_empty());
    }

    #[tokio::test]
    async fn turns_invalid_token_into_api_error_without_exposing_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:secret/getMe"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "ok": false,
                "error_code": 401,
                "description": "Unauthorized"
            })))
            .mount(&server)
            .await;

        let api = HttpTelegramApi::with_base_url(
            BotToken::new("123:secret"),
            Duration::from_secs(2),
            server.uri(),
        )
        .expect("client");
        let error = api.get_me().await.unwrap_err();
        assert!(error.is_authentication_error());
        assert!(!error.user_message().contains("secret"));
    }
}
