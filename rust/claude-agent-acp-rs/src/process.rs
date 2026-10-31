//! Process transport: spawning the `claude` child, its argv/env, the kill
//! ladder, process-group teardown, and the stderr tail (Decision D12, items
//! 3.1–3.6).
//!
//! This is the **only** file in the crate allowed to use `cfg(unix)` /
//! `cfg(windows)` (guard G9). The crate stays `#![forbid(unsafe_code)]` by
//! using only safe APIs (D12, R33): tokio's `process_group(0)`,
//! `creation_flags`, `kill_on_drop`, and `nix::sys::signal::killpg`.
//!
//! Kill ladder (D12):
//!
//! ```text
//! stdin EOF (forwarded abort) → stdin_close_wait (2 s)
//!   → SIGTERM to the process group → term_to_kill_wait (5 s)
//!   → SIGKILL to the process group
//! ```
//!
//! A user abort is forwarded via the stdin pipe (the child exits on stdin EOF,
//! INV-28) and **never** hard-kills directly; the ladder is only a safety net.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;

use crate::codec;

/// The platform a binary name is being resolved for (3.2 / 3.T10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    /// `claude.exe` on Windows.
    Windows,
    /// `claude` on Linux and macOS.
    Unix,
}

/// Resolution failure for the `claude` executable (3.2).
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no claude executable found: `claude_path` not set, `CLAUDE_CODE_EXECUTABLE` not set, and `{name}` is not on PATH")]
    NotFound { name: String },
}

/// Grace constants for the process lifecycle (Decision D12 / B1).
#[derive(Debug, Clone)]
pub struct Timings {
    /// Wait after closing stdin before SIGTERM. Default 2 s.
    pub stdin_close_wait: Duration,
    /// Wait after SIGTERM before SIGKILL. Default 5 s.
    pub term_to_kill_wait: Duration,
    /// Force-cancel grace for a wedged stream (R29); unused by phase 3.
    pub force_cancel_grace: Duration,
    /// Drain bound for stderr after the child exits. Default 200 ms.
    pub stderr_drain_cap: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            stdin_close_wait: Duration::from_secs(2),
            term_to_kill_wait: Duration::from_secs(5),
            force_cancel_grace: Duration::from_secs(30),
            stderr_drain_cap: Duration::from_millis(200),
        }
    }
}

/// Resolve the path of the `claude` binary (3.2).
///
/// Precedence: `claude_path` (an explicit option) → `CLAUDE_CODE_EXECUTABLE`
/// env var → `PATH` lookup of the platform binary name. Pure function taking an
/// injected [`Os`] so it is unit-testable without touching the OS (3.T10).
pub fn resolve_binary(
    claude_path: Option<&Path>,
    executable_env: Option<&str>,
    path: &str,
    os: Os,
) -> Result<PathBuf, ResolveError> {
    if let Some(p) = claude_path {
        return Ok(p.to_path_buf());
    }
    if let Some(env) = executable_env {
        if !env.is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    let name = match os {
        Os::Windows => "claude.exe",
        Os::Unix => "claude",
    };
    if path
        .split(':')
        .any(|dir| !dir.is_empty() && Path::new(dir).join(name).exists())
    {
        Ok(PathBuf::from(name))
    } else {
        Err(ResolveError::NotFound {
            name: name.to_string(),
        })
    }
}

/// The values needed to build the child's argv (B2 table, item 3.1).
///
/// Conditional flags are `Option`/`bool`; the fixed SDK prefix and
/// `--setting-sources=user,project,local` are always present.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// Explicit path to the `claude` binary; `None` → `CLAUDE_CODE_EXECUTABLE`
    /// → PATH.
    pub claude_path: Option<PathBuf>,
    /// Extra env vars merged over the inherited environment.
    pub extra_env: Vec<(String, String)>,
    /// Working directory for the child.
    pub default_cwd: Option<PathBuf>,
    /// `--session-id=<uuid>` for a fresh session.
    pub session_id: Option<String>,
    /// `--resume=<id>` for `session/load`.
    pub resume: Option<String>,
    /// `--model <m>` (conditional, B2 / phase 8).
    pub model: Option<String>,
    /// `--effort <e>` (rust-parity-fixes Decision 2).
    pub effort: Option<String>,
    /// `--max-turns <n>`.
    pub max_turns: Option<u32>,
    /// `--max-budget-usd <n>`.
    pub max_budget_usd: Option<f64>,
    /// `--agent <a>`.
    pub agent: Option<String>,
    /// `--betas=a,b`.
    pub betas: Vec<String>,
    /// `--fallback-model <m>`.
    pub fallback_model: Option<String>,
    /// `--include-hook-events`.
    pub include_hook_events: bool,
    /// `--strict-mcp-config`.
    pub strict_mcp_config: bool,
    /// `--project-config-root <r>`.
    pub project_config_root: Option<String>,
    /// `--add-dir=<d>` (one per entry).
    pub additional_directories: Vec<PathBuf>,
    /// `--allowedTools <csv>`.
    pub allowed_tools: Vec<String>,
    /// `--permission-mode <mode>`.
    pub permission_mode: Option<String>,
    /// `--setting-sources=<csv>` (default `user,project,local`).
    pub setting_sources: Vec<String>,
    /// `--disallowedTools <csv>`.
    pub disallowed_tools: Vec<String>,
    /// `--tools default` / `--tools <csv>`; `None` omits the flag.
    pub tools: Option<Vec<String>>,
    /// User `extraArgs` in insertion order; a `None` value emits a bare
    /// `--key`, otherwise `--key`,`value` (or `--key=value` when the value
    /// starts with `-`). Emitted after `--session-id`.
    pub extra_args: Vec<(String, Option<String>)>,
    /// The SDK's programmatic settings tier (`--settings`): appended after the
    /// user `extraArgs` and `--replay-user-messages`, or overriding a user
    /// `settings` key in place (O-49/O-129/O-130).
    pub settings_arg: Option<String>,
    /// `--include-partial-messages`.
    pub include_partial_messages: bool,
    /// `--permission-prompt-tool stdio`.
    pub permission_prompt_tool: bool,
    /// `--allow-dangerously-skip-permissions`.
    pub allow_dangerously_skip_permissions: bool,
    /// `--replay-user-messages` (plus the SDK's trailing empty value).
    pub replay_user_messages: bool,
    /// `--thinking=adaptive|disabled`, `--max-thinking-tokens=<n>` and
    /// `--thinking-display=<d>` (O-62/O-117/O-118).
    pub thinking: Option<crate::sdk_options::ThinkingConfig>,
    /// The `--mcp-config` JSON payload (WB-13/O-131).
    pub mcp_config: Option<String>,
    /// `_meta.claudeCode.options.executableArgs`, prepended to the SDK argv.
    pub executable_args: Vec<String>,
    /// `--plugin-dir` / `--plugin-dir-no-mcp` entries (O-123).
    pub plugins: Vec<crate::sdk_options::Plugin>,
    /// `--await-initialize` when plugins are delivered over initialize.
    pub plugin_delivery: crate::sdk_options::PluginDelivery,
    /// `--no-session-persistence` when explicitly `persistSession:false` (O-124).
    pub persist_session: Option<bool>,
    /// Environment snapshot for executable resolution.
    pub env_snapshot: crate::env_snapshot::EnvSnapshot,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            claude_path: None,
            extra_env: Vec::new(),
            default_cwd: None,
            session_id: None,
            resume: None,
            model: None,
            effort: None,
            max_turns: None,
            max_budget_usd: None,
            agent: None,
            betas: Vec::new(),
            fallback_model: None,
            include_hook_events: false,
            strict_mcp_config: false,
            project_config_root: None,
            additional_directories: Vec::new(),
            allowed_tools: Vec::new(),
            permission_mode: None,
            setting_sources: vec!["user".into(), "project".into(), "local".into()],
            disallowed_tools: Vec::new(),
            tools: None,
            extra_args: Vec::new(),
            settings_arg: None,
            include_partial_messages: false,
            permission_prompt_tool: false,
            allow_dangerously_skip_permissions: false,
            replay_user_messages: false,
            thinking: None,
            mcp_config: None,
            executable_args: Vec::new(),
            plugins: Vec::new(),
            plugin_delivery: crate::sdk_options::PluginDelivery::Argv,
            persist_session: None,
            env_snapshot: crate::env_snapshot::EnvSnapshot::default(),
        }
    }
}

