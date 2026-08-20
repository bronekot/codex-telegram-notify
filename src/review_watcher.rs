use crate::config::{ConfigStore, RuntimeConfig};
use crate::error::AppError;
use crate::message::{build_review_notification, truncate_unicode};
use crate::paths::codex_home;
use crate::telegram::{HttpTelegramApi, SendMessageRequest, TelegramApi};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const STATE_FILE_NAME: &str = "review-watcher-state.json";
const LOCK_FILE_NAME: &str = "review-watcher.lock";
const SERVICE_NAME: &str = "codex-telegram-notify-review";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ReviewSummary {
    review_id: String,
    cwd: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    findings: Option<usize>,
    explanation: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SessionFileState {
    offset: u64,
    is_review: bool,
    review_id: Option<String>,
    parent_session_id: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    completion: Option<ReviewSummary>,
    notified: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WatcherState {
    files: BTreeMap<String, SessionFileState>,
}

#[derive(Debug, Deserialize)]
struct SessionRecord {
    #[serde(rename = "type")]
    record_type: String,
    payload: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct SessionMetaPayload {
    id: Option<String>,
    session_id: Option<String>,
    parent_thread_id: Option<String>,
    cwd: Option<PathBuf>,
    source: Option<SessionSource>,
}

#[derive(Debug, Deserialize)]
struct SessionSource {
    subagent: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TurnContextPayload {
    cwd: Option<PathBuf>,
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventMessagePayload {
    #[serde(rename = "type")]
    event_type: Option<String>,
    turn_id: Option<String>,
    last_agent_message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReviewResult {
    findings: Option<Vec<Value>>,
    overall_explanation: Option<String>,
}

pub async fn run(codex_home_override: Option<PathBuf>) -> Result<(), AppError> {
    let store = ConfigStore::discover()?;
    let codex_home = codex_home_override
        .or_else(codex_home)
        .ok_or_else(|| AppError::Config("Unable to determine Codex home directory".to_string()))?;
    let sessions_dir = codex_home.join("sessions");
    let config_dir = store.config_dir()?;
    fs::create_dir_all(&config_dir).map_err(|error| {
        AppError::Config(format!("Unable to create daemon state directory: {error}"))
    })?;
    restrict_unix_permissions(&config_dir, 0o700)?;

    let lock_path = config_dir.join(LOCK_FILE_NAME);
    let _lock = acquire_lock(&lock_path)?;
    let state_path = config_dir.join(STATE_FILE_NAME);
    let first_run = !state_path.exists();
    let mut watcher = ReviewWatcher::load(sessions_dir, state_path)?;
    let _ = watcher.scan_once(first_run)?;
    watcher.persist_if_dirty()?;

    eprintln!("Codex Telegram Notify review watcher is running.");
    loop {
        if let Err(error) = watcher.scan_once(false) {
            eprintln!("Review watcher scan failed: {error}");
        }
        if let Err(error) = watcher.send_pending(&store).await {
            eprintln!("Review watcher notification failed: {error}");
        }
        if let Err(error) = watcher.persist_if_dirty() {
            eprintln!("Review watcher state save failed: {error}");
        }

        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result.map_err(|_| AppError::Cancelled("Daemon stopped".to_string()))?;
                return Ok(());
            }
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    }
}

struct ReviewWatcher {
    sessions_dir: PathBuf,
    state_path: PathBuf,
    state: WatcherState,
    dirty: bool,
}

impl ReviewWatcher {
    fn load(sessions_dir: PathBuf, state_path: PathBuf) -> Result<Self, AppError> {
        let state = if state_path.exists() {
            let contents = fs::read_to_string(&state_path).map_err(|error| {
                AppError::Config(format!("Unable to read review watcher state: {error}"))
            })?;
            serde_json::from_str(&contents).map_err(|error| {
                AppError::Config(format!("Review watcher state is invalid JSON: {error}"))
            })?
        } else {
            WatcherState::default()
        };

        Ok(Self {
            sessions_dir,
            state_path,
            state,
            dirty: false,
        })
    }

    fn scan_once(&mut self, suppress_notifications: bool) -> Result<Vec<ReviewSummary>, AppError> {
        let files = discover_session_files(&self.sessions_dir).map_err(|error| {
            AppError::Config(format!("Unable to inspect Codex sessions: {error}"))
        })?;

        for path in files {
            self.scan_file(&path, suppress_notifications)?;
        }

        Ok(self
            .state
            .files
            .values()
            .filter_map(|state| {
                (!state.notified)
                    .then(|| state.completion.clone())
                    .flatten()
            })
            .collect())
    }

    fn scan_file(&mut self, path: &Path, suppress_notifications: bool) -> Result<(), AppError> {
        let key = path.to_string_lossy().into_owned();
        let mut file_state = self.state.files.get(&key).cloned().unwrap_or_default();
        let previous_offset = file_state.offset;
        let (bytes, next_offset) = read_appended_bytes(path, previous_offset).map_err(|error| {
            AppError::Config(format!(
                "Unable to read Codex session {}: {error}",
                path.display()
            ))
        })?;
        let complete_length = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(0);

        for line in bytes[..complete_length].split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_slice::<SessionRecord>(line) else {
                continue;
            };
            apply_record(&mut file_state, record);
        }

        let consumed_offset = previous_offset.saturating_add(complete_length as u64);
        let new_offset = if complete_length == bytes.len() {
            next_offset
        } else {
            consumed_offset
        };
        if file_state.offset != new_offset {
            file_state.offset = new_offset;
            self.dirty = true;
        }
        if suppress_notifications && file_state.completion.is_some() && !file_state.notified {
            file_state.notified = true;
            self.dirty = true;
        }
        self.state.files.insert(key, file_state);
        Ok(())
    }

    async fn send_pending(&mut self, store: &ConfigStore) -> Result<(), AppError> {
        let pending: Vec<ReviewSummary> = self
            .state
            .files
            .values()
            .filter_map(|state| {
                (!state.notified)
                    .then(|| state.completion.clone())
                    .flatten()
            })
            .collect();
        if pending.is_empty() {
            return Ok(());
        }

        let effective = store.effective()?;
        if !effective.enabled {
            return Ok(());
        }
        let runtime = effective.runtime()?;
        let api = HttpTelegramApi::new(runtime.bot_token.clone(), runtime.timeout)
            .map_err(|error| AppError::Telegram(error.user_message()))?;

        for summary in pending {
            notify_review_with_api(&summary, &runtime, &api).await?;
            for state in self.state.files.values_mut() {
                if state
                    .completion
                    .as_ref()
                    .is_some_and(|completion| completion.review_id == summary.review_id)
                {
                    state.notified = true;
                    self.dirty = true;
                }
            }
        }
        Ok(())
    }

    fn persist_if_dirty(&mut self) -> Result<(), AppError> {
        if !self.dirty {
            return Ok(());
        }
        let serialized = serde_json::to_vec_pretty(&self.state).map_err(|error| {
            AppError::Config(format!("Unable to serialize review watcher state: {error}"))
        })?;
        let temporary_path = self
            .state_path
            .with_file_name(format!(".review-watcher-state.{}.tmp", std::process::id()));
        let result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temporary_path)?;
            restrict_unix_permissions(&temporary_path, 0o600)
                .map_err(|_| io::Error::other("unable to restrict state permissions"))?;
            file.write_all(&serialized)?;
            file.sync_all()?;
            drop(file);
            replace_file(&temporary_path, &self.state_path)
        })();

        if result.is_err() {
            let _ = fs::remove_file(&temporary_path);
            return Err(AppError::Config(
                "Unable to save review watcher state".to_string(),
            ));
        }
        self.dirty = false;
        Ok(())
    }
}

async fn notify_review_with_api(
    summary: &ReviewSummary,
    config: &RuntimeConfig,
    api: &dyn TelegramApi,
) -> Result<(), AppError> {
    let text = build_review_notification(
        summary.cwd.as_deref().map(Path::new),
        summary.model.as_deref(),
        summary.effort.as_deref(),
        summary.findings,
        summary.explanation.as_deref(),
        config.max_length,
    );
    api.send_message(SendMessageRequest {
        chat_id: config.chat_id,
        text,
        disable_notification: config.silent,
    })
    .await
    .map_err(|error| AppError::Telegram(error.user_message()))
}

fn apply_record(state: &mut SessionFileState, record: SessionRecord) {
    let Some(payload) = record.payload else {
        return;
    };

    match record.record_type.as_str() {
        "session_meta" => {
            let Ok(meta) = serde_json::from_value::<SessionMetaPayload>(payload) else {
                return;
            };
            if meta
                .source
                .as_ref()
                .and_then(|source| source.subagent.as_deref())
                != Some("review")
            {
                return;
            }
            state.is_review = true;
            state.review_id = meta.id;
            state.parent_session_id = meta.session_id.or(meta.parent_thread_id);
            state.cwd = meta.cwd.map(|path| path.to_string_lossy().into_owned());
        }
        "turn_context" if state.is_review => {
            let Ok(context) = serde_json::from_value::<TurnContextPayload>(payload) else {
                return;
            };
            if state.cwd.is_none() {
                state.cwd = context.cwd.map(|path| path.to_string_lossy().into_owned());
            }
            state.model = context.model.or_else(|| state.model.take());
            state.effort = context.effort.or_else(|| state.effort.take());
        }
        "event_msg" if state.is_review => {
            let Ok(event) = serde_json::from_value::<EventMessagePayload>(payload) else {
                return;
            };
            if event.event_type.as_deref() != Some("task_complete") || state.completion.is_some() {
                return;
            }
            let review_id = state
                .review_id
                .clone()
                .or(event.turn_id)
                .unwrap_or_else(|| "unknown-review".to_string());
            let (findings, explanation) = summarize_review(event.last_agent_message.as_deref());
            state.completion = Some(ReviewSummary {
                review_id,
                cwd: state.cwd.clone(),
                model: state.model.clone(),
                effort: state.effort.clone(),
                findings,
                explanation,
            });
        }
        _ => {}
    }
}

fn summarize_review(message: Option<&str>) -> (Option<usize>, Option<String>) {
    let Some(message) = message.map(str::trim).filter(|value| !value.is_empty()) else {
        return (None, None);
    };
    if let Ok(result) = serde_json::from_str::<ReviewResult>(message) {
        return (
            result.findings.map(|findings| findings.len()),
            result.overall_explanation.and_then(normalize_explanation),
        );
    }
    (None, Some(truncate_unicode(message, 1000)))
}

fn normalize_explanation(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then(|| truncate_unicode(&value, 1000))
}

fn discover_session_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    collect_session_files(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_session_files(root: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_session_files(&path, files)?;
        } else if file_type.is_file()
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("rollout-"))
            && path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        {
            files.push(path);
        }
    }
    Ok(())
}

