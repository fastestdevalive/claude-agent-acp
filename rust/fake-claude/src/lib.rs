#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! `fake-claude` — replays a recorded stream-json transcript to stand in for
//! the `claude` binary (Decision D13, item 2.1).
//!
//! The binary reads newline-delimited JSON from stdin. For each line it checks
//! the current transcript step's `expect` (a subset match). On a match it
//! writes the step's `emit` frames to stdout and advances; on a mismatch it
//! fails loudly (the frame in stderr) and exits non-zero.
//!
//! It ignores its own argv but records the argv and an env **allow-list** it
//! received to `$FAKE_CLAUDE_ARGV_OUT` (JSON
//! `{"argv":[...],"env":{"CLAUDE_CODE_ENTRYPOINT":..,"CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS":..},"node_options_present":bool}`)
//! and the first `initialize` control_request to `$FAKE_CLAUDE_INIT_OUT`.

use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

/// The only `FAKE_CLAUDE_*` environment variables this binary may read (F1).
/// Anything else named `FAKE_CLAUDE_*` is ignored, so a stray variable cannot
/// change replay behaviour; the binary never prints any variable VALUE.
pub const FAKE_CLAUDE_ENV_ALLOWLIST: [&str; 6] = [
    "FAKE_CLAUDE_SCRIPT",
    "FAKE_CLAUDE_ARGV_OUT",
    "FAKE_CLAUDE_INIT_OUT",
    "FAKE_CLAUDE_PID_OUT",
    "FAKE_CLAUDE_LOG_FRAMES",
    "FAKE_CLAUDE_CURSOR",
];

/// Read a `FAKE_CLAUDE_*` variable through [`FAKE_CLAUDE_ENV_ALLOWLIST`].
/// Returns `None` for any name outside the allow-list, even when it is set.
pub fn fake_claude_env(name: &str) -> Option<std::ffi::OsString> {
    if FAKE_CLAUDE_ENV_ALLOWLIST.contains(&name) {
        env::var_os(name)
    } else {
        None
    }
}

