//! Process transport: spawning the `claude` child, its argv/env, the kill
//! ladder, process-group teardown, and the stderr tail (Decision D12, items
//! 3.1–3.6).
//!
//! This is the **only** file in the crate allowed to use `cfg(unix)` /
//! `cfg(windows)` (guard G9). The crate keeps `#![forbid(unsafe_code)]` on every
//! non-Windows target (D12, R33); Windows keeps `#![deny(unsafe_code)]` with a
//! scoped `#[allow(unsafe_code)]` limited to the tiny private `windows_job`
//! module (Win32 Job Object FFI, Decision D8; `forbid` cannot coexist with an
//! inner module `allow`). All other APIs use safe wrappers: tokio's
//! `process_group(0)`, `creation_flags`, `kill_on_drop`, and
//! `nix::sys::signal::killpg`.
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
use std::sync::atomic::{AtomicBool, Ordering};
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

/// The bundled native CLI candidate paths, in preference order, that Node's
/// `claudeCliPath()` tries (`acp-agent.ts:2036-2067`):
/// `@anthropic-ai/claude-agent-sdk-<platform>-<arch>[-musl]/claude[.exe]`.
///
/// `platform`/`arch` are Node's spellings (`darwin`, `linux`, `win32`;
/// `x64`, `arm64`, ...), so this stays a pure function and the host mapping is
/// the only platform-specific part (below).
pub fn bundled_cli_candidates(platform: &str, arch: &str, musl: bool) -> Vec<String> {
    let ext = if platform == "win32" { ".exe" } else { "" };
    let base = "@anthropic-ai/claude-agent-sdk";
    let packages = if platform == "linux" {
        let plain = format!("{base}-linux-{arch}");
        let musl_package = format!("{base}-linux-{arch}-musl");
        // Both variants may be installed side by side; the runtime libc picks
        // the first candidate, the other is only a fallback.
        if musl {
            vec![musl_package, plain]
        } else {
            vec![plain, musl_package]
        }
    } else {
        vec![format!("{base}-{platform}-{arch}")]
    };
    packages
        .into_iter()
        .map(|package| format!("{package}/claude{ext}"))
        .collect()
}

/// Search `start` and its ancestors for the bundled SDK native binary under
/// `node_modules`, mirroring Node's module resolution for the optional
/// dependency. Returns the first existing candidate.
pub fn find_bundled_cli(start: &Path, platform: &str, arch: &str, musl: bool) -> Option<PathBuf> {
    let candidates = bundled_cli_candidates(platform, arch, musl);
    for ancestor in start.ancestors() {
        for candidate in &candidates {
            let path = ancestor.join("node_modules").join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// Node's `process.platform` spelling for the running host.
#[cfg(target_os = "macos")]
fn node_platform() -> &'static str {
    "darwin"
}

/// Node's `process.platform` spelling for the running host.
#[cfg(target_os = "linux")]
fn node_platform() -> &'static str {
    "linux"
}

/// Node's `process.platform` spelling for the running host.
#[cfg(windows)]
fn node_platform() -> &'static str {
    "win32"
}

/// Node's `process.platform` spelling for any other host.
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn node_platform() -> &'static str {
    std::env::consts::OS
}

/// Node's `process.arch` spelling for the running host (`x86_64` -> `x64`,
/// `aarch64` -> `arm64`, ...).
fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        "arm" => "arm",
        other => other,
    }
}

/// Whether the host links musl rather than glibc. Node reads
/// `process.report.getReport().header.glibcVersionRuntime`; a native binary has
/// no such report, so the musl dynamic loader's presence is the equivalent
/// signal (`isMuslLibc`, `acp-agent.ts:2069-2074`).
#[cfg(target_os = "linux")]
fn is_musl_libc() -> bool {
    let loader = match std::env::consts::ARCH {
        "x86_64" => "/lib/ld-musl-x86_64.so.1",
        "aarch64" => "/lib/ld-musl-aarch64.so.1",
        _ => return false,
    };
    Path::new(loader).exists()
}

/// Non-Linux hosts are never musl.
#[cfg(not(target_os = "linux"))]
fn is_musl_libc() -> bool {
    false
}

/// The bundled CLI found next to the running binary (or the working
/// directory), if any.
fn bundled_cli_from_host() -> Option<PathBuf> {
    let platform = node_platform();
    let arch = node_arch();
    let musl = is_musl_libc();
    let from_exe = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .and_then(|dir| find_bundled_cli(&dir, platform, arch, musl));
    from_exe.or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| find_bundled_cli(&cwd, platform, arch, musl))
    })
}

