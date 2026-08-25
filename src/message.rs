use crate::hook::HookPayload;
use serde_json::Value;
use std::path::Path;

const UNKNOWN_PROJECT: &str = "Проект не определён";
const EMPTY_MESSAGE: &str = "Codex завершил выполнение, но итоговое сообщение не получено.";
const TRUNCATION_SUFFIX: &str = "\n…\n\n[сообщение сокращено]";
const ANSWER_KEYS: &[&str] = &[
    "answer",
    "response",
    "message",
    "text",
    "output_text",
    "content",
    "result",
    "output",
    "data",
    "choices",
    "overall_explanation",
    "explanation",
    "summary",
];

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
        .and_then(extract_assistant_answer)
        .unwrap_or_else(|| EMPTY_MESSAGE.to_string());

    let mut output = format!("✅ Codex завершил выполнение\n\n📁 {project}\n");
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

fn extract_assistant_answer(message: &str) -> Option<String> {
    let message = message.trim();
    if message.is_empty() {
        return None;
    }

    let Some(value) = parse_json_answer(message) else {
        return Some(message.to_string());
    };

    // A valid JSON response should never be sent as a raw JSON blob. Unknown
    // envelopes are reduced to their textual leaves by format_json_answer.
    format_json_answer(&value)
}

fn parse_json_answer(message: &str) -> Option<Value> {
    let message = message.trim();
    let mut candidates = vec![message];
    if let Some(fenced) = fenced_json_body(message) {
        candidates.push(fenced);
    }

    for candidate in candidates {
        if let Ok(value) = serde_json::from_str::<Value>(candidate) {
            return Some(value);
        }
    }

    for (opening, closing) in [(b'{', b'}'), (b'[', b']')] {
        let Some(start) = message.as_bytes().iter().position(|byte| *byte == opening) else {
            continue;
        };
        let Some(end) = message.as_bytes().iter().rposition(|byte| *byte == closing) else {
            continue;
        };
        if start <= end {
            if let Ok(value) = serde_json::from_str::<Value>(&message[start..=end]) {
                return Some(value);
            }
        }
    }
    None
}

fn fenced_json_body(message: &str) -> Option<&str> {
    let message = message.trim();
    let body = message.strip_prefix("```")?.strip_suffix("```")?.trim();
    if let Some((language, contents)) = body.split_once('\n') {
        if language.trim().eq_ignore_ascii_case("json") {
            return Some(contents.trim());
        }
    }
    Some(body.strip_prefix("json").map(str::trim).unwrap_or(body))
}

fn format_json_answer(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => {
            let value = value.trim();
            if value.is_empty() {
                return None;
            }
            if let Some(nested) = parse_json_answer(value) {
                if let Some(answer) = format_json_answer(&nested) {
                    return Some(answer);
                }
            }
            Some(value.to_string())
        }
        Value::Array(values) => {
            let findings = values
                .iter()
                .enumerate()
                .filter_map(|(index, value)| format_json_finding(value, index))
                .collect::<Vec<_>>();
            if findings.len() == values.len() && !findings.is_empty() {
                return Some(findings.join("\n\n"));
            }

            let answers = values
                .iter()
                .filter_map(format_json_answer)
                .collect::<Vec<_>>();
            if !answers.is_empty() {
                return Some(answers.join("\n"));
            }

            (!findings.is_empty()).then(|| findings.join("\n\n"))
        }
        Value::Object(object) => {
            if let Some(answer) = format_json_review_answer(object) {
                return Some(answer);
            }

            for key in ANSWER_KEYS {
                if let Some(value) = object.get(*key) {
                    if let Some(answer) = format_json_answer(value) {
                        return Some(answer);
                    }
                }
            }

            if object.len() == 1 {
                if let Some(value) = object.values().next() {
                    return format_json_answer(value);
                }
            }

            if object.contains_key("title") || object.contains_key("body") {
                return format_json_finding(value, 0);
            }
            fallback_json_text(value)
        }
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::Null => None,
    }
}

fn fallback_json_text(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_string())
        }
        Value::Array(values) => {
            let values = values
                .iter()
                .filter_map(fallback_json_text)
                .collect::<Vec<_>>();
            (!values.is_empty()).then(|| values.join("\n"))
        }
        Value::Object(object) => object
            .values()
            .filter_map(fallback_json_text)
            .max_by_key(|value| value.chars().count()),
        Value::Bool(_) | Value::Number(_) | Value::Null => None,
    }
}

