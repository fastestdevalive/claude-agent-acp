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
//!    synthetic test notification missing on one side),
//! 2. removing nodes at plain-field ignore paths (e.g. in test fixtures),
//! 3. materialising ACP defaults the pinned crate omits on `tool_call` frames
//!    (absent `status` → `"pending"`, absent `content` → `[]`, absent `kind` →
//!    `"other"`) on both sides,
//! 4. normalising ids/uuids/timestamps to first-appearance placeholders.
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
        .map(|f| {
            (
                f.0,
                normalize(
                    normalize_request_permission_id(normalize_tool_call_defaults(f.1)),
                    &mut norm_a,
                ),
            )
        })
        .collect();
    let b: Vec<(Direction, Value)> = filtered_b
        .into_iter()
        .map(|f| {
            (
                f.0,
                normalize(
                    normalize_request_permission_id(normalize_tool_call_defaults(f.1)),
                    &mut norm_b,
                ),
            )
        })
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

/// Materialise ACP defaults the pinned crate omits, so an omitted default
/// equals an explicit default on BOTH sides of a differential (Decision D13).
///
/// The `agent-client-protocol` crate marks `ToolCall.status` and `ToolCall.content`
/// with `skip_serializing_if = is_default` / `Vec::is_empty`, so a `tool_call`
/// frame with `Pending` status and empty content serialises without them, while
/// the Node adapter always emits `"pending"` / `[]`. We re-insert those defaults
/// on any `params.update` whose `sessionUpdate == "tool_call"`. This is done for
/// `tool_call` frames ONLY — never for `tool_call_update` or any other kind,
/// where `status`/`content` must match exactly.
fn normalize_tool_call_defaults(value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            for (_, v) in map.iter_mut() {
                *v = normalize_tool_call_defaults(std::mem::take(v));
            }
            if map.get("sessionUpdate").and_then(Value::as_str) == Some("tool_call") {
                map.entry("status")
                    .or_insert_with(|| Value::String("pending".to_string()));
                map.entry("content")
                    .or_insert_with(|| Value::Array(Vec::new()));
                // Same crate limitation for `kind`: `ToolKind::Other` is the
                // default and is omitted from the wire, while the Node always
                // emits `"kind":"other"` (an MCP / Skill tool call).
                map.entry("kind")
                    .or_insert_with(|| Value::String("other".to_string()));
            }
            if map.get("type").and_then(Value::as_str) == Some("diff")
                && map.get("oldText") == Some(&Value::Null)
            {
                map.remove("oldText");
            }
            Value::Object(map)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(normalize_tool_call_defaults)
                .collect(),
        ),
        other => other,
    }
}

/// Normalise the JSON-RPC `id` of a `session/request_permission` request (and
/// its response) to a fixed token. The Node adapter sends these with a fixed
/// `id:0` while the pinned crate assigns a fresh uuid to every outgoing
/// request; the `id` is a volatile correlation id (like a uuid/timestamp), so
/// it is mapped to a fixed `$PERM_REQ` on both sides. The response is detected
/// by its `result.outcome` shape (the permission decision). A fixed token (not
/// a `Norm` placeholder) is used because the committed fixtures are already
/// pre-normalised by `capture.sh` (their other ids are `$0..$k` strings that do
/// not consume `Norm` slots), while the live Rust side produces real uuids that
/// do.
fn normalize_request_permission_id(value: Value) -> Value {
    let is_request =
        value.get("method").and_then(Value::as_str) == Some("session/request_permission");
    let is_response = value.get("result").and_then(|r| r.get("outcome")).is_some();
    if !is_request && !is_response {
        return value;
    }
    match value {
        Value::Object(mut map) => {
            if map.contains_key("id") {
                map.insert("id".into(), Value::String("$PERM_REQ".into()));
            }
            Value::Object(map)
        }
        other => other,
    }
}