/// Build the child argv for the given options (B2 table).
///
/// Order reproduces the SDK's `ProcessTransport` argv at `0.94.0` (verified
/// against `porting/fixtures/*.argv.json` by 3.T1). Valued flags use
/// `--flag=value` (E-13): `--model=`, `--permission-prompt-tool=stdio`,
/// `--disallowedTools=<csv>`, `--tools=<csv|default>` (empty -> `--tools=`),
/// `--permission-mode=<m>`; `--resume=`, `--setting-sources=`, `--session-id=`
/// and the trailing `""` after `--replay-user-messages` are unchanged.
pub fn build_argv(opts: &SpawnOptions) -> Vec<String> {
    // `executableArgs` are prepended before the SDK's fixed prefix
    // (`sdk.mjs` ProcessTransport: `[...executableArgs, ...W]`).
    let mut argv: Vec<String> = opts.executable_args.clone();
    argv.extend(
        [
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json",
        ]
        .map(String::from),
    );

    if let Some(thinking) = &opts.thinking {
        use crate::sdk_options::ThinkingConfig;
        match thinking {
            ThinkingConfig::Enabled {
                budget_tokens: Some(budget),
                ..
            } => argv.push(format!("--max-thinking-tokens={budget}")),
            ThinkingConfig::Enabled {
                budget_tokens: None,
                ..
            }
            | ThinkingConfig::Adaptive { .. } => argv.push("--thinking=adaptive".into()),
            ThinkingConfig::Disabled => argv.push("--thinking=disabled".into()),
        }
        let display = match thinking {
            ThinkingConfig::Adaptive { display } | ThinkingConfig::Enabled { display, .. } => {
                display.as_deref()
            }
            ThinkingConfig::Disabled => None,
        };
        if let Some(display) = display {
            argv.push(format!("--thinking-display={display}"));
        }
    }

    if let Some(effort) = &opts.effort {
        argv.push(format!("--effort={effort}"));
    }

    if let Some(max_turns) = &opts.max_turns {
        argv.push(format!("--max-turns={max_turns}"));
    }

    if let Some(max_budget) = &opts.max_budget_usd {
        argv.push(format!("--max-budget-usd={max_budget}"));
    }

    if let Some(model) = &opts.model {
        argv.push(format!("--model={model}"));
    }

    if let Some(agent) = &opts.agent {
        argv.push(format!("--agent={agent}"));
    }

    if !opts.betas.is_empty() {
        argv.push(format!("--betas={}", opts.betas.join(",")));
    }

    if opts.permission_prompt_tool {
        argv.push("--permission-prompt-tool=stdio".into());
    }

    if let Some(resume) = &opts.resume {
        argv.push(format!("--resume={resume}"));
    }

    if !opts.allowed_tools.is_empty() {
        argv.push(format!("--allowedTools={}", opts.allowed_tools.join(",")));
    }

    if !opts.disallowed_tools.is_empty() {
        argv.push(format!(
            "--disallowedTools={}",
            opts.disallowed_tools.join(",")
        ));
    }

    match &opts.tools {
        Some(tools) if tools.is_empty() => {
            argv.push("--tools=".into());
        }
        Some(tools) => {
            argv.push(format!("--tools={}", tools.join(",")));
        }
        None => {
            argv.push("--tools=default".into());
        }
    }

    // `oy("--mcp-config", JSON.stringify({mcpServers}))`: the payload does not
    // start with `-`, so it is a separate token (O-131 / WB-13).
    if let Some(config) = &opts.mcp_config {
        argv.push("--mcp-config".into());
        argv.push(config.clone());
    }

    argv.push(format!(
        "--setting-sources={}",
        opts.setting_sources.join(",")
    ));

    if opts.strict_mcp_config {
        argv.push("--strict-mcp-config".into());
    }

    if let Some(mode) = &opts.permission_mode {
        argv.push(format!("--permission-mode={mode}"));
    }

    if opts.allow_dangerously_skip_permissions {
        argv.push("--allow-dangerously-skip-permissions".into());
    }

    if let Some(fallback) = &opts.fallback_model {
        argv.push(format!("--fallback-model={fallback}"));
    }

    if opts.include_hook_events {
        argv.push("--include-hook-events".into());
    }

    if opts.include_partial_messages {
        argv.push("--include-partial-messages".into());
    }

    if let Some(project_root) = &opts.project_config_root {
        argv.push(format!("--project-config-root={project_root}"));
    }

    for dir in &opts.additional_directories {
        argv.push(format!("--add-dir={}", dir.display()));
    }

    // Plugins: `--await-initialize` when delivered over initialize, else one
    // `--plugin-dir`/`--plugin-dir-no-mcp` per local plugin (O-123/O-158).
    if opts.plugin_delivery == crate::sdk_options::PluginDelivery::Initialize
        && !opts.plugins.is_empty()
    {
        argv.push("--await-initialize".into());
    } else {
        for plugin in &opts.plugins {
            // Only `local` plugins reach here (validated before spawning).
            let flag = if plugin.skip_mcp_discovery {
                "--plugin-dir-no-mcp"
            } else {
                "--plugin-dir"
            };
            argv.push(format!(
                "{flag}={}",
                plugin.path.as_deref().unwrap_or_default()
            ));
        }
    }

    if let Some(session_id) = &opts.session_id {
        argv.push(format!("--session-id={session_id}"));
    }

    if opts.persist_session == Some(false) {
        argv.push("--no-session-persistence".into());
    }

    for (key, value) in &opts.extra_args {
        let overridden = if key == "settings" {
            opts.settings_arg.as_deref()
        } else {
            None
        };
        push_extra_arg(&mut argv, key, overridden.or(value.as_deref()));
    }

    // The SDK's `extraArgs` always ends with `replay-user-messages`, and the
    // programmatic `settings` tier is appended after it (or overrides an
    // existing user `settings` key in place).
    if opts.replay_user_messages {
        push_extra_arg(&mut argv, "replay-user-messages", Some(""));
    }
    if let Some(settings) = &opts.settings_arg {
        if !opts.extra_args.iter().any(|(key, _)| key == "settings") {
            push_extra_arg(&mut argv, "settings", Some(settings));
        }
    }

    argv
}