/// Resolve the `claude` binary with the adapter's precedence: an explicit
/// `claude_path` → `CLAUDE_CODE_EXECUTABLE` → the bundled SDK native binary
/// (`claudeCliPath()`) → a `PATH` lookup (the last step is a Rust-port
/// fallback for hosts without the SDK package; Node throws there).
pub fn resolve_claude_path(
    claude_path: Option<&Path>,
    executable_env: Option<&str>,
    bundled: Option<PathBuf>,
    path: &str,
    os: Os,
) -> Result<PathBuf, ResolveError> {
    if let Some(explicit) = claude_path {
        return Ok(explicit.to_path_buf());
    }
    if let Some(env) = executable_env.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(env));
    }
    if let Some(bundled) = bundled {
        return Ok(bundled);
    }
    resolve_binary(None, None, path, os)
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
    /// `--continue` (O-108); suppressed when `session_mirror` is set.
    pub continue_conversation: bool,
    /// `--task-budget=<n>` (O-119).
    pub task_budget: Option<String>,
    /// `--json-schema=<json>` (O-120).
    pub json_schema: Option<String>,
    /// `--debug` (O-121); only when `debug_file` is absent.
    pub debug: bool,
    /// `--debug-file=<path>` (O-121).
    pub debug_file: Option<String>,
    /// `--session-mirror` (O-126).
    pub session_mirror: bool,
    /// One `--channels=<c>` per entry (O-127).
    pub channels: Vec<String>,
    /// `--permission-prompts=<v>` (O-132).
    pub permission_prompts: Option<String>,
    /// `--managed-settings=<path|json>` (O-122).
    pub managed_settings_arg: Option<String>,
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
            continue_conversation: false,
            task_budget: None,
            json_schema: None,
            debug: false,
            debug_file: None,
            session_mirror: false,
            channels: Vec::new(),
            permission_prompts: None,
            managed_settings_arg: None,
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

    // `--task-budget` sits between `--max-budget-usd` and `--model` (O-119).
    if let Some(task_budget) = &opts.task_budget {
        argv.push(format!("--task-budget={task_budget}"));
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

    // `--json-schema`, then `--debug-file`/`--debug` (O-120/O-121), before the
    // permission-prompt-tool flags, in the SDK's order.
    if let Some(schema) = &opts.json_schema {
        argv.push(format!("--json-schema={schema}"));
    }
    if let Some(debug_file) = &opts.debug_file {
        argv.push(format!("--debug-file={debug_file}"));
    } else if opts.debug {
        argv.push("--debug".into());
    }

    if opts.permission_prompt_tool {
        argv.push("--permission-prompt-tool=stdio".into());
    }

    // `--permission-prompts`, `--continue`, `--resume` and one `--channels`
    // per entry follow the prompt-tool flag (O-132/O-108/O-127).
    if let Some(prompts) = &opts.permission_prompts {
        argv.push(format!("--permission-prompts={prompts}"));
    }

    // The SDK suppresses `--continue` when a `sessionStore` is set
    // (`continueConversation: sessionStore ? void 0 : continue`).
    if opts.continue_conversation && !opts.session_mirror {
        argv.push("--continue".into());
    }

    if let Some(resume) = &opts.resume {
        argv.push(format!("--resume={resume}"));
    }

    for channel in &opts.channels {
        argv.push(format!("--channels={channel}"));
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

    // `--session-mirror` follows `--include-partial-messages` and precedes
    // `--project-config-root` (O-126).
    if opts.session_mirror {
        argv.push("--session-mirror".into());
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

    if let Some(managed_settings) = &opts.managed_settings_arg {
        push_extra_arg(&mut argv, "managed-settings", Some(managed_settings));
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
    #[cfg(unix)]
    _reaper_stdin: Option<ChildStdin>,
    #[cfg(unix)]
    reaper_pid: Option<u32>,
    #[cfg(windows)]
    _job: Option<WindowsJobObject>,
}

const STDERR_TAIL_CAP: usize = 2048;

impl Process {
    /// The child's pid, if still known.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// The supervisor reaper pid, if spawned (unix).
    #[cfg(unix)]
    pub fn reaper_pid(&self) -> Option<u32> {
        self.reaper_pid
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

    /// Send a graceful interrupt (SIGTERM to the group on unix). On Windows
    /// POSIX signals are not supported; the caller should rely on the kill ladder.
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
    /// `killpg`; Windows terminates the child process and its job object.
    async fn signal_group(&mut self, signal: Signal) -> Result<(), SpawnError> {
        #[cfg(windows)]
        if let Some(job) = &self._job {
            let _ = job.terminate(1);
        }
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

/// Platform-specific group signalling (windows): kill the direct child process via `child.kill()`.
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

// =========================================================================
// Process Tree Lifecycle and Orphan Protection Architecture (F41 / INV-28):
//
// Linux & macOS (Unix):
// We intentionally do NOT use Linux PR_SET_PDEATHSIG. In Linux, PR_SET_PDEATHSIG
// binds to the calling *thread*, not the parent process or thread group. In an
// async runtime such as tokio, process spawning occurs on worker or blocking
// threads that can exit after being idle, which would prematurely trigger
// PDEATHSIG and kill active sessions while the host process remains healthy.
// Instead, Unix orphan prevention relies on:
// 1. Cooperative exit: host stdin closure triggers child stdin EOF (INV-28).
// 2. Controlled teardown: Process::dispose force-kills the process group (`killpg`).
// 3. Host crash protection: an out-of-process supervisor (`--reaper`) is spawned
//    in a separate process group. It monitors the host via stdin EOF and periodic
//    `getppid()` checks (detecting process termination and reparenting to init/subreaper).
//    When the host terminates, the reaper terminates the target process group with SIGKILL.
// 4. Safe reaper resolution and recursion guard (Turn 1, Group G7a-fix6):
//    To prevent self-sustaining process explosions (fork bombs), the supervisor binary
//    is resolved ONLY if registered by the adapter entry point (`set_reaper_binary(current_exe())`),
//    or explicitly configured via `ACP_REAPER_BIN` (passing `--reaper --probe`).
//    Arbitrary host binaries / embedders whose `main` did not register get NO reaper
//    (logging one warning; sessions run normally). Furthermore, every probe and reaper launch
//    injects `CLAUDE_ACP_REAPER_GUARD=1` on the child, guaranteeing that any mishandling binary
//    is halted at depth 1 without recursive execution.
//
// Windows:
// Windows does not have POSIX process groups or signals. Instead, orphan protection
// is achieved via Win32 Job Objects configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
// The spawned child process is assigned to this Job Object. When the host process terminates
// for any reason (normal exit, unhandled exception, or hard termination), the operating
// system automatically closes all open handles held by the host process. When the Job Object
// handle closes, the Windows kernel terminates every process still in the job: descendants
// created AFTER the assignment inherit the job, so the tree is covered from that point on.
//
// Known limitation (assignment window, Finding 6 / Decision D8):
// The child is spawned with standard tokio Command and then assigned to the Job
// Object via `AssignProcessToJobObject`. Because standard library / tokio `Command` does not
// expose `CREATE_SUSPENDED` or `PROC_THREAD_ATTRIBUTE_JOB_LIST` without raw `CreateProcessW` FFI,
// any descendant the child spawns BEFORE `AssignProcessToJobObject` completes escapes the job.
// That window is narrow but real (e.g. a `.cmd` shim or a fast-forking wrapper), so Windows
// coverage is NOT claimed to be complete. Closing it would require bypassing tokio Command
// with raw Win32 process creation.
// =========================================================================

/// Platform-specific spawn configuration: process group and kill_on_drop on unix.
/// The supervisor reaper process ensures grandchildren are terminated on hard host death.
#[cfg(unix)]
fn configure_platform(cmd: &mut Command) {
    cmd.process_group(0);
    cmd.kill_on_drop(true);
}

/// Platform-specific spawn configuration (windows): `CREATE_NO_WINDOW` and `kill_on_drop(true)`.
#[cfg(windows)]
fn configure_platform(cmd: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.kill_on_drop(true);
}

/// Win32 Job Object handle wrapper configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
/// Unsafe FFI is scoped strictly to this private submodule (Finding 5, Decision D8).
#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_job {
    use std::mem::size_of;
    use std::ptr::null;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    #[derive(Debug)]
    pub(crate) struct WindowsJobObject {
        handle: HANDLE,
    }

    unsafe impl Send for WindowsJobObject {}
    unsafe impl Sync for WindowsJobObject {}

    impl WindowsJobObject {
        pub(crate) fn create_kill_on_close() -> Result<Self, std::io::Error> {
            let handle = unsafe { CreateJobObjectW(null(), null()) };
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }

            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            let res = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if res == 0 {
                let err = std::io::Error::last_os_error();
                unsafe {
                    windows_sys::Win32::Foundation::CloseHandle(handle);
                }
                return Err(err);
            }

            Ok(Self { handle })
        }

        pub(crate) fn assign_child(
            &self,
            child_handle: std::os::windows::io::RawHandle,
        ) -> Result<(), std::io::Error> {
            let res = unsafe { AssignProcessToJobObject(self.handle, child_handle as _) };
            if res == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }

        pub(crate) fn terminate(&self, exit_code: u32) -> Result<(), std::io::Error> {
            let res = unsafe { TerminateJobObject(self.handle, exit_code) };
            if res == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
    }

    impl Drop for WindowsJobObject {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                unsafe {
                    windows_sys::Win32::Foundation::CloseHandle(self.handle);
                }
            }
        }
    }
}
#[cfg(windows)]
use windows_job::WindowsJobObject;

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

/// Opens `path` read-only without waiting on a named pipe, and keeps it only
/// if the opened handle is a regular file (upstream `readPatchSource` opens
/// with `O_NONBLOCK` and runs `fstat` on the handle). The kind is read from
/// the handle, so a path swapped to a FIFO between a check and the open cannot
/// block the caller. A non-regular file is `ErrorKind::InvalidInput`.
#[cfg(unix)]
pub fn open_regular_file_nonblocking(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)?;
    ensure_regular_file(file)
}

/// Elsewhere a plain open does the job: Windows has no named-pipe paths in
/// the file namespace that block on open.
#[cfg(not(unix))]
pub fn open_regular_file_nonblocking(path: &Path) -> std::io::Result<std::fs::File> {
    ensure_regular_file(std::fs::File::open(path)?)
}

fn ensure_regular_file(file: std::fs::File) -> std::io::Result<std::fs::File> {
    if file.metadata()?.file_type().is_file() {
        Ok(file)
    } else {
        Err(std::io::Error::from(std::io::ErrorKind::InvalidInput))
    }
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

/// Read all JSON settings documents from a directory (such as `managed-settings.d`),
/// sorted alphabetically by filename, skipping hidden files.
pub fn read_managed_settings_dir(dir: &Path) -> Vec<Value> {
    let mut docs = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return docs;
    };
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if !name.starts_with('.') && name.ends_with(".json") {
                    paths.push(path);
                }
            }
        }
    }
    paths.sort();
    for path in paths {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(val @ Value::Object(_)) = serde_json::from_str::<Value>(&text) {
                docs.push(val);
            }
        }
    }
    docs
}

