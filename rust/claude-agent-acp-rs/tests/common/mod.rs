//! Shared differential-harness helpers for `claude-agent-acp-rs` integration tests.
//!
//! This module is `tests/`-only: it is never compiled into the crate itself and
//! never `pub` in `src/` (guard G6). It provides the ordered-frame differ used
//! by the Node-vs-Rust differential harness (Decision D13) plus process-transport
//! helpers (phase 3).
//!
//! A "frame" is one JSON-RPC message exchanged over the wire in one direction.
//! The differ compares two ordered frame lists after:
//!
//! 1. dropping whole frames matched by an ignore path's value filter (e.g. a
//!    `usage_update` frame the Rust side never emits),
//! 2. removing nodes at plain-field ignore paths (e.g. `result.configOptions`),
//! 3. normalising ids/uuids/timestamps to first-appearance placeholders.
//!
//! Because this shared module is `mod`-included by several independent test
//! binaries (each with a different slice of helpers in use), `dead_code` is
//! allowed: a helper unused by one test binary is used by another.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde_json::Value;

/// The direction a frame travelled across the client/agent boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// client -> agent (written to the agent's stdin).
    Send,
    /// agent -> client (read from the agent's stdout).
    Recv,
}

/// One ordered JSON-RPC frame plus its direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub direction: Direction,
    pub json: Value,
}

impl Frame {
    /// A frame sent client -> agent.
    pub fn send(json: Value) -> Self {
        Self {
            direction: Direction::Send,
            json,
        }
    }

    /// A frame received agent -> client.
    pub fn recv(json: Value) -> Self {
        Self {
            direction: Direction::Recv,
            json,
        }
    }
}

/// A JSON-path expression used to ignore volatile or intentionally-divergent
/// parts of a frame.
///
/// A path is a dot-separated list of object-field names. A trailing
/// `[key=value]` filter turns the segment into a filter: it matches when the
/// value at that segment is an object whose `key == value`, or an array any of
/// whose elements satisfy that.
///
/// Examples:
///
/// ```text
/// result.configOptions
/// result.modes
/// params.update[sessionUpdate=usage_update]
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonPath {
    segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Field(String),
    Filtered {
        field: String,
        key: String,
        value: String,
    },
}

impl JsonPath {
    fn has_filter(&self) -> bool {
        self.segments
            .iter()
            .any(|s| matches!(s, Segment::Filtered { .. }))
    }

    /// Whether any segment of the path resolves within `value`.
    fn matches(&self, value: &Value) -> bool {
        resolve(value, &self.segments).is_some()
    }

    /// Remove every node in `value` that the path targets, returning the
    /// (possibly unchanged) JSON with those nodes removed.
    fn remove_nodes(&self, value: Value) -> Value {
        strip(value, &self.segments)
    }
}

/// Walk `segments` through `value`; return `Some(())` if every segment
/// resolves and the final filtered segment matches.
fn resolve(value: &Value, segments: &[Segment]) -> Option<()> {
    let mut current = value;
    for (i, segment) in segments.iter().enumerate() {
        match segment {
            Segment::Field(name) => {
                current = current.get(name)?;
            }
            Segment::Filtered { field, key, value } => {
                let target = current.get(field)?;
                let matched = filter_matches(target, key, value);
                if !matched {
                    return None;
                }
                if i + 1 < segments.len() {
                    return None;
                }
                return Some(());
            }
        }
    }
    Some(())
}

fn filter_matches(target: &Value, key: &str, value: &str) -> bool {
    match target {
        Value::Object(map) => map.get(key).and_then(Value::as_str) == Some(value),
        Value::Array(items) => items.iter().any(|item| {
            item.as_object()
                .and_then(|m| m.get(key))
                .and_then(Value::as_str)
                == Some(value)
        }),
        _ => false,
    }
}

/// Rebuild `value` with the nodes targeted by `segments` removed.
///
/// Only called with filter-free paths (filter paths are handled by whole-frame
/// removal in [`filter_frames`]). A final `Field` segment removes its key
/// entirely; intermediate segments descend into the object tree.
fn strip(value: Value, segments: &[Segment]) -> Value {
    match segments.split_first() {
        None => value,
        Some((Segment::Field(name), [])) => match value {
            Value::Object(mut map) => {
                map.remove(name);
                Value::Object(map)
            }
            other => other,
        },
        Some((Segment::Field(name), rest)) => match value {
            Value::Object(mut map) => {
                if let Some(inner) = map.get(name).cloned() {
                    map.insert(name.clone(), strip(inner, rest));
                }
                Value::Object(map)
            }
            Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .map(|item| match item {
                        Value::Object(mut item_map) => {
                            if let Some(inner) = item_map.get(name).cloned() {
                                item_map.insert(name.clone(), strip(inner, rest));
                            }
                            Value::Object(item_map)
                        }
                        other => other,
                    })
                    .collect(),
            ),
            other => other,
        },
        Some((Segment::Filtered { .. }, _)) => value,
    }
}

