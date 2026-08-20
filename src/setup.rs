use crate::config::{BotToken, ConfigStore};
use crate::error::AppError;
use crate::telegram::{
    BotUser, GetUpdatesRequest, SendMessageRequest, TelegramApi, TelegramError, Update,
};
use std::future::Future;
use std::io::Write;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::{sleep, Instant};

const SETUP_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const UPDATE_TIMEOUT_SECONDS: u64 = 15;
const UPDATE_HTTP_TIMEOUT: Duration = Duration::from_secs(25);
const CHAT_COLLECTION_WINDOW: Duration = Duration::from_secs(2);
const TEST_MESSAGE: &str = "🧪 Codex Telegram Notify\n\nТестовое уведомление успешно отправлено.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatCandidate {
    pub id: i64,
    pub name: String,
    pub chat_type: String,
}

pub async fn run_setup(store: &ConfigStore) -> Result<(), AppError> {
    let token = rpassword::prompt_password("Telegram bot token: ")
        .map_err(|_| AppError::Config("Unable to read the Telegram bot token".to_string()))?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err(AppError::Config(
            "Telegram bot token cannot be empty".to_string(),
        ));
    }

    let api =
        crate::telegram::HttpTelegramApi::new(BotToken::new(token.clone()), UPDATE_HTTP_TIMEOUT)
            .map_err(|error| AppError::Telegram(error.user_message()))?;
    run_setup_with_api(store, BotToken::new(token), &api).await
}

pub async fn run_setup_with_api(
    store: &ConfigStore,
    token: BotToken,
    api: &dyn TelegramApi,
) -> Result<(), AppError> {
    let bot = await_api(api.get_me()).await?;
    println!("✓ Бот найден: {}", bot_display_name(&bot));
    println!();
    println!("Откройте бота в Telegram и отправьте ему /start.");
    println!("Ожидание сообщения...");
    println!();

    let baseline_updates = await_api(api.get_updates(GetUpdatesRequest {
        offset: Some(-1),
        limit: Some(100),
        timeout: Some(0),
    }))
    .await?;
    let mut offset = next_offset(None, &baseline_updates);
    let deadline = Instant::now() + SETUP_TIMEOUT;
    let mut candidates = Vec::new();
    let mut quiet_deadline = None;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_seconds = if candidates.is_empty() {
            UPDATE_TIMEOUT_SECONDS.min(remaining.as_secs().max(1))
        } else {
            let quiet_remaining = quiet_deadline
                .unwrap_or_else(|| Instant::now() + CHAT_COLLECTION_WINDOW)
                .saturating_duration_since(Instant::now());
            if quiet_remaining.is_zero() {
                break;
            }
            quiet_remaining.as_secs().clamp(1, 2)
        };

        let request = api.get_updates(GetUpdatesRequest {
            offset,
            limit: Some(100),
            timeout: Some(timeout_seconds),
        });
        let updates = tokio::select! {
            result = request => result.map_err(setup_api_error)?,
            _ = sleep(remaining) => break,
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|_| AppError::Cancelled("Setup cancelled".to_string()))?;
                return Err(AppError::Cancelled("Setup cancelled".to_string()));
            }
        };

        offset = next_offset(offset, &updates);
        let mut found_new = false;
        for update in &updates {
            if let Some(candidate) = candidate_from_update(update, bot.username.as_deref()) {
                if !candidates
                    .iter()
                    .any(|existing: &ChatCandidate| existing.id == candidate.id)
                {
                    candidates.push(candidate);
                    found_new = true;
                }
            }
        }
        if found_new {
            quiet_deadline = Some(Instant::now() + CHAT_COLLECTION_WINDOW);
        }
        if !candidates.is_empty()
            && quiet_deadline.is_some_and(|candidate_deadline| Instant::now() >= candidate_deadline)
        {
            break;
        }
    }

    if candidates.is_empty() {
        return Err(AppError::Cancelled(
            "No /start message received within 5 minutes".to_string(),
        ));
    }

    let selected = choose_candidate(&candidates, deadline).await?;
    let previous = store.load_file()?;
    await_api(api.send_message(SendMessageRequest {
        chat_id: selected.id,
        text: TEST_MESSAGE.to_string(),
        disable_notification: previous.silent,
    }))
    .await?;
    println!("✓ Тестовое уведомление отправлено");

    let mut saved = previous;
    saved.bot_token = Some(token.as_str().to_string());
    saved.chat_id = Some(selected.id);
    store.save(&saved)?;
    println!("✓ Конфигурация сохранена");
    Ok(())
}