/// Parse a string value from Windows `reg query` output (O-57, pure parser).
pub fn parse_windows_reg_output(stdout: &str, value_name: &str) -> Option<String> {
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(value_name) {
            let rest = rest.trim_start();
            if let Some(after_type) = rest
                .strip_prefix("REG_SZ")
                .or_else(|| rest.strip_prefix("REG_EXPAND_SZ"))
            {
                let val = after_type.trim();
                return Some(val.to_string());
            }
        }
    }
    None
}

/// A definitive OS-managed read result. The SDK caches the MDM/registry load per
/// process (`sdk.mjs` `R4.startLoad` / `loadPromise`), including "absent", so
/// `Absent` is cached too. A transient failure ([`OsSourceRead::Unreadable`]) is
/// NOT cached, so a timeout or unreadable plist is retried next time and never
/// freezes the policy for the life of the process (Finding 2).
#[cfg(any(target_os = "macos", windows))]
#[derive(Clone)]
enum OsSourceRead {
    /// No policy at this tier (definitive).
    Absent,
    /// A readable policy document.
    Present(Value),
    /// The read failed transiently (timeout, unreadable file, oversized output).
    Unreadable,
    /// Windows only: the HKLM value exists but did not load (malformed JSON,
    /// a non-object value, or an oversized value). sdk.mjs records
    /// `loadState:"didNotLoad"`; `fa` is then true and `Tx` returns early with
    /// `hkcu:xc`, so the user-writable HKCU tier is NEVER consulted. The file
    /// tier still applies (it outranks HKCU). Cached: the SDK caches the
    /// per-process `loadPromise` result, including `didNotLoad`.
    #[cfg(windows)]
    DidNotLoad,
}

/// Test hook: while armed, [`read_managed_sources`] panics rather than reading
/// the host managed-policy paths. This makes hermeticity a hard failure instead
/// of a host-dependent observation (item 1 of the G7a-fix4 review).
static MANAGED_SOURCE_GUARD: AtomicBool = AtomicBool::new(false);

/// Arms [`MANAGED_SOURCE_GUARD`] for its lifetime and disarms it on drop (also
/// on unwind), so a test that asserts a host read panics cannot leave the guard
/// armed for its neighbours.
#[doc(hidden)]
#[derive(Debug)]
pub struct ManagedSourceGuard;

impl ManagedSourceGuard {
    /// Arm the guard for this scope.
    #[must_use]
    pub fn arm() -> Self {
        MANAGED_SOURCE_GUARD.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for ManagedSourceGuard {
    fn drop(&mut self) {
        MANAGED_SOURCE_GUARD.store(false, Ordering::SeqCst);
    }
}

#[cfg(any(target_os = "macos", windows))]
static OS_MANAGED_CACHE: std::sync::OnceLock<OsSourceRead> = std::sync::OnceLock::new();
#[cfg(windows)]
static HKCU_CACHE: std::sync::OnceLock<OsSourceRead> = std::sync::OnceLock::new();

/// Read one OS-managed tier through its per-process cache.
///
/// `read` runs off the async worker on a multi-thread runtime (`block_in_place`,
/// Finding 5). A definitive result is cached; `Unreadable` is not.
#[cfg(any(target_os = "macos", windows))]
fn read_cached_os_source(
    cache: &'static std::sync::OnceLock<OsSourceRead>,
    read: fn() -> OsSourceRead,
) -> Option<Value> {
    if let Some(cached) = cache.get() {
        return match cached {
            OsSourceRead::Present(doc) => Some(doc.clone()),
            OsSourceRead::Absent | OsSourceRead::Unreadable => None,
            #[cfg(windows)]
            OsSourceRead::DidNotLoad => None,
        };
    }

    let is_multi_thread = tokio::runtime::Handle::try_current()
        .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    let result = if is_multi_thread {
        tokio::task::block_in_place(read)
    } else {
        read()
    };

    if !matches!(result, OsSourceRead::Unreadable) {
        let _ = cache.set(result.clone());
    }
    match result {
        OsSourceRead::Present(doc) => Some(doc),
        OsSourceRead::Absent | OsSourceRead::Unreadable => None,
        #[cfg(windows)]
        OsSourceRead::DidNotLoad => None,
    }
}

/// Read the managed FILE tier at `path`: `managed-settings.json` itself plus the
/// `managed-settings.d` directory beside it, sorted by filename and skipping
/// hidden files (sdk.mjs `hx`). This tier is re-read on every resolve (sdk.mjs
/// `k4` builds `store: new ds` per call), so an edit is visible to the next
/// SettingsManager reload (O-60, Finding 2).
pub fn read_managed_file_sources_at(path: &Path) -> Vec<Value> {
    let mut docs = Vec::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(val @ Value::Object(_)) = serde_json::from_str::<Value>(&text) {
            docs.push(val);
        }
    }
    if let Some(parent) = path.parent() {
        docs.extend(read_managed_settings_dir(
            &parent.join("managed-settings.d"),
        ));
    }
    docs
}

/// Pick the winning managed tier (sdk.mjs `e4`): remote > MDM (plist/HKLM) >
/// file > HKCU.
///
/// An unread or unparseable OS source is `None` and drops ONLY that tier:
/// sdk.mjs `Sx` maps empty settings to `null` (`settings: t.settings &&
/// Object.keys(t.settings).length > 0 ? t.settings : null`), and `Oc` then
/// keeps the file slot in its winner list, so the managed file still applies
/// (Finding 1). A `reg.exe` timeout is treated as absent (sdk.mjs `y4` keeps
/// only `hklmStdout: status === "ok" ? stdout : null`). The SDK's `{}`
/// placeholder exists only when NO tier has content (`documentHasPolicyContent`
/// false), so an empty result here means "no managed policy", never "policy
/// present but empty".
/// Whether the HKCU tier may be consulted (sdk.mjs `Tx`): only when HKLM was
/// absent — not merely "present but did not load" — and the managed file tier
/// is empty. For a malformed/oversized HKLM value `xp` yields
/// `loadState:"didNotLoad"`, so `fa(a)` is true and `Tx` returns
/// `{mdm:a, hkcu:xc}` before it ever looks at HKCU (item 2, G7a-fix4).
#[cfg(any(windows, test))]
fn hkcu_tier_applies(os_absent: bool, hklm_did_not_load: bool, file_empty: bool) -> bool {
    os_absent && !hklm_did_not_load && file_empty
}

/// Classify the JSON text of the HKLM `Settings` value the way sdk.mjs `xp`
/// does, before `Tx` selects a winner:
/// - a JSON object with at least one key loads as a policy (`Present`);
/// - an empty JSON object loads to `settings:null` with no policy content, so
///   it is `Absent` and HKCU may still apply;
/// - malformed JSON, or JSON that is not an object, is `didNotLoad`, which
///   suppresses HKCU.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum HklmValue {
    Present(Value),
    Absent,
    DidNotLoad,
}

