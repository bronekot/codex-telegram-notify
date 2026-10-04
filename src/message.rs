use crate::hook::HookPayload;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

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

pub fn build_review_notification(
    cwd: Option<&Path>,
    model: Option<&str>,
    effort: Option<&str>,
    findings: Option<usize>,
    explanation: Option<&str>,
    max_length: usize,
) -> String {
    let project = cwd
        .and_then(project_name)
        .unwrap_or_else(|| UNKNOWN_PROJECT.to_string());
    let model = model.map(str::trim).filter(|value| !value.is_empty());
    let effort = effort.map(str::trim).filter(|value| !value.is_empty());

    let mut output = format!("🔎 Codex завершил review\n\n📁 {project}\n");
    if let Some(model) = model {
        output.push_str(&format!("🤖 {model}"));
        if let Some(effort) = effort {
            output.push_str(&format!(" ({effort})"));
        }
        output.push('\n');
    }

    match findings {
        Some(0) => output.push_str("\n✅ Замечаний не найдено."),
        Some(count) => output.push_str(&format!("\n⚠️ Найдено замечаний: {count}.")),
        None => output.push_str("\n✅ Результат ревью получен."),
    }
    if let Some(explanation) = explanation.map(str::trim).filter(|value| !value.is_empty()) {
        output.push_str("\n\n");
        output.push_str(explanation);
    }

    truncate_unicode(&output, max_length)
}

fn project_name(path: &Path) -> Option<String> {
    let root = managed_project_root(path)
        .map(Path::to_path_buf)
        .or_else(|| linked_worktree_project_root(path));
    root.as_deref()
        .unwrap_or(path)
        .file_name()
        .map(|name| name.to_string_lossy().trim().to_string())
        .filter(|name| !name.is_empty())
}

fn managed_project_root(path: &Path) -> Option<&Path> {
    // Review notifications can arrive after fixloop has removed the worktree.
    path.ancestors().find_map(|worktree| {
        let worktrees = worktree.parent()?;
        if worktrees.file_name()? != "worktrees" {
            return None;
        }
        let fixloop = worktrees.parent()?;
        if fixloop.file_name()? != "codex-fixloop" {
            return None;
        }
        let git_dir = fixloop.parent()?;
        if git_dir.file_name()? != ".git" {
            return None;
        }
        git_dir.parent()
    })
}

fn linked_worktree_project_root(path: &Path) -> Option<PathBuf> {
    for directory in path.ancestors() {
        let git_file = directory.join(".git");
        if git_file.is_dir() {
            // A normal checkout marks the nearest repository boundary.
            return None;
        }
        if !git_file.is_file() {
            continue;
        }
        let git_dir = directory.join(read_git_path(&git_file, "gitdir:")?);
        let common_dir = git_dir.join(read_git_path(&git_dir.join("commondir"), "")?);
        let common_dir = common_dir.canonicalize().ok()?;
        if common_dir.file_name()? != ".git" {
            return None;
        }
        return common_dir.parent().map(Path::to_path_buf);
    }
    None
}

fn read_git_path(path: &Path, prefix: &str) -> Option<PathBuf> {
    const MAX_METADATA_BYTES: u64 = 8192;
    let mut contents = String::new();
    File::open(path)
        .ok()?
        .take(MAX_METADATA_BYTES + 1)
        .read_to_string(&mut contents)
        .ok()?;
    if contents.len() > MAX_METADATA_BYTES as usize {
        return None;
    }
    let value = contents.trim().strip_prefix(prefix)?.trim();
    (!value.is_empty()).then(|| PathBuf::from(value))
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
    fn formats_review_without_findings() {
        let result = build_review_notification(
            Some(Path::new("/home/user/project")),
            Some("gpt-5.6-luna"),
            Some("max"),
            Some(0),
            Some("Изменения выглядят корректно."),
            3500,
        );
        assert!(result.contains("завершил review"));
        assert!(result.contains("🤖 gpt-5.6-luna (max)"));
        assert!(result.contains("Замечаний не найдено"));
        assert!(result.contains("Изменения выглядят корректно"));
    }

    fn assert_notification_project(cwd: &Path, expected: &str) {
        let mut payload = payload();
        payload.cwd = Some(cwd.to_path_buf());
        let expected_line = format!("📁 {expected}\n");
        assert!(build_notification(&payload, 3500).contains(&expected_line));
        assert!(
            build_review_notification(Some(cwd), None, None, Some(0), None, 3500)
                .contains(&expected_line)
        );
    }

    #[test]
    fn managed_worktree_notifications_keep_project_after_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("мой проект");
        let worktree = project.join(".git/codex-fixloop/worktrees/123-456");
        assert!(!worktree.exists());
        assert_notification_project(&worktree, "мой проект");
        assert_notification_project(&worktree.join("src"), "мой проект");
    }

    #[test]
    fn linked_worktree_notifications_resolve_absolute_and_relative_git_paths() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("мой проект");
        let git_dir = project.join(".git/worktrees/review");
        let worktree = temp.path().join("123-456");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();

        for target in [
            git_dir,
            PathBuf::from("../мой проект/.git/worktrees/review"),
        ] {
            std::fs::write(
                worktree.join(".git"),
                format!("gitdir: {}\n", target.display()),
            )
            .unwrap();
            assert_notification_project(&worktree, "мой проект");
            assert_notification_project(&worktree.join("src"), "мой проект");
        }
    }

    #[test]
    fn invalid_worktree_metadata_keeps_original_folder_name() {
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("work-folder");
        std::fs::create_dir(&worktree).unwrap();
        for metadata in [
            "not a git directory".to_string(),
            "gitdir: missing-directory".to_string(),
            format!("gitdir: {}", "x".repeat(9000)),
        ] {
            std::fs::write(worktree.join(".git"), metadata).unwrap();
            assert_notification_project(&worktree, "work-folder");
        }
    }

    #[test]
    fn only_the_full_managed_layout_identifies_a_project() {
        for cwd in [
            "/projects/example/.git/other/worktrees/123-456",
            "/projects/example/codex-fixloop/worktrees/123-456",
            "/projects/example/.git/codex-fixloop/123-456",
        ] {
            assert_notification_project(Path::new(cwd), "123-456");
        }
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