fn setup_api_error(error: TelegramError) -> AppError {
    if error.is_authentication_error() {
        AppError::Telegram(
            "Telegram отклонил bot token. Проверьте токен, полученный у BotFather.".to_string(),
        )
    } else {
        AppError::Telegram(error.user_message())
    }
}

async fn await_api<T, F>(future: F) -> Result<T, AppError>
where
    F: Future<Output = Result<T, TelegramError>>,
{
    tokio::select! {
        result = future => result.map_err(setup_api_error),
        signal = tokio::signal::ctrl_c() => {
            signal.map_err(|_| AppError::Cancelled("Setup cancelled".to_string()))?;
            Err(AppError::Cancelled("Setup cancelled".to_string()))
        }
    }
}

fn bot_display_name(bot: &BotUser) -> String {
    if let Some(username) = bot.username.as_deref().filter(|value| !value.is_empty()) {
        return format!("@{}", clean_terminal_text(username));
    }
    let mut name = bot.first_name.clone();
    if let Some(last_name) = bot.last_name.as_deref().filter(|value| !value.is_empty()) {
        name.push(' ');
        name.push_str(last_name);
    }
    clean_terminal_text(&name)
}

fn next_offset(current: Option<i64>, updates: &[Update]) -> Option<i64> {
    let maximum = updates.iter().map(|update| update.update_id).max();
    match (current, maximum) {
        (Some(current), Some(maximum)) => Some(current.max(maximum.saturating_add(1))),
        (Some(current), None) => Some(current),
        (None, Some(maximum)) => Some(maximum.saturating_add(1)),
        (None, None) => None,
    }
}

fn candidate_from_update(update: &Update, bot_username: Option<&str>) -> Option<ChatCandidate> {
    let message = update.message.as_ref()?;
    let text = message.text.as_deref()?;
    if !is_start_command(text, bot_username) {
        return None;
    }
    match message.chat.chat_type.as_str() {
        "private" | "group" | "supergroup" => Some(ChatCandidate {
            id: message.chat.id,
            name: chat_display_name(&message.chat),
            chat_type: message.chat.chat_type.clone(),
        }),
        _ => None,
    }
}

fn is_start_command(text: &str, bot_username: Option<&str>) -> bool {
    let command = text.split_whitespace().next().unwrap_or_default();
    let Some(command) = command.strip_prefix('/') else {
        return false;
    };
    let mut parts = command.splitn(2, '@');
    if !parts
        .next()
        .is_some_and(|name| name.eq_ignore_ascii_case("start"))
    {
        return false;
    }
    match parts.next() {
        None => true,
        Some(username) => bot_username.is_some_and(|bot| username.eq_ignore_ascii_case(bot)),
    }
}

fn chat_display_name(chat: &crate::telegram::Chat) -> String {
    let name = match chat.chat_type.as_str() {
        "private" => {
            let mut name = chat.first_name.clone().unwrap_or_default();
            if let Some(last_name) = chat.last_name.as_deref().filter(|value| !value.is_empty()) {
                if !name.is_empty() {
                    name.push(' ');
                }
                name.push_str(last_name);
            }
            if name.is_empty() {
                chat.username
                    .as_deref()
                    .map(|value| format!("@{value}"))
                    .unwrap_or_else(|| chat.id.to_string())
            } else {
                name
            }
        }
        _ => chat
            .title
            .clone()
            .or_else(|| chat.username.as_deref().map(|value| format!("@{value}")))
            .unwrap_or_else(|| chat.id.to_string()),
    };
    clean_terminal_text(&name)
}

fn clean_terminal_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