#[cfg(any(windows, test))]
fn classify_hklm_value(value: &str) -> HklmValue {
    match serde_json::from_str::<Value>(value) {
        Ok(doc @ Value::Object(_)) if doc.as_object().is_some_and(|o| !o.is_empty()) => {
            HklmValue::Present(doc)
        }
        Ok(Value::Object(_)) => HklmValue::Absent,
        _ => HklmValue::DidNotLoad,
    }
}

fn select_managed_docs(os: Option<Value>, file: Vec<Value>, hkcu: Option<Value>) -> Vec<Value> {
    if let Some(doc) = os {
        return vec![doc];
    }
    if !file.is_empty() {
        return file;
    }
    match hkcu {
        Some(doc) => vec![doc],
        None => Vec::new(),
    }
}

/// Read all platform-managed settings documents in precedence order.
///
/// Precedence follows the SDK (`sdk.mjs` `e4`):
/// remote > MDM (plist/HKLM) > file > HKCU.
/// The winning origin's documents are returned.
///
/// Per sdk.mjs `R4.startLoad` / `loadPromise` vs `jzt` (`k4`) fresh store:
/// - OS sources (MDM plist / registry / HKCU) are cached per-process, including
///   a definitive "absent" (Finding 2, Finding 5). A transient failure is not
///   cached, so a timeout or unreadable plist is retried on the next resolve.
/// - The managed FILE tier (`managed-settings.json` + `.d`) is re-read on every
///   resolve to support live hot reloading (SettingsManager, O-60).
pub fn read_managed_sources() -> Vec<Value> {
    // Test-mode guard (item 1 of the G7a-fix4 review): integration tests arm
    // this so any settings load that would consult the host managed-policy
    // paths (`/Library/Managed Preferences`, `/etc/claude-code`, the Windows
    // policy registry) fails loudly instead of silently depending on the
    // machine. Production never arms it.
    assert!(
        !MANAGED_SOURCE_GUARD.load(Ordering::SeqCst),
        "test-mode settings load touched the host managed-policy paths; \
         inject EnvSnapshot::empty() or EnvSnapshot::with_managed_docs(...) instead"
    );
    let os = read_os_managed_source();
    // A malformed/oversized HKLM value is cached as `DidNotLoad`: HKLM was
    // PRESENT, so the SDK's `Tx` returns before reading HKCU (item 2).
    #[cfg(windows)]
    let hklm_did_not_load = matches!(OS_MANAGED_CACHE.get(), Some(OsSourceRead::DidNotLoad));
    let file = read_managed_file_sources_at(&managed_settings_path());
    // HKCU is the lowest tier; it only applies when HKLM was absent (not
    // "did not load") and the file tier is empty (sdk.mjs `Oc` / `Tx`).
    #[cfg(windows)]
    let hkcu = if hkcu_tier_applies(os.is_none(), hklm_did_not_load, file.is_empty()) {
        read_hkcu_source()
    } else {
        None
    };
    #[cfg(not(windows))]
    let hkcu = None;
    select_managed_docs(os, file, hkcu)
}

#[cfg(target_os = "macos")]
fn read_os_managed_source() -> Option<Value> {
    read_cached_os_source(&OS_MANAGED_CACHE, read_os_managed_source_uncached)
}

#[cfg(target_os = "macos")]
fn read_os_managed_source_uncached() -> OsSourceRead {
    // In sdk.mjs R4.startLoad / Tx: loops through MDM plists (per-user first, then device).
    // An unreadable plist produces didNotLoad with empty settings: {}.
    // Sx turns empty settings into null:
    //   settings: t.settings && Object.keys(t.settings).length > 0 ? t.settings : null
    // In Oc, null drops ONLY the MDM tier so the file tier f in H still applies (Finding 1).
    // A transient failure (timeout, corrupt JSON) is Unreadable and is not cached,
    // so future attempts can retry.
    let mut unreadable = false;
    if let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::getuid()) {
        let user_plist = PathBuf::from(format!(
            "/Library/Managed Preferences/{}/com.anthropic.claudecode.plist",
            user.name
        ));
        if user_plist.exists() {
            match read_plist_doc(&user_plist) {
                Some(doc) if doc.as_object().is_some_and(|o| !o.is_empty()) => {
                    return OsSourceRead::Present(doc);
                }
                Some(_) => {}
                None => unreadable = true,
            }
        }
    }
    let device_plist = PathBuf::from("/Library/Managed Preferences/com.anthropic.claudecode.plist");
    if device_plist.exists() {
        match read_plist_doc(&device_plist) {
            Some(doc) if doc.as_object().is_some_and(|o| !o.is_empty()) => {
                return OsSourceRead::Present(doc);
            }
            Some(_) => {}
            None => unreadable = true,
        }
    }
    if unreadable {
        OsSourceRead::Unreadable
    } else {
        OsSourceRead::Absent
    }
}

#[cfg(target_os = "macos")]
pub fn read_plist_doc(path: &Path) -> Option<Value> {
    if !path.exists() {
        return None;
    }
    // Subprocess with 5s timeout and 2 MiB cap (Finding 12)
    let mut child = std::process::Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-", "--"])
        .arg(path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut take = (&mut stdout).take(2 * 1024 * 1024 + 1);
        let _ = take.read_to_end(&mut buf);
        buf
    });

    let timeout = Duration::from_secs(5);
    let start = std::time::Instant::now();
    let mut status = None;
    while start.elapsed() < timeout {
        if let Ok(Some(s)) = child.try_wait() {
            status = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        return None;
    }
    let Ok(buf) = reader.join() else {
        return None;
    };
    if buf.len() > 2 * 1024 * 1024 {
        return None;
    }
    match serde_json::from_slice::<Value>(&buf) {
        Ok(val @ Value::Object(_)) => Some(val),
        _ => None,
    }
}

/// Read all platform-managed settings documents in precedence order (Linux / other unix).
#[cfg(all(unix, not(target_os = "macos")))]
fn read_os_managed_source() -> Option<Value> {
    // No MDM tier on Linux: the file tier is the only OS-adjacent source and is
    // re-read separately in `read_managed_sources`.
    None
}

/// Read all platform-managed settings documents in precedence order (Windows).
#[cfg(windows)]
fn read_os_managed_source() -> Option<Value> {
    read_cached_os_source(&OS_MANAGED_CACHE, read_os_managed_source_uncached)
}

#[cfg(windows)]
fn read_os_managed_source_uncached() -> OsSourceRead {
    // 1. MDM via HKLM (highest precedence on Windows)
    // In sdk.mjs y4: keeps only `hklmStdout: n.status === "ok" ? n.stdout : null`.
    // A reg.exe timeout or failure is treated as absent for this resolve; the
    // file tier still applies (Finding 1) and the failure is not cached.
    match query_windows_reg_json(r"HKLM\SOFTWARE\Policies\ClaudeCode", "Settings") {
        RegQuery::Doc(doc) if doc.as_object().is_some_and(|o| !o.is_empty()) => {
            OsSourceRead::Present(doc)
        }
        RegQuery::Doc(_) | RegQuery::Absent => OsSourceRead::Absent,
        RegQuery::DidNotLoad => OsSourceRead::DidNotLoad,
        RegQuery::Unreadable => OsSourceRead::Unreadable,
    }
}