fn format_json_review_answer(object: &serde_json::Map<String, Value>) -> Option<String> {
    let findings = object.get("findings").and_then(Value::as_array);
    let explanation = [
        "overall_explanation",
        "explanation",
        "summary",
        "answer",
        "response",
        "message",
    ]
    .iter()
    .find_map(|key| object.get(*key).and_then(format_json_answer));
    let correctness = object
        .get("overall_correctness")
        .and_then(format_json_answer);

    if findings.is_none() && explanation.is_none() && correctness.is_none() {
        return None;
    }

    let mut sections = Vec::new();
    if let Some(findings) = findings {
        if findings.is_empty() {
            sections.push("✅ Проблем не найдено.".to_string());
        } else {
            let details = findings
                .iter()
                .enumerate()
                .filter_map(|(index, value)| format_json_finding(value, index))
                .collect::<Vec<_>>();
            let mut section = format!("⚠️ Найдено проблем: {}.", findings.len());
            if !details.is_empty() {
                section.push_str("\n\nПроблемы:\n\n");
                section.push_str(&details.join("\n\n"));
            }
            sections.push(section);
        }
    }

    if let Some(explanation) = explanation {
        sections.push(format!("📝 Итог:\n\n{explanation}"));
    } else if let Some(correctness) = correctness {
        sections.push(format!("📝 Итог: {correctness}"));
    }

    (!sections.is_empty()).then(|| sections.join("\n\n"))
}

fn format_json_finding(value: &Value, index: usize) -> Option<String> {
    if let Some(value) = value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(format!("{}. {value}", index + 1));
    }

    let object = value.as_object()?;
    let title = ["title", "name", "summary"]
        .iter()
        .find_map(|key| object.get(*key).and_then(format_json_answer));
    let body = ["body", "description", "details", "message", "explanation"]
        .iter()
        .find_map(|key| object.get(*key).and_then(format_json_answer));
    let priority = object.get("priority").and_then(format_json_priority);
    let location = object
        .get("code_location")
        .or_else(|| object.get("location"))
        .and_then(format_json_location);

    if title.is_none() && body.is_none() && location.is_none() {
        return None;
    }

    let mut label = title.unwrap_or_else(|| "Замечание".to_string());
    if let Some(priority) = priority {
        if !label.starts_with("[P") {
            label = format!("[{priority}] {label}");
        }
    }

    let mut output = format!("{}. {label}", index + 1);
    if let Some(body) = body {
        output.push('\n');
        output.push_str(&body);
    }
    if let Some(location) = location {
        output.push_str("\n📍 ");
        output.push_str(&location);
    }
    Some(output)
}

fn format_json_priority(value: &Value) -> Option<String> {
    if let Some(priority) = value.as_u64() {
        return Some(format!("P{priority}"));
    }
    value.as_str().and_then(|value| {
        let value = value.trim();
        let value = value.strip_prefix('[').unwrap_or(value);
        let value = value.strip_suffix(']').unwrap_or(value);
        let value = value.strip_prefix('P').unwrap_or(value);
        (!value.is_empty()).then(|| format!("P{value}"))
    })
}

