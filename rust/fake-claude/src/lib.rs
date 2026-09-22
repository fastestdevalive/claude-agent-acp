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
    /// The ordered steps.
    pub steps: Vec<Step>,
}

impl Transcript {
    /// Parse a transcript from its JSONL text.
    pub fn parse(text: &str) -> Result<Self, TranscriptError> {
        let mut steps = Vec::new();
        let mut keep_alive_every_ms = None;
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
            let step: Step = serde_json::from_value(value).map_err(|e| TranscriptError::Parse {
                line: idx,
                source: e,
            })?;
            steps.push(step);
        }
        Ok(Self {
            keep_alive_every_ms,
            steps,
        })
    }

    /// Whether the transcript requests periodic keep_alive injection.
    pub fn keep_alive_interval(&self) -> Option<std::time::Duration> {
        self.keep_alive_every_ms
            .map(std::time::Duration::from_millis)
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
}

/// Load a transcript from `$FAKE_CLAUDE_SCRIPT` (or an explicit path).
pub fn load_transcript(path: &Path) -> Result<Transcript, TranscriptError> {
    let text = std::fs::read_to_string(path).map_err(TranscriptError::ReadScript)?;
    Transcript::parse(&text)
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
const ARGV_ENV_ALLOWLIST: [&str; 2] = [
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS",
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
pub fn record_argv_env(out: &Path, argv: &[String]) -> std::io::Result<()> {
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
/// using the matched stdin frame.
pub fn substitute(value: &Value, matched: &Value) -> Value {
    match value {
        Value::String(s) => {
            if s == "$REQ" {
                field_of(matched, "request_id")
            } else if let Some(rest) = s.strip_prefix("$MATCH.") {
                field_of(matched, rest)
            } else {
                Value::String(s.clone())
            }
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute(v, matched)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|v| substitute(v, matched)).collect()),
        other => other.clone(),
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
    mut stdout: impl Write,
) -> Result<usize, ReplayError> {
    if let Some(out) = arg_out {
        record_argv_env(out, argv)?;
    }

    let mut matched_init = false;
    let mut step_index = 0usize;
    let mut reader = BufReader::new(stdin);

    loop {
        if step_index >= transcript.steps.len() {
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
            if std::env::var_os("FAKE_CLAUDE_LOG_FRAMES").is_some() && !line.is_empty() {
                eprintln!("[fake-claude drain] {line}");
            }
            continue;
        }

        let mut line = String::new();
        let bytes = match reader.read_line(&mut line) {
            Ok(b) => b,
            Err(error) => return Err(ReplayError::Io(error)),
        };
        if bytes == 0 {
            // stdin EOF before the transcript finished.
            return Err(ReplayError::Eof {
                remaining: transcript.steps.len() - step_index,
            });
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if std::env::var_os("FAKE_CLAUDE_LOG_FRAMES").is_some() {
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

        let step = &transcript.steps[step_index];
        if !subset_match(&step.expect, &frame) {
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

        for emit in &step.emit {
            // A transcript may insert a pacing marker to separate two emits that
            // otherwise race in the host (e.g. hold the `can_use_tool`
            // control_request until the `assistant` tool_use has been processed,
            // making a permission capture deterministic — phase 10). The marker
            // is not a real frame: fake-claude sleeps instead of writing it.
            if let Some(ms) = emit.get("sleep_ms").and_then(Value::as_u64) {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                continue;
            }
            let resolved = substitute(emit, &frame);
            let mut text = serde_json::to_string(&resolved)?;
            text.push('\n');
            stdout.write_all(text.as_bytes())?;
        }
        if step.keep_alive {
            stdout.write_all(b"{\"type\":\"keep_alive\"}\n")?;
        }
        stdout.flush()?;
        step_index += 1;
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

    /// A `{"sleep_ms": N}` emit marker is consumed (never written as a frame)
    /// and pauses replay, so a transcript can pace two otherwise-racing emits
    /// (phase 10 deterministic-capture fix).
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

    #[test]
    fn subset_match_fails_on_missing_field() {
        let expect = serde_json::json!({"type": "user", "uuid": "x"});
        let frame = serde_json::json!({"type": "user"});
        assert!(!subset_match(&expect, &frame));
    }
}