/// Encode one `extraArgs` entry exactly like the SDK's `oy` helper: a `None`
/// value is a bare `--key`; a value longer than one char that starts with `-`
/// becomes `--key=value`; anything else (including `""`) is `--key`, `value`.
fn push_extra_arg(argv: &mut Vec<String>, key: &str, value: Option<&str>) {
    match value {
        None => argv.push(format!("--{key}")),
        Some(v) if v.len() > 1 && v.starts_with('-') => argv.push(format!("--{key}={v}")),
        Some(v) => {
            argv.push(format!("--{key}"));
            argv.push(v.to_string());
        }
    }
}

/// Errors produced while spawning or driving the child process.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("failed to spawn `{binary}`: {source}")]
    Spawn {
        binary: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write to child stdin: {0}")]
    Stdin(std::io::Error),
    #[error("failed to signal child: {0}")]
    Signal(std::io::Error),
    #[error("child exited early (no result frame): {tail}")]
    ExitedEarly { tail: String },
    #[error("failed to resolve the claude executable: {0}")]
    Resolve(#[from] ResolveError),
}

/// A spawned child process with its stdin writer, the line-codec stream of its
/// stdout, and a rolling stderr tail.
///
/// The child is spawned in its own process group (unix) or job object +
/// `CREATE_NO_WINDOW` (windows) and killed on drop (D12, INV-28).
pub struct Process {
    child: Child,
    stdin: Option<ChildStdin>,
    /// The parsed stdout line stream (fed by the codec task).
    pub lines: mpsc::UnboundedReceiver<Value>,
    /// Current rolling stderr tail (2 KB).
    stderr_tail: watch::Receiver<String>,
    /// Resolves with the final stderr tail when the stderr task reaches EOF.
    stderr_eof: Option<oneshot::Receiver<String>>,
}

const STDERR_TAIL_CAP: usize = 2048;

impl Process {
    /// The child's pid, if still known.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Take ownership of the child's stdin writer.
    ///
    /// The control channel (Decision D6) is the sole writer to child stdin; the
    /// session actor hands it this handle when it spawns the control task.
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.stdin.take()
    }

    /// Write raw bytes to the child's stdin.
    pub async fn write_stdin(&mut self, data: &[u8]) -> Result<(), SpawnError> {
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            SpawnError::Stdin(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stdin already closed",
            ))
        })?;
        stdin.write_all(data).await.map_err(SpawnError::Stdin)
    }

    /// Close the child's stdin. For a user abort this is the **forwarded
    /// abort**: the child exits on stdin EOF (INV-28). Never hard-kills.
    pub async fn close_stdin(&mut self) -> Result<(), SpawnError> {
        if let Some(mut stdin) = self.stdin.take() {
            stdin.shutdown().await.map_err(SpawnError::Stdin)?;
        }
        Ok(())
    }

    /// Send a graceful interrupt (SIGTERM to the group on unix). On windows
    /// this maps to a job-object kill; the caller should rely on the ladder.
    pub async fn send_term(&mut self) -> Result<(), SpawnError> {
        self.signal_group(Signal::Term).await
    }

    /// Force-kill the process group (SIGKILL on unix).
    pub async fn send_kill(&mut self) -> Result<(), SpawnError> {
        self.signal_group(Signal::Kill).await
    }

    /// The current rolling stderr tail.
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail.borrow().clone()
    }

    /// Wait for the child to exit within `wait`, returning `Ok(true)` if it
    /// exited, `Ok(false)` on timeout.
    async fn wait_within(&mut self, wait: Duration) -> Result<bool, SpawnError> {
        match timeout(wait, self.child.wait()).await {
            Ok(status) => {
                // Reap happened; drop status to avoid an unused warning path.
                let _ = status.map_err(SpawnError::Stdin)?;
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }

    /// Wait for the child to exit, draining stderr up to `stderr_drain_cap`
    /// before returning. Returns the exit status.
    pub async fn wait_with_stderr(
        &mut self,
        timings: &Timings,
    ) -> Result<std::process::ExitStatus, SpawnError> {
        let status = self.child.wait().await.map_err(SpawnError::Stdin)?;
        // Drain: give the stderr task up to the cap to reach EOF and report the
        // final tail (INV-4).
        if let Some(eof) = self.stderr_eof.take() {
            let _ = timeout(timings.stderr_drain_cap, eof).await;
        }
        Ok(status)
    }

    /// Wait for the child to exit, returning the stderr tail captured after
    /// draining (used for `ExitedEarly` errors).
    pub async fn wait_and_tail(&mut self, timings: &Timings) -> Result<String, SpawnError> {
        self.wait_with_stderr(timings).await?;
        Ok(self.stderr_tail())
    }

    /// The full kill ladder (D12): close stdin (forwarded abort) → wait
    /// `stdin_close_wait` → SIGTERM → wait `term_to_kill_wait` → SIGKILL.
    ///
    /// A user abort enters through the forwarded abort (stdin EOF) and is never
    /// a straight hard-kill.
    pub async fn shutdown(&mut self, timings: &Timings) -> Result<(), SpawnError> {
        let _ = self.close_stdin().await;
        if self.wait_within(timings.stdin_close_wait).await? {
            return Ok(());
        }
        self.send_term().await?;
        if self.wait_within(timings.term_to_kill_wait).await? {
            return Ok(());
        }
        self.send_kill().await?;
        self.wait_within(Duration::from_secs(5)).await?;
        Ok(())
    }

    /// Teardown: force-kill the process group and reap (INV-6, 3.T7). After
    /// this returns the recorded child pid is gone (`kill(pid, 0)` = `ESRCH`).
    pub async fn dispose(&mut self, timings: &Timings) {
        let _ = self.close_stdin().await;
        // Give the child a moment to exit on stdin EOF, then force-kill the
        // group so no member survives.
        let _ = self.wait_within(timings.stdin_close_wait).await;
        let _ = self.send_kill().await;
        let _ = self.wait_within(Duration::from_secs(5)).await;
        let _ = self.wait_with_stderr(timings).await;
    }

    /// Signal the process group (D12). Unix sends to the group via
    /// `killpg`; Windows kills the job object via `child.kill()`.
    async fn signal_group(&mut self, signal: Signal) -> Result<(), SpawnError> {
        signal_group_impl(&mut self.child, signal).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    Term,
    Kill,
    /// Only used by the unix `--cli` relay (`index.ts:18-26`).
    #[cfg(unix)]
    Interrupt,
    /// Only used by the unix `--cli` relay (`index.ts:18-26`).
    #[cfg(unix)]
    Hangup,
}

/// Platform-specific group signalling. All `cfg` lives in `process.rs` (G9).
#[cfg(unix)]
async fn signal_group_impl(child: &mut Child, signal: Signal) -> Result<(), SpawnError> {
    let Some(pid) = child.id() else {
        return Ok(());
    };
    use nix::sys::signal::{killpg, Signal as NixSignal};
    use nix::unistd::Pid;
    let sig = match signal {
        Signal::Term => NixSignal::SIGTERM,
        Signal::Kill => NixSignal::SIGKILL,
        Signal::Interrupt => NixSignal::SIGINT,
        Signal::Hangup => NixSignal::SIGHUP,
    };
    killpg(Pid::from_raw(pid as i32), sig)
        .map_err(|e| SpawnError::Signal(std::io::Error::other(format!("killpg: {e}"))))
}

/// Platform-specific group signalling (windows): kill the job object. tokio's
/// `kill_on_drop(true)` put the child in a job object; `child.kill()` tears the
/// whole tree down.
#[cfg(windows)]
async fn signal_group_impl(child: &mut Child, _signal: Signal) -> Result<(), SpawnError> {
    child
        .kill()
        .await
        .map_err(|e| SpawnError::Signal(std::io::Error::other(format!("child.kill: {e}"))))
}

/// The SDK version the pinned adapter embeds in the child env when the host
/// does not set `CLAUDE_AGENT_SDK_VERSION` (`sdk.mjs` 0.3.303).
pub const CLAUDE_AGENT_SDK_VERSION: &str = "0.3.303";

/// JS truthiness for `DEBUG_CLAUDE_AGENT_SDK` (`Te()` in the SDK): a boolean
/// true, or a case-insensitive `1`/`true`/`yes`/`on`.
fn env_truthy(value: Option<&str>) -> bool {
    match value {
        None => false,
        Some(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
    }
}

/// Compute fixed environment variables for the child process.
///
/// `CLAUDE_CODE_ENTRYPOINT` and `CLAUDE_CODE_SDK_READS_SESSION_STATE` are set
/// ONLY when absent from both the parent environment and `extra_env` (mirroring
/// Node SDK behavior). `CLAUDE_AGENT_SDK_VERSION` follows the same rule
/// (O-167).
pub fn compute_claude_env_defaults(
    parent_vars: impl IntoIterator<Item = (String, String)>,
    extra_env: &[(String, String)],
) -> Vec<(&'static str, &'static str)> {
    let mut defaults = Vec::new();
    let parent_map: std::collections::HashMap<String, String> = parent_vars.into_iter().collect();

    let has_entrypoint = extra_env
        .iter()
        .any(|(k, v)| k == "CLAUDE_CODE_ENTRYPOINT" && !v.is_empty())
        || parent_map
            .get("CLAUDE_CODE_ENTRYPOINT")
            .is_some_and(|v| !v.is_empty());
    if !has_entrypoint {
        defaults.push(("CLAUDE_CODE_ENTRYPOINT", "sdk-ts"));
    }

    let has_sdk_version = extra_env
        .iter()
        .any(|(k, v)| k == "CLAUDE_AGENT_SDK_VERSION" && !v.is_empty())
        || parent_map
            .get("CLAUDE_AGENT_SDK_VERSION")
            .is_some_and(|v| !v.is_empty());
    if !has_sdk_version {
        defaults.push(("CLAUDE_AGENT_SDK_VERSION", CLAUDE_AGENT_SDK_VERSION));
    }

    defaults.push(("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1"));

    let has_sdk_reads = extra_env
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("CLAUDE_CODE_SDK_READS_SESSION_STATE"))
        || parent_map
            .keys()
            .any(|k| k.eq_ignore_ascii_case("CLAUDE_CODE_SDK_READS_SESSION_STATE"));
    if !has_sdk_reads {
        defaults.push(("CLAUDE_CODE_SDK_READS_SESSION_STATE", "1"));
    }

    defaults
}

/// The `DEBUG` override from the merged env (O-167): `Some("1")` when
/// `DEBUG_CLAUDE_AGENT_SDK` is truthy, else `None` (the SDK deletes `DEBUG`).
pub fn compute_debug_override(
    parent_vars: impl IntoIterator<Item = (String, String)>,
    extra_env: &[(String, String)],
) -> Option<&'static str> {
    let parent_map: std::collections::HashMap<String, String> = parent_vars.into_iter().collect();
    let value = extra_env
        .iter()
        .rev()
        .find(|(k, _)| k == "DEBUG_CLAUDE_AGENT_SDK")
        .map(|(_, v)| v.as_str())
        .or_else(|| parent_map.get("DEBUG_CLAUDE_AGENT_SDK").map(String::as_str));
    env_truthy(value).then_some("1")
}

/// Apply the fixed `claude` environment (B2): merge `extra_env`, set
/// `CLAUDE_CODE_ENTRYPOINT`, set `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1`,
/// remove `NODE_OPTIONS`, and normalise `DEBUG` (O-167): the SDK deletes
/// `DEBUG` unless `DEBUG_CLAUDE_AGENT_SDK` is truthy, in which case it forces
/// `DEBUG=1`. Cross-platform; no `cfg` needed here.
///
/// Note: this only ADDS/OVERRIDES/REMOVES environment variables. The [`Command`]
/// is never `.env_clear()`ed, so the child inherits the full parent process env
/// by default — exactly the Node adapter's `{ ...process.env, ...providerEnv }`
/// (`acp-agent.js:4851`). It does NOT restrict which env vars reach `claude`.
fn apply_claude_env(cmd: &mut Command, extra_env: &[(String, String)]) {
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    for (k, v) in compute_claude_env_defaults(std::env::vars(), extra_env) {
        cmd.env(k, v);
    }
    match compute_debug_override(std::env::vars(), extra_env) {
        Some(debug) => {
            cmd.env("DEBUG", debug);
        }
        None => {
            cmd.env_remove("DEBUG");
        }
    }
    cmd.env_remove("NODE_OPTIONS");
}

/// Platform-specific spawn configuration: process group on unix, job object +
/// `CREATE_NO_WINDOW` on windows.
#[cfg(unix)]
fn configure_platform(cmd: &mut Command) {
    cmd.process_group(0);
    cmd.kill_on_drop(true);
}

/// Platform-specific spawn configuration (windows): `CREATE_NO_WINDOW` and a
/// job object via `kill_on_drop(true)` (D12).
#[cfg(windows)]
fn configure_platform(cmd: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.kill_on_drop(true);
}

/// Whether the adapter runs as root, which removes bypass-permissions
/// availability unless the process is sandboxed (`ALLOW_BYPASS`, Decision 3).
/// All `cfg` lives in `process.rs` (guard G9).
#[cfg(unix)]
pub fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// Windows has no euid; Node's `process.getuid?.() === 0` is false there.
#[cfg(windows)]
pub fn is_root() -> bool {
    false
}

/// The managed (policy) settings file per OS, mirroring
/// `getManagedSettingsPath` in `dist/settings.js` (Decision 3).
#[cfg(target_os = "macos")]
pub fn managed_settings_path() -> PathBuf {
    PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
}

/// Windows managed settings path (`getManagedSettingsPath`).
#[cfg(windows)]
pub fn managed_settings_path() -> PathBuf {
    PathBuf::from(r"C:\Program Files\ClaudeCode\managed-settings.json")
}

/// Every other unix uses the `/etc` policy path.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn managed_settings_path() -> PathBuf {
    PathBuf::from("/etc/claude-code/managed-settings.json")
}