async fn choose_candidate(
    candidates: &[ChatCandidate],
    deadline: Instant,
) -> Result<ChatCandidate, AppError> {
    if candidates.len() == 1 {
        return Ok(candidates[0].clone());
    }

    println!("Обнаружено несколько чатов:");
    for (index, candidate) in candidates.iter().enumerate() {
        println!(
            "{}. {} — {} — {}",
            index + 1,
            candidate.name,
            candidate.chat_type,
            candidate.id
        );
    }

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    loop {
        print!("Выберите чат: ");
        std::io::stdout()
            .flush()
            .map_err(|_| AppError::Config("Unable to write setup prompt".to_string()))?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(AppError::Cancelled("Setup cancelled".to_string()));
        }
        let mut line = String::new();
        let read = tokio::select! {
            result = reader.read_line(&mut line) => result.map_err(|_| AppError::Config("Unable to read chat selection".to_string()))?,
            _ = sleep(remaining) => return Err(AppError::Cancelled("Setup cancelled".to_string())),
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|_| AppError::Cancelled("Setup cancelled".to_string()))?;
                return Err(AppError::Cancelled("Setup cancelled".to_string()));
            }
        };
        if read == 0 {
            return Err(AppError::Cancelled("Setup cancelled".to_string()));
        }
        if let Ok(number) = line.trim().parse::<usize>() {
            if let Some(candidate) = candidates.get(number.saturating_sub(1)) {
                return Ok(candidate.clone());
            }
        }
        println!("Введите номер чата из списка.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telegram::{BotUser, Chat, Message};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    fn update(id: i64, chat_type: &str, chat_id: i64, text: &str) -> Update {
        Update {
            update_id: id,
            message: Some(Message {
                chat: Chat {
                    id: chat_id,
                    chat_type: chat_type.to_string(),
                    title: Some("Codex Dev".to_string()),
                    username: None,
                    first_name: Some("BroneKot".to_string()),
                    last_name: None,
                },
                text: Some(text.to_string()),
            }),
        }
    }

    #[test]
    fn accepts_private_group_and_supergroup_start_messages() {
        for (kind, id) in [("private", 1), ("group", -2), ("supergroup", -3)] {
            let result = candidate_from_update(&update(10, kind, id, "/start"), Some("bot"));
            assert_eq!(result.expect("candidate").id, id);
        }
    }

    #[test]
    fn filters_commands_for_another_bot() {
        assert!(
            candidate_from_update(&update(10, "group", -2, "/start@other"), Some("bot")).is_none()
        );
        assert!(
            candidate_from_update(&update(10, "group", -2, "/start@bot"), Some("bot")).is_some()
        );
        assert!(candidate_from_update(&update(10, "group", -2, "hello"), Some("bot")).is_none());
    }

    #[test]
    fn ignores_unsupported_chat_types_and_advances_offset() {
        assert!(candidate_from_update(&update(10, "channel", -4, "/start"), None).is_none());
        let updates = vec![
            update(10, "private", 1, "hello"),
            update(14, "private", 1, "/start"),
        ];
        assert_eq!(next_offset(None, &updates), Some(15));
        assert_eq!(next_offset(Some(20), &updates), Some(20));
    }

    struct FakeSetupApi {
        update_requests: Arc<Mutex<Vec<GetUpdatesRequest>>>,
        sent_messages: Arc<Mutex<Vec<SendMessageRequest>>>,
    }

    #[async_trait]
    impl TelegramApi for FakeSetupApi {
        async fn get_me(&self) -> Result<BotUser, TelegramError> {
            Ok(BotUser {
                id: 1,
                is_bot: true,
                first_name: "Codex".to_string(),
                last_name: None,
                username: Some("codex_bot".to_string()),
            })
        }

        async fn get_updates(
            &self,
            request: GetUpdatesRequest,
        ) -> Result<Vec<Update>, TelegramError> {
            let call_number = {
                let mut requests = self.update_requests.lock().expect("lock");
                requests.push(request);
                requests.len()
            };
            match call_number {
                1 => Ok(vec![update(100, "private", 111, "/start")]),
                2 => Ok(vec![update(101, "supergroup", -100222, "/start@codex_bot")]),
                _ => {
                    sleep(Duration::from_secs(2)).await;
                    Ok(Vec::new())
                }
            }
        }

        async fn send_message(&self, request: SendMessageRequest) -> Result<(), TelegramError> {
            self.sent_messages.lock().expect("lock").push(request);
            Ok(())
        }
    }

    #[tokio::test]
    async fn setup_ignores_baseline_updates_and_saves_after_test_send() {
        let directory = tempdir().expect("tempdir");
        let store = ConfigStore::from_path(directory.path().join("config.toml"));
        let update_requests = Arc::new(Mutex::new(Vec::new()));
        let sent_messages = Arc::new(Mutex::new(Vec::new()));
        let api = FakeSetupApi {
            update_requests: update_requests.clone(),
            sent_messages: sent_messages.clone(),
        };

        run_setup_with_api(&store, BotToken::new("123:secret"), &api)
            .await
            .expect("setup");

        let requests = update_requests.lock().expect("lock");
        assert_eq!(requests[0].offset, Some(-1));
        assert_eq!(requests[1].offset, Some(101));
        drop(requests);

        let sent = sent_messages.lock().expect("lock");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].chat_id, -100222);
        drop(sent);

        let saved = store.load_file().expect("saved config");
        assert_eq!(saved.bot_token.as_deref(), Some("123:secret"));
        assert_eq!(saved.chat_id, Some(-100222));
    }
}