fn read_appended_bytes(path: &Path, offset: u64) -> io::Result<(Vec<u8>, u64)> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let offset = if offset > length { 0 } else { offset };
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let next_offset = offset.saturating_add(bytes.len() as u64);
    Ok((bytes, next_offset))
}

fn acquire_lock(path: &Path) -> Result<File, AppError> {
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|error| AppError::Config(format!("Unable to open daemon lock: {error}")))?;
    restrict_unix_permissions(path, 0o600)?;
    file.try_lock_exclusive()
        .map_err(|_| AppError::Config("Review watcher is already running".to_string()))?;
    Ok(file)
}

fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)]
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(source, destination)
}

fn restrict_unix_permissions(path: &Path, mode: u32) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| {
            AppError::Config(format!(
                "Unable to restrict daemon file permissions: {error}"
            ))
        })?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

fn validate_install_prerequisites() -> Result<(), AppError> {
    let store = ConfigStore::discover()?;
    let file = store.load_file()?;
    if file
        .bot_token
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
        || file.chat_id.is_none()
    {
        return Err(AppError::Config(
            "The daemon needs a saved Telegram configuration. Run setup first.".to_string(),
        ));
    }
    let effective = store.effective()?;
    let _ = effective.runtime()?;
    Ok(())
}

pub fn install_daemon() -> Result<(), AppError> {
    validate_install_prerequisites()?;
    let executable = std::env::current_exe().map_err(|error| {
        AppError::Config(format!("Unable to determine executable path: {error}"))
    })?;
    let codex_home = codex_home()
        .ok_or_else(|| AppError::Config("Unable to determine Codex home directory".to_string()))?;
    platform::install(&executable, &codex_home)
}