#[cfg(windows)]
fn read_hkcu_source() -> Option<Value> {
    read_cached_os_source(&HKCU_CACHE, read_hkcu_source_uncached)
}

#[cfg(windows)]
fn read_hkcu_source_uncached() -> OsSourceRead {
    // 3. HKCU (only if neither HKLM nor file exists, never merges, Finding 10)
    match query_windows_reg_json(r"HKCU\SOFTWARE\Policies\ClaudeCode", "Settings") {
        RegQuery::Doc(doc) if doc.as_object().is_some_and(|o| !o.is_empty()) => {
            OsSourceRead::Present(doc)
        }
        // A user-writable HKCU value that does not load contributes nothing
        // (sdk.mjs `xp` -> empty settings/errors), so it is `Absent` here.
        RegQuery::Doc(_) | RegQuery::Absent | RegQuery::DidNotLoad => OsSourceRead::Absent,
        RegQuery::Unreadable => OsSourceRead::Unreadable,
    }
}

/// The outcome of one `reg query` invocation.
#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum RegQuery {
    /// The key/value is absent (reg.exe reports not found).
    Absent,
    /// A parseable JSON object value.
    Doc(Value),
    /// The value exists but did not load: malformed JSON, a non-object value,
    /// or a value that exceeded the buffer. The SDK records
    /// `loadState:"didNotLoad"` and suppresses HKCU (item 2).
    DidNotLoad,
    /// reg.exe could not run or timed out. The SDK has no value and no unread
    /// reason, so this is treated as absent for this resolve but never cached
    /// (Finding 1/2).
    Unreadable,
}

#[cfg(windows)]
fn query_windows_reg_json(key: &str, value_name: &str) -> RegQuery {
    let system_root = std::env::var("SYSTEMROOT").unwrap_or_else(|_| r"C:\Windows".to_string());
    let reg_exe = PathBuf::from(system_root).join("System32").join("reg.exe");

    let mut child = match std::process::Command::new(reg_exe)
        .args(["query", key, "/v", value_name])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return RegQuery::Unreadable,
    };

    let mut stdout = match child.stdout.take() {
        Some(s) => s,
        None => return RegQuery::Unreadable,
    };
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut take = (&mut stdout).take(2 * 1024 * 1024 + 1);
        let _ = take.read_to_end(&mut buf);
        buf
    });

    let timeout = Duration::from_secs(5);
    let start = std::time::Instant::now();
    let mut status = None;
    while start.elapsed() < timeout {
        if let Ok(Some(s)) = child.try_wait() {
            status = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        return RegQuery::Unreadable;
    }
    let Ok(buf) = reader.join() else {
        return RegQuery::Unreadable;
    };
    if buf.len() > 2 * 1024 * 1024 {
        // `oZe` sees ERR_CHILD_PROCESS_STDIO_MAXBUFFER -> `hklmUnreadReason`
        // -> didNotLoad, which suppresses HKCU (item 2).
        return RegQuery::DidNotLoad;
    }
    let stdout_str = String::from_utf8_lossy(&buf);
    let Some(val_str) = parse_windows_reg_output(&stdout_str, value_name) else {
        // `y4` sets `hklmStdout` only for a successful `reg.exe`. A successful
        // query whose value line does not parse still reaches `xp("")` =
        // didNotLoad; a failed query leaves the value null (absent).
        return if status.is_some_and(|s| s.success()) {
            RegQuery::DidNotLoad
        } else {
            RegQuery::Absent
        };
    };
    match classify_hklm_value(&val_str) {
        HklmValue::Present(doc) => RegQuery::Doc(doc),
        HklmValue::Absent => RegQuery::Absent,
        HklmValue::DidNotLoad => RegQuery::DidNotLoad,
    }
}

#[cfg(unix)]
const REAPER_PROBE_MARKER: &str = "claude-agent-acp-rs-reaper-v1";

/// Hard recursion guard environment variable (Turn 1, Group G7a-fix6).
/// When set on a child process, prevents that child from launching or probing
/// any further reaper processes.
pub const REAPER_GUARD_ENV: &str = "CLAUDE_ACP_REAPER_GUARD";

static REGISTERED_REAPER_BIN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

#[cfg(unix)]
/// Definitive probe outcomes (pass/fail), cached for the life of the process.
#[cfg(unix)]
static PROBED_REAPER_CACHE: [std::sync::OnceLock<(PathBuf, bool)>; 8] = [
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
];

/// Indeterminate probe outcomes (spawn failure or 2 s timeout), each with the
/// monotonic time it was observed. Remembered for [`PROBE_FAILURE_TTL`] so a
/// hanging candidate does not cost the 2 s bound on every session spawn; after
/// the TTL it is retried (item 4, G7a-fix4). The timestamp is an `AtomicU64`
/// so the cache stays lock-free (the crate forbids mutexes).
#[cfg(unix)]
static PROBED_REAPER_FAILURES: [std::sync::OnceLock<(PathBuf, std::sync::atomic::AtomicU64)>; 8] = [
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
    std::sync::OnceLock::new(),
];

/// How long an indeterminate probe is remembered. Long enough to avoid paying
/// the 2 s timeout on every spawn in a burst, short enough that a candidate
/// that becomes usable (or stops hanging) is retried within a session.
#[cfg(unix)]
const PROBE_FAILURE_TTL: Duration = Duration::from_secs(30);

/// Monotonic epoch for the failure timestamps.
#[cfg(unix)]
static PROBE_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

#[cfg(unix)]
fn probe_now_ms() -> u64 {
    PROBE_EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// The cached verdict for `path`, if any. A stale indeterminate entry returns
/// `None` so the caller re-probes.
#[cfg(unix)]
fn cached_probe_outcome(path: &Path) -> Option<bool> {
    for slot in &PROBED_REAPER_CACHE {
        if let Some((cached_path, result)) = slot.get() {
            if cached_path == path {
                return Some(*result);
            }
        }
    }
    let now = probe_now_ms();
    for slot in &PROBED_REAPER_FAILURES {
        if let Some((cached_path, observed)) = slot.get() {
            if cached_path == path {
                let observed = observed.load(Ordering::SeqCst);
                if now.saturating_sub(observed) < PROBE_FAILURE_TTL.as_millis() as u64 {
                    return Some(false);
                }
                return None;
            }
        }
    }
    None
}

#[cfg(unix)]
fn cache_probe_result(path: &Path, result: bool) {
    for slot in &PROBED_REAPER_CACHE {
        if slot.set((path.to_path_buf(), result)).is_ok() {
            break;
        }
    }
}

#[cfg(unix)]
fn cache_probe_failure(path: &Path) {
    let now = probe_now_ms();
    for slot in &PROBED_REAPER_FAILURES {
        if let Some((cached_path, observed)) = slot.get() {
            if cached_path == path {
                observed.store(now, Ordering::SeqCst);
                return;
            }
        }
    }
    for slot in &PROBED_REAPER_FAILURES {
        if slot
            .set((path.to_path_buf(), std::sync::atomic::AtomicU64::new(now)))
            .is_ok()
        {
            break;
        }
    }
}

/// Explicitly register the path of the adapter binary (called from `bin/claude-agent-acp-rs.rs`).
/// This allows renamed installations to reliably locate the reaper binary without name heuristics (Finding 6).
pub fn set_reaper_binary(path: PathBuf) {
    let _ = REGISTERED_REAPER_BIN.set(path);
}

/// Positively verify if `path` is an adapter binary capable of running `--reaper` (Finding 6, Finding 8).
/// Executes `--reaper --probe` with a 2 s timeout (off the async worker) and
/// caches the result per path: definitive outcomes for the life of the
/// process, indeterminate ones for [`PROBE_FAILURE_TTL`] (item 4, G7a-fix4).
#[cfg(unix)]
pub fn is_reaper_binary(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    if std::env::var_os(REAPER_GUARD_ENV).is_some() {
        return false;
    }

    if let Some(result) = cached_probe_outcome(path) {
        return result;
    }

    match probe_reaper_binary(path) {
        ProbeOutcome::Pass => {
            cache_probe_result(path, true);
            true
        }
        ProbeOutcome::Fail => {
            cache_probe_result(path, false);
            false
        }
        ProbeOutcome::Indeterminate => {
            cache_probe_failure(path);
            false
        }
    }
}

/// The outcome of one `--reaper --probe` handshake.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeOutcome {
    /// The candidate ran, exited 0 and printed the marker.
    Pass,
    /// The candidate ran and exited, but did not print the marker (or failed).
    Fail,
    /// The candidate could not be started or did not exit within the timeout.
    /// Cached for [`PROBE_FAILURE_TTL`] so a hanging candidate does not cost
    /// the 2 s bound on every spawn; retried after the TTL.
    Indeterminate,
}