/// Spawn the `claude` child with the full B2 argv/env and return a [`Process`]
/// (3.1, 3.2, 3.6).
pub async fn spawn(opts: &SpawnOptions) -> Result<Process, SpawnError> {
    let binary = resolve_binary(
        opts.claude_path.as_deref(),
        opts.env_snapshot.get("CLAUDE_CODE_EXECUTABLE"),
        opts.env_snapshot.get("PATH").unwrap_or_default(),
        current_os(),
    )?;
    let argv = build_argv(opts);
    spawn_raw(binary, argv, opts).await
}

/// Resolve the `claude` binary for a one-shot CLI call (`auth status`,
/// `auth logout`) with the same precedence as [`spawn`].
pub fn resolve_claude(claude_path: Option<&Path>) -> Result<PathBuf, ResolveError> {
    resolve_claude_with_env(
        claude_path,
        &crate::env_snapshot::EnvSnapshot::from_process(),
    )
}

/// Resolve `claude` binary using an explicit environment snapshot.
pub fn resolve_claude_with_env(
    claude_path: Option<&Path>,
    env: &crate::env_snapshot::EnvSnapshot,
) -> Result<PathBuf, ResolveError> {
    resolve_binary(
        claude_path,
        env.get("CLAUDE_CODE_EXECUTABLE"),
        env.get("PATH").unwrap_or_default(),
        current_os(),
    )
}