/// Errors produced during replay.
#[derive(Debug, Error)]
pub enum ReplayError {
    #[error("stdin frame did not match transcript step:\n  expected subset: {expected}\n  actual frame:      {actual}")]
    Mismatch { expected: String, actual: String },
    #[error("stdin reached EOF with {remaining} transcript steps remaining")]
    Eof { remaining: usize },
    #[error("failed to parse stdin line `{frame}`: {source}")]
    BadFrame {
        frame: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to serialise an emit frame: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// A parsed transcript step (one JSONL line).
#[derive(Debug, Clone, Deserialize)]
pub struct Step {
    /// Subset of the next stdin frame this step matches.
    pub expect: Value,
    /// Frames written to stdout when the step matches.
    #[serde(default)]
    pub emit: Vec<Value>,
    /// Inject a `keep_alive` frame after `emit` (test of R6).
    #[serde(default)]
    pub keep_alive: bool,
    /// Exit (close stdout) after emitting this step — simulating `claude`
    /// dying mid-turn before its `result` (8.T11, stream-EOF lane).
    #[serde(default)]
    pub exit: bool,
}

/// A whole transcript (the file `$FAKE_CLAUDE_SCRIPT`).
#[derive(Debug, Clone, Deserialize)]
pub struct Transcript {
    /// Periodically inject `keep_alive` while idle on stdin.
    #[serde(default)]
    pub keep_alive_every_ms: Option<u64>,
    /// The `claude auth status --json` stdout (special transcript line
    /// `{"auth_status_stdout": {...}}`); never replayed as a step.
    pub auth_status_stdout: Option<Value>,
    /// The ordered steps (all segments).
    pub steps: Vec<Step>,
    /// Step indices where segment 1, 2, ... start. Segment 0 starts at 0; a
    /// transcript with no `{"segment": N}` marker has exactly one segment.
    #[serde(skip)]
    pub segment_starts: Vec<usize>,
}

impl Transcript {
    /// Parse a transcript from its JSONL text.
    ///
    /// A line `{"segment": N}` starts the 0-based segment `N` (segments must be
    /// declared in order, 1, 2, ...). Multi-spawn transcripts use this so a
    /// restarted process can replay only its own segment (F1).
    pub fn parse(text: &str) -> Result<Self, TranscriptError> {
        let mut steps = Vec::new();
        let mut segment_starts = Vec::new();
        let mut keep_alive_every_ms = None;
        let mut auth_status_stdout = None;
        for (idx, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line).map_err(|e| TranscriptError::Parse {
                line: idx,
                source: e,
            })?;
            if let Some(ka) = value.get("keep_alive_every_ms").and_then(Value::as_u64) {
                keep_alive_every_ms = Some(ka);
                continue;
            }
            if let Some(auth) = value.get("auth_status_stdout") {
                auth_status_stdout = Some(auth.clone());
                continue;
            }
            if let Some(segment) = value.get("segment").and_then(Value::as_u64) {
                let expected = segment_starts.len() as u64 + 1;
                if segment != expected {
                    return Err(TranscriptError::BadSegment {
                        line: idx,
                        expected,
                        got: segment,
                    });
                }
                segment_starts.push(steps.len());
                continue;
            }
            let step: Step = serde_json::from_value(value).map_err(|e| TranscriptError::Parse {
                line: idx,
                source: e,
            })?;
            steps.push(step);
        }
        Ok(Self {
            keep_alive_every_ms,
            auth_status_stdout,
            steps,
            segment_starts,
        })
    }

    /// Whether the transcript requests periodic keep_alive injection.
    pub fn keep_alive_interval(&self) -> Option<std::time::Duration> {
        self.keep_alive_every_ms
            .map(std::time::Duration::from_millis)
    }

    /// Number of segments (1 for a transcript without segment markers).
    pub fn segment_count(&self) -> usize {
        self.segment_starts.len() + 1
    }

    /// The steps of segment `index` (clamped to the last segment).
    pub fn segment(&self, index: usize) -> &[Step] {
        let index = index.min(self.segment_count() - 1);
        let start = if index == 0 {
            0
        } else {
            self.segment_starts[index - 1]
        };
        let end = self
            .segment_starts
            .get(index)
            .copied()
            .unwrap_or(self.steps.len());
        &self.steps[start..end]
    }
}

/// Errors produced while loading or replaying a transcript.
#[derive(Debug, Error)]
pub enum TranscriptError {
    #[error("failed to parse transcript line {line}: {source}")]
    Parse {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to read transcript: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to read script path: {0}")]
    ReadScript(#[source] std::io::Error),
    #[error("transcript line {line}: segment marker expected {expected}, got {got}")]
    BadSegment {
        line: usize,
        expected: u64,
        got: u64,
    },
}

/// Load a transcript from `$FAKE_CLAUDE_SCRIPT` (or an explicit path).
pub fn load_transcript(path: &Path) -> Result<Transcript, TranscriptError> {
    let text = std::fs::read_to_string(path).map_err(TranscriptError::ReadScript)?;
    Transcript::parse(&text)
}

/// The 0-based segment a fresh spawn should replay (F1 multi-spawn).
///
/// When the transcript has more than one segment and `FAKE_CLAUDE_CURSOR` names
/// a file, the file holds the ordinal of this spawn (0 for the first) and is
/// advanced for the next spawn — so a second process after a restart reads the
/// next segment. Without the env var (or for a single-segment transcript) every
/// spawn replays segment 0: existing fixtures are unchanged and nothing is ever
/// written into the repo.
pub fn next_segment_index(transcript: &Transcript) -> usize {
    if transcript.segment_count() <= 1 {
        return 0;
    }
    let Some(cursor) = fake_claude_env("FAKE_CLAUDE_CURSOR") else {
        return 0;
    };
    let cursor = std::path::PathBuf::from(cursor);
    let ordinal = std::fs::read_to_string(&cursor)
        .ok()
        .and_then(|text| text.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let _ = std::fs::write(&cursor, (ordinal + 1).to_string());
    ordinal.min(transcript.segment_count() - 1)
}

/// The only environment variables `fake-claude` may ever persist to
/// `$FAKE_CLAUDE_ARGV_OUT`. Everything else in the process environment
/// (tokens, sockets, paths, credentials) is discarded — never written to disk.
///
/// This is a test-fixture hygiene rule ONLY: it governs what gets written into
/// the committed fixture JSON (so live secrets never reach git), NOT what env
/// vars the spawned `claude` process receives at runtime. `fake-claude` itself
/// is launched via a [`std::process::Command`] that inherits the full parent
/// env (no `.env_clear()`), matching the crate's `apply_claude_env`
/// (`process.rs`) and the Node adapter's `{ ...process.env, ...providerEnv }`.
/// Do not read this list as the set of env vars forwarded to `claude`.
const ARGV_ENV_ALLOWLIST: [&str; 5] = [
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS",
    "CLAUDE_CODE_SDK_READS_SESSION_STATE",
    "CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING",
    "CLAUDE_CODE_ARTIFACT",
];

/// Record the argv and the allow-listed env we were launched with to `out`.
///
/// Security: only `ARGV_ENV_ALLOWLIST` entries are written, plus a boolean
/// `node_options_present` (whether `NODE_OPTIONS` was set). No other
/// environment variable may ever be persisted, so live credentials and
/// machine-specific paths never reach the committed fixtures.
///
/// Portability: the *presence* of `CLAUDE_CODE_ENTRYPOINT` is what matters
/// (B2 requires the port to set it); its raw value differs by environment
/// (`sdk-cli` vs `cli`) and must never reach a committed fixture (2.T1, the
/// deterministic-capture invariant). It is therefore recorded only as
/// `"$ENTRYPOINT"` — the committed argv fixture is byte-stable across
/// environments even though the value is not.
///
/// Exactly one argv record is kept per capture script: the SESSION spawn. The
/// 0.94+ Node adapter also spawns a probe process (`claude auth status --json`,
/// argv `["auth", ...]`) sharing `$FAKE_CLAUDE_ARGV_OUT`; a probe invocation
/// must never clobber the session record, and the record is written (overwriting
/// any stale probe file) only on a non-probe spawn.
pub fn record_argv_env(out: &Path, argv: &[String]) -> std::io::Result<()> {
    let is_auth_probe = argv.first().map(String::as_str) == Some("auth");
    if is_auth_probe {
        // The `claude auth status --json` probe is not the spawn this fixture
        // records — keep the session argv record (or no record) untouched.
        return Ok(());
    }
    let mut env_map = serde_json::Map::new();
    for key in ARGV_ENV_ALLOWLIST {
        if env::var_os(key).is_some() {
            let value = if key == "CLAUDE_CODE_ENTRYPOINT" {
                // Presence-only: normalise away the environment-specific value.
                "$ENTRYPOINT".to_string()
            } else {
                env::var_os(key)
                    .map(|v| v.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            env_map.insert(key.to_string(), Value::String(value));
        }
    }
    let node_options_present = env::var_os("NODE_OPTIONS").is_some();
    let json = serde_json::json!({
        "argv": argv,
        "env": env_map,
        "node_options_present": node_options_present,
    });
    write_json(out, &json)
}

/// Record a captured stdin frame to `out` as JSON.
pub fn record_frame(out: &Path, frame: &Value) -> std::io::Result<()> {
    write_json(out, frame)
}

fn write_json(out: &Path, value: &Value) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    std::fs::write(out, text)
}

/// Control-request subtypes `fake-claude` auto-acks (Decision 5): emitted by
/// the 0.94+ Node adapter after session creation (`get_context_usage`), these
/// are answered without consuming a transcript step.
const AUTO_ACK_SUBTYPES: [&str; 1] = ["get_context_usage"];

/// If `frame` is a `control_request` whose `request.subtype` is in
/// [`AUTO_ACK_SUBTYPES`], return the Decision-5 success `control_response` to
/// write back. `None` for every other frame.
fn auto_ack_response(frame: &Value) -> Option<Value> {
    let is_control_request = frame.get("type").and_then(Value::as_str) == Some("control_request");
    if !is_control_request {
        return None;
    }
    let subtype = frame
        .get("request")
        .and_then(|r| r.get("subtype"))
        .and_then(Value::as_str)?;
    if !AUTO_ACK_SUBTYPES.contains(&subtype) {
        return None;
    }
    let request_id = frame.get("request_id").cloned().unwrap_or(Value::Null);
    Some(serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {}
        }
    }))
}

/// Write one JSON frame to `stdout`, followed by a newline, and flush.
fn write_frame(stdout: &mut impl Write, value: &Value) -> std::io::Result<()> {
    let mut text = serde_json::to_string(value)?;
    text.push('\n');
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

/// True when every field of `expect` is present in `frame` with an equal value
/// (a subset match).
pub fn subset_match(expect: &Value, frame: &Value) -> bool {
    match (expect, frame) {
        (Value::Object(exp), Value::Object(frm)) => exp
            .iter()
            .all(|(k, ev)| frm.get(k).map(|fv| subset_match(ev, fv)).unwrap_or(false)),
        (Value::Array(exp), Value::Array(frm)) => {
            exp.len() <= frm.len() && exp.iter().zip(frm.iter()).all(|(e, f)| subset_match(e, f))
        }
        (Value::Number(a), Value::Number(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Null, Value::Null) => true,
        _ => false,
    }
}

/// Substitute `$REQ` (== `$MATCH.request_id`) and `$MATCH.<field>` in `value`
/// using the matched stdin frame. `$CWD` expands inside any string.
pub fn substitute(value: &Value, matched: &Value) -> Value {
    substitute_with(value, matched, &[])
}

/// As [`substitute`], additionally resolving `$REQ.<n>` (`n >= 1`) to the
/// `request_id` of the frame the n-th PREVIOUS transcript step matched, so a
/// step can answer a request that an earlier step deliberately left unanswered
/// (`$REQ.1` is the immediately previous step, `$REQ.2` the one before, ...).
pub fn substitute_with(value: &Value, matched: &Value, history: &[Value]) -> Value {
    match value {
        Value::String(s) => {
            if s == "$REQ" {
                field_of(matched, "request_id")
            } else if let Some(rest) = s.strip_prefix("$REQ.") {
                match rest.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= history.len() => {
                        field_of(&history[history.len() - n], "request_id")
                    }
                    _ => Value::Null,
                }
            } else if let Some(rest) = s.strip_prefix("$MATCH.") {
                field_of(matched, rest)
            } else if s.contains("$CWD") {
                Value::String(s.replace("$CWD", &canonical_cwd()))
            } else {
                Value::String(s.clone())
            }
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_with(v, matched, history)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| substitute_with(v, matched, history))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The canonical current directory, as text (`$CWD`).
fn canonical_cwd() -> String {
    let cwd = env::current_dir().unwrap_or_else(|_| "/".into());
    cwd.canonicalize()
        .unwrap_or(cwd)
        .to_string_lossy()
        .into_owned()
}

/// File whose presence makes `auth status` report logged-out.
const LOGGED_OUT_MARKER: &str = ".fake-claude-logged-out";

/// Result of an `auth` invocation: stdout text and the exit code.
#[derive(Debug, PartialEq, Eq)]
pub struct AuthOutcome {
    pub stdout: String,
    pub code: i32,
}

/// Emulate `claude auth ...` (`argv[0] == "auth"`). `status` prints the
/// transcript's `auth_status_stdout` line (exit 0), or exits 1 with no output
/// when the transcript has none. `logout` creates the marker under `config_dir`
/// (exit 0); while it exists `status` prints `{"loggedIn":false}` and exits 1.
pub fn run_auth(
    argv: &[String],
    transcript: Option<&Transcript>,
    config_dir: Option<&Path>,
) -> AuthOutcome {
    let marker = config_dir.map(|dir| dir.join(LOGGED_OUT_MARKER));
    let fail = AuthOutcome {
        stdout: String::new(),
        code: 1,
    };
    match argv.get(1).map(String::as_str) {
        Some("logout") => match marker {
            Some(path) if std::fs::write(&path, b"").is_ok() => AuthOutcome {
                stdout: String::new(),
                code: 0,
            },
            _ => fail,
        },
        Some("status") => {
            if marker.is_some_and(|path| path.exists()) {
                return AuthOutcome {
                    stdout: "{\"loggedIn\":false}\n".to_string(),
                    code: 1,
                };
            }
            match transcript.and_then(|t| t.auth_status_stdout.as_ref()) {
                Some(value) => AuthOutcome {
                    stdout: format!("{value}\n"),
                    code: 0,
                },
                None => fail,
            }
        }
        _ => fail,
    }
}

/// Look up a dotted field path in a matched frame, serialising non-string
/// values to JSON.
fn field_of(matched: &Value, path: &str) -> Value {
    let mut current = matched;
    for part in path.split('.') {
        match current.get(part) {
            Some(next) => current = next,
            None => return Value::Null,
        }
    }
    match current {
        Value::String(s) => Value::String(s.clone()),
        other => other.clone(),
    }
}

/// Replay `transcript` against stdin/stdout. Returns the number of steps
/// consumed; fails loudly (stderr) on a mismatch.
///
/// The out params allow tests and the binary to capture the argv/env and the
/// initialize frame. `arg_out`/`init_out` are written before replay starts.
pub fn replay(
    transcript: &Transcript,
    argv: &[String],
    arg_out: Option<&Path>,
    init_out: Option<&Path>,
    stdin: impl Read,
    stdout: impl Write,
) -> Result<usize, ReplayError> {
    replay_segment(transcript, 0, argv, arg_out, init_out, stdin, stdout)
}

/// Replay only `segment_index` of `transcript` (F1 multi-spawn: a restarted
/// process reads the next segment). [`replay`] is `replay_segment(..., 0, ...)`.
pub fn replay_segment(
    transcript: &Transcript,
    segment_index: usize,
    argv: &[String],
    arg_out: Option<&Path>,
    init_out: Option<&Path>,
    stdin: impl Read,
    mut stdout: impl Write,
) -> Result<usize, ReplayError> {
    if let Some(out) = arg_out {
        record_argv_env(out, argv)?;
    }

    let steps = transcript.segment(segment_index);
    let mut matched_init = false;
    let mut step_index = 0usize;
    // Frames matched by earlier steps, in order; `$REQ.<n>` indexes backwards.
    let mut history: Vec<Value> = Vec::new();
    let mut reader = BufReader::new(stdin);

    loop {
        if step_index >= steps.len() {
            // All steps consumed: keep reading stdin until EOF (claude stays
            // alive until its stdin closes), logging any late frames so we can
            // see what the host still sends after the transcript is spent.
            let mut line = String::new();
            let bytes = match reader.read_line(&mut line) {
                Ok(b) => b,
                Err(error) => return Err(ReplayError::Io(error)),
            };
            if bytes == 0 {
                return Ok(step_index);
            }
            let line = line.trim();
            if fake_claude_env("FAKE_CLAUDE_LOG_FRAMES").is_some() && !line.is_empty() {
                eprintln!("[fake-claude drain] {line}");
            }
            // After the transcript is spent, still auto-ack a late
            // get_context_usage so the host never blocks on an unanswered
            // control_request (Decision 5).
            if !line.is_empty() {
                if let Ok(frame) = serde_json::from_str::<Value>(line) {
                    if let Some(response) = auto_ack_response(&frame) {
                        write_frame(&mut stdout, &response)?;
                    }
                }
            }
            continue;
        }

        let mut line = String::new();
        let bytes = match reader.read_line(&mut line) {
            Ok(b) => b,
            Err(error) => return Err(ReplayError::Io(error)),
        };
        if bytes == 0 {
            // stdin EOF before the transcript segment finished.
            return Err(ReplayError::Eof {
                remaining: steps.len() - step_index,
            });
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if fake_claude_env("FAKE_CLAUDE_LOG_FRAMES").is_some() {
            eprintln!("[fake-claude stdin] {line}");
        }
        let frame: Value = match serde_json::from_str(line) {
            Ok(frame) => frame,
            Err(error) => {
                return Err(ReplayError::BadFrame {
                    frame: line.into(),
                    source: error,
                })
            }
        };

        let step = &steps[step_index];
        if !subset_match(&step.expect, &frame) {
            // Not the expected step: if it is an auto-ackable control_request
            // (e.g. `get_context_usage`), respond without consuming a step
            // rather than failing (Decision 5). The incoming frame was already
            // logged under FAKE_CLAUDE_LOG_FRAMES above.
            if let Some(response) = auto_ack_response(&frame) {
                write_frame(&mut stdout, &response)?;
                continue;
            }
            return Err(ReplayError::Mismatch {
                expected: serde_json::to_string(&step.expect).unwrap_or_default(),
                actual: line.to_string(),
            });
        }

        if !matched_init && is_initialize(&frame) {
            if let Some(out) = init_out {
                record_frame(out, &frame)?;
            }
            matched_init = true;
        }

        // Write every non-marker frame of this step in ONE `write_all` (F42).
        //
        // The host reads the child's stdout line-by-line. Two frames written
        // separately can be read in two different wake-ups, so a host that
        // processes one line per read can finish a whole round-trip (e.g. the
        // first `can_use_tool` permission) before it ever reads the second
        // line. A single <= PIPE_BUF write keeps the step's frames in one pipe
        // buffer, so the host reads them all before dispatching the first one —
        // the duplicate-`request_id` scenario (`sdkctl-duplicate-request`)
        // then deterministically routes both duplicates before the first one
        // can complete, instead of flaking under load.
        let mut batch: Vec<u8> = Vec::new();
        for emit in &step.emit {
            // A transcript may insert a pacing marker to separate two emits that
            // otherwise race in the host (e.g. hold the `can_use_tool`
            // control_request until the `assistant` tool_use has been processed,
            // making a permission capture deterministic — phase 10). The marker
            // is not a real frame: fake-claude flushes the frames emitted before
            // it, sleeps, and continues. Only transcripts whose Node capture
            // needs the pause keep it (F42).
            if let Some(ms) = emit.get("sleep_ms").and_then(Value::as_u64) {
                stdout.write_all(&batch)?;
                stdout.flush()?;
                batch.clear();
                std::thread::sleep(std::time::Duration::from_millis(ms));
                continue;
            }
            let resolved = substitute_with(emit, &frame, &history);
            let mut text = serde_json::to_string(&resolved)?;
            text.push('\n');
            batch.extend_from_slice(text.as_bytes());
        }
        if step.keep_alive {
            batch.extend_from_slice(b"{\"type\":\"keep_alive\"}\n");
        }
        stdout.write_all(&batch)?;
        stdout.flush()?;
        step_index += 1;
        history.push(frame);
        if step.exit {
            // The transcript asks us to die here (e.g. mid-turn before a
            // `result`): return so main exits and stdout closes, letting the
            // host see the stream EOF.
            return Ok(step_index);
        }
    }
}

fn is_initialize(frame: &Value) -> bool {
    frame.get("type").and_then(Value::as_str) == Some("control_request")
        && frame
            .get("request")
            .and_then(|r| r.get("subtype"))
            .and_then(Value::as_str)
            == Some("initialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_transcript_with_keep_alive_option() {
        let text = "{\"keep_alive_every_ms\": 500}\n{\"expect\": {\"type\": \"user\"}, \"emit\": [{\"type\": \"result\", \"subtype\": \"success\"}]}\n";
        let transcript = Transcript::parse(text).unwrap();
        assert_eq!(transcript.steps.len(), 1);
        assert!(transcript.keep_alive_interval().is_some());
    }

    #[test]
    fn subset_match_is_recursive() {
        let expect = serde_json::json!({"type": "user", "message": {"role": "user"}});
        let frame = serde_json::json!({
            "type": "user",
            "uuid": "abc",
            "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}
        });
        assert!(subset_match(&expect, &frame));
    }

    #[test]
    fn subset_match_misses_on_differing_value() {
        let expect = serde_json::json!({"type": "result", "subtype": "success"});
        let frame = serde_json::json!({"type": "result", "subtype": "error"});
        assert!(!subset_match(&expect, &frame));
    }

    /// A writer that records the size of every `write`/`flush` call, so a test
    /// can assert how many pipe writes one transcript step produced (F42).
    #[derive(Default)]
    struct CountingWriter {
        writes: Vec<Vec<u8>>,
        flushes: usize,
    }

    impl Write for CountingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes.push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    /// F42: every frame of one step is emitted in a SINGLE write. The host then
    /// reads the whole step before dispatching its first line, which keeps the
    /// duplicate-`request_id` scenario deterministic without a sleep.
    #[test]
    fn emits_of_one_step_are_written_in_one_write() {
        let text = "{\"expect\":{\"type\":\"user\"},\"emit\":[{\"type\":\"assistant\",\"n\":1},{\"type\":\"control_request\",\"request_id\":\"dup\"},{\"type\":\"control_request\",\"request_id\":\"dup\"}]}\n";
        let transcript = Transcript::parse(text).unwrap();
        let mut writer = CountingWriter::default();
        let steps = replay(
            &transcript,
            &[],
            None,
            None,
            br#"{"type":"user","uuid":"u","message":{}}"#.as_slice(),
            &mut writer,
        )
        .unwrap();
        assert_eq!(steps, 1);
        assert_eq!(
            writer.writes.len(),
            1,
            "all frames of one step must travel in one write"
        );
        let frames: Vec<Value> = writer.writes[0]
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(frames.len(), 3);
    }

    /// A `{"sleep_ms": N}` emit marker is consumed (never written as a frame),
    /// flushes what came before it, and pauses replay, so a transcript can pace
    /// two otherwise-racing emits (phase 10 deterministic-capture fix; F42 keeps
    /// it only where the Node capture needs the pause).
    #[test]
    fn sleep_marker_pauses_without_writing_a_frame() {
        let text = "{\"expect\":{\"type\":\"user\"},\"emit\":[{\"type\":\"assistant\",\"n\":1},{\"sleep_ms\":1},{\"type\":\"result\",\"n\":2}]}\n";
        let transcript = Transcript::parse(text).unwrap();
        let mut stdout = Vec::new();
        let start = std::time::Instant::now();
        let steps = replay(
            &transcript,
            &[],
            None,
            None,
            br#"{"type":"user","uuid":"u","message":{}}"#.as_slice(),
            &mut stdout,
        )
        .unwrap();
        assert_eq!(steps, 1);
        let stdout = String::from_utf8(stdout).unwrap();
        let frames: Vec<serde_json::Value> = stdout
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            frames.len(),
            2,
            "sleep marker must not be written as a frame"
        );
        assert_eq!(frames[0]["n"], 1);
        assert_eq!(frames[1]["n"], 2);
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(1),
            "sleep marker must pause replay"
        );
    }

    /// F42: a sleep marker flushes the frames emitted before it, so the pause
    /// really happens after they reached the host.
    #[test]
    fn sleep_marker_splits_the_write() {
        let text = "{\"expect\":{\"type\":\"user\"},\"emit\":[{\"type\":\"assistant\",\"n\":1},{\"sleep_ms\":1},{\"type\":\"control_request\",\"n\":2}]}\n";
        let transcript = Transcript::parse(text).unwrap();
        let mut writer = CountingWriter::default();
        let steps = replay(
            &transcript,
            &[],
            None,
            None,
            br#"{"type":"user","uuid":"u","message":{}}"#.as_slice(),
            &mut writer,
        )
        .unwrap();
        assert_eq!(steps, 1);
        assert_eq!(
            writer.writes.len(),
            2,
            "the sleep marker must flush before pausing"
        );
        assert_eq!(writer.flushes, 2);
    }

    #[test]
    fn subset_match_fails_on_missing_field() {
        let expect = serde_json::json!({"type": "user", "uuid": "x"});
        let frame = serde_json::json!({"type": "user"});
        assert!(!subset_match(&expect, &frame));
    }

    /// F1 multi-spawn: `{"segment": N}` markers split the transcript, and each
    /// segment slices only its own steps (the last segment is clamped).
    #[test]
    fn parses_segment_markers_and_slices() {
        let text = concat!(
            "{\"expect\":{\"type\":\"a\"}}\n",
            "{\"segment\":1}\n",
            "{\"expect\":{\"type\":\"b\"}}\n",
            "{\"segment\":2}\n",
            "{\"expect\":{\"type\":\"c\"}}\n"
        );
        let transcript = Transcript::parse(text).unwrap();
        assert_eq!(transcript.segment_count(), 3);
        assert_eq!(transcript.segment(0).len(), 1);
        assert_eq!(transcript.segment(1).len(), 1);
        assert_eq!(transcript.segment(2).len(), 1);
        assert_eq!(
            transcript.segment(99).len(),
            1,
            "clamped to the last segment"
        );
        assert_eq!(
            transcript.segment(1)[0].expect,
            serde_json::json!({"type": "b"}),
            "segment 1 starts after its marker"
        );
    }

    /// F1: a single-segment transcript stays one segment and `replay` is segment 0.
    #[test]
    fn transcript_without_markers_has_one_segment() {
        let transcript =
            Transcript::parse("{\"expect\":{\"type\":\"user\"},\"emit\":[]}\n").unwrap();
        assert_eq!(transcript.segment_count(), 1);
        assert_eq!(transcript.segment(0).len(), 1);
    }

    /// F1: segment markers must be declared in order (a typo fails loudly).
    #[test]
    fn rejects_out_of_order_segment_marker() {
        let err = Transcript::parse("{\"segment\":2}\n").unwrap_err();
        assert!(
            matches!(
                err,
                TranscriptError::BadSegment {
                    expected: 1,
                    got: 2,
                    ..
                }
            ),
            "got: {err}"
        );
    }

    /// F1 `$REQ.n`: n-th previous matched frame's `request_id` (1 = previous).
    #[test]
    fn req_placeholder_indexes_backwards() {
        let history = vec![
            serde_json::json!({"request_id": "R1"}),
            serde_json::json!({"request_id": "R2"}),
        ];
        let matched = serde_json::json!({"request_id": "R3"});
        assert_eq!(
            substitute_with(&serde_json::json!("$REQ"), &matched, &history),
            serde_json::json!("R3")
        );
        assert_eq!(
            substitute_with(&serde_json::json!("$REQ.1"), &matched, &history),
            serde_json::json!("R2")
        );
        assert_eq!(
            substitute_with(&serde_json::json!("$REQ.2"), &matched, &history),
            serde_json::json!("R1")
        );
        assert_eq!(
            substitute_with(&serde_json::json!("$REQ.3"), &matched, &history),
            serde_json::Value::Null
        );
        assert_eq!(
            substitute_with(&serde_json::json!("$REQ.0"), &matched, &history),
            serde_json::Value::Null
        );
    }

    /// F1: only allow-listed `FAKE_CLAUDE_*` names are readable; a stray name is
    /// ignored even when set (names only — no value is ever printed).
    #[test]
    fn fake_claude_env_is_allowlisted() {
        std::env::set_var("FAKE_CLAUDE_NOT_ALLOWED", "set-but-ignored");
        assert_eq!(fake_claude_env("FAKE_CLAUDE_NOT_ALLOWED"), None);
        assert!(FAKE_CLAUDE_ENV_ALLOWLIST.contains(&"FAKE_CLAUDE_SCRIPT"));
        assert!(FAKE_CLAUDE_ENV_ALLOWLIST.contains(&"FAKE_CLAUDE_CURSOR"));
        assert_eq!(FAKE_CLAUDE_ENV_ALLOWLIST.len(), 6);
        std::env::remove_var("FAKE_CLAUDE_NOT_ALLOWED");
    }
}