#[cfg(unix)]
fn probe_reaper_binary(path: &Path) -> ProbeOutcome {
    let path = path.to_path_buf();
    let handle = std::thread::spawn(move || {
        use std::io::Read;
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(&path);
        cmd.args(["--reaper", "--probe"])
            .env(REAPER_GUARD_ENV, "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .process_group(0);

        // Retry ONLY on ETXTBSY ("executable file busy"): a freshly written
        // candidate can still be open for writing in another thread. Any other
        // spawn error (missing, not executable, ...) is definitive. Worst case
        // 50 retries x 20 ms = 1 s of sleeping before giving up.
        let mut attempts = 0;
        let mut child = loop {
            match cmd.spawn() {
                Ok(c) => break c,
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 50 => {
                    attempts += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => return ProbeOutcome::Indeterminate,
            }
        };

        let mut stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return ProbeOutcome::Indeterminate;
            }
        };

        let (tx, rx) = std::sync::mpsc::channel();
        let _reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut take = (&mut stdout).take(1024);
            let _ = take.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });

        let timeout = Duration::from_secs(2);
        let start = std::time::Instant::now();
        let mut status = None;
        while start.elapsed() < timeout {
            match child.try_wait() {
                Ok(Some(s)) => {
                    status = Some(s);
                    break;
                }
                Ok(None) => {}
                Err(_) => break,
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        if status.is_none() {
            let pid = child.id();
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = child.kill();
            let _ = child.wait();
            return ProbeOutcome::Indeterminate;
        }

        let remaining = timeout.saturating_sub(start.elapsed());
        let Ok(buf) = rx.recv_timeout(remaining) else {
            return ProbeOutcome::Indeterminate;
        };

        if !status.is_some_and(|s| s.success()) {
            return ProbeOutcome::Fail;
        }

        if String::from_utf8_lossy(&buf).trim() == REAPER_PROBE_MARKER {
            ProbeOutcome::Pass
        } else {
            ProbeOutcome::Fail
        }
    });

    // `is_reaper_binary` is called synchronously from `spawn_raw`, so joining
    // the 2 s-bounded probe thread here would block the async worker. On a
    // multi-thread runtime move the join off the worker (`block_in_place`,
    // like `read_cached_os_source`); on a current-thread runtime (tests) join
    // directly — there is no other worker to starve.
    let joined = if tokio::runtime::Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        tokio::task::block_in_place(|| handle.join())
    } else {
        handle.join()
    };
    joined.unwrap_or(ProbeOutcome::Indeterminate)
}

/// Resolve path to the reaper supervisor binary (F41).
///
/// A reaper binary is resolved ONLY if:
/// (a) explicitly overridden via `ACP_REAPER_BIN` (passing probe handshake), or
/// (b) explicitly registered by the adapter entry point (`set_reaper_binary`).
///
/// Unregistered hosts (embedders, examples) get NO reaper (log one warning; sessions
/// run normally). Any candidate fallback to `current_exe()` or searching parent directories
/// is prohibited to eliminate fork-bomb / probe-recursion hazards.
#[cfg(unix)]
fn resolve_reaper_binary(env: &crate::env_snapshot::EnvSnapshot) -> Option<PathBuf> {
    if env.flag(REAPER_GUARD_ENV) || std::env::var_os(REAPER_GUARD_ENV).is_some() {
        return None;
    }
    // 1. Explicit override via ACP_REAPER_BIN through the env snapshot
    if let Some(bin) = env.get("ACP_REAPER_BIN") {
        let p = PathBuf::from(bin);
        if is_reaper_binary(&p) {
            return Some(p);
        } else {
            return None;
        }
    }
    // 2. Explicitly registered adapter binary (from main() via set_reaper_binary)
    if let Some(registered) = REGISTERED_REAPER_BIN.get() {
        if is_reaper_binary(registered) {
            return Some(registered.clone());
        } else {
            return None;
        }
    }
    None
}

/// Spawn the reaper supervisor process for a child's process group (F41).
#[cfg(unix)]
fn spawn_reaper(
    child_pid: u32,
    env: &crate::env_snapshot::EnvSnapshot,
) -> (Option<ChildStdin>, Option<u32>) {
    if child_pid == 0 || env.flag(REAPER_GUARD_ENV) || std::env::var_os(REAPER_GUARD_ENV).is_some()
    {
        return (None, None);
    }
    let reaper_bin = match resolve_reaper_binary(env) {
        Some(bin) => bin,
        None => {
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                eprintln!("claude-agent-acp-rs: warning: cannot resolve claude-agent-acp-rs binary for process group reaper; running without reaper");
            }
            return (None, None);
        }
    };
    let host_pid = std::process::id();
    let mut reaper_cmd = Command::new(reaper_bin);
    reaper_cmd.arg("--reaper");
    reaper_cmd.arg(host_pid.to_string());
    reaper_cmd.arg(child_pid.to_string());
    reaper_cmd.env(REAPER_GUARD_ENV, "1");
    reaper_cmd.stdin(std::process::Stdio::piped());
    reaper_cmd.stdout(std::process::Stdio::null());
    reaper_cmd.stderr(std::process::Stdio::null());
    reaper_cmd.process_group(0);
    match reaper_cmd.spawn() {
        Ok(mut child) => {
            let pid = child.id();
            let stdin = child.stdin.take();
            // Automatically reap the supervisor child when it exits so it does not
            // linger as a zombie in the process table (Finding 7 / G7a-fix6).
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            (stdin, pid)
        }
        Err(e) => {
            eprintln!("claude-agent-acp-rs: warning: failed to spawn reaper: {e}");
            (None, None)
        }
    }
}

