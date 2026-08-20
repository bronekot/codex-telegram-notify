# codex-telegram-notify

Small Rust CLI that sends Telegram notifications when a Codex `Stop` hook
finishes the main agent turn and when a Codex `/review` session completes.
Review completion is watched from Codex's local session JSONL files because
`/review` is a dedicated review session rather than a `SubagentStop` hook.

## Install

With a current stable Rust toolchain:

```text
cargo install --path .
```

The binary is normally installed into Cargo's `bin` directory. Run the setup
once:

```text
codex-telegram-notify setup
```

Setup asks for the bot token without echoing it, checks the token, waits for a
new `/start` message, discovers the chat ID, sends a test message, and saves
the configuration. If messages arrive from more than one chat, setup displays
the candidates and asks for a number. Press `Ctrl+C` to cancel; setup waits no
longer than five minutes.

The setup command does not edit Codex configuration. Add the `Stop` hook to the
Codex config layer you use, then review/trust it in Codex with `/hooks`:

```toml
[[hooks.Stop]]

[[hooks.Stop.hooks]]
type = "command"
command = "/home/user/.cargo/bin/codex-telegram-notify"
timeout = 10
```

Only `Stop` should be configured for this notifier. Do not add
`SubagentStop`, `PreToolUse`, or `PostToolUse` entries for it.

Install the review watcher separately. It does not change how Codex is started
or how `/review` is invoked:

```text
codex-telegram-notify daemon install
codex-telegram-notify daemon status
codex-telegram-notify daemon uninstall
```

On Linux this installs a per-user `systemd` service. On Windows it installs a
per-user Task Scheduler task that starts at logon. The watcher reads only
Codex session metadata and the final review result, filters
`source.subagent = "review"`, and stores offsets/deduplication state in the
application configuration directory. The first daemon start ignores reviews
that had already completed; new reviews are notified once.

The daemon uses the saved configuration from `setup`. Do not rely only on
temporary shell environment variables for its bot token or chat ID.

## Commands

```text
codex-telegram-notify setup
codex-telegram-notify test
codex-telegram-notify daemon install
codex-telegram-notify daemon status
codex-telegram-notify daemon uninstall

codex-telegram-notify config show
codex-telegram-notify config get chat-id
codex-telegram-notify config set chat-id -1001234567890
codex-telegram-notify config set enabled true
codex-telegram-notify config set silent false
codex-telegram-notify config set max-length 3500
codex-telegram-notify config set timeout 5
codex-telegram-notify config reset
```

`config show` and `config get bot-token` only show a masked token. The token
cannot be set through command-line arguments; use `setup` or
`CODEX_TELEGRAM_BOT_TOKEN` so it does not end up in shell history or process
arguments.

## Configuration

On Linux the file is:

```text
$XDG_CONFIG_HOME/codex-telegram-notify/config.toml
```

If `XDG_CONFIG_HOME` is not set, the default is
`~/.config/codex-telegram-notify/config.toml`. macOS and Windows use their
normal per-user application configuration directories.

Example:

```toml
bot_token = "123456:ABC..."
chat_id = 123456789
enabled = true
max_length = 3500
timeout_seconds = 5
silent = false
always_success = true
```

Environment overrides have priority over the file:

```text
CODEX_TELEGRAM_BOT_TOKEN
CODEX_TELEGRAM_CHAT_ID
CODEX_TELEGRAM_ENABLED
CODEX_TELEGRAM_MAX_LENGTH
CODEX_TELEGRAM_TIMEOUT
CODEX_TELEGRAM_SILENT
CODEX_TELEGRAM_ALWAYS_SUCCESS
```

`chat_id` is stored as a signed 64-bit integer, so Telegram group and
supergroup IDs are supported.

## Hook behavior

With no command-line arguments, the binary reads the Codex hook JSON from
stdin. It uses `cwd`, `model`, `effort`, and `last_assistant_message` to produce
a plain text Telegram message without Markdown or HTML parsing. When the
current Codex version does not include `effort` in the hook payload, the
notifier falls back to `model_reasoning_effort` in
`$CODEX_HOME/config.toml` (or `~/.codex/config.toml`). Unknown payload fields
are ignored.

The hidden `probe-subagent` command is available for diagnosing actual
`SubagentStop` events. It records only lifecycle metadata (`agent_type`, IDs,
project, and model) in
`$CODEX_HOME/codex-telegram-notify-subagent-events.jsonl` (or
`~/.codex/codex-telegram-notify-subagent-events.jsonl`) and never records or
sends the assistant message. It is not used to detect `/review` completion.

The default `always_success = true` means Telegram or configuration failures
are logged to `stderr` but do not fail Codex's hook. Set it to `false` when
real hook exit codes are desired:

```text
0 — success
1 — configuration or setup error
2 — invalid hook payload
3 — Telegram/API/network error
```

The default notification length is 3500 Unicode characters and may be set up
to Telegram's 4096-character text limit.

## Development

```text
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```