/// The outcome of a one-shot `claude <args>` run ([`run_cli`]).
#[derive(Debug, Clone)]
pub struct CliRun {
    /// Captured stdout (lossy UTF-8).
    pub stdout: String,
    /// Captured stderr (lossy UTF-8).
    pub stderr: String,
    /// Whether the process exited with status 0.
    pub success: bool,
    /// Whether the run was killed at the time limit (output then discarded).
    pub timed_out: bool,
}

/// Run `binary args…` once with stdin closed and a time limit; the child is
/// killed on timeout. The parent environment is inherited (plus `extra_env`).
pub async fn run_cli(
    binary: &Path,
    args: &[&str],
    cwd: Option<&Path>,
    extra_env: &[(String, String)],
    limit: Duration,
) -> std::io::Result<CliRun> {
    let mut cmd = Command::new(binary);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let child = cmd.spawn()?;
    match timeout(limit, child.wait_with_output()).await {
        Ok(output) => {
            let output = output?;
            Ok(CliRun {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                success: output.status.success(),
                timed_out: false,
            })
        }
        Err(_) => Ok(CliRun {
            stdout: String::new(),
            stderr: String::new(),
            success: false,
            timed_out: true,
        }),
    }
}

/// Low-level spawn used by [`spawn`] and by tests that need an arbitrary child.
pub async fn spawn_raw(
    binary: PathBuf,
    argv: Vec<String>,
    opts: &SpawnOptions,
) -> Result<Process, SpawnError> {
    let mut cmd = Command::new(&binary);
    cmd.args(&argv);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    if let Some(cwd) = &opts.default_cwd {
        cmd.current_dir(cwd);
    }
    apply_claude_env(&mut cmd, &opts.extra_env);
    configure_platform(&mut cmd);

    let mut child = cmd.spawn().map_err(|source| SpawnError::Spawn {
        binary: binary.display().to_string(),
        source,
    })?;

    let stdout = child.stdout.take().ok_or_else(|| SpawnError::Spawn {
        binary: binary.display().to_string(),
        source: std::io::Error::other("stdout not piped"),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| SpawnError::Spawn {
        binary: binary.display().to_string(),
        source: std::io::Error::other("stderr not piped"),
    })?;
    let stdin = child.stdin.take();

    let (lines, _codec_task) = codec::spawn_codec(stdout);
    let (stderr_tail, stderr_eof) = spawn_stderr_tail(stderr);

    Ok(Process {
        child,
        stdin,
        lines,
        stderr_tail,
        stderr_eof: Some(stderr_eof),
    })
}

/// Spawn a task that reads the child's stderr, keeping a 2 KB rolling tail and
/// resolving `eof` with the final tail when the pipe closes (3.4).
fn spawn_stderr_tail(
    mut stderr: tokio::process::ChildStderr,
) -> (watch::Receiver<String>, oneshot::Receiver<String>) {
    let (tail_tx, tail_rx) = watch::channel(String::new());
    let (eof_tx, eof_rx) = oneshot::channel();
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 1024];
        let mut ring: std::collections::VecDeque<u8> = std::collections::VecDeque::new();
        loop {
            let n = match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            for &b in &buf[..n] {
                ring.push_back(b);
                if ring.len() > STDERR_TAIL_CAP {
                    ring.pop_front();
                }
            }
            let _ = tail_tx.send(
                String::from_utf8_lossy(&ring.iter().copied().collect::<Vec<_>>()).into_owned(),
            );
        }
        let _ = eof_tx
            .send(String::from_utf8_lossy(&ring.iter().copied().collect::<Vec<_>>()).into_owned());
    });
    (tail_rx, eof_rx)
}