/// The supervisor process loop executed under `--reaper <parent_pid> <target_pgid>` (F41).
#[cfg(unix)]
pub fn run_reaper(parent_pid: u32, target_pgid: u32) {
    use nix::sys::signal::{killpg, Signal as NixSignal};
    use nix::unistd::{getppid, Pid};
    use std::io::Read;

    let target = Pid::from_raw(target_pgid as i32);
    let expected_parent = Pid::from_raw(parent_pid as i32);

    // Initial check: if the group never existed or is already gone, exit immediately.
    if killpg(target, None) == Err(nix::errno::Errno::ESRCH) {
        return;
    }

    // Spawn a background thread to do a blocking read on stdin.
    // When the host drops the pipe (session closed) or the host dies
    // (even as an un-reaped zombie), the pipe write-end closes and read() returns EOF.
    let (eof_tx, eof_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1];
        let _ = stdin.read(&mut buf);
        let _ = eof_tx.send(());
    });

    loop {
        // 1. Guard against pgid reuse: if the target group already died, exit cleanly (Finding 6).
        if killpg(target, None) == Err(nix::errno::Errno::ESRCH) {
            break;
        }

        // 2. Check for reparenting (parent process died and reaper was reparented).
        if getppid() != expected_parent {
            if killpg(target, None) == Ok(()) {
                let _ = killpg(target, NixSignal::SIGKILL);
            }
            break;
        }

        // 3. Wait for stdin EOF with a bounded timeout (200 ms).
        // On pipe EOF (session drop or host death), this unblocks immediately.
        match eof_rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if killpg(target, None) == Ok(()) {
                    let _ = killpg(target, NixSignal::SIGKILL);
                }
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

/// Windows reaper stub (Windows does not run the Unix process-group supervisor).
#[cfg(windows)]
pub fn run_reaper(_parent_pid: u32, _target_pgid: u32) {}

/// Spawn the `claude` child with the full B2 argv/env and return a [`Process`]
/// (3.1, 3.2, 3.6).
pub async fn spawn(opts: &SpawnOptions) -> Result<Process, SpawnError> {
    let binary = resolve_claude_path(
        opts.claude_path.as_deref(),
        opts.env_snapshot.get("CLAUDE_CODE_EXECUTABLE"),
        bundled_cli_from_host(),
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
    resolve_claude_path(
        claude_path,
        env.get("CLAUDE_CODE_EXECUTABLE"),
        bundled_cli_from_host(),
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

    #[cfg(unix)]
    let (reaper_stdin, reaper_pid) = spawn_reaper(child.id().unwrap_or(0), &opts.env_snapshot);

    #[cfg(windows)]
    let _job = match WindowsJobObject::create_kill_on_close() {
        Ok(job) => {
            if let Some(raw_handle) = child.raw_handle() {
                if let Err(err) = job.assign_child(raw_handle) {
                    eprintln!("claude-agent-acp-rs: warning: failed to assign child to Windows Job Object: {err}");
                }
            }
            Some(job)
        }
        Err(err) => {
            eprintln!("claude-agent-acp-rs: warning: failed to create Windows Job Object: {err}");
            None
        }
    };

    Ok(Process {
        child,
        stdin,
        lines,
        stderr_tail,
        stderr_eof: Some(stderr_eof),
        #[cfg(unix)]
        _reaper_stdin: reaper_stdin,
        #[cfg(unix)]
        reaper_pid,
        #[cfg(windows)]
        _job,
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
/// safely without unsafe C signal handlers is not available.
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
/// the child process (`child.kill()`).
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

    /// F4b — the bundled native CLI candidates mirror Node's `claudeCliPath()`
    /// (`acp-agent.ts:2036-2067`): `<platform>-<arch>[-musl]/claude[.exe]`,
    /// with the musl variant preferred only on a musl host.
    #[test]
    fn bundled_cli_candidates_match_node() {
        assert_eq!(
            bundled_cli_candidates("darwin", "arm64", false),
            vec!["@anthropic-ai/claude-agent-sdk-darwin-arm64/claude".to_string()]
        );
        assert_eq!(
            bundled_cli_candidates("win32", "x64", false),
            vec!["@anthropic-ai/claude-agent-sdk-win32-x64/claude.exe".to_string()]
        );
        assert_eq!(
            bundled_cli_candidates("linux", "x64", false),
            vec![
                "@anthropic-ai/claude-agent-sdk-linux-x64/claude".to_string(),
                "@anthropic-ai/claude-agent-sdk-linux-x64-musl/claude".to_string(),
            ]
        );
        assert_eq!(
            bundled_cli_candidates("linux", "x64", true),
            vec![
                "@anthropic-ai/claude-agent-sdk-linux-x64-musl/claude".to_string(),
                "@anthropic-ai/claude-agent-sdk-linux-x64/claude".to_string(),
            ]
        );
    }

    /// F4b — `find_bundled_cli` walks ancestors for
    /// `node_modules/<candidate>` and returns the first existing binary.
    #[test]
    fn find_bundled_cli_walks_node_modules() {
        let root = std::env::temp_dir().join(format!(
            "acp-bundled-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        let pkg = root
            .join("node_modules")
            .join("@anthropic-ai")
            .join("claude-agent-sdk-darwin-arm64");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("claude"), b"#!/bin/sh\n").unwrap();

        let found = find_bundled_cli(&nested, "darwin", "arm64", false);
        assert_eq!(found, Some(pkg.join("claude")));

        // A missing platform/arch yields nothing.
        assert_eq!(find_bundled_cli(&nested, "linux", "x64", false), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// F4b — resolution precedence: explicit path, `CLAUDE_CODE_EXECUTABLE`,
    /// the bundled binary, then PATH (`claudeCliPath()`).
    #[test]
    fn resolve_claude_path_precedence() {
        let bundled = Some(PathBuf::from("/bundled/claude"));
        assert_eq!(
            resolve_claude_path(
                Some(Path::new("/explicit")),
                Some("/env/claude"),
                bundled.clone(),
                "/nope",
                Os::Unix
            )
            .unwrap(),
            PathBuf::from("/explicit")
        );
        assert_eq!(
            resolve_claude_path(
                None,
                Some("/env/claude"),
                bundled.clone(),
                "/nope",
                Os::Unix
            )
            .unwrap(),
            PathBuf::from("/env/claude")
        );
        assert_eq!(
            resolve_claude_path(None, None, bundled, "/nope", Os::Unix).unwrap(),
            PathBuf::from("/bundled/claude")
        );
        // With no bundled binary the PATH lookup is the last resort.
        assert!(resolve_claude_path(None, None, None, "/nope", Os::Unix).is_err());
    }

    /// O-57: unit test for Windows registry key output parsing.
    #[test]
    fn parse_windows_reg_output_cases() {
        let sample = "HKEY_LOCAL_MACHINE\\SOFTWARE\\Policies\\ClaudeCode\r\n    Settings    REG_SZ    {\"effortLevel\":\"high\",\"deniedModels\":[\"bad\"]}\r\n";
        let parsed = parse_windows_reg_output(sample, "Settings");
        assert_eq!(
            parsed,
            Some("{\"effortLevel\":\"high\",\"deniedModels\":[\"bad\"]}".to_string())
        );

        let expand_sample = "    Settings    REG_EXPAND_SZ    {\"model\":\"opus\"}  \n";
        assert_eq!(
            parse_windows_reg_output(expand_sample, "Settings"),
            Some("{\"model\":\"opus\"}".to_string())
        );

        let missing = "    OtherValue    REG_SZ    foo\n";
        assert_eq!(parse_windows_reg_output(missing, "Settings"), None);
    }

    fn managed_temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acp-managed-sources-{}-{label}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Finding 1: an unread/unparseable MDM plist or a timed-out `reg.exe` is
    /// treated as ABSENT (`None`), which drops ONLY that tier. The managed file
    /// tier still applies; no `{}` fail-closed sentinel is produced.
    #[test]
    fn managed_source_unread_os_keeps_file_tier() {
        let file_doc = serde_json::json!({
            "deniedModels": ["forbidden-model-v1"],
            "permissions": {"disableBypassPermissionsMode": "disable"},
            "apiKeyHelper": "file-managed-helper",
            "env": {"MANAGED_VAR": "managed_val"}
        });

        // Unreadable MDM (None) -> the file documents win.
        assert_eq!(
            select_managed_docs(None, vec![file_doc.clone()], None),
            vec![file_doc.clone()]
        );

        // Nothing readable anywhere -> no managed tier (NOT `[{}]`, which would
        // suppress the file tier on a later call).
        assert_eq!(
            select_managed_docs(None, Vec::new(), None),
            Vec::<Value>::new()
        );

        // A readable MDM document outranks the file tier.
        let mdm_doc = serde_json::json!({"deniedModels": ["mdm-model"]});
        assert_eq!(
            select_managed_docs(Some(mdm_doc.clone()), vec![file_doc], None),
            vec![mdm_doc]
        );

        // HKCU is the last resort only when both higher tiers are empty.
        let hkcu_doc = serde_json::json!({"model": "hkcu-model"});
        assert_eq!(
            select_managed_docs(None, Vec::new(), Some(hkcu_doc.clone())),
            vec![hkcu_doc]
        );
    }

    /// Item 2 (G7a-fix4): an HKLM value that exists but did not load
    /// (malformed, oversized, non-object) must suppress HKCU exactly like
    /// sdk.mjs `Tx` (`fa` is true for `loadState:"didNotLoad"`). This is the
    /// pure winner-selection rule; the Windows registry reader feeds it.
    #[test]
    fn hklm_did_not_load_suppresses_hkcu_tier() {
        // HKLM absent, no file -> HKCU may apply.
        assert!(hkcu_tier_applies(true, false, true));
        // HKLM present but did not load -> HKCU is never consulted.
        assert!(!hkcu_tier_applies(true, true, true));
        // The managed file tier outranks HKCU.
        assert!(!hkcu_tier_applies(true, false, false));
        // HKLM present -> HKCU not consulted.
        assert!(!hkcu_tier_applies(false, false, true));
    }

    /// Item 2 (G7a-fix4): the HKLM value classification matches sdk.mjs `xp`:
    /// a non-empty object loads, an empty object is not policy content, and
    /// malformed/non-object values are `didNotLoad`.
    #[test]
    fn classify_hklm_value_matches_sdk_did_not_load() {
        assert_eq!(
            classify_hklm_value(r#"{"deniedModels":["m"]}"#),
            HklmValue::Present(serde_json::json!({"deniedModels": ["m"]}))
        );
        assert_eq!(classify_hklm_value("{}"), HklmValue::Absent);
        assert_eq!(classify_hklm_value("{not json"), HklmValue::DidNotLoad);
        assert_eq!(classify_hklm_value("[]"), HklmValue::DidNotLoad);
        assert_eq!(classify_hklm_value("\"str\""), HklmValue::DidNotLoad);
    }

    /// Finding 2: the OS-source cache stores a definitive result (present or
    /// absent) like the SDK's `loadPromise`, but never freezes a transient
    /// failure: an `Unreadable` read is retried on the next call.
    /// (macOS/Windows only: Linux has no OS-managed tier.)
    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn os_source_cache_caches_definitive_results_only() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static RETRY_CACHE: std::sync::OnceLock<OsSourceRead> = std::sync::OnceLock::new();
        static CALLS: AtomicUsize = AtomicUsize::new(0);

        fn unreadable() -> OsSourceRead {
            CALLS.fetch_add(1, Ordering::SeqCst);
            OsSourceRead::Unreadable
        }
        fn absent() -> OsSourceRead {
            CALLS.fetch_add(1, Ordering::SeqCst);
            OsSourceRead::Absent
        }

        assert_eq!(read_cached_os_source(&RETRY_CACHE, unreadable), None);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1, "first read happens");
        // Unreadable was not cached, so the next call reads again.
        assert_eq!(read_cached_os_source(&RETRY_CACHE, absent), None);
        assert_eq!(CALLS.load(Ordering::SeqCst), 2, "unreadable is retried");
        // Absent IS cached: the reader is not called again.
        assert_eq!(read_cached_os_source(&RETRY_CACHE, unreadable), None);
        assert_eq!(CALLS.load(Ordering::SeqCst), 2, "absent is cached");

        static PRESENT_CACHE: std::sync::OnceLock<OsSourceRead> = std::sync::OnceLock::new();
        static PRESENT_CALLS: AtomicUsize = AtomicUsize::new(0);
        fn present() -> OsSourceRead {
            PRESENT_CALLS.fetch_add(1, Ordering::SeqCst);
            OsSourceRead::Present(serde_json::json!({"model": "mdm"}))
        }

        assert_eq!(
            read_cached_os_source(&PRESENT_CACHE, present),
            Some(serde_json::json!({"model": "mdm"}))
        );
        assert_eq!(
            read_cached_os_source(&PRESENT_CACHE, present),
            Some(serde_json::json!({"model": "mdm"}))
        );
        assert_eq!(
            PRESENT_CALLS.load(Ordering::SeqCst),
            1,
            "a present document is cached"
        );
    }

    /// Finding 2: the managed file tier is re-read on every resolve (sdk.mjs
    /// `k4` fresh store), unlike the OS source cache. An edit after the first
    /// resolve must be visible to the next one.
    #[test]
    fn managed_file_tier_is_reread_per_resolve() {
        let root = managed_temp_dir("reread");
        let managed = root.join("managed-settings.json");
        std::fs::write(&managed, r#"{"model":"managed-v1"}"#).unwrap();

        let first = read_managed_file_sources_at(&managed);
        assert_eq!(first, vec![serde_json::json!({"model": "managed-v1"})]);

        std::fs::write(&managed, r#"{"model":"managed-v2"}"#).unwrap();
        let second = read_managed_file_sources_at(&managed);
        assert_eq!(second, vec![serde_json::json!({"model": "managed-v2"})]);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The file tier includes `managed-settings.d`, sorted by filename and
    /// skipping hidden files and unparseable documents (sdk.mjs `hx`).
    #[test]
    fn managed_file_tier_reads_d_sorted_skips_hidden_and_invalid() {
        let root = managed_temp_dir("d-dir");
        let managed = root.join("managed-settings.json");
        let d = root.join("managed-settings.d");
        std::fs::create_dir_all(&d).unwrap();

        std::fs::write(&managed, r#"{"model":"base"}"#).unwrap();
        std::fs::write(d.join("20-second.json"), r#"{"model":"second"}"#).unwrap();
        std::fs::write(d.join("10-first.json"), r#"{"model":"first"}"#).unwrap();
        std::fs::write(d.join(".hidden.json"), r#"{"model":"hidden"}"#).unwrap();
        std::fs::write(d.join("30-invalid.json"), "{not json").unwrap();
        std::fs::write(d.join("40-not-json.txt"), r#"{"model":"ignored"}"#).unwrap();

        let docs = read_managed_file_sources_at(&managed);
        assert_eq!(
            docs,
            vec![
                serde_json::json!({"model": "base"}),
                serde_json::json!({"model": "first"}),
                serde_json::json!({"model": "second"}),
            ]
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
