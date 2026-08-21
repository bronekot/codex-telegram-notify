use crate::error::AppError;
use crate::paths::codex_home;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const HOOK_TIMEOUT_SECONDS: u64 = 10;
const BACKUP_SUFFIX: &str = ".codex-telegram-notify.bak";

pub fn install() -> Result<(), AppError> {
    let codex_home = codex_home()
        .ok_or_else(|| AppError::Config("Unable to determine Codex home directory".to_string()))?;
    let config_path = codex_home.join("config.toml");
    let executable = std::env::current_exe().map_err(|error| {
        AppError::Config(format!("Unable to determine executable path: {error}"))
    })?;
    install_at(&config_path, &executable)
}

fn install_at(config_path: &Path, executable: &Path) -> Result<(), AppError> {
    let contents = match fs::read_to_string(config_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(AppError::Config(format!(
                "Unable to read Codex configuration {}: {error}",
                config_path.display()
            )))
        }
    };
    let command = executable.to_string_lossy().into_owned();

    if has_command_hook(&contents, &command)? {
        println!("Codex Stop hook is already installed.");
        return Ok(());
    }

    let updated = append_hook(&contents, &command);
    let backup = write_config(config_path, &updated)?;
    println!("Codex Stop hook installed in {}", config_path.display());
    if let Some(backup) = backup {
        println!("Previous configuration backed up to {}", backup.display());
    }
    Ok(())
}

fn has_command_hook(contents: &str, command: &str) -> Result<bool, AppError> {
    if contents.trim().is_empty() {
        return Ok(false);
    }
    let document: toml::Value = toml::from_str(contents).map_err(|error| {
        AppError::Config(format!(
            "Codex configuration contains invalid TOML: {error}"
        ))
    })?;

    let Some(hooks) = document.as_table().and_then(|table| table.get("hooks")) else {
        return Ok(false);
    };
    let Some(hooks) = hooks.as_table() else {
        return Err(AppError::Config(
            "Codex configuration has an invalid hooks table".to_string(),
        ));
    };
    let Some(stop) = hooks.get("Stop") else {
        return Ok(false);
    };
    let Some(stop_entries) = stop.as_array() else {
        return Err(AppError::Config(
            "Codex configuration has an invalid hooks.Stop array".to_string(),
        ));
    };

    for stop_entry in stop_entries {
        let Some(stop_entry) = stop_entry.as_table() else {
            return Err(AppError::Config(
                "Codex configuration has an invalid hooks.Stop entry".to_string(),
            ));
        };
        let Some(hook_entries) = stop_entry.get("hooks") else {
            continue;
        };
        let Some(hook_entries) = hook_entries.as_array() else {
            return Err(AppError::Config(
                "Codex configuration has an invalid hooks.Stop.hooks array".to_string(),
            ));
        };

        for hook_entry in hook_entries {
            let Some(hook_entry) = hook_entry.as_table() else {
                continue;
            };
            let hook_type = hook_entry.get("type").and_then(toml::Value::as_str);
            let hook_command = hook_entry.get("command").and_then(toml::Value::as_str);
            if hook_type == Some("command")
                && hook_command.is_some_and(|value| same_command(value, command))
            {
                return Ok(true);
            }
        }
    }

    Ok(false)
}