pub fn uninstall_daemon() -> Result<(), AppError> {
    platform::uninstall()
}

pub fn daemon_status() -> Result<(), AppError> {
    platform::status()
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{restrict_unix_permissions, AppError, Path, PathBuf, SERVICE_NAME};
    use directories::BaseDirs;
    use std::env;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::process::{Command, Output};

    const PROXY_VARIABLES: &[&str] = &[
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
    ];

    pub fn install(executable: &Path, codex_home: &Path) -> Result<(), AppError> {
        let path = unit_path()?;
        let parent = path.parent().ok_or_else(|| {
            AppError::Config("Unable to determine systemd user unit directory".to_string())
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            AppError::Config(format!(
                "Unable to create systemd user unit directory: {error}"
            ))
        })?;
        let proxy_environment = current_proxy_environment();
        let unit = render_unit(executable, codex_home, &proxy_environment);
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .map_err(|error| AppError::Config(format!("Unable to write systemd unit: {error}")))?;
        file.write_all(unit.as_bytes())
            .map_err(|error| AppError::Config(format!("Unable to write systemd unit: {error}")))?;
        file.sync_all()
            .map_err(|error| AppError::Config(format!("Unable to save systemd unit: {error}")))?;
        restrict_unix_permissions(&path, 0o600)?;

        systemctl(&["--user", "daemon-reload"])?;
        systemctl(&["--user", "enable", SERVICE_NAME])?;
        systemctl(&["--user", "restart", SERVICE_NAME])?;
        println!("Daemon installed and started: {SERVICE_NAME}");
        Ok(())
    }

    pub fn uninstall() -> Result<(), AppError> {
        let path = unit_path()?;
        if !path.exists() {
            println!("Daemon is not installed.");
            return Ok(());
        }
        let _ = systemctl(&["--user", "disable", "--now", SERVICE_NAME]);
        fs::remove_file(&path)
            .map_err(|error| AppError::Config(format!("Unable to remove systemd unit: {error}")))?;
        systemctl(&["--user", "daemon-reload"])?;
        println!("Daemon uninstalled: {SERVICE_NAME}");
        Ok(())
    }

    pub fn status() -> Result<(), AppError> {
        let path = unit_path()?;
        if !path.exists() {
            println!("Daemon is not installed.");
            return Ok(());
        }
        let output = systemctl_output(&["--user", "status", "--no-pager", SERVICE_NAME])?;
        print!("{}", String::from_utf8_lossy(&output.stdout));
        if !output.status.success() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }
        Ok(())
    }

    fn unit_path() -> Result<PathBuf, AppError> {
        let config_home = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| BaseDirs::new().map(|dirs| dirs.home_dir().join(".config")))
            .ok_or_else(|| {
                AppError::Config("Unable to determine XDG config directory".to_string())
            })?;
        Ok(config_home
            .join("systemd")
            .join("user")
            .join(format!("{SERVICE_NAME}.service")))
    }

    fn current_proxy_environment() -> Vec<(String, String)> {
        PROXY_VARIABLES
            .iter()
            .filter_map(|name| {
                env::var_os(name)
                    .filter(|value| !value.is_empty())
                    .map(|value| ((*name).to_string(), value.to_string_lossy().into_owned()))
            })
            .collect()
    }

    fn render_unit(
        executable: &Path,
        codex_home: &Path,
        proxy_environment: &[(String, String)],
    ) -> String {
        let environment = proxy_environment
            .iter()
            .map(|(name, value)| {
                format!(
                    "Environment={}\n",
                    quote_systemd_argument(&format!("{name}={value}"))
                )
            })
            .collect::<String>();
        format!(
            "[Unit]\nDescription=Codex Telegram Notify review watcher\nAfter=default.target\n\n[Service]\nType=simple\n{}ExecStart={} daemon run --codex-home {}\nRestart=always\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
            environment,
            quote_systemd_argument(&executable.to_string_lossy()),
            quote_systemd_argument(&codex_home.to_string_lossy())
        )
    }

    fn quote_systemd_argument(value: &str) -> String {
        let mut quoted = String::from("\"");
        for character in value.chars() {
            match character {
                '\\' => quoted.push_str("\\\\"),
                '"' => quoted.push_str("\\\""),
                _ => quoted.push(character),
            }
        }
        quoted.push('"');
        quoted
    }

    fn systemctl(args: &[&str]) -> Result<(), AppError> {
        let output = systemctl_output(args)?;
        if output.status.success() {
            return Ok(());
        }
        Err(AppError::Config(format_command_error("systemctl", &output)))
    }

    fn systemctl_output(args: &[&str]) -> Result<Output, AppError> {
        Command::new("systemctl")
            .args(args)
            .output()
            .map_err(|error| AppError::Config(format!("Unable to run systemctl: {error}")))
    }

    fn format_command_error(command: &str, output: &Output) -> String {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            format!("{command} exited with status {}", output.status)
        } else {
            format!("{command} failed: {stderr}")
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn quotes_binary_paths_for_systemd() {
            let unit = render_unit(
                Path::new("/home/user/Codex Tools/codex"),
                Path::new("/home/user/.codex"),
                &[],
            );
            assert!(unit.contains(
                "ExecStart=\"/home/user/Codex Tools/codex\" daemon run --codex-home \"/home/user/.codex\""
            ));
        }

        #[test]
        fn writes_proxy_environment_to_systemd_unit() {
            let proxy_environment = vec![(
                "HTTPS_PROXY".to_string(),
                "http://proxy.example:8080".to_string(),
            )];
            let unit = render_unit(
                Path::new("/home/user/codex"),
                Path::new("/home/user/.codex"),
                &proxy_environment,
            );
            assert!(unit.contains("Environment=\"HTTPS_PROXY=http://proxy.example:8080\""));
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{AppError, Path, SERVICE_NAME};
    use std::process::{Command, Output};

    const TASK_NAME: &str = "Codex Telegram Notify Review Watcher";

    pub fn install(executable: &Path, codex_home: &Path) -> Result<(), AppError> {
        let action = format!(
            "\"{}\" daemon run --codex-home \"{}\"",
            executable.display(),
            codex_home.display()
        );
        let output = schtasks(&[
            "/Create", "/TN", TASK_NAME, "/SC", "ONLOGON", "/TR", &action, "/RL", "LIMITED", "/F",
        ])?;
        ensure_success("schtasks", &output)?;
        let output = schtasks(&["/Run", "/TN", TASK_NAME])?;
        ensure_success("schtasks", &output)?;
        println!("Daemon installed and started: {SERVICE_NAME}");
        Ok(())
    }

    pub fn uninstall() -> Result<(), AppError> {
        let query = schtasks(&["/Query", "/TN", TASK_NAME, "/FO", "LIST"])?;
        if !query.status.success() {
            println!("Daemon is not installed.");
            return Ok(());
        }
        let _ = schtasks(&["/End", "/TN", TASK_NAME]);
        let output = schtasks(&["/Delete", "/TN", TASK_NAME, "/F"])?;
        ensure_success("schtasks", &output)?;
        println!("Daemon uninstalled: {SERVICE_NAME}");
        Ok(())
    }

    pub fn status() -> Result<(), AppError> {
        let output = schtasks(&["/Query", "/TN", TASK_NAME, "/FO", "LIST"])?;
        if !output.status.success() {
            println!("Daemon is not installed.");
            return Ok(());
        }
        print!("{}", String::from_utf8_lossy(&output.stdout));
        Ok(())
    }

    fn schtasks(args: &[&str]) -> Result<Output, AppError> {
        Command::new("schtasks.exe")
            .args(args)
            .output()
            .map_err(|error| AppError::Config(format!("Unable to run schtasks: {error}")))
    }

    fn ensure_success(command: &str, output: &Output) -> Result<(), AppError> {
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let message = if !stderr.is_empty() { stderr } else { stdout };
        Err(AppError::Config(format!("{command} failed: {message}")))
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod platform {
    use super::{AppError, Path};

    pub fn install(_executable: &Path, _codex_home: &Path) -> Result<(), AppError> {
        Err(AppError::Config(
            "Daemon installation is currently supported on Linux and Windows only".to_string(),
        ))
    }

    pub fn uninstall() -> Result<(), AppError> {
        Err(AppError::Config(
            "Daemon installation is currently supported on Linux and Windows only".to_string(),
        ))
    }

    pub fn status() -> Result<(), AppError> {
        Err(AppError::Config(
            "Daemon installation is currently supported on Linux and Windows only".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn review_meta(id: &str) -> String {
        serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": id,
                "session_id": "parent-session",
                "cwd": "/home/user/project",
                "source": {"subagent": "review"}
            }
        })
        .to_string()
    }

    #[test]
    fn detects_completed_review_and_ignores_regular_session() {
        let directory = tempdir().expect("tempdir");
        let sessions = directory.path().join("sessions");
        fs::create_dir_all(sessions.join("2026/08/20")).expect("sessions");
        let review_path = sessions.join("2026/08/20/rollout-review.jsonl");
        let regular_path = sessions.join("2026/08/20/rollout-regular.jsonl");
        let turn_context = serde_json::json!({
            "type": "turn_context",
            "payload": {
                "cwd": "/home/user/project",
                "model": "gpt-5.6-luna",
                "effort": "max"
            }
        });
        let task_complete = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "task_complete",
                "turn_id": "turn-review",
                "last_agent_message": "{\"findings\":[],\"overall_explanation\":\"Всё хорошо.\"}"
            }
        });
        fs::write(
            &review_path,
            format!(
                "{}\n{}\n{}\n",
                review_meta("review-1"),
                turn_context,
                task_complete
            ),
        )
        .expect("review");
        fs::write(
            &regular_path,
            format!(
                "{}\n{}\n",
                review_meta("regular-1").replace("review", "cli"),
                task_complete
            ),
        )
        .expect("regular");

        let state_path = directory.path().join("state.json");
        let mut watcher = ReviewWatcher::load(sessions, state_path).expect("watcher");
        let pending = watcher.scan_once(false).expect("scan");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].review_id, "review-1");
        assert_eq!(pending[0].model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(pending[0].effort.as_deref(), Some("max"));
        assert_eq!(pending[0].findings, Some(0));
        assert_eq!(pending[0].explanation.as_deref(), Some("Всё хорошо."));
    }

    #[test]
    fn waits_for_a_partial_last_line() {
        let directory = tempdir().expect("tempdir");
        let sessions = directory.path().join("sessions/2026/08/20");
        fs::create_dir_all(&sessions).expect("sessions");
        let path = sessions.join("rollout-review.jsonl");
        fs::write(&path, format!("{}\n", review_meta("review-2"))).expect("meta");
        let mut watcher = ReviewWatcher::load(
            directory.path().join("sessions"),
            directory.path().join("state.json"),
        )
        .expect("watcher");
        assert!(watcher.scan_once(false).expect("first scan").is_empty());
        let task = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "task_complete",
                "turn_id": "turn-review",
                "last_agent_message": "{\"findings\":[{}]}"
            }
        })
        .to_string();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append")
            .write_all(task.as_bytes())
            .expect("partial");
        assert!(watcher.scan_once(false).expect("partial scan").is_empty());
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append newline")
            .write_all(b"\n")
            .expect("newline");
        let pending = watcher.scan_once(false).expect("complete scan");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].findings, Some(1));
    }

    #[test]
    fn suppresses_existing_reviews_only_on_first_run() {
        let directory = tempdir().expect("tempdir");
        let sessions = directory.path().join("sessions/2026/08/20");
        fs::create_dir_all(&sessions).expect("sessions");
        let path = sessions.join("rollout-review.jsonl");
        let task_complete = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "task_complete",
                "turn_id": "turn-review",
                "last_agent_message": "{\"findings\":[]}"
            }
        });
        fs::write(
            &path,
            format!("{}\n{}\n", review_meta("review-3"), task_complete),
        )
        .expect("review");

        let state_path = directory.path().join("state.json");
        let mut watcher =
            ReviewWatcher::load(sessions.clone(), state_path.clone()).expect("watcher");
        assert!(watcher.scan_once(true).expect("first scan").is_empty());
        watcher.persist_if_dirty().expect("persist");

        let mut restarted = ReviewWatcher::load(sessions, state_path).expect("restart");
        assert!(restarted.scan_once(false).expect("restart scan").is_empty());
    }
}