/// The current platform for binary-name resolution.
#[cfg(unix)]
fn current_os() -> Os {
    Os::Unix
}

/// The current platform for binary-name resolution.
#[cfg(windows)]
fn current_os() -> Os {
    Os::Windows
}

/// A future that resolves when the host requests shutdown: SIGINT or SIGTERM
/// on unix (`index.ts:98-104` installs the same handler for both).
///
/// On non-unix platforms there is no SIGTERM; the future never resolves
/// (shutdown happens only via stdin EOF), matching the D12 platform split.
/// All `cfg` lives in `process.rs` (guard G9), so the stdio binary can await
/// this without any platform-specific code (12.2).
#[cfg(unix)]
pub async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let interrupt = signal(SignalKind::interrupt());
    let terminate = signal(SignalKind::terminate());
    match (interrupt, terminate) {
        (Ok(mut interrupt), Ok(mut terminate)) => {
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
            }
        }
        (Ok(mut interrupt), Err(_)) => {
            interrupt.recv().await;
        }
        (Err(_), Ok(mut terminate)) => {
            terminate.recv().await;
        }
        (Err(_), Err(_)) => {
            // No handler could be installed; park (shutdown via stdin EOF).
            std::future::pending::<()>().await;
        }
    }
}

/// Non-unix: no SIGTERM; shutdown happens only via stdin EOF.
#[cfg(not(unix))]
pub async fn wait_for_shutdown_signal() {
    std::future::pending::<()>().await
}