/// Replace ids/uuids/timestamps with first-appearance placeholders.
fn normalize(value: Value, norm: &mut Norm) -> Value {
    match value {
        Value::String(s) => {
            if is_uuid(&s) || is_timestamp(&s) {
                Value::String(norm.placeholder(&s))
            } else {
                // Mirrors capture.sh `replaceUuids`: uuids EMBEDDED in a longer
                // string (`"<promptUuid>:error"`) get first-appearance
                // placeholders too.
                Value::String(replace_embedded_uuids(&s, norm))
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

/// Replace every uuid substring of `text` with its first-appearance placeholder.
fn replace_embedded_uuids(text: &str, norm: &mut Norm) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        match text.get(i..i + 36) {
            Some(window) if is_uuid(window) => {
                out.push_str(&norm.placeholder(window));
                i += 36;
            }
            _ => {
                let ch = text[i..].chars().next().expect("index is a char boundary");
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// inv_49_normalize_embedded_uuid helper: exposed so the unit test can drive it.
pub fn normalize_for_test(value: Value) -> Value {
    normalize(value, &mut Norm::default())
}

/// Normalise a captured `initialize` control_request exactly as
/// `capture.sh normalize_object` (plain mode) does, so a Rust-side request can
/// be compared against a committed `<name>.init.json`: timestamps and short
/// random ids (`^[a-z0-9]{8,20}$`) become first-appearance `$N`, embedded uuids
/// too, and the worktree / fake-claude paths become `$ROOT` / `$FAKE_CLAUDE`.
pub fn normalize_init(value: Value, root: &Path, fake: &Path) -> Value {
    fn short_id(s: &str) -> bool {
        (8..=20).contains(&s.len())
            && s.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    }
    fn walk(value: Value, norm: &mut Norm, root: &str, fake: &str) -> Value {
        match value {
            Value::String(s) => {
                if is_timestamp(&s) || short_id(&s) {
                    return Value::String(norm.placeholder(&s));
                }
                let s = replace_embedded_uuids(&s, norm);
                let s = if s.contains(fake) {
                    s.replace(fake, "$FAKE_CLAUDE")
                } else if s.contains(root) {
                    s.replace(root, "$ROOT")
                } else {
                    s
                };
                Value::String(s)
            }
            Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .map(|v| walk(v, norm, root, fake))
                    .collect(),
            ),
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(k, v)| (k, walk(v, norm, root, fake)))
                    .collect(),
            ),
            other => other,
        }
    }
    walk(
        value,
        &mut Norm::default(),
        &root.to_string_lossy(),
        &fake.to_string_lossy(),
    )
}

/// Normalise a fake-claude argv record (`{"argv":[...],"env":{...},
/// "node_options_present":bool}`) the same way `capture.sh normalize_object ...
/// argv` does: every string is walked with uuid -> `$0..` placeholders,
/// `$ROOT`/`$FAKE_CLAUDE` path rewriting and short-id placeholder normalisation.
///
/// fake-claude already writes `CLAUDE_CODE_ENTRYPOINT` as the literal
/// `"$ENTRYPOINT"`, so `env` needs no env-specific rewriting here; the
/// `--session-id=<uuid>` argv token is normalised to `--session-id=$0` by the
/// uuid walk, matching the committed `<name>.argv.json` fixtures.
pub fn normalize_argv(value: Value, root: &Path, fake: &Path) -> Value {
    normalize_init(value, root, fake)
}

/// Normalise the two sides' `agentInfo.version` so a composed Rust version can
/// be compared against the fixture's plain npm pin (Decision 3).
///
/// The Node FIXTURE carries the npm version verbatim (e.g. `0.70.0`); the Rust
/// side now composes `<CARGO_PKG_VERSION>+upstream.<pin>` at build time. To make
/// the differential honest (the `agentInfo` frame is NOT an ignore path):
///
/// 1. assert the fixture side's `result.agentInfo.version` == `pin`,
/// 2. rewrite that fixture value to `format!("{CARGO_PKG_VERSION}+upstream.{pin}")`,
/// 3. leave the Rust side's value UNCHANGED so it is compared exactly,
/// 4. assert exactly one `agentInfo` frame exists per side (cannot pass vacuously).
///
/// Returns `(rust, fixture)` ready for `diff_frames`.
pub fn normalize_agent_info_version(
    rust: Vec<Frame>,
    mut fixture: Vec<Frame>,
    pin: &str,
) -> (Vec<Frame>, Vec<Frame>) {
    fn agent_info_version(frame: &Frame) -> Option<&str> {
        frame
            .json
            .get("result")
            .and_then(|r| r.get("agentInfo"))
            .and_then(|a| a.get("version"))
            .and_then(Value::as_str)
    }

    let rust_count = rust
        .iter()
        .filter(|f| agent_info_version(f).is_some())
        .count();
    let fixture_count = fixture
        .iter()
        .filter(|f| agent_info_version(f).is_some())
        .count();
    assert_eq!(
        rust_count, 1,
        "expected exactly one agentInfo frame on the Rust side, found {rust_count}"
    );
    assert_eq!(
        fixture_count, 1,
        "expected exactly one agentInfo frame on the fixture side, found {fixture_count}"
    );

    let composed = format!("{}+upstream.{pin}", env!("CARGO_PKG_VERSION"));
    let mut rewrote = false;
    for frame in fixture.iter_mut() {
        if let Some(version) = agent_info_version(frame) {
            assert_eq!(
                version, pin,
                "fixture agentInfo.version is {version:?}, expected pin {pin:?}"
            );
            if let Some(a) = frame
                .json
                .get_mut("result")
                .and_then(|r| r.get_mut("agentInfo"))
            {
                a.as_object_mut()
                    .expect("agentInfo is an object")
                    .insert("version".into(), Value::String(composed.clone()));
            }
            rewrote = true;
        }
    }
    assert!(rewrote, "agentInfo frame existed but was not rewritten");
    (rust, fixture)
}

/// The pinned upstream npm version, read from `porting/UPSTREAM_VERSION`
/// (trimmed). Mirrors `build.rs`/`capture.sh`; used by tests that assert the
/// composed version string.
pub fn pinned_upstream_version() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let pinned = std::fs::read_to_string(root.join("porting/UPSTREAM_VERSION"))
        .expect("porting/UPSTREAM_VERSION exists");
    pinned.trim().to_string()
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

/// The built `acp-recorder` binary (the gate builds it under `rust/target/debug`).
pub fn acp_recorder_binary() -> PathBuf {
    repo_root().join("rust/target/debug/acp-recorder")
}

/// The built `claude-agent-acp-rs` binary under test.
pub fn rust_agent_binary() -> PathBuf {
    repo_root().join("rust/target/debug/claude-agent-acp-rs")
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

/// Parse a `{direction, frame}` JSONL fixture into [`Frame`]s (phase 8, 8.6).
pub fn parse_fixture_frames(path: &Path) -> Vec<Frame> {
    let text = std::fs::read_to_string(path).expect("read fixture frames");
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).expect("fixture frame line");
        let direction = match v["direction"].as_str() {
            Some("send") => Direction::Send,
            Some("recv") => Direction::Recv,
            other => panic!("bad direction: {other:?}"),
        };
        out.push(Frame {
            direction,
            json: v["frame"].clone(),
        });
    }
    out
}

/// Run the `acp-recorder` binary against `agent_cmd` with the given `script`,
/// a fake-`claude` transcript, and an output path. Returns the ordered frames.
///
/// The agent command is the Rust binary under test; `FAKE_CLAUDE_SCRIPT`
/// selects the transcript, and `CLAUDE_CODE_EXECUTABLE` points the agent at
/// the fake-`claude` binary. A missing binary fails loudly (8.6).
pub fn run_recorder(
    agent_cmd: &str,
    script_path: &Path,
    transcript_path: &Path,
    out_path: &Path,
) -> Vec<Frame> {
    run_recorder_inner(
        agent_cmd,
        script_path,
        transcript_path,
        out_path,
        None,
        None,
    )
}

/// As [`run_recorder`], also returning the NORMALISED first `initialize`
/// control_request fake-claude received (compare with `<name>.init.json`).
pub fn run_recorder_with_init(
    agent_cmd: &str,
    script_path: &Path,
    transcript_path: &Path,
    out_path: &Path,
) -> (Vec<Frame>, Value) {
    let init_out = out_path.with_extension("init.raw");
    let _ = std::fs::remove_file(&init_out);
    let frames = run_recorder_inner(
        agent_cmd,
        script_path,
        transcript_path,
        out_path,
        Some(&init_out),
        None,
    );
    let raw = read_json(&init_out);
    let _ = std::fs::remove_file(&init_out);
    // `normalize_init` mirrors capture.sh's `^[a-z0-9]{8,20}$` rule, which also
    // rewrites the 10-character subtype "initialize" to a placeholder on both
    // sides. Compare the real values here, before they disappear.
    assert_eq!(
        raw["type"], "control_request",
        "first frame is a control_request"
    );
    assert_eq!(
        raw["request"]["subtype"], "initialize",
        "the captured request is the initialize handshake"
    );
    (
        frames,
        normalize_init(raw, &repo_root(), &fake_claude_binary()),
    )
}

/// Run the `acp-recorder` against `agent_cmd` while also asking fake-claude to
/// record the argv (+ env allow-list) it was spawned with, then apply the argv
/// normalisation mirroring `capture.sh normalize_object ... argv` (uuids ->
/// `$0..`, paths -> `$ROOT`/`$FAKE_CLAUDE`, `$ENTRYPOINT` already present).
///
/// Returns the ordered frames and the NORMALISED argv record. A corpus script
/// that never spawns the CLI (its request fails before spawn) owns no argv
/// fixture; callers iterate only scripts with a committed `<name>.argv.json`.
pub fn run_recorder_with_argv(
    agent_cmd: &str,
    script_path: &Path,
    transcript_path: &Path,
    out_path: &Path,
) -> (Vec<Frame>, Value) {
    let argv_out = out_path.with_extension("argv.raw");
    let _ = std::fs::remove_file(&argv_out);
    let frames = run_recorder_inner(
        agent_cmd,
        script_path,
        transcript_path,
        out_path,
        None,
        Some(&argv_out),
    );
    let raw = read_json(&argv_out);
    let _ = std::fs::remove_file(&argv_out);
    let normalized = normalize_argv(raw, &repo_root(), &fake_claude_binary());
    (frames, normalized)
}

/// Per-run unique suffix for the hermetic HOME dirs.
static NEXT_HOME_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn run_recorder_inner(
    agent_cmd: &str,
    script_path: &Path,
    transcript_path: &Path,
    out_path: &Path,
    init_out: Option<&Path>,
    argv_out: Option<&Path>,
) -> Vec<Frame> {
    let recorder = acp_recorder_binary();
    let fake = fake_claude_binary();
    assert!(
        recorder.exists(),
        "acp-recorder binary missing: {}",
        recorder.display()
    );
    assert!(
        fake.exists(),
        "fake-claude binary missing: {}",
        fake.display()
    );
    assert!(
        transcript_path.exists(),
        "transcript missing: {}",
        transcript_path.display()
    );

    // Hermetic HOME / CLAUDE_CONFIG_DIR per run (mirrors capture.sh): the agent
    // and the CLI never see the host user's settings, skills or transcripts, and
    // `seed_files` land in a store nobody else shares.
    let home = std::env::temp_dir().join(format!(
        "acp-home-{}-{}",
        std::process::id(),
        NEXT_HOME_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".claude")).expect("create hermetic HOME");

    let mut command = std::process::Command::new(&recorder);
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", tmpdir);
    } else {
        command.env("TMPDIR", "/tmp");
    }
    if let Some(lang) = std::env::var_os("LANG") {
        command.env("LANG", lang);
    } else {
        command.env("LANG", "C");
    }
    for (k, v) in std::env::vars() {
        if k.starts_with("LC_") {
            command.env(k, v);
        }
    }
    #[cfg(windows)]
    for key in ["SystemRoot", "SYSTEMROOT", "COMSPEC", "PATHEXT"] {
        if let Some(val) = std::env::var_os(key) {
            command.env(key, val);
        }
    }
    command
        .arg(agent_cmd)
        .arg(script_path)
        .arg(out_path)
        .current_dir(repo_root())
        .env("HOME", &home)
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("FAKE_CLAUDE_SCRIPT", transcript_path)
        .env("CLAUDE_CODE_EXECUTABLE", &fake)
        // Shorten the force-cancel backstop so a wedged-cancel settle
        // (`cancel-mid-turn`, 11.T4) completes fast instead of waiting the 30 s
        // default. The grace only fires when a cancel leaves the active turn
        // un-settled past the deadline, which only that corpus exercises.
        .env("CLAUDE_ACP_FORCE_CANCEL_GRACE_MS", "1000");
    if let Some(init_out) = init_out {
        command.env("FAKE_CLAUDE_INIT_OUT", init_out);
    }
    if let Some(argv_out) = argv_out {
        command.env("FAKE_CLAUDE_ARGV_OUT", argv_out);
    }
    // Hermetic bypass behavior: if running as root (e.g. in containerized CI),
    // set IS_SANDBOX=1 so that argv fixtures (captured on non-root with
    // allow_bypass=true) evaluate allow_bypass=true identically.
    #[cfg(unix)]
    if nix::unistd::geteuid().is_root() {
        command.env("IS_SANDBOX", "1");
    }
    let status = command.status().expect("spawn acp-recorder");
    let _ = std::fs::remove_dir_all(&home);
    assert!(
        status.success(),
        "acp-recorder exited non-zero for {agent_cmd}"
    );
    parse_fixture_frames(out_path)
}

/// Normalise repo-root absolute paths inside captured frames to the `$ROOT`
/// placeholder, mirroring `capture.sh`'s `normalize_frames`. The fixture is
/// committed with `$ROOT` in place of the machine-specific worktree path.
pub fn normalize_root_paths(frames: Vec<Frame>, root: &Path) -> Vec<Frame> {
    let root_str = root.to_string_lossy().into_owned();
    frames
        .into_iter()
        .map(|frame| Frame {
            direction: frame.direction,
            json: replace_strings(frame.json, &root_str, "$ROOT"),
        })
        .collect()
}

/// Recursively replace exact occurrences of `from` in string nodes with `to`.
fn replace_strings(value: Value, from: &str, to: &str) -> Value {
    match value {
        Value::String(s) => Value::String(s.replace(from, to)),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, replace_strings(v, from, to)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| replace_strings(v, from, to))
                .collect(),
        ),
        other => other,
    }
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