impl fmt::Display for JsonPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for segment in &self.segments {
            match segment {
                Segment::Field(name) => {
                    if !first {
                        write!(f, ".")?;
                    }
                    write!(f, "{name}")?;
                }
                Segment::Filtered { field, key, value } => {
                    if !first {
                        write!(f, ".")?;
                    }
                    write!(f, "{field}[{key}={value}]")?;
                }
            }
            first = false;
        }
        Ok(())
    }
}

impl FromStr for JsonPath {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err("empty json path".to_string());
        }
        let mut segments = Vec::new();
        for part in s.split('.') {
            if part.is_empty() {
                return Err(format!("empty segment in json path `{s}`"));
            }
            if let Some((field, filter)) = part.split_once('[') {
                let field = field.to_string();
                let filter = filter
                    .strip_suffix(']')
                    .ok_or_else(|| format!("unterminated filter in `{part}`"))?;
                let (key, value) = filter
                    .split_once('=')
                    .ok_or_else(|| format!("filter `{filter}` must be `key=value`"))?;
                segments.push(Segment::Filtered {
                    field,
                    key: key.to_string(),
                    value: value.to_string(),
                });
            } else {
                segments.push(Segment::Field(part.to_string()));
            }
        }
        Ok(Self { segments })
    }
}

impl JsonPath {
    /// Build a [`JsonPath`] from a string; panics on an invalid path (test helper).
    pub fn new(s: &str) -> Self {
        Self::from_str(s).expect("valid json path")
    }
}

/// A single mismatching position in an ordered-frame comparison.
#[derive(Debug)]
pub struct FrameDiff {
    /// The zero-based index of the first mismatching position.
    pub index: usize,
    /// The left frame's (normalised) value.
    pub left: Box<Value>,
    /// The right frame's (normalised) value.
    pub right: Box<Value>,
    /// Left and right directions at the mismatch.
    pub left_direction: Direction,
    pub right_direction: Direction,
}

impl fmt::Display for FrameDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "frame {} differs:\n  left  ({:?}) {}\n  right ({:?}) {}",
            self.index, self.left_direction, self.left, self.right_direction, self.right
        )
    }
}

impl std::error::Error for FrameDiff {}

/// Normalisation bookkeeping shared across a whole comparison so a repeated id,
/// uuid or timestamp maps to the same placeholder (Decision D13).
#[derive(Default)]
struct Norm {
    next: usize,
    map: HashMap<String, String>,
}

impl Norm {
    fn placeholder(&mut self, original: &str) -> String {
        if let Some(p) = self.map.get(original) {
            return p.clone();
        }
        let p = format!("${}", self.next);
        self.next += 1;
        self.map.insert(original.to_string(), p.clone());
        p
    }
}

/// Compare two ordered frame lists.
///
/// Frames are compared positionally after applying the ignore paths and
/// normalisation described at the top of this module. Returns `Err(FrameDiff)`
/// on the first mismatch.
pub fn diff_frames(a: &[Frame], b: &[Frame], ignore: &[JsonPath]) -> Result<(), FrameDiff> {
    let filtered_a = filter_frames(a, ignore);
    let filtered_b = filter_frames(b, ignore);

    // Normalise each side with its own counter. The two adapters run the same
    // script, so ids/uuids/timestamps appear in the same positions on both
    // sides; numbering each side independently by first appearance maps the
    // k-th distinct id of `a` to `$k` and the k-th of `b` to `$k`.
    let mut norm_a = Norm::default();
    let mut norm_b = Norm::default();
    let a: Vec<(Direction, Value)> = filtered_a
        .into_iter()
        .map(|f| (f.0, normalize(f.1, &mut norm_a)))
        .collect();
    let b: Vec<(Direction, Value)> = filtered_b
        .into_iter()
        .map(|f| (f.0, normalize(f.1, &mut norm_b)))
        .collect();

    for (i, (left, right)) in a.iter().zip(b.iter()).enumerate() {
        if left != right {
            return Err(FrameDiff {
                index: i,
                left: Box::new(left.1.clone()),
                right: Box::new(right.1.clone()),
                left_direction: left.0,
                right_direction: right.0,
            });
        }
    }

    if a.len() != b.len() {
        let shorter = a.len().min(b.len());
        return Err(FrameDiff {
            index: shorter,
            left: Box::new(
                a.get(shorter)
                    .map(|(_, v)| v.clone())
                    .unwrap_or(Value::Null),
            ),
            right: Box::new(
                b.get(shorter)
                    .map(|(_, v)| v.clone())
                    .unwrap_or(Value::Null),
            ),
            left_direction: a.get(shorter).map(|(d, _)| *d).unwrap_or(Direction::Send),
            right_direction: b.get(shorter).map(|(d, _)| *d).unwrap_or(Direction::Send),
        });
    }

    Ok(())
}