/// Run the wrapped native `claude` CLI (`--cli`, `index.ts:13-42`).
///
/// The child inherits this process's stdin/stdout/stderr. SIGINT/SIGTERM
/// (plus SIGHUP on unix) are forwarded to it, and its exit code is mirrored.
/// A child killed by a signal exits with the conventional `128+N` (the value
/// the upstream's re-raise produces), since resetting the signal disposition
/// safely is not available under `#![forbid(unsafe_code)]`.
#[cfg(unix)]
pub async fn run_cli_passthrough(binary: &Path, args: &[String]) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    use tokio::signal::unix::{signal, SignalKind};

    let mut child = match Command::new(binary)
        .args(args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };

    let mut interrupt = signal(SignalKind::interrupt()).ok();
    let mut terminate = signal(SignalKind::terminate()).ok();
    let mut hangup = signal(SignalKind::hangup()).ok();

    // Node's `if (!child.killed) child.kill(sig)` forwards only the first
    // signal: `child.killed` is latched by the first `kill()` call.
    let mut relayed = false;

    loop {
        tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(status) => {
                        if let Some(signal) = status.signal() {
                            return 128 + signal;
                        }
                        return status.code().unwrap_or(1);
                    }
                    Err(err) => {
                        eprintln!("{err}");
                        return 1;
                    }
                }
            }
            _ = recv_signal(&mut interrupt) => {
                if !relayed {
                    forward_to_child(&mut child, Signal::Interrupt);
                    relayed = true;
                }
            }
            _ = recv_signal(&mut terminate) => {
                if !relayed {
                    forward_to_child(&mut child, Signal::Term);
                    relayed = true;
                }
            }
            _ = recv_signal(&mut hangup) => {
                if !relayed {
                    forward_to_child(&mut child, Signal::Hangup);
                    relayed = true;
                }
            }
        }
    }
}

/// Windows `--cli`: inherit stdio, mirror the exit code, and forward Ctrl-C to
/// the child's job object (`child.kill()` tears the tree down).
#[cfg(windows)]
pub async fn run_cli_passthrough(binary: &Path, args: &[String]) -> i32 {
    let mut child = match Command::new(binary)
        .args(args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };
    tokio::select! {
        status = child.wait() => match status {
            Ok(status) => status.code().unwrap_or(1),
            Err(err) => {
                eprintln!("{err}");
                1
            }
        },
        _ = tokio::signal::ctrl_c() => {
            let _ = child.kill().await;
            1
        }
    }
}

/// Wait for the next signal on an optional stream; park forever when the
/// handler could not be installed.
#[cfg(unix)]
async fn recv_signal(stream: &mut Option<tokio::signal::unix::Signal>) -> bool {
    match stream {
        Some(stream) => stream.recv().await.is_some(),
        None => std::future::pending::<bool>().await,
    }
}

