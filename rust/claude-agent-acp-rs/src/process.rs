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
    /// `--permission-mode <mode>`.
    pub permission_mode: Option<String>,
    /// `--setting-sources=<csv>` (default `user,project,local`).
    pub setting_sources: Vec<String>,
    /// `--disallowedTools <csv>`.
    pub disallowed_tools: Vec<String>,
    /// `--tools default` / `--tools <csv>`; `None` omits the flag.
    pub tools: Option<Vec<String>>,
    /// `--include-partial-messages`.
    pub include_partial_messages: bool,
    /// `--permission-prompt-tool stdio`.
    pub permission_prompt_tool: bool,
    /// `--allow-dangerously-skip-permissions`.
    pub allow_dangerously_skip_permissions: bool,
    /// `--replay-user-messages` (plus the SDK's trailing empty value).
    pub replay_user_messages: bool,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            claude_path: None,
            extra_env: Vec::new(),
            default_cwd: None,
            session_id: None,
            resume: None,
            permission_mode: None,
            setting_sources: vec!["user".into(), "project".into(), "local".into()],
            disallowed_tools: Vec::new(),
            tools: None,
            include_partial_messages: false,
            permission_prompt_tool: false,
            allow_dangerously_skip_permissions: false,
            replay_user_messages: false,
        }
    }
}

/// Build the child argv for the given options (B2 table).
///
/// Order reproduces the SDK's `ProcessTransport` argv at `v0.70.0` (verified
/// against `porting/fixtures/*.argv.json` by 3.T1).
pub fn build_argv(opts: &SpawnOptions) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--input-format".into(),
        "stream-json".into(),
    ];

    if opts.permission_prompt_tool {
        argv.push("--permission-prompt-tool".into());
        argv.push("stdio".into());
    }

    if let Some(resume) = &opts.resume {
        argv.push(format!("--resume={resume}"));
    }

    if !opts.disallowed_tools.is_empty() {
        argv.push("--disallowedTools".into());
        argv.push(opts.disallowed_tools.join(","));
    }

    match &opts.tools {
        Some(tools) if tools.is_empty() => {
            argv.push("--tools".into());
            argv.push(String::new());
        }
        Some(tools) => {
            argv.push("--tools".into());
            argv.push(tools.join(","));
        }
        None => {
            argv.push("--tools".into());
            argv.push("default".into());
        }
    }

    argv.push(format!(
        "--setting-sources={}",
        opts.setting_sources.join(",")
    ));

    if let Some(mode) = &opts.permission_mode {
        argv.push("--permission-mode".into());
        argv.push(mode.clone());
    }

    if opts.allow_dangerously_skip_permissions {
        argv.push("--allow-dangerously-skip-permissions".into());
    }

    if opts.include_partial_messages {
        argv.push("--include-partial-messages".into());
    }

    if let Some(session_id) = &opts.session_id {
        argv.push(format!("--session-id={session_id}"));
    }

    if opts.replay_user_messages {
        // The SDK pushes `--replay-user-messages` and an empty trailing value
        // via `Tk(extraArgs)`.
        argv.push("--replay-user-messages".into());
        argv.push(String::new());
    }

    argv
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

/// Apply the fixed `claude` environment (B2): merge `extra_env`, set
/// `CLAUDE_CODE_ENTRYPOINT`, set `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1`,
/// remove `NODE_OPTIONS`. Cross-platform; no `cfg` needed here.
fn apply_claude_env(cmd: &mut Command, extra_env: &[(String, String)]) {
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts");
    cmd.env("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1");
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

/// Spawn the `claude` child with the full B2 argv/env and return a [`Process`]
/// (3.1, 3.2, 3.6).
pub async fn spawn(opts: &SpawnOptions) -> Result<Process, SpawnError> {
    let binary = resolve_binary(
        opts.claude_path.as_deref(),
        std::env::var("CLAUDE_CODE_EXECUTABLE").ok().as_deref(),
        &std::env::var("PATH").unwrap_or_default(),
        current_os(),
    )?;
    let argv = build_argv(opts);
    spawn_raw(binary, argv, opts).await
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