fn format_json_location(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    let path = ["absolute_file_path", "file_path", "path", "file"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let line_range = object.get("line_range").and_then(Value::as_object);
    let start = line_range
        .and_then(|range| range.get("start"))
        .and_then(json_usize)
        .or_else(|| object.get("line").and_then(json_usize));
    let end = line_range
        .and_then(|range| range.get("end"))
        .and_then(json_usize)
        .or(start);

    if path.is_none() && start.is_none() {
        return None;
    }

    let mut output = path.unwrap_or("строка").to_string();
    if let Some(start) = start {
        output.push(':');
        output.push_str(&start.to_string());
        if let Some(end) = end.filter(|end| *end != start) {
            output.push('-');
            output.push_str(&end.to_string());
        }
    }
    Some(output)
}

fn json_usize(value: &Value) -> Option<usize> {
    value.as_u64().and_then(|value| usize::try_from(value).ok())
}

pub fn build_review_notification(
    cwd: Option<&Path>,
    model: Option<&str>,
    effort: Option<&str>,
    findings: Option<usize>,
    explanation: Option<&str>,
    max_length: usize,
) -> String {
    build_review_notification_with_findings(
        cwd,
        model,
        effort,
        findings,
        None,
        explanation,
        max_length,
    )
}

pub fn build_review_notification_with_findings(
    cwd: Option<&Path>,
    model: Option<&str>,
    effort: Option<&str>,
    findings: Option<usize>,
    finding_details: Option<&str>,
    explanation: Option<&str>,
    max_length: usize,
) -> String {
    let project = cwd
        .and_then(project_name)
        .unwrap_or_else(|| UNKNOWN_PROJECT.to_string());
    let model = model.map(str::trim).filter(|value| !value.is_empty());
    let effort = effort.map(str::trim).filter(|value| !value.is_empty());

    let mut output = format!("🔎 Проверка изменений завершена\n\n📁 {project}\n");
    if let Some(model) = model {
        output.push_str(&format!("🤖 {model}"));
        if let Some(effort) = effort {
            output.push_str(&format!(" ({effort})"));
        }
        output.push('\n');
    }

    match findings {
        Some(0) => output.push_str("\n✅ Проблем не найдено."),
        Some(count) => output.push_str(&format!("\n⚠️ Найдено проблем: {count}.")),
        None => output.push_str("\n✅ Результат проверки получен."),
    }
    let finding_details = finding_details
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(finding_details) = finding_details {
        output.push_str("\n\n⚠️ Проблемы:\n\n");
        output.push_str(finding_details);
    }
    if let Some(explanation) = explanation.map(str::trim).filter(|value| !value.is_empty()) {
        if finding_details.is_some() {
            output.push_str("\n\n📝 Итог:\n\n");
        } else {
            output.push_str("\n\n");
        }
        output.push_str(explanation);
    }

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
            agent_id: None,
            agent_type: None,
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
    fn extracts_answer_from_review_json_in_stop_message() {
        let mut payload = payload();
        payload.last_assistant_message = Some(
            serde_json::json!({
                "findings": [{
                    "title": "[P2] Исправить обработку ответа",
                    "body": "Тело проблемы должно попасть в уведомление.",
                    "priority": 2,
                    "code_location": {
                        "absolute_file_path": "/home/user/project/src/lib.rs",
                        "line_range": {"start": 12, "end": 14}
                    }
                }],
                "overall_explanation": "Патч требует доработки."
            })
            .to_string(),
        );

        let result = build_notification(&payload, 3500);
        assert!(result.contains("Проблемы:"));
        assert!(result.contains("[P2] Исправить обработку ответа"));
        assert!(result.contains("Тело проблемы должно попасть в уведомление."));
        assert!(result.contains("📍 /home/user/project/src/lib.rs:12-14"));
        assert!(result.contains("Патч требует доработки."));
        assert!(!result.contains("\"findings\""));
    }

    #[test]
    fn extracts_answer_from_fenced_and_nested_json_formats() {
        let cases = [
            (
                "```json\n{\"answer\":\"Ответ из fenced JSON\"}\n```",
                "Ответ из fenced JSON",
            ),
            (
                "{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Ответ из content\"}]}",
                "Ответ из content",
            ),
            (
                "{\"result\":{\"response\":\"Ответ из вложенной обёртки\"}}",
                "Ответ из вложенной обёртки",
            ),
            (
                "{\"meta\":{\"kind\":\"event\"},\"payload\":{\"value\":\"Ответ из неизвестной обёртки\"}}",
                "Ответ из неизвестной обёртки",
            ),
        ];

        for (raw, expected) in cases {
            let mut payload = payload();
            payload.last_assistant_message = Some(raw.to_string());
            let result = build_notification(&payload, 3500);
            assert!(result.contains(expected), "message was: {result}");
            assert!(!result.contains("\"answer\""));
            assert!(!result.contains("\"meta\""));
        }
    }

    #[test]
    fn formats_review_without_findings() {
        let result = build_review_notification(
            Some(Path::new("/home/user/project")),
            Some("gpt-5.6-luna"),
            Some("max"),
            Some(0),
            Some("Изменения выглядят корректно."),
            3500,
        );
        assert!(result.contains("Проверка изменений завершена"));
        assert!(result.contains("🤖 gpt-5.6-luna (max)"));
        assert!(result.contains("Проблем не найдено"));
        assert!(result.contains("Изменения выглядят корректно"));
    }

    #[test]
    fn formats_review_findings_before_overall_explanation() {
        let result = build_review_notification_with_findings(
            Some(Path::new("/home/user/project")),
            None,
            None,
            Some(1),
            Some("1. [P1] Исправить обработку ответа\nПроблема описана здесь."),
            Some("Патч требует доработки."),
            3500,
        );
        assert!(result.contains("Проблемы:"));
        assert!(result.contains("Проблема описана здесь."));
        assert!(result.contains("Итог:"));
        assert!(
            result.find("Проблема описана здесь.").unwrap()
                < result.find("Патч требует доработки.").unwrap()
        );
    }

    #[test]
    fn omits_missing_model_and_uses_missing_message_fallback() {
        let mut payload = payload();
        payload.cwd = None;
        payload.model = None;
        payload.effort = None;
        payload.last_assistant_message = Some("  \n ".to_string());
        let result = build_notification(&payload, 3500);
        assert!(result.contains("📁 Проект не определён"));
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