/// Forward a signal to the wrapped CLI's pid (Node `child.kill(sig)`), never to
/// the adapter's own process group.
#[cfg(unix)]
fn forward_to_child(child: &mut Child, signal: Signal) {
    let Some(pid) = child.id() else {
        return;
    };
    use nix::sys::signal::{kill, Signal as NixSignal};
    use nix::unistd::Pid;
    let sig = match signal {
        Signal::Term => NixSignal::SIGTERM,
        Signal::Kill => NixSignal::SIGKILL,
        Signal::Interrupt => NixSignal::SIGINT,
        Signal::Hangup => NixSignal::SIGHUP,
    };
    let _ = kill(Pid::from_raw(pid as i32), sig);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3.T10 — `resolve_binary(Os::Windows)` resolves `claude.exe`; unix
    /// resolves `claude` (the platform name is chosen, not hard-coded).
    #[test]
    fn inv_25_resolve_binary_platform_names() {
        // No path, no env, no binary on PATH -> typed NotFound naming the
        // platform-specific binary.
        let err = resolve_binary(None, None, "/definitely/empty", Os::Windows)
            .expect_err("windows lookup must fail");
        assert!(
            err.to_string().contains("claude.exe"),
            "windows name: {err}"
        );

        let err = resolve_binary(None, None, "/definitely/empty", Os::Unix)
            .expect_err("unix lookup must fail");
        assert!(err.to_string().contains("claude"), "unix name: {err}");

        // The platform name is what is searched on PATH.
        assert_eq!(
            platform_binary_name(Os::Windows),
            "claude.exe",
            "3.T10 windows"
        );
        assert_eq!(platform_binary_name(Os::Unix), "claude", "3.T10 unix");
    }

    #[test]
    fn inv_57_claude_env_defaults_absent_only() {
        // When absent from both parent and extra_env, defaults are set.
        let empty_parent: Vec<(String, String)> = Vec::new();
        let empty_extra: Vec<(String, String)> = Vec::new();
        let defs = compute_claude_env_defaults(empty_parent, &empty_extra);
        assert_eq!(
            defs,
            vec![
                ("CLAUDE_CODE_ENTRYPOINT", "sdk-ts"),
                ("CLAUDE_AGENT_SDK_VERSION", CLAUDE_AGENT_SDK_VERSION),
                ("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1"),
                ("CLAUDE_CODE_SDK_READS_SESSION_STATE", "1"),
            ]
        );

        // When present in parent env, entrypoint and reads_session_state are NOT defaulted.
        let parent_with_vars = vec![
            ("CLAUDE_CODE_ENTRYPOINT".into(), "custom-ts".into()),
            ("CLAUDE_CODE_SDK_READS_SESSION_STATE".into(), "0".into()),
            ("CLAUDE_AGENT_SDK_VERSION".into(), "9.9.9".into()),
        ];
        let defs = compute_claude_env_defaults(parent_with_vars, &empty_extra);
        assert_eq!(defs, vec![("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1")]);

        // When present in extra_env, entrypoint and reads_session_state are NOT defaulted.
        let extra_with_vars = vec![
            ("CLAUDE_CODE_ENTRYPOINT".into(), "user-ep".into()),
            ("claude_code_sdk_reads_session_state".into(), "1".into()),
            ("CLAUDE_AGENT_SDK_VERSION".into(), "1.2.3".into()),
        ];
        let empty_parent: Vec<(String, String)> = Vec::new();
        let defs = compute_claude_env_defaults(empty_parent, &extra_with_vars);
        assert_eq!(defs, vec![("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1")]);

        // An EMPTY `CLAUDE_AGENT_SDK_VERSION` is treated as unset (JS falsy).
        let extra_empty_version = vec![("CLAUDE_AGENT_SDK_VERSION".into(), String::new())];
        let defs = compute_claude_env_defaults(Vec::new(), &extra_empty_version);
        assert!(defs.iter().any(|(k, _)| *k == "CLAUDE_AGENT_SDK_VERSION"));
    }

    /// O-167: `DEBUG` is stripped unless `DEBUG_CLAUDE_AGENT_SDK` is truthy, in
    /// which case it is forced to `1` (extra_env overrides the parent).
    #[test]
    fn inv_57_debug_env_override() {
        let none: Vec<(String, String)> = Vec::new();
        assert_eq!(compute_debug_override(Vec::new(), &none), None);

        let parent_debug = vec![("DEBUG".into(), "1".into())];
        assert_eq!(compute_debug_override(parent_debug, &none), None);

        let parent_flag = vec![("DEBUG_CLAUDE_AGENT_SDK".into(), "1".into())];
        assert_eq!(compute_debug_override(parent_flag, &none), Some("1"));

        let extra_flag = vec![("DEBUG_CLAUDE_AGENT_SDK".into(), "TRUE".into())];
        assert_eq!(compute_debug_override(Vec::new(), &extra_flag), Some("1"));

        // A falsy extra_env value overrides a truthy parent value.
        let parent_flag = vec![("DEBUG_CLAUDE_AGENT_SDK".into(), "1".into())];
        let extra_off = vec![("DEBUG_CLAUDE_AGENT_SDK".into(), "0".into())];
        assert_eq!(compute_debug_override(parent_flag, &extra_off), None);
    }

    #[test]
    fn resolve_binary_precedence() {
        // Env var takes precedence over PATH.
        let r =
            resolve_binary(None, Some("/opt/claude"), "/no/such/path", Os::Unix).expect("env path");
        assert_eq!(r, PathBuf::from("/opt/claude"));

        // Explicit claude_path wins over env.
        let r = resolve_binary(
            Some(Path::new("/explicit")),
            Some("/opt/claude"),
            "/nope",
            Os::Unix,
        )
        .expect("explicit path");
        assert_eq!(r, PathBuf::from("/explicit"));
    }

    fn platform_binary_name(os: Os) -> &'static str {
        match os {
            Os::Windows => "claude.exe",
            Os::Unix => "claude",
        }
    }
}