fn same_command(left: &str, right: &str) -> bool {
    #[cfg(windows)]
    {
        left.eq_ignore_ascii_case(right)
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn append_hook(contents: &str, command: &str) -> String {
    let line_ending = if contents.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut updated = contents.to_string();
    if !updated.is_empty() {
        if !updated.ends_with('\n') {
            updated.push_str(line_ending);
        }
        updated.push_str(line_ending);
    }
    updated.push_str("[[hooks.Stop]]");
    updated.push_str(line_ending);
    updated.push_str(line_ending);
    updated.push_str("[[hooks.Stop.hooks]]");
    updated.push_str(line_ending);
    updated.push_str("type = \"command\"");
    updated.push_str(line_ending);
    updated.push_str("command = ");
    updated.push_str(&quote_toml_string(command));
    updated.push_str(line_ending);
    updated.push_str("timeout = ");
    updated.push_str(&HOOK_TIMEOUT_SECONDS.to_string());
    updated.push_str(line_ending);
    updated
}

fn quote_toml_string(value: &str) -> String {
    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character if character.is_control() => {
                quoted.push_str(&format!("\\u{:04X}", character as u32))
            }
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

fn write_config(config_path: &Path, contents: &str) -> Result<Option<PathBuf>, AppError> {
    let parent = config_path.parent().ok_or_else(|| {
        AppError::Config("Unable to determine Codex configuration directory".to_string())
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        AppError::Config(format!(
            "Unable to create Codex configuration directory: {error}"
        ))
    })?;

    let file_name = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let temporary_path = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    let backup_path = parent.join(format!("{file_name}{BACKUP_SUFFIX}"));

    let write_result = (|| -> io::Result<Option<PathBuf>> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary_path)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);

        let had_configuration = config_path.exists();
        if had_configuration {
            fs::copy(config_path, &backup_path)?;
        }
        replace_config(
            &temporary_path,
            config_path,
            &backup_path,
            had_configuration,
        )?;
        Ok(had_configuration.then_some(backup_path))
    })();

    match write_result {
        Ok(backup) => Ok(backup),
        Err(error) => {
            let _ = fs::remove_file(&temporary_path);
            Err(AppError::Config(format!(
                "Unable to update Codex configuration {}: {error}",
                config_path.display()
            )))
        }
    }
}

fn replace_config(
    temporary_path: &Path,
    config_path: &Path,
    backup_path: &Path,
    had_configuration: bool,
) -> io::Result<()> {
    #[cfg(not(windows))]
    let _ = (backup_path, had_configuration);

    #[cfg(windows)]
    if had_configuration {
        fs::remove_file(config_path)?;
    }

    match fs::rename(temporary_path, config_path) {
        Ok(()) => Ok(()),
        Err(error) => {
            #[cfg(windows)]
            if had_configuration {
                let _ = fs::copy(backup_path, config_path);
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn appends_after_repeated_keys_and_keeps_toml_valid() {
        let contents = concat!(
            "[mcp_servers.node_repl.env]\n",
            "NODE_REPL_TRUSTED_CODE_PATHS = 'C:\\\\Users\\\\BroneKot\\\\.codex'\n",
            "CODEX_HOME = 'C:\\\\Users\\\\BroneKot\\\\.codex'\n",
            "\n",
            "[shell_environment_policy.set]\n",
            "NODE_REPL_TRUSTED_CODE_PATHS = 'C:\\\\Users\\\\BroneKot\\\\.codex'\n",
        );
        let command = r"C:\Users\BroneKot\.cargo\bin\codex-telegram-notify.exe";
        let updated = append_hook(contents, command);

        assert!(
            updated.rfind("NODE_REPL_TRUSTED_CODE_PATHS").unwrap()
                < updated.find("[[hooks.Stop]]").unwrap()
        );
        assert_eq!(updated.matches("[[hooks.Stop]]").count(), 1);
        assert!(has_command_hook(&updated, command).expect("valid TOML"));
    }

    #[test]
    fn does_not_duplicate_an_existing_command_hook() {
        let contents = concat!(
            "[hooks]\n",
            "[[hooks.Stop]]\n",
            "[[hooks.Stop.hooks]]\n",
            "type = \"command\"\n",
            "command = \"C:\\\\Users\\\\BroneKot\\\\.cargo\\\\bin\\\\codex-telegram-notify.exe\"\n",
            "timeout = 10\n",
        );
        let command = r"C:\Users\BroneKot\.cargo\bin\codex-telegram-notify.exe";
        assert!(has_command_hook(contents, command).expect("valid TOML"));
    }

    #[test]
    fn writes_backup_and_replaces_configuration_atomically() {
        let directory = tempdir().expect("tempdir");
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, "model = \"test\"\n").expect("config");
        let command = Path::new(r"C:\Users\BroneKot\.cargo\bin\codex-telegram-notify.exe");

        install_at(&config_path, command).expect("install");

        let updated = fs::read_to_string(&config_path).expect("updated config");
        let backup =
            fs::read_to_string(config_path.with_file_name("config.toml.codex-telegram-notify.bak"))
                .expect("backup");
        assert!(
            has_command_hook(&updated, command.to_string_lossy().as_ref()).expect("valid TOML")
        );
        assert_eq!(backup, "model = \"test\"\n");
    }
}
