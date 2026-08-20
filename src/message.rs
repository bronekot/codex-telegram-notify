use crate::hook::HookPayload;
use std::path::Path;

const UNKNOWN_PROJECT: &str = "неизвестный проект";
const EMPTY_MESSAGE: &str = "Codex завершил выполнение без итогового сообщения.";
const TRUNCATION_SUFFIX: &str = "\n…\n\n[сообщение сокращено]";

pub fn build_notification(payload: &HookPayload, max_length: usize) -> String {
    let project = payload
        .cwd
        .as_deref()
        .and_then(project_name)
        .unwrap_or_else(|| UNKNOWN_PROJECT.to_string());
    let model = payload
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let effort = payload
        .effort
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let message = payload
        .last_assistant_message
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(EMPTY_MESSAGE);

    let mut output = format!("✅ Codex завершил turn\n\n📁 {project}\n");
    if let Some(model) = model {
        output.push_str(&format!("🤖 {model}"));
        if let Some(effort) = effort {
            output.push_str(&format!(" ({effort})"));
        }
        output.push('\n');
    }
    output.push_str(&format!("\n{message}"));

    truncate_unicode(&output, max_length)
}

fn project_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().trim().to_string())
        .filter(|name| !name.is_empty())
}

pub fn truncate_unicode(value: &str, max_length: usize) -> String {
    let length = value.chars().count();
    if length <= max_length {
        return value.to_string();
    }

    let suffix_length = TRUNCATION_SUFFIX.chars().count();
    if max_length <= suffix_length {
        return value.chars().take(max_length).collect();
    }

    let prefix_length = max_length - suffix_length;
    let mut output: String = value.chars().take(prefix_length).collect();
    output.push_str(TRUNCATION_SUFFIX);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn payload() -> HookPayload {
        HookPayload {
            session_id: None,
            cwd: Some(PathBuf::from("/home/user/проекты/моя-папка")),
            hook_event_name: Some("Stop".to_string()),
            model: Some("gpt-5.6-sol".to_string()),
            effort: Some("high".to_string()),
            turn_id: None,
            last_assistant_message: Some("Готово 🚀".to_string()),
        }
    }

    #[test]
    fn formats_project_model_and_unicode_message() {
        let result = build_notification(&payload(), 3500);
        assert!(result.contains("📁 моя-папка"));
        assert!(result.contains("🤖 gpt-5.6-sol (high)"));
        assert!(result.contains("Готово 🚀"));
    }

    #[test]
    fn omits_missing_model_and_uses_missing_message_fallback() {
        let mut payload = payload();
        payload.cwd = None;
        payload.model = None;
        payload.effort = None;
        payload.last_assistant_message = Some("  \n ".to_string());
        let result = build_notification(&payload, 3500);
        assert!(result.contains("📁 неизвестный проект"));
        assert!(!result.contains("🤖"));
        assert!(result.contains(EMPTY_MESSAGE));
    }

    #[test]
    fn truncates_without_splitting_unicode() {
        let result = truncate_unicode("Привет 🚀 мир", 8);
        assert_eq!(result.chars().count(), 8);
        assert!(result.is_char_boundary(result.len()));
    }

    #[test]
    fn adds_truncation_marker_when_it_fits() {
        let result = truncate_unicode("0123456789", 40);
        assert_eq!(result, "0123456789");

        let result = truncate_unicode(&"x".repeat(100), 40);
        assert!(result.ends_with("[сообщение сокращено]"));
        assert_eq!(result.chars().count(), 40);
    }
}