/// Apply the ignore paths to a frame list, dropping whole frames matched by a
/// filter path and removing nodes at plain-field paths. Returns `(direction,
/// json)` pairs.
fn filter_frames(frames: &[Frame], ignore: &[JsonPath]) -> Vec<(Direction, Value)> {
    let mut out = Vec::with_capacity(frames.len());
    for frame in frames {
        if ignore
            .iter()
            .any(|p| p.has_filter() && p.matches(&frame.json))
        {
            continue;
        }
        let mut json = frame.json.clone();
        for p in ignore {
            if !p.has_filter() {
                json = p.remove_nodes(json);
            }
        }
        out.push((frame.direction, json));
    }
    out
}

/// Replace ids/uuids/timestamps with first-appearance placeholders.
fn normalize(value: Value, norm: &mut Norm) -> Value {
    match value {
        Value::String(s) => {
            if is_uuid(&s) || is_timestamp(&s) {
                Value::String(norm.placeholder(&s))
            } else {
                Value::String(s)
            }
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, normalize(v, norm)))
                .collect(),
        ),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|v| normalize(v, norm)).collect())
        }
        other => other,
    }
}

/// True when `s` looks like a canonical 8-4-4-4-12 hex UUID.
fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    if b[8] != b'-' || b[13] != b'-' || b[18] != b'-' || b[23] != b'-' {
        return false;
    }
    b.iter()
        .enumerate()
        .all(|(i, &c)| matches!(i, 8 | 13 | 18 | 23) || c.is_ascii_hexdigit())
}

/// True when `s` looks like an ISO-8601 timestamp (`YYYY-MM-DD…`).
fn is_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 11
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(|c| c.is_ascii_digit())
        && b[5].is_ascii_digit()
        && b[6].is_ascii_digit()
        && b[8].is_ascii_digit()
        && b[9].is_ascii_digit()
}

// ---------------------------------------------------------------------------
// Process-transport test helpers (phase 3).
// ---------------------------------------------------------------------------

use std::path::{Path, PathBuf};
use std::time::Duration;

/// The fork root (parent of `rust/`).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("repo root resolves")
}

/// The built `fake-claude` binary (the gate builds it under `rust/target/debug`).
pub fn fake_claude_binary() -> PathBuf {
    repo_root().join("rust/target/debug/fake-claude")
}

/// Build and return the path to a crate example binary.
pub fn build_example(name: &str) -> PathBuf {
    let root = repo_root();
    let status = std::process::Command::new("cargo")
        .args([
            "build",
            "--manifest-path",
            "rust/Cargo.toml",
            "--example",
            name,
        ])
        .current_dir(&root)
        .status()
        .expect("spawn cargo build --example");
    assert!(
        status.success(),
        "cargo build --example {name} must succeed"
    );
    root.join("rust/target/debug/examples").join(name)
}

/// Read a JSON file into a `serde_json::Value`.
pub fn read_json(path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).expect("read json file");
    serde_json::from_str(&text).expect("parse json file")
}

/// Poll for `path` to exist and be non-empty, up to `timeout`.
pub fn wait_for_file(path: &Path, timeout: Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() > 0 {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("timed out waiting for {}", path.display()));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Replace the `--session-id=<uuid>` argument with `--session-id=$0` so a
/// captured argv can be compared against the normalised fixture (3.T1).
pub fn normalize_argv_session_id(argv: &mut [String]) {
    for arg in argv.iter_mut() {
        if arg.starts_with("--session-id=") {
            *arg = "--session-id=$0".to_string();
        }
    }
}

/// Poll up to `timeout` for the child pid to be gone (`kill(pid,0)` = `ESRCH`).
pub fn wait_pid_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if pid_is_gone(pid) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Whether `kill(pid, 0)` reports the process does not exist (ESRCH).
#[cfg(unix)]
pub fn pid_is_gone(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    matches!(
        kill(Pid::from_raw(pid as i32), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

/// Windows runtime is out of scope for phase-3 tests (D12): pid liveness checks
/// are unix-only.
#[cfg(not(unix))]
pub fn pid_is_gone(_pid: u32) -> bool {
    true
}
