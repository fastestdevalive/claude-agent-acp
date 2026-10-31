//! The on-disk session store behind `session/list`, `session/delete` and
//! `session/fork` (rust-air-gaps G6, Decisions 14, 15).
//!
//! Claude Code keeps the transcript of a working directory in
//! `<config>/projects/<slug>/<sessionId>.jsonl`. Node reads and writes it
//! through the Agent SDK (`listSessions`, `deleteSession`, `forkSession` in
//! `sdk.mjs`); this module mirrors those three operations over plain files.
//! Every function takes the `projects` root explicitly (no env reads) so tests
//! run against a temp directory; [`projects_root`] derives it from
//! `CLAUDE_CONFIG_DIR` / `$HOME/.claude`.
//!
//! Not ported (PARITY): git-worktree sibling projects in a `cwd` listing, the
//! sidecar title store, the `isSidechain`-first-line archive checks of `pF`,
//! command-tag prompt filtering of `vi`, and `hu`'s 0600 fork-file mode (no
//! platform mode code outside `process.rs`; the file is created exclusively).

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};

use crate::session_index::js_text::{cut_utf16, is_js_ws, js_trim, utf16_len};
use crate::session_index::project_dirs::{self, Platform};

/// Sessions per `session/list` page (`SESSION_LIST_PAGE_SIZE`).
pub const SESSION_LIST_PAGE_SIZE: usize = 1000;
/// The SDK cuts an encoded project path at this length and appends a hash.
const MAX_SLUG_LENGTH: usize = 200;
/// Head / tail window the SDK reads of each transcript (`mn`).
const WINDOW: u64 = 65536;
/// Longest title (`MAX_TITLE_LENGTH`, `session-titles.js`).
const MAX_TITLE_LENGTH: usize = 256;
const TITLE_SCAN_LENGTH: usize = 4096;

/// Failure of a store operation.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A filesystem error.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// The session has no transcript in the searched project directories.
    #[error("{0}")]
    NotFound(String),
    /// The transcript has no messages to fork.
    #[error("Session {0} has no messages to fork")]
    NoMessages(String),
    /// A malformed argument (cursor, id).
    #[error("{0}")]
    Invalid(String),
}

/// `<CLAUDE_CONFIG_DIR>/projects`, else `$HOME/.claude/projects`.
pub fn projects_root() -> PathBuf {
    projects_root_with_env(&crate::env_snapshot::EnvSnapshot::from_process())
}

/// Derive the projects root from an explicit environment snapshot.
pub fn projects_root_with_env(env: &crate::env_snapshot::EnvSnapshot) -> PathBuf {
    // upstream `projectsRoot`: path.join(claudeConfigDir(), "projects").normalize("NFC")
    let root = config_dir_with_env(env).join("projects");
    match root.to_str() {
        Some(text) => PathBuf::from(crate::session_index::nfc::nfc(text)),
        None => root,
    }
}

/// The Claude configuration directory: `CLAUDE_CONFIG_DIR`, else `$HOME/.claude`
/// (`claudeConfigDir`, `paths.ts`).
pub fn config_dir_with_env(env: &crate::env_snapshot::EnvSnapshot) -> PathBuf {
    env.get_path("CLAUDE_CONFIG_DIR")
        .or_else(|| env.get_path("HOME").map(|h| h.join(".claude")))
        .unwrap_or_default()
}

/// Whether `value` names a transcript (a UUID).
pub fn is_session_id(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    let lens = [8usize, 4, 4, 4, 12];
    parts.len() == 5
        && parts
            .iter()
            .zip(lens)
            .all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The SDK's string hash over UTF-16 code units, base 36 of its absolute value.
#[cfg(test)]
fn path_hash(path: &str) -> String {
    crate::session_index::project_dirs::hash_base36(path)
}

/// The project directory name of `path` (`_u`): the sanitized path, cut at 200
/// characters plus `-<hash>` when longer (the host's path rules).
pub fn project_slug(path: &str) -> String {
    project_dirs::encode_project_path(path, Platform::host())
}

/// The existing project directories for `dir`: the exact slug (found through the
/// file system, so in any case on macOS), plus (for a long path, whose hash the
/// CLI computes differently) the directories sharing its 200-character prefix.
fn project_dirs_for(root: &Path, dir: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let exact = root.join(project_slug(dir));
    if exact.is_dir() {
        out.push(exact.clone());
    }
    let slug = project_dirs::sanitize(dir);
    if slug.len() > MAX_SLUG_LENGTH {
        if let Ok(read) = std::fs::read_dir(root) {
            let mut names: Vec<_> = read.flatten().collect();
            names.sort_by_key(std::fs::DirEntry::file_name);
            for entry in names {
                let path = entry.path();
                if path.is_dir()
                    && project_dirs::project_dir_matches(
                        &entry.file_name().to_string_lossy(),
                        dir,
                        Platform::host(),
                    )
                    && path != exact
                    && !out.contains(&path)
                {
                    out.push(path);
                }
            }
        }
    }
    out
}

/// Every project directory under `root` (a listing without a `cwd`).
fn all_project_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// The (non-empty) transcripts of `session_id` under `root`, any project.
pub fn find_transcripts(root: &Path, session_id: &str) -> Vec<PathBuf> {
    if !is_session_id(session_id) {
        return Vec::new();
    }
    all_project_dirs(root)
        .into_iter()
        .map(|d| d.join(format!("{session_id}.jsonl")))
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() > 0))
        .collect()
}

/// The transcript of `session_id` in `dir`'s project directory, without
/// scanning every project (`getSessionInfo(id, {dir})` reads only the cwd's
/// project: `sdk.mjs` `As(…, dir, …)`). The directory is canonicalized first,
/// like `list`.
pub fn find_transcript_for_dir(root: &Path, dir: &str, session_id: &str) -> Option<PathBuf> {
    if !is_session_id(session_id) {
        return None;
    }
    let real = std::fs::canonicalize(dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| dir.to_string());
    project_dirs_for(root, &real)
        .into_iter()
        .map(|d| d.join(format!("{session_id}.jsonl")))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() > 0))
}

// ---- JSON-ish text scanning (the SDK's `Kt` / `ll` / `am` / `cm`) -----------

fn unescape(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_string();
    }
    serde_json::from_str::<String>(&format!("\"{raw}\"")).unwrap_or_else(|_| raw.to_string())
}

/// The end of a JSON string body starting at `from` (index of the closing quote).
fn string_end(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// `Kt` — the value of the LAST `"key":"…"` occurrence in `text`.
fn last_string_field(text: &str, key: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for pattern in [format!("\"{key}\":\""), format!("\"{key}\": \"")] {
        let mut from = 0usize;
        while let Some(rel) = text.get(from..).and_then(|t| t.find(&pattern)) {
            let start = from + rel;
            let value_start = start + pattern.len();
            match string_end(text, value_start) {
                Some(end) => {
                    if best.as_ref().is_none_or(|(at, _)| start > *at) {
                        best = Some((start, unescape(&text[value_start..end])));
                    }
                    from = end + 1;
                }
                None => break,
            }
        }
    }
    best.map(|(_, v)| v)
}

/// `ll` — the value of the FIRST `"key":"…"` occurrence in `text`.
fn first_string_field(text: &str, key: &str) -> Option<String> {
    for pattern in [format!("\"{key}\":\""), format!("\"{key}\": \"")] {
        let Some(start) = text.find(&pattern) else {
            continue;
        };
        let value_start = start + pattern.len();
        if let Some(end) = string_end(text, value_start) {
            return Some(unescape(&text[value_start..end]));
        }
    }
    None
}

/// `am(tail, "relocated", "relocatedCwd")` — `field` of the last JSON line of
/// type `line_type`.
fn last_typed_line_field(text: &str, line_type: &str, field: &str) -> Option<String> {
    let type_marker = format!("\"type\":\"{line_type}\"");
    let field_marker = format!("\"{field}\":");
    for line in text.lines().rev() {
        if !line.contains(&field_marker) || !line.contains(&type_marker) {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            if value.get("type").and_then(Value::as_str) == Some(line_type) {
                if let Some(s) = value.get(field).and_then(Value::as_str) {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

/// `cm` — the first real user prompt in `head`, one line, at most 200 chars.
fn first_prompt(head: &str) -> Option<String> {
    Some(crate::session_index::first_prompt::first_prompt(head)).filter(|s| !s.is_empty())
}

/// `replace(/\s+/g, " ")` with JavaScript's `\s`.
fn collapse_js_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for c in text.chars() {
        if is_js_ws(c) {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            in_run = false;
            out.push(c);
        }
    }
    out
}

/// `sanitizeTitle` (`session-titles.ts`): collapse whitespace, cap at 256 UTF-16
/// units with an ellipsis. A text longer than 4096 units is judged by its first
/// 4096 only: collapsing the white space of a prefix gives a prefix of the
/// collapsed text, and two more units than a title hold at least one that is not
/// a space, so the full text is longer than a title and gets the same cut.
pub fn sanitize_title(text: &str) -> String {
    if utf16_len(text) > TITLE_SCAN_LENGTH {
        let head = collapse_js_whitespace(cut_utf16(text, TITLE_SCAN_LENGTH));
        let head = head.trim_start_matches(is_js_ws);
        if utf16_len(head) >= MAX_TITLE_LENGTH + 2 {
            return format!("{}…", cut_utf16(head, MAX_TITLE_LENGTH - 1));
        }
    }
    let collapsed = collapse_js_whitespace(text);
    let sanitized = js_trim(&collapsed);
    if utf16_len(sanitized) <= MAX_TITLE_LENGTH {
        return sanitized.to_string();
    }
    format!("{}…", cut_utf16(sanitized, MAX_TITLE_LENGTH - 1))
}

/// `new Date(ms).toISOString()`.
pub fn iso8601(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let millis = ms % 1000;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!(
        "{year:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---- listing -----------------------------------------------------------------

/// One listed session (the adapter's `{sessionId,cwd,title,updatedAt}` row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub session_id: String,
    pub cwd: String,
    pub title: String,
    pub last_modified_ms: u64,
    /// `SDKSessionInfo.customTitle`: kept for `hideArchived`, not on the wire.
    pub custom_title: Option<String>,
}

impl SessionRow {
    /// The `session/list` row.
    pub fn to_json(&self) -> Value {
        json!({
            "sessionId": self.session_id,
            "cwd": self.cwd,
            "title": self.title,
            "updatedAt": iso8601(self.last_modified_ms),
        })
    }
}

struct Candidate {
    session_id: String,
    path: PathBuf,
    mtime_ms: u64,
    project_path: Option<String>,
}

fn read_window(path: &Path) -> Option<(String, String, u64)> {
    let mut file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut head = Vec::new();
    (&mut file).take(WINDOW).read_to_end(&mut head).ok()?;
    if head.is_empty() {
        return None;
    }
    let tail = if size > WINDOW {
        let mut buf = Vec::new();
        file.seek(SeekFrom::Start(size - WINDOW)).ok()?;
        (&mut file).take(WINDOW).read_to_end(&mut buf).ok()?;
        String::from_utf8_lossy(&buf).into_owned()
    } else {
        String::from_utf8_lossy(&head).into_owned()
    };
    Some((String::from_utf8_lossy(&head).into_owned(), tail, mtime))
}

/// `El` + the adapter filter: the list row of one transcript, or `None`.
fn row_of(candidate: &Candidate) -> Option<SessionRow> {
    let (head, tail, mtime) = read_window(&candidate.path)?;
    let first_line = head.lines().next().unwrap_or("");
    if first_line.contains("\"isSidechain\":true") || first_line.contains("\"isSidechain\": true") {
        return None;
    }
    let custom = last_string_field(&tail, "customTitle")
        .filter(|s| !s.is_empty())
        .or_else(|| last_string_field(&head, "customTitle").filter(|s| !s.is_empty()))
        .or_else(|| last_string_field(&tail, "aiTitle").filter(|s| !s.is_empty()))
        .or_else(|| last_string_field(&head, "aiTitle").filter(|s| !s.is_empty()));
    let custom_title = custom.clone();
    let summary = custom
        .or_else(|| last_string_field(&tail, "lastPrompt").filter(|s| !s.is_empty()))
        .or_else(|| last_string_field(&tail, "summary").filter(|s| !s.is_empty()))
        .or_else(|| first_prompt(&head).filter(|s| !s.is_empty()))
        .or_else(|| {
            Some(crate::session_index::first_prompt::media_prompt(&head).to_string())
                .filter(|s| !s.is_empty())
        })?;
    let cwd = last_typed_line_field(&tail, "relocated", "relocatedCwd")
        .filter(|s| !s.is_empty())
        .or_else(|| first_string_field(&head, "cwd").filter(|s| !s.is_empty()))
        .or_else(|| candidate.project_path.clone())?;
    let last_modified_ms = if candidate.mtime_ms > 0 {
        candidate.mtime_ms
    } else {
        mtime
    };
    Some(SessionRow {
        session_id: candidate.session_id.clone(),
        cwd,
        title: sanitize_title(&summary),
        last_modified_ms,
        custom_title,
    })
}

/// Summary and custom title read from stored transcript files.
#[derive(Debug, Clone)]
pub struct StoredSessionInfo {
    pub custom_title: Option<String>,
    pub summary: Option<String>,
    pub mtime_ms: u64,
}

/// Read the stored session info (custom title / summary) for a session ID,
/// searching every project directory.
pub fn read_stored_session_info(root: &Path, session_id: &str) -> Option<StoredSessionInfo> {
    find_transcripts(root, session_id)
        .into_iter()
        .find_map(|path| read_stored_session_info_at(&path))
}

/// Read the stored session info from one already-resolved transcript path.
pub fn read_stored_session_info_at(path: &Path) -> Option<StoredSessionInfo> {
    let (head, tail, mtime) = read_window(path)?;
    let custom_title = last_string_field(&tail, "customTitle")
        .filter(|s| !s.is_empty())
        .or_else(|| last_string_field(&head, "customTitle").filter(|s| !s.is_empty()))
        .or_else(|| last_string_field(&tail, "aiTitle").filter(|s| !s.is_empty()))
        .or_else(|| last_string_field(&head, "aiTitle").filter(|s| !s.is_empty()));
    let summary = custom_title
        .clone()
        .or_else(|| last_string_field(&tail, "lastPrompt").filter(|s| !s.is_empty()))
        .or_else(|| last_string_field(&tail, "summary").filter(|s| !s.is_empty()))
        .or_else(|| first_prompt(&head).filter(|s| !s.is_empty()));
    Some(StoredSessionInfo {
        custom_title,
        summary,
        mtime_ms: mtime,
    })
}

/// A page of [`SessionRow`]s plus the next cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListPage {
    pub sessions: Vec<SessionRow>,
    pub next_cursor: Option<String>,
}

/// Parse a `session/list` cursor (`offset:N`).
pub fn parse_cursor(cursor: Option<&str>) -> Result<usize, StoreError> {
    let Some(cursor) = cursor else { return Ok(0) };
    cursor
        .strip_prefix("offset:")
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| StoreError::Invalid(format!("Unknown session/list cursor: {cursor}")))
}

/// `listSessions` as the adapter calls it: sessions newest first (ties by id,
/// descending), `offset` valid sessions skipped, one page of `page_size` and a
/// `nextCursor` when more exist. `dir` filters by the REAL path's project slug.
pub fn list(root: &Path, dir: Option<&str>, offset: usize, page_size: usize) -> ListPage {
    let mut candidates = Vec::new();
    let dirs: Vec<(PathBuf, Option<String>)> = match dir {
        Some(dir) => {
            let real = std::fs::canonicalize(dir)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| dir.to_string());
            project_dirs_for(root, &real)
                .into_iter()
                .map(|d| (d, Some(real.clone())))
                .collect()
        }
        None => all_project_dirs(root)
            .into_iter()
            .map(|d| (d, None))
            .collect(),
    };
    for (project_dir, project_path) in dirs {
        let Ok(read) = std::fs::read_dir(&project_dir) else {
            continue;
        };
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".jsonl") else {
                continue;
            };
            if !is_session_id(stem) {
                continue;
            }
            let mtime_ms = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            candidates.push(Candidate {
                session_id: stem.to_string(),
                path: entry.path(),
                mtime_ms,
                project_path: project_path.clone(),
            });
        }
    }
    candidates.sort_by(|a, b| {
        b.mtime_ms
            .cmp(&a.mtime_ms)
            .then_with(|| b.session_id.cmp(&a.session_id))
    });
    let want = page_size + 1;
    let mut seen = HashSet::new();
    let mut skipped = 0usize;
    let mut rows = Vec::new();
    for candidate in &candidates {
        if rows.len() >= want {
            break;
        }
        let Some(row) = row_of(candidate) else {
            continue;
        };
        if !seen.insert(row.session_id.clone()) {
            continue;
        }
        if skipped < offset {
            skipped += 1;
            continue;
        }
        rows.push(row);
    }
    let has_more = rows.len() > page_size;
    rows.truncate(page_size);
    ListPage {
        sessions: rows,
        next_cursor: has_more.then(|| format!("offset:{}", offset + page_size)),
    }
}

// ---- delete ------------------------------------------------------------------

/// `session/delete` for a non-AIR client (`deleteSessionIfStored`): remove the
/// transcript and its session directory; an unknown session or an invalid id
/// succeeds.
pub fn delete(root: &Path, session_id: &str) -> Result<(), StoreError> {
    // The SDK deletes the first project directory holding a non-empty transcript.
    if let Some(transcript) = find_transcripts(root, session_id).into_iter().next() {
        std::fs::remove_file(&transcript)?;
        if let Some(parent) = transcript.parent() {
            let sidecar = parent.join(session_id);
            if sidecar.is_dir() {
                std::fs::remove_dir_all(&sidecar)?;
            }
        }
    }
    Ok(())
}

// ---- fork --------------------------------------------------------------------

const MESSAGE_TYPES: [&str; 5] = ["user", "assistant", "attachment", "system", "progress"];
/// Keys `forkSession` sets to `undefined` (dropped from the JSON).
const FORK_DROPPED_KEYS: [&str; 7] = [
    "teamName",
    "agentName",
    "sessionKind",
    "slug",
    "sourceToolAssistantUUID",
    "summarizedTurnPromptUuids",
    "droppedPromptUuids",
];

/// One entry of the SDK `ed` parent-resolution map (`sdk.mjs` `ed`): the entry
/// plus the parent it resolves to after the compaction rewrites.
struct ChainNode {
    value: Value,
    parent: Option<String>,
}

/// The result of `ed`: the parent map, the active chain (root first) and the
/// leaf candidate that reached it (`found.terminal`), used by `sve`.
struct ResolvedChain {
    chain: Vec<String>,
    terminal: Option<String>,
}

/// JavaScript truthiness of an optional JSON value (`!y.isSidechain`, etc.).
fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

fn uuid_of(value: &Value) -> &str {
    value.get("uuid").and_then(Value::as_str).unwrap_or("")
}

/// The `ed` candidate predicate: an entry that can end a fork chain.
fn is_forkable(node: &ChainNode) -> bool {
    let value = &node.value;
    !js_truthy(value.get("isSidechain"))
        && !js_truthy(value.get("teamName"))
        && value.get("type").and_then(Value::as_str) != Some("progress")
        && !(value.get("type").and_then(Value::as_str) == Some("attachment")
            && value
                .get("attachment")
                .and_then(|attachment| attachment.get("type"))
                .and_then(Value::as_str)
                == Some("fork_briefing"))
}

/// `ed` (`sdk.mjs`): resolve the parent map through the compaction boundaries,
/// pick the leaf of the branch that ends at the newest user/assistant message
/// and return that branch's chain, root first. `found.terminal` is the leaf
/// candidate that reached the chain root (the boundary `sve` may drop).
///
/// The SDK's `wwe` regrouping (siblings that share an assistant message id,
/// plus their queued attachments) is not applied here: it only widens the
/// chain's liveness set for those grouped entries, which this cut never needs.
fn resolve_chain(entries: &[Value]) -> ResolvedChain {
    let mut order: Vec<String> = Vec::new();
    let mut nodes: HashMap<String, ChainNode> = HashMap::new();
    for entry in entries {
        let Some(uuid) = entry.get("uuid").and_then(Value::as_str) else {
            continue;
        };
        if !nodes.contains_key(uuid) {
            order.push(uuid.to_string());
        }
        nodes.insert(
            uuid.to_string(),
            ChainNode {
                value: entry.clone(),
                parent: entry
                    .get("parentUuid")
                    .and_then(Value::as_str)
                    .map(String::from),
            },
        );
    }
    // `ed` step 2: a compact boundary re-parents the messages it preserved.
    for uuid in order.clone() {
        let Some(node) = nodes.get(&uuid) else {
            continue;
        };
        if node.value.get("type").and_then(Value::as_str) != Some("system")
            || node.value.get("subtype").and_then(Value::as_str) != Some("compact_boundary")
        {
            continue;
        }
        let compact = node.value.get("compactMetadata");
        if let Some(preserved) = compact
            .and_then(|metadata| metadata.get("preservedMessages"))
            .filter(|value| js_truthy(Some(value)))
        {
            let uuids: Vec<String> = preserved
                .get("uuids")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|uuid| uuid.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if uuids.is_empty() || uuids.iter().any(|uuid| !nodes.contains_key(uuid)) {
                continue;
            }
            let anchor = preserved
                .get("anchorUuid")
                .and_then(Value::as_str)
                .map(String::from);
            let mut cursor = anchor.clone();
            for uuid in &uuids {
                if let Some(node) = nodes.get_mut(uuid) {
                    node.parent = cursor.clone();
                }
                cursor = Some(uuid.clone());
            }
            let first = uuids[0].clone();
            let last = uuids[uuids.len() - 1].clone();
            for key in order.clone() {
                let Some(node) = nodes.get_mut(&key) else {
                    continue;
                };
                if node.parent == anchor && key != first {
                    node.parent = Some(last.clone());
                }
            }
        } else if let Some(segment) = compact
            .and_then(|metadata| metadata.get("preservedSegment"))
            .filter(|value| js_truthy(Some(value)))
        {
            let head = segment
                .get("headUuid")
                .and_then(Value::as_str)
                .map(String::from);
            let anchor = segment
                .get("anchorUuid")
                .and_then(Value::as_str)
                .map(String::from);
            let tail = segment
                .get("tailUuid")
                .and_then(Value::as_str)
                .map(String::from);
            if let Some(head) = &head {
                if let Some(node) = nodes.get_mut(head) {
                    node.parent = anchor.clone();
                }
            }
            for key in order.clone() {
                let Some(node) = nodes.get_mut(&key) else {
                    continue;
                };
                if node.parent == anchor && head.as_ref() != Some(&key) {
                    node.parent = tail.clone();
                }
            }
        }
    }
    // `ed` steps 3-8: the leaves, the forkable tips and the newest one.
    let mut file_index: HashMap<String, usize> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if let Some(uuid) = entry.get("uuid").and_then(Value::as_str) {
            file_index.insert(uuid.to_string(), index);
        }
    }
    let mut referenced: HashSet<String> = HashSet::new();
    for node in nodes.values() {
        if let Some(parent) = &node.parent {
            referenced.insert(parent.clone());
        }
    }
    let leaves: Vec<String> = order
        .iter()
        .filter(|uuid| !referenced.contains(*uuid))
        .cloned()
        .collect();
    let mut parent_of_forkable: HashSet<String> = HashSet::new();
    for node in nodes.values() {
        if let Some(parent) = &node.parent {
            if is_forkable(node) {
                parent_of_forkable.insert(parent.clone());
            }
        }
    }
    let index_of = |uuid: &str| file_index.get(uuid).map_or(-1i64, |index| *index as i64);
    let mut candidates: Vec<String> = order
        .iter()
        .filter(|uuid| {
            nodes.get(*uuid).is_some_and(is_forkable) && !parent_of_forkable.contains(*uuid)
        })
        .cloned()
        .collect();
    candidates.sort_by_key(|uuid| std::cmp::Reverse(index_of(uuid)));
    let mut dead: HashSet<String> = HashSet::new();
    let mut root: Option<String> = None;
    let mut terminal: Option<String> = None;
    for candidate in candidates {
        let mut trail: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut cursor = Some(candidate.clone());
        while let Some(uuid) = cursor {
            if dead.contains(&uuid) || seen.contains(&uuid) {
                break;
            }
            let Some(node) = nodes.get(&uuid) else {
                break;
            };
            let kind = node.value.get("type").and_then(Value::as_str).unwrap_or("");
            if kind == "user" || kind == "assistant" {
                root = Some(uuid);
                terminal = Some(candidate.clone());
                break;
            }
            seen.insert(uuid.clone());
            trail.push(uuid.clone());
            cursor = node.parent.clone();
        }
        if root.is_some() {
            break;
        }
        for uuid in trail {
            dead.insert(uuid);
        }
    }
    // `ed` step 9: no candidate reached a message; walk every leaf instead.
    if root.is_none() {
        let mut reached: Vec<String> = Vec::new();
        for leaf in &leaves {
            let mut seen: HashSet<String> = HashSet::new();
            let mut cursor = Some(leaf.clone());
            while let Some(uuid) = cursor {
                if !seen.insert(uuid.clone()) {
                    break;
                }
                let Some(node) = nodes.get(&uuid) else {
                    break;
                };
                let kind = node.value.get("type").and_then(Value::as_str).unwrap_or("");
                if kind == "user" || kind == "assistant" {
                    reached.push(uuid);
                    break;
                }
                cursor = node.parent.clone();
            }
        }
        if reached.is_empty() {
            return ResolvedChain {
                chain: Vec::new(),
                terminal: None,
            };
        }
        let visible: Vec<String> = reached
            .iter()
            .filter(|uuid| {
                let value = &nodes[*uuid].value;
                !js_truthy(value.get("isSidechain"))
                    && !js_truthy(value.get("teamName"))
                    && !js_truthy(value.get("isMeta"))
            })
            .cloned()
            .collect();
        let pool = if visible.is_empty() { reached } else { visible };
        let mut best = pool[0].clone();
        let mut best_index = index_of(&best);
        for uuid in pool.iter().skip(1) {
            let index = index_of(uuid);
            if index > best_index {
                best = uuid.clone();
                best_index = index;
            }
        }
        root = Some(best);
    }
    // `ed` step 11: the chain from the root down to the chosen message.
    let mut chain: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut cursor = root;
    while let Some(uuid) = cursor {
        if !seen.insert(uuid.clone()) {
            break;
        }
        let Some(node) = nodes.get(&uuid) else {
            break;
        };
        chain.push(uuid);
        cursor = node.parent.clone();
    }
    chain.reverse();
    ResolvedChain { chain, terminal }
}

/// `ove`: the uuids a compact boundary says it preserved.
fn compact_metadata_refs(entry: &Value) -> Vec<String> {
    let Some(metadata) = entry
        .get("compactMetadata")
        .filter(|value| value.is_object())
    else {
        return Vec::new();
    };
    let mut refs = Vec::new();
    if let Some(uuids) = metadata
        .get("preservedMessages")
        .filter(|value| value.is_object())
        .and_then(|preserved| preserved.get("uuids"))
        .and_then(Value::as_array)
    {
        refs.extend(
            uuids
                .iter()
                .filter_map(|uuid| uuid.as_str().map(String::from)),
        );
    }
    if let Some(segment) = metadata
        .get("preservedSegment")
        .filter(|value| value.is_object())
    {
        for key in ["headUuid", "tailUuid"] {
            if let Some(uuid) = segment.get(key).and_then(Value::as_str) {
                refs.push(uuid.to_string());
            }
        }
    }
    refs
}

/// `fR`: a compact boundary that is not live and sits at/after the chain start.
fn is_stale_boundary(
    uuid: &str,
    live: &HashSet<String>,
    first_index: &HashMap<String, usize>,
    start: usize,
) -> bool {
    !live.contains(uuid) && first_index.get(uuid).copied().unwrap_or(start) >= start
}

/// `wF`: mark a stale boundary and the branch hanging off it as left out —
/// its ancestors up to the live chain, then their descendants.
fn mark_stale_branch(
    uuid: &str,
    entries: &[Value],
    by_uuid: &HashMap<String, usize>,
    first_index: &HashMap<String, usize>,
    live: &HashSet<String>,
    start: usize,
    left_out: &mut HashSet<String>,
) {
    let mut cursor = by_uuid.get(uuid).copied();
    while let Some(index) = cursor {
        let current = uuid_of(&entries[index]).to_string();
        if !is_stale_boundary(&current, live, first_index, start) || left_out.contains(&current) {
            break;
        }
        left_out.insert(current);
        cursor = entries[index]
            .get("parentUuid")
            .and_then(Value::as_str)
            .and_then(|parent| by_uuid.get(parent))
            .copied();
    }
    for entry in entries {
        let Some(current) = entry.get("uuid").and_then(Value::as_str) else {
            continue;
        };
        let Some(parent) = entry.get("parentUuid").and_then(Value::as_str) else {
            continue;
        };
        if left_out.contains(parent) {
            left_out.insert(current.to_string());
        }
    }
}

/// `sve` (`sdk.mjs`): from a fork slice, drop the compact boundaries that are
/// not on its live chain, with the stale branches they anchor. Re-resolves the
/// chain until it stops changing, so a boundary that only becomes stale after
/// another was dropped is dropped too.
fn live_chain_slice(entries: &[Value], fresh_uuid: &mut dyn FnMut() -> String) -> Vec<Value> {
    let Some(last) = entries.last() else {
        return entries.to_vec();
    };
    // `sve` first applies `PF` to every entry: sidechain/team fields reset.
    let mut prepared: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let mut copy = entry.clone();
            if let Some(map) = copy.as_object_mut() {
                map.insert("isSidechain".into(), json!(false));
                for key in FORK_DROPPED_KEYS {
                    map.remove(key);
                }
            }
            copy
        })
        .collect();
    prepared.push(json!({
        "type": "system",
        "uuid": fresh_uuid(),
        "parentUuid": last.get("uuid").cloned().unwrap_or(Value::Null),
        "sessionId": last.get("sessionId").cloned().unwrap_or(Value::Null),
        "timestamp": last.get("timestamp").cloned().unwrap_or(Value::Null),
    }));
    let resolved = resolve_chain(&prepared);
    let Some(start_uuid) = resolved.chain.first().cloned() else {
        return entries.to_vec();
    };
    let mut first_index: HashMap<String, usize> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if let Some(uuid) = entry.get("uuid").and_then(Value::as_str) {
            first_index.entry(uuid.to_string()).or_insert(index);
        }
    }
    let Some(start) = first_index.get(&start_uuid).copied() else {
        return entries.to_vec();
    };
    let chain: HashSet<String> = resolved.chain.iter().cloned().collect();
    let mut by_uuid: HashMap<String, usize> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if let Some(uuid) = entry.get("uuid").and_then(Value::as_str) {
            by_uuid.insert(uuid.to_string(), index);
        }
    }
    // `l`: the slice's last entry plus every chain entry, and their ancestors.
    let mut live: HashSet<String> = HashSet::new();
    let mut seeds: Vec<String> = Vec::new();
    if let Some(uuid) = last.get("uuid").and_then(Value::as_str) {
        seeds.push(uuid.to_string());
    }
    seeds.extend(resolved.chain.iter().cloned());
    for seed in seeds {
        let mut cursor = by_uuid.get(&seed).copied();
        while let Some(index) = cursor {
            let uuid = uuid_of(&entries[index]).to_string();
            if live.contains(&uuid) {
                break;
            }
            live.insert(uuid);
            cursor = entries[index]
                .get("parentUuid")
                .and_then(Value::as_str)
                .and_then(|parent| by_uuid.get(parent))
                .copied();
        }
    }
    let mut left_out: HashSet<String> = HashSet::new();
    for entry in entries {
        if entry.get("type").and_then(Value::as_str) != Some("system")
            || entry.get("subtype").and_then(Value::as_str) != Some("compact_boundary")
        {
            continue;
        }
        let uuid = uuid_of(entry);
        if !is_stale_boundary(uuid, &live, &first_index, start)
            || compact_metadata_refs(entry)
                .iter()
                .any(|uuid| live.contains(uuid))
        {
            continue;
        }
        mark_stale_branch(
            uuid,
            entries,
            &by_uuid,
            &first_index,
            &live,
            start,
            &mut left_out,
        );
    }
    // The SDK re-runs `ed` without the left-out entries and drops the boundary
    // that became stale, until the chain stops changing.
    loop {
        let remaining: Vec<Value> = prepared
            .iter()
            .filter(|entry| !left_out.contains(uuid_of(entry)))
            .cloned()
            .collect();
        let found = resolve_chain(&remaining);
        if found.chain.len() == chain.len() && found.chain.iter().all(|uuid| chain.contains(uuid)) {
            break;
        }
        let Some(terminal) = found.terminal else {
            break;
        };
        // The SDK would loop here if `wF` marked nothing; stop instead.
        if left_out.contains(&terminal) || !is_stale_boundary(&terminal, &live, &first_index, start)
        {
            break;
        }
        mark_stale_branch(
            &terminal,
            entries,
            &by_uuid,
            &first_index,
            &live,
            start,
            &mut left_out,
        );
    }
    if left_out.is_empty() {
        return entries.to_vec();
    }
    entries
        .iter()
        .filter(|entry| !left_out.contains(uuid_of(entry)))
        .cloned()
        .collect()
}

/// `c0` — the non-empty string field `field` of a `queued_command` attachment.
fn queued_command_field<'a>(entry: &'a Value, field: &str) -> Option<&'a str> {
    if entry.get("type").and_then(Value::as_str) != Some("attachment") {
        return None;
    }
    let attachment = entry.get("attachment")?;
    if attachment.get("type").and_then(Value::as_str) != Some("queued_command") {
        return None;
    }
    attachment
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// `qA` — the file index of `target` by uuid, else of the first
/// `queued_command` attachment whose `source_uuid` is `target`.
fn fork_cut_index(entries: &[Value], target: &str) -> Option<usize> {
    entries
        .iter()
        .position(|entry| entry.get("uuid").and_then(Value::as_str) == Some(target))
        .or_else(|| {
            entries
                .iter()
                .position(|entry| queued_command_field(entry, "source_uuid") == Some(target))
        })
}

/// Copy the latest transcript of `source_id` (found under `dir`'s project)
/// into a new session, as SDK `forkSession` (`sdk.mjs` `IF`/`eve`). With
/// `up_to` the copy stops at that transcript uuid; without it every message is
/// copied. Each entry gets a fresh uuid (`progress` entries dropped, ancestors
/// remapped past them), `sessionId` rewritten, compactMetadata/attachment uuids
/// remapped, `forkedFrom` recorded and a closing `custom-title`
/// `"<title> (fork)"`. Returns the new session id.
pub fn fork(
    root: &Path,
    source_id: &str,
    dir: &str,
    new_uuid: &mut dyn FnMut() -> String,
    up_to: Option<&str>,
) -> Result<String, StoreError> {
    if !is_session_id(source_id) {
        return Err(StoreError::Invalid(format!(
            "Invalid sessionId: {source_id}"
        )));
    }
    let real = std::fs::canonicalize(dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| dir.to_string());
    let (project_dir, raw) = project_dirs_for(root, &real)
        .into_iter()
        .find_map(|d| {
            let bytes = std::fs::read(d.join(format!("{source_id}.jsonl"))).ok()?;
            (!bytes.is_empty()).then_some((d, bytes))
        })
        .ok_or_else(|| {
            StoreError::NotFound(format!(
                "Session {source_id} not found in project directory for {dir}"
            ))
        })?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let mut transcript: Vec<Value> = Vec::new();
    let mut replacements: Vec<Value> = Vec::new();
    let mut history_suppressed = false;
    let mut atis_latch: Option<String> = None;
    let mut relocated_cwd: Option<String> = None;
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if MESSAGE_TYPES.contains(&kind) && entry.get("uuid").is_some_and(Value::is_string) {
            transcript.push(entry);
        } else if kind == "content-replacement"
            && entry.get("sessionId").and_then(Value::as_str) == Some(source_id)
        {
            if let Some(items) = entry.get("replacements").and_then(Value::as_array) {
                replacements.extend(items.iter().cloned());
            }
        } else if kind == "history-suppression" {
            history_suppressed = true;
        } else if kind == "atis-latch"
            && entry.get("sessionId").and_then(Value::as_str) == Some(source_id)
        {
            if let Some(atis) = entry.get("atis").and_then(Value::as_str) {
                if atis.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
                    atis_latch = Some(atis.to_string());
                }
            }
        } else if kind == "relocated"
            && entry.get("sessionId").and_then(Value::as_str) == Some(source_id)
        {
            if let Some(cwd) = entry
                .get("relocatedCwd")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.is_empty())
            {
                relocated_cwd = Some(cwd.to_string());
            }
        }
    }
    transcript.retain(|e| e.get("isSidechain").and_then(Value::as_bool) != Some(true));
    // `upToMessageId`: stop at the target message. The SDK finds the uuid
    // directly, else a `queued_command` attachment's `source_uuid` (`qA`), then
    // drops the compact boundaries that are not on the live chain of the slice
    // (`sve`).
    if let Some(target) = up_to {
        let Some(index) = fork_cut_index(&transcript, target) else {
            return Err(StoreError::NotFound(format!(
                "Message {target} not found in session {source_id}"
            )));
        };
        transcript = live_chain_slice(&transcript[..=index], new_uuid);
    }
    let uuid_of = |e: &Value| {
        e.get("uuid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let kept: Vec<&Value> = transcript
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) != Some("progress"))
        .collect();
    if kept.is_empty() {
        return Err(StoreError::NoMessages(source_id.to_string()));
    }
    let mut new_ids: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for entry in &transcript {
        new_ids.entry(uuid_of(entry)).or_insert_with(&mut *new_uuid);
    }
    // `l`: `ive` remaps compactMetadata anchors through the kept (non-progress)
    // entries only, so an anchor that points at a progress entry stays as it is.
    let kept_ids: HashMap<String, String> = kept
        .iter()
        .filter_map(|entry| {
            let uuid = uuid_of(entry);
            new_ids.get(&uuid).map(|new| (uuid.clone(), new.clone()))
        })
        .collect();
    let by_uuid: std::collections::HashMap<String, &Value> =
        transcript.iter().map(|e| (uuid_of(e), e)).collect();
    let fork_id = new_uuid();
    let now = iso8601(now_ms());
    let mut out: Vec<String> = Vec::new();
    if history_suppressed {
        out.push(
            json!({"type": "history-suppression", "sessionId": fork_id,
                "cause": "fork_inherit", "ts": now})
            .to_string(),
        );
    }
    let last = kept.len() - 1;
    let remap_id = |ids: &HashMap<String, String>, value: &Value| -> Value {
        value.as_str().map_or_else(
            || value.clone(),
            |id| ids.get(id).map_or_else(|| value.clone(), |new| json!(new)),
        )
    };
    for (i, entry) in kept.iter().enumerate() {
        let mut parent = entry
            .get("parentUuid")
            .and_then(Value::as_str)
            .map(String::from);
        let mut mapped_parent = Value::Null;
        let mut visited: HashSet<String> = HashSet::new();
        while let Some(p) = parent {
            let Some(ancestor) = by_uuid.get(&p) else {
                break;
            };
            if ancestor.get("type").and_then(Value::as_str) != Some("progress") {
                mapped_parent = new_ids.get(&p).map_or(Value::Null, |n| json!(n));
                break;
            }
            if !visited.insert(p.clone()) {
                mapped_parent = new_ids.get(&p).map_or(Value::Null, |n| json!(n));
                break;
            }
            parent = ancestor
                .get("parentUuid")
                .and_then(Value::as_str)
                .map(String::from);
        }
        let mut forked = (*entry).clone();
        let Some(map) = forked.as_object_mut() else {
            continue;
        };
        if map.get("type").and_then(Value::as_str) == Some("system")
            && map.get("subtype").and_then(Value::as_str) == Some("model_refusal_fallback")
        {
            map.insert("neutralizedByFork".into(), json!(true));
        }
        // `ive`: compactMetadata preserved-message anchors follow the new ids.
        if map.get("type").and_then(Value::as_str) == Some("system") {
            if let Some(compact) = map
                .get("compactMetadata")
                .filter(|value| value.is_object())
                .cloned()
            {
                let mut compact = compact;
                if let Some(compact_map) = compact.as_object_mut() {
                    let has_anchors = compact_map.contains_key("preservedMessages")
                        || compact_map.contains_key("preservedSegment")
                        || compact_map.contains_key("runningTurnSends");
                    compact_map.remove("runningTurnSends");
                    if has_anchors {
                        if let Some(preserved) = compact_map
                            .get_mut("preservedMessages")
                            .and_then(Value::as_object_mut)
                        {
                            if let Some(value) = preserved.get("anchorUuid").cloned() {
                                preserved.insert("anchorUuid".into(), remap_id(&kept_ids, &value));
                            }
                            for key in ["uuids", "allUuids"] {
                                if let Some(Value::Array(items)) = preserved.get(key).cloned() {
                                    preserved.insert(
                                        key.into(),
                                        Value::Array(
                                            items
                                                .iter()
                                                .map(|item| remap_id(&kept_ids, item))
                                                .collect(),
                                        ),
                                    );
                                }
                            }
                        }
                        if let Some(segment) = compact_map
                            .get_mut("preservedSegment")
                            .and_then(Value::as_object_mut)
                        {
                            for key in ["headUuid", "anchorUuid", "tailUuid"] {
                                if let Some(value) = segment.get(key).cloned() {
                                    segment.insert(key.into(), remap_id(&kept_ids, &value));
                                }
                            }
                        }
                        map.insert("compactMetadata".into(), compact);
                    }
                }
            }
        }
        // `ave`: deferred tool announcements follow the new ids.
        if map.get("type").and_then(Value::as_str) == Some("attachment") {
            if let Some(attachment) = map
                .get("attachment")
                .filter(|value| value.is_object())
                .cloned()
            {
                if let Some(attachment_map) = attachment.as_object() {
                    if attachment_map.get("type").and_then(Value::as_str)
                        == Some("deferred_tools_record")
                    {
                        if let Some(Value::Array(items)) =
                            attachment_map.get("nameOnlyAnnouncements")
                        {
                            let mapped: Vec<Value> = items
                                .iter()
                                .filter_map(|item| {
                                    item.as_str()
                                        .and_then(|id| new_ids.get(id))
                                        .map(|new| json!(new))
                                })
                                .collect();
                            let mut updated = attachment.clone();
                            if let Some(obj) = updated.as_object_mut() {
                                obj.insert("nameOnlyAnnouncements".into(), Value::Array(mapped));
                            }
                            map.insert("attachment".into(), updated);
                        }
                    }
                }
            }
            // `u0`: a `queued_command` attachment's `source_uuid` follows the
            // new ids (`qu`/`c0`: other attachments keep their source_uuid).
            let source_uuid = queued_command_field(entry, "source_uuid").map(String::from);
            if let Some(source_uuid) = source_uuid {
                if let Some(new) = new_ids.get(&source_uuid) {
                    if let Some(attachment) =
                        map.get_mut("attachment").and_then(Value::as_object_mut)
                    {
                        attachment.insert("source_uuid".into(), json!(new));
                    }
                }
            }
        }
        for key in FORK_DROPPED_KEYS {
            map.remove(key);
        }
        let logical = match entry.get("logicalParentUuid") {
            None => None,
            Some(Value::Null) => Some(Value::Null),
            Some(Value::String(s)) => Some(new_ids.get(s).map_or(Value::Null, |n| json!(n))),
            Some(other) => Some(other.clone()),
        };
        if let Some(logical) = logical {
            map.insert("logicalParentUuid".into(), logical);
        }
        map.insert("uuid".into(), json!(new_ids.get(&uuid_of(entry))));
        map.insert("parentUuid".into(), mapped_parent);
        map.insert("sessionId".into(), json!(fork_id));
        if i == last {
            map.insert("timestamp".into(), json!(now));
        }
        map.insert("isSidechain".into(), json!(false));
        map.insert(
            "forkedFrom".into(),
            json!({"sessionId": source_id, "messageUuid": uuid_of(entry)}),
        );
        out.push(forked.to_string());
    }
    if !replacements.is_empty() {
        out.push(
            json!({"type": "content-replacement", "sessionId": fork_id,
                "replacements": replacements, "uuid": new_uuid(), "timestamp": now})
            .to_string(),
        );
    }
    if let Some(atis) = atis_latch {
        out.push(json!({"type": "atis-latch", "sessionId": fork_id, "atis": atis}).to_string());
    }
    if let Some(cwd) = relocated_cwd {
        out.push(
            json!({"type": "relocated", "sessionId": fork_id, "relocatedCwd": cwd}).to_string(),
        );
    }
    // `nve`'s title: the tail window's customTitle first, then the head
    // window's (the sidecar title store is not ported), then the tail and head
    // aiTitle, then the first prompt of the head window.
    let head = String::from_utf8_lossy(&raw[..raw.len().min(WINDOW as usize)]).into_owned();
    let tail =
        String::from_utf8_lossy(&raw[raw.len().saturating_sub(WINDOW as usize)..]).into_owned();
    let custom =
        last_string_field(&tail, "customTitle").or_else(|| last_string_field(&head, "customTitle"));
    let title = custom
        .filter(|title| !title.is_empty())
        .or_else(|| last_string_field(&tail, "aiTitle").filter(|title| !title.is_empty()))
        .or_else(|| last_string_field(&head, "aiTitle").filter(|title| !title.is_empty()))
        .or_else(|| first_prompt(&head).filter(|prompt| !prompt.is_empty()))
        .unwrap_or_else(|| "Forked session".to_string());
    out.push(
        json!({"type": "custom-title", "sessionId": fork_id,
            "customTitle": format!("{title} (fork)"), "uuid": new_uuid(), "timestamp": now})
        .to_string(),
    );
    let mut body = out.join("\n");
    body.push('\n');
    // `hu` writes the fork transcript with `mode: 384` (0600); platform-specific
    // mode code is confined to `process.rs`, so only the exclusive-create half
    // is portable here. `create_new` also refuses to overwrite an existing id.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(project_dir.join(format!("{fork_id}.jsonl")))?;
    file.write_all(body.as_bytes())?;
    Ok(fork_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A unique, empty temp directory (removed on drop).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("inv52-{}-{n}-{}", std::process::id(), now_ms()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const C: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

    fn write_session(root: &Path, slug: &str, id: &str, body: &str, mtime_s: u64) {
        let dir = root.join(slug);
        std::fs::create_dir_all(&dir).expect("project dir");
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(&path, body).expect("write");
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open");
        file.set_modified(UNIX_EPOCH + std::time::Duration::from_secs(mtime_s))
            .expect("mtime");
    }

    fn user_line(uuid: &str, parent: Option<&str>, cwd: &str, text: &str) -> String {
        json!({"type": "user", "uuid": uuid, "parentUuid": parent, "cwd": cwd,
            "sessionId": A, "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {"role": "user", "content": text}})
        .to_string()
    }

    #[test]
    fn inv_52_slug_sanitizes_and_hashes_long_paths() {
        assert_eq!(project_slug("/Users/a b/c.d"), "-Users-a-b-c-d");
        let long = format!("/{}", "x".repeat(250));
        let slug = project_slug(&long);
        assert!(slug.starts_with(&format!("-{}", "x".repeat(199))));
        assert_eq!(slug.chars().filter(|c| *c == '-').count(), 2);
        assert_eq!(slug.len(), 200 + 1 + path_hash(&long).len());
        // The SDK hash is `Math.abs(h).toString(36)` over UTF-16 units.
        assert_eq!(path_hash("a"), "2p");
        assert_eq!(path_hash(""), "0");
    }

    #[test]
    fn inv_52_session_id_shape() {
        assert!(is_session_id(A));
        assert!(!is_session_id("not-a-uuid"));
        assert!(!is_session_id("../../etc/passwd"));
    }

    #[test]
    fn inv_52_iso8601_and_title_sanitizer() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(1_767_225_600_123), "2026-01-01T00:00:00.123Z");
        assert_eq!(sanitize_title("  a\n\n b\t c "), "a b c");
        let long = "x".repeat(300);
        let t = sanitize_title(&long);
        assert_eq!(t.chars().count(), 256);
        assert!(t.ends_with('…'));
    }

    /// The sanitizer before it learned to read only the start of a long text
    /// (`fullScanTitle` in `session-titles.test.ts`).
    fn full_scan_title(text: &str) -> String {
        let sanitized = collapse_js_whitespace(text);
        let sanitized = js_trim(&sanitized);
        if utf16_len(sanitized) <= 256 {
            sanitized.to_string()
        } else {
            format!("{}…", cut_utf16(sanitized, 255))
        }
    }

    /// sanitizeTitle > gives a long text the same title as a scan of the whole text
    #[test]
    fn t2b_sanitize_title_gives_a_long_text_the_title_of_a_full_scan() {
        let texts = [
            "lorem ipsum dolor sit amet\n".repeat(400_000),
            format!("{}{}", " \n\t".repeat(3000), "word ".repeat(2000)),
            // The cut of the scanned prefix falls inside a run of whitespace.
            format!("{}{}tail", "x".repeat(300), " ".repeat(5000)),
            // Mostly whitespace: the prefix collapses below a title's length.
            format!(
                "{}{}{}",
                "a ".repeat(100),
                " ".repeat(10_000),
                "b".repeat(300)
            ),
            format!("{}short", " ".repeat(10_000)),
            format!(
                "{}{}",
                "\u{a0}\u{feff}".repeat(2100),
                "\u{1f600}".repeat(2000)
            ),
            format!("{}{}", "y".repeat(4095), "\u{1f600}".repeat(10)),
            format!("{}{}", "z".repeat(257), " ".repeat(4096)),
            format!("{}{}", "z".repeat(256), " ".repeat(4096)),
        ];
        for text in &texts {
            assert_eq!(sanitize_title(text), full_scan_title(text));
        }
    }

    /// sanitizeTitle: lengths are UTF-16 units and the cut never splits a pair
    #[test]
    fn t2b_sanitize_title_counts_utf16_units_with_a_256_cap() {
        let astral = "\u{1f600}".repeat(200);
        let title = sanitize_title(&astral);
        // 255 units hold 127 whole pairs (254 units); the ellipsis follows.
        assert_eq!(title.chars().count(), 128);
        assert!(title.ends_with('…'));
        let exact = "w".repeat(256);
        assert_eq!(sanitize_title(&exact), exact);
        assert_eq!(utf16_len(&sanitize_title(&format!("{exact}w"))), 256);
        // Newlines and the JavaScript white space set collapse to one space.
        assert_eq!(sanitize_title("a\r\n\u{a0}\u{2028}b"), "a b");
        // U+FEFF is white space to JavaScript's `\s`, U+0085 is not.
        assert_eq!(sanitize_title("a\u{feff}b"), "a b");
        assert_eq!(sanitize_title("a\u{85}b"), "a\u{85}b");
    }

    #[test]
    fn inv_52_list_titles_order_and_paging() {
        let tmp = TempDir::new();
        let root = tmp.0.join("projects");
        let work = tmp.0.join("work");
        std::fs::create_dir_all(&work).expect("work");
        let real = std::fs::canonicalize(&work).expect("canon");
        let real = real.to_string_lossy().into_owned();
        let slug = project_slug(&real);
        // A: first prompt only. B: aiTitle in the tail. C: customTitle wins.
        write_session(
            &root,
            &slug,
            A,
            &user_line("u1", None, &real, "first prompt"),
            100,
        );
        write_session(
            &root,
            &slug,
            B,
            &format!(
                "{}\n{}\n",
                user_line("u2", None, &real, "other"),
                json!({"type": "ai-title", "aiTitle": "AI named"})
            ),
            300,
        );
        write_session(
            &root,
            &slug,
            C,
            &format!(
                "{}\n{}\n{}\n",
                user_line("u3", None, &real, "third"),
                json!({"type": "ai-title", "aiTitle": "AI named"}),
                json!({"type": "custom-title", "customTitle": "Mine"})
            ),
            200,
        );
        let page = list(&root, Some(&real), 0, 10);
        let got: Vec<(&str, &str)> = page
            .sessions
            .iter()
            .map(|r| (r.session_id.as_str(), r.title.as_str()))
            .collect();
        assert_eq!(got, vec![(B, "AI named"), (C, "Mine"), (A, "first prompt")]);
        assert_eq!(page.sessions[0].cwd, real);
        assert_eq!(page.sessions[0].last_modified_ms, 300_000);
        assert_eq!(page.next_cursor, None);

        let first = list(&root, Some(&real), 0, 2);
        assert_eq!(first.sessions.len(), 2);
        assert_eq!(first.next_cursor.as_deref(), Some("offset:2"));
        let second = list(&root, Some(&real), 2, 2);
        assert_eq!(second.sessions.len(), 1);
        assert_eq!(second.sessions[0].session_id, A);
        assert_eq!(second.next_cursor, None);

        // A different cwd matches another project: nothing here.
        assert!(list(&root, Some("/definitely/not/here"), 0, 10)
            .sessions
            .is_empty());
        // No cwd: every project.
        assert_eq!(list(&root, None, 0, 10).sessions.len(), 3);
    }

    #[test]
    fn inv_52_cursor_parse() {
        assert_eq!(parse_cursor(None).expect("none"), 0);
        assert_eq!(parse_cursor(Some("offset:7")).expect("7"), 7);
        assert!(parse_cursor(Some("page:2")).is_err());
        assert!(parse_cursor(Some("offset:")).is_err());
    }

    #[test]
    fn inv_52_delete_is_idempotent_and_removes_sidecar() {
        let tmp = TempDir::new();
        let root = tmp.0.join("projects");
        write_session(&root, "-p", A, &user_line("u1", None, "/p", "x"), 10);
        std::fs::create_dir_all(root.join("-p").join(A)).expect("sidecar");
        assert_eq!(find_transcripts(&root, A).len(), 1);
        delete(&root, A).expect("delete");
        assert!(find_transcripts(&root, A).is_empty());
        assert!(!root.join("-p").join(A).exists());
        delete(&root, A).expect("second delete");
        delete(&root, "not-an-id").expect("invalid id");
    }

    #[test]
    fn inv_52_fork_remaps_and_titles() {
        let tmp = TempDir::new();
        let root = tmp.0.join("projects");
        let work = tmp.0.join("work");
        std::fs::create_dir_all(&work).expect("work");
        let real = std::fs::canonicalize(&work).expect("canon");
        let real = real.to_string_lossy().into_owned();
        let slug = project_slug(&real);
        let progress = json!({"type": "progress", "uuid": "p1", "parentUuid": "u1",
            "sessionId": A, "timestamp": "t"});
        let reply = json!({"type": "assistant", "uuid": "a1", "parentUuid": "p1",
            "sessionId": A, "timestamp": "2026-01-01T00:00:01.000Z", "slug": "s",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]}});
        let sidechain = json!({"type": "user", "uuid": "s1", "isSidechain": true,
            "sessionId": A, "message": {"role": "user", "content": "side"}});
        let body = format!(
            "{}\n{progress}\n{reply}\n{sidechain}\n",
            user_line("u1", None, &real, "hello")
        );
        write_session(&root, &slug, A, &body, 50);
        let mut n = 0u32;
        let mut gen = || {
            n += 1;
            format!("00000000-0000-4000-8000-{n:012}")
        };
        let fork_id = fork(&root, A, &real, &mut gen, None).expect("fork");
        let raw = std::fs::read_to_string(root.join(&slug).join(format!("{fork_id}.jsonl")))
            .expect("fork file");
        let lines: Vec<Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        // user, assistant (progress and sidechain dropped), custom-title.
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["sessionId"], json!(fork_id));
        assert_ne!(lines[0]["uuid"], json!("u1"));
        assert_eq!(
            lines[0]["forkedFrom"],
            json!({"sessionId": A, "messageUuid": "u1"})
        );
        assert_eq!(lines[0]["parentUuid"], Value::Null);
        // The assistant's parent was a progress entry: it remaps to the user.
        assert_eq!(lines[1]["parentUuid"], lines[0]["uuid"]);
        assert!(lines[1].get("slug").is_none(), "slug is dropped");
        assert_eq!(lines[1]["isSidechain"], json!(false));
        assert_eq!(lines[2]["type"], json!("custom-title"));
        assert_eq!(lines[2]["customTitle"], json!("hello (fork)"));
        // The fork lists with its title; the source is untouched.
        let page = list(&root, Some(&real), 0, 10);
        assert!(page
            .sessions
            .iter()
            .any(|r| r.session_id == fork_id && r.title == "hello (fork)"));
        assert!(page.sessions.iter().any(|r| r.session_id == A));
    }

    /// M-33/M-90 — a fork with `upToMessageId` copies only up to that message;
    /// an unknown message is `NotFound` (the caller answers invalidParams).
    #[test]
    fn m33_fork_up_to_message_stops_at_the_point() {
        let tmp = TempDir::new();
        let root = tmp.0.join("projects");
        let work = tmp.0.join("work");
        std::fs::create_dir_all(&work).expect("work");
        let real = std::fs::canonicalize(&work).expect("canon");
        let real = real.to_string_lossy().into_owned();
        let slug = project_slug(&real);
        let body = format!(
            "{}\n{}\n{}\n{}\n",
            user_line("u1", None, &real, "one"),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":A,
                "message":{"role":"assistant","content":[{"type":"text","text":"first"}]}}),
            user_line("u2", Some("a1"), &real, "two"),
            json!({"type":"assistant","uuid":"a2","parentUuid":"u2","sessionId":A,
                "message":{"role":"assistant","content":[{"type":"text","text":"second"}]}}),
        );
        write_session(&root, &slug, A, &body, 50);
        let mut n = 0u32;
        let mut gen = || {
            n += 1;
            format!("00000000-0000-4000-8000-{n:012}")
        };
        let fork_id = fork(&root, A, &real, &mut gen, Some("a1")).expect("fork");
        let raw = std::fs::read_to_string(root.join(&slug).join(format!("{fork_id}.jsonl")))
            .expect("fork file");
        let lines: Vec<Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        // u1, a1, custom-title: u2/a2 are past the fork point.
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0]["message"]["content"], "one");
        assert_eq!(lines[1]["message"]["content"][0]["text"], "first");
        assert_eq!(lines[2]["type"], "custom-title");
        assert!(fork(&root, A, &real, &mut gen, Some("missing")).is_err());
    }

    /// M-32/M-90 — the fork copy remaps compactMetadata anchors and queued
    /// attachment source uuids, and writes history-suppression/atis-latch/
    /// relocated records (`IF`/`ive`/`ave`/`u0`).
    #[test]
    fn m32_fork_copies_fork_metadata_records() {
        let tmp = TempDir::new();
        let root = tmp.0.join("projects");
        let work = tmp.0.join("work");
        std::fs::create_dir_all(&work).expect("work");
        let real = std::fs::canonicalize(&work).expect("canon");
        let real = real.to_string_lossy().into_owned();
        let slug = project_slug(&real);
        let body = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            user_line("u1", None, &real, "one"),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":A,
                "message":{"role":"assistant","content":[{"type":"text","text":"first"}]}}),
            json!({"type":"attachment","uuid":"q1","parentUuid":"a1","sessionId":A,
                "attachment":{"type":"queued_command","prompt":"q","source_uuid":"u1"}}),
            json!({"type":"system","subtype":"compact_boundary","uuid":"b1",
                "parentUuid":"q1","sessionId":A,
                "compactMetadata":{"preservedMessages":{"anchorUuid":"u1","uuids":["a1"],
                    "allUuids":["u1","a1"]},"runningTurnSends":{"x":1},"keep":"y"}}),
            json!({"type":"history-suppression","cause":"x"}),
            json!({"type":"atis-latch","sessionId":A,"atis":"abc"}),
            json!({"type":"relocated","sessionId":A,"relocatedCwd":"/new"}),
        );
        write_session(&root, &slug, A, &body, 50);
        let mut n = 0u32;
        let mut gen = || {
            n += 1;
            format!("00000000-0000-4000-8000-{n:012}")
        };
        let fork_id = fork(&root, A, &real, &mut gen, None).expect("fork");
        let raw = std::fs::read_to_string(root.join(&slug).join(format!("{fork_id}.jsonl")))
            .expect("fork file");
        let lines: Vec<Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        let by_type = |kind: &str| lines.iter().find(|l| l["type"] == json!(kind));
        let suppression = by_type("history-suppression").expect("history-suppression");
        assert_eq!(suppression["cause"], "fork_inherit");
        assert_eq!(suppression["sessionId"], json!(fork_id));
        let atis = by_type("atis-latch").expect("atis-latch");
        assert_eq!(atis["atis"], "abc");
        assert_eq!(atis["sessionId"], json!(fork_id));
        let relocated = by_type("relocated").expect("relocated");
        assert_eq!(relocated["relocatedCwd"], "/new");
        let user = lines
            .iter()
            .find(|l| l["forkedFrom"]["messageUuid"] == json!("u1"))
            .expect("user");
        let assistant = lines
            .iter()
            .find(|l| l["message"]["role"] == "assistant")
            .expect("assistant");
        let boundary = by_type("system").expect("boundary");
        assert_eq!(
            boundary["compactMetadata"]["preservedMessages"]["anchorUuid"],
            user["uuid"]
        );
        assert_eq!(
            boundary["compactMetadata"]["preservedMessages"]["uuids"],
            json!([assistant["uuid"]])
        );
        assert_eq!(
            boundary["compactMetadata"]["preservedMessages"]["allUuids"],
            json!([user["uuid"], assistant["uuid"]])
        );
        assert!(boundary["compactMetadata"]
            .get("runningTurnSends")
            .is_none());
        assert_eq!(boundary["compactMetadata"]["keep"], "y");
        let attachment = by_type("attachment").expect("attachment");
        assert_eq!(attachment["attachment"]["source_uuid"], user["uuid"]);
    }

    /// M-33/M-90 — `sve`: the fork cut follows the live chain, so a compact
    /// boundary (and the rewind branch it anchors) that is not on the chain is
    /// dropped from the slice.
    #[test]
    fn m33_fork_cut_drops_a_stale_compact_boundary_and_its_branch() {
        let tmp = TempDir::new();
        let root = tmp.0.join("projects");
        let work = tmp.0.join("work");
        std::fs::create_dir_all(&work).expect("work");
        let real = std::fs::canonicalize(&work).expect("canon");
        let real = real.to_string_lossy().into_owned();
        let slug = project_slug(&real);
        let entry = |kind: &str, uuid: &str, parent: &str, text: &str| {
            json!({"type": kind, "uuid": uuid, "parentUuid": parent, "sessionId": A,
                "timestamp": "2026-01-01T00:00:00.000Z",
                "message": {"role": kind, "content": [{"type": "text", "text": text}]}})
        };
        let body = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            user_line("u1", None, &real, "one"),
            entry("assistant", "a1", "u1", "first"),
            user_line("u2", Some("a1"), &real, "two"),
            entry("assistant", "a2", "u2", "second"),
            // A rewind branch off a1, carrying a superseded compaction boundary.
            user_line("u3", Some("a1"), &real, "rewound"),
            entry("assistant", "a3", "u3", "rewritten"),
            json!({"type": "system", "subtype": "compact_boundary", "uuid": "cb1",
                "parentUuid": "a3", "sessionId": A,
                "timestamp": "2026-01-01T00:00:04.000Z",
                "compactMetadata": {"preservedMessages": {"anchorUuid": "gone",
                    "uuids": ["gone-1"]}}}),
            user_line("u4", Some("a2"), &real, "three"),
            entry("assistant", "a4", "u4", "third"),
        );
        write_session(&root, &slug, A, &body, 50);
        let mut n = 0u32;
        let mut gen = || {
            n += 1;
            format!("00000000-0000-4000-8000-{n:012}")
        };
        let fork_id = fork(&root, A, &real, &mut gen, Some("a4")).expect("fork");
        let raw = std::fs::read_to_string(root.join(&slug).join(format!("{fork_id}.jsonl")))
            .expect("fork file");
        let lines: Vec<Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        // u1, a1, u2, a2, u4, a4, custom-title: the rewind branch (u3/a3) and
        // the superseded boundary are not on the live chain.
        assert_eq!(lines.len(), 7, "{lines:?}");
        let forked: Vec<&str> = lines
            .iter()
            .filter_map(|l| l["forkedFrom"]["messageUuid"].as_str())
            .collect();
        assert_eq!(forked, ["u1", "a1", "u2", "a2", "u4", "a4"], "{lines:?}");
        assert!(!lines.iter().any(|l| l["type"] == json!("system")));
    }

    /// M-33 — `qA` falls back to a `queued_command` attachment's `source_uuid`
    /// only: another attachment type's `source_uuid` does not move the cut.
    #[test]
    fn m33_fork_cut_queued_command_source_uuid_only() {
        let tmp = TempDir::new();
        let root = tmp.0.join("projects");
        let work = tmp.0.join("work");
        std::fs::create_dir_all(&work).expect("work");
        let real = std::fs::canonicalize(&work).expect("canon");
        let real = real.to_string_lossy().into_owned();
        let slug = project_slug(&real);
        let body = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            user_line("u1", None, &real, "one"),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":A,
                "message":{"role":"assistant","content":[{"type":"text","text":"first"}]}}),
            // Not a queued command: its source_uuid is not a cut candidate.
            json!({"type":"attachment","uuid":"x1","parentUuid":"a1","sessionId":A,
                "attachment":{"type":"deferred_tools_record","source_uuid":"target"}}),
            json!({"type":"attachment","uuid":"q1","parentUuid":"x1","sessionId":A,
                "attachment":{"type":"queued_command","prompt":"q","source_uuid":"target"}}),
            user_line("u2", Some("q1"), &real, "two"),
        );
        write_session(&root, &slug, A, &body, 50);
        let mut n = 0u32;
        let mut gen = || {
            n += 1;
            format!("00000000-0000-4000-8000-{n:012}")
        };
        let fork_id = fork(&root, A, &real, &mut gen, Some("target")).expect("fork");
        let raw = std::fs::read_to_string(root.join(&slug).join(format!("{fork_id}.jsonl")))
            .expect("fork file");
        let lines: Vec<Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        // u1, a1, x1, q1, custom-title: u2 is past the queued command.
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert_eq!(lines[3]["attachment"]["type"], json!("queued_command"));
        assert_eq!(lines[4]["type"], json!("custom-title"));
    }

    /// M-33/M-32 — `nve`: the fork title reads the tail window's customTitle,
    /// then the head window's, then the aiTitles, then the first prompt. A
    /// customTitle only in the middle of the file is not a title.
    #[test]
    fn m32_fork_title_reads_only_the_head_and_tail_windows() {
        let tmp = TempDir::new();
        let root = tmp.0.join("projects");
        let work = tmp.0.join("work");
        std::fs::create_dir_all(&work).expect("work");
        let real = std::fs::canonicalize(&work).expect("canon");
        let real = real.to_string_lossy().into_owned();
        let slug = project_slug(&real);
        let filler = |from: usize, to: usize| -> String {
            (from..to)
                .map(|i| {
                    json!({"type": "progress", "uuid": format!("f{i}"), "parentUuid": "u1",
                        "sessionId": A, "timestamp": "2026-01-01T00:00:00.000Z",
                        "data": "x".repeat(200)})
                    .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        // The middle title sits past the 64 KiB head window and before the
        // 64 KiB tail window.
        let body = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            user_line("u1", None, &real, "hello"),
            filler(0, 400),
            json!({"type": "custom-title", "sessionId": A, "customTitle": "MID-TITLE"}),
            filler(400, 800),
            json!({"type": "progress", "uuid": "last", "parentUuid": "u1",
                "sessionId": A, "timestamp": "2026-01-01T00:00:00.000Z", "data": "end"}),
        );
        assert!(
            body.len() > (3 * WINDOW) as usize,
            "the title is in the middle"
        );
        write_session(&root, &slug, A, &body, 50);
        let mut n = 0u32;
        let mut gen = || {
            n += 1;
            format!("00000000-0000-4000-8000-{n:012}")
        };
        let fork_id = fork(&root, A, &real, &mut gen, None).expect("fork");
        let raw = std::fs::read_to_string(root.join(&slug).join(format!("{fork_id}.jsonl")))
            .expect("fork file");
        let lines: Vec<Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[1]["customTitle"], json!("hello (fork)"));

        // A tail customTitle wins over a head one.
        let body = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            json!({"type": "custom-title", "sessionId": A, "customTitle": "HEAD-TITLE"}),
            user_line("u1", None, &real, "hello"),
            filler(0, 400),
            json!({"type": "custom-title", "sessionId": A, "customTitle": "MID-TITLE"}),
            json!({"type": "custom-title", "sessionId": A, "customTitle": "TAIL-TITLE"}),
        );
        write_session(&root, &slug, A, &body, 60);
        let fork_id = fork(&root, A, &real, &mut gen, None).expect("fork");
        let raw = std::fs::read_to_string(root.join(&slug).join(format!("{fork_id}.jsonl")))
            .expect("fork file");
        let lines: Vec<Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        assert_eq!(
            lines[1]["customTitle"],
            json!("TAIL-TITLE (fork)"),
            "{lines:?}"
        );
    }

    /// M-32/LOW-6 — `ive` remaps compactMetadata anchors through the kept
    /// (non-progress) map, so an anchor that points at a progress entry is
    /// left alone.
    #[test]
    fn m32_fork_compact_anchor_at_a_progress_entry_is_not_remapped() {
        let tmp = TempDir::new();
        let root = tmp.0.join("projects");
        let work = tmp.0.join("work");
        std::fs::create_dir_all(&work).expect("work");
        let real = std::fs::canonicalize(&work).expect("canon");
        let real = real.to_string_lossy().into_owned();
        let slug = project_slug(&real);
        let body = format!(
            "{}\n{}\n{}\n{}\n",
            user_line("u1", None, &real, "one"),
            json!({"type":"progress","uuid":"p1","parentUuid":"u1","sessionId":A,
                "timestamp":"2026-01-01T00:00:00.000Z"}),
            json!({"type":"system","subtype":"compact_boundary","uuid":"b1",
                "parentUuid":"p1","sessionId":A,
                "compactMetadata":{"preservedMessages":{"anchorUuid":"p1","uuids":["p1"]}}}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"p1","sessionId":A,
                "message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}),
        );
        write_session(&root, &slug, A, &body, 50);
        let mut n = 0u32;
        let mut gen = || {
            n += 1;
            format!("00000000-0000-4000-8000-{n:012}")
        };
        let fork_id = fork(&root, A, &real, &mut gen, None).expect("fork");
        let raw = std::fs::read_to_string(root.join(&slug).join(format!("{fork_id}.jsonl")))
            .expect("fork file");
        let lines: Vec<Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).expect("json"))
            .collect();
        let boundary = lines
            .iter()
            .find(|l| l["type"] == json!("system"))
            .expect("boundary");
        assert_eq!(
            boundary["compactMetadata"]["preservedMessages"]["anchorUuid"],
            json!("p1")
        );
        assert_eq!(
            boundary["compactMetadata"]["preservedMessages"]["uuids"],
            json!(["p1"])
        );
    }

    /// LOW-4 — the fork file is created exclusively: an id that already has a
    /// file is refused, never overwritten (`hu`'s `mode: 384` write, carried
    /// without platform mode code).
    #[test]
    fn m32_fork_refuses_to_overwrite_an_existing_id() {
        let tmp = TempDir::new();
        let root = tmp.0.join("projects");
        let work = tmp.0.join("work");
        std::fs::create_dir_all(&work).expect("work");
        let real = std::fs::canonicalize(&work).expect("canon");
        let real = real.to_string_lossy().into_owned();
        let slug = project_slug(&real);
        write_session(&root, &slug, A, &user_line("u1", None, &real, "one"), 50);
        write_session(&root, &slug, "dup", "existing\n", 60);
        let mut gen = || "dup".to_string();
        assert!(matches!(
            fork(&root, A, &real, &mut gen, None),
            Err(StoreError::Io(_))
        ));
        assert_eq!(
            std::fs::read_to_string(root.join(&slug).join("dup.jsonl")).expect("kept"),
            "existing\n"
        );
    }

    #[test]
    fn inv_52_fork_errors() {
        let tmp = TempDir::new();
        let root = tmp.0.join("projects");
        let mut gen = || "x".to_string();
        assert!(matches!(
            fork(&root, "bad", "/p", &mut gen, None),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            fork(&root, A, "/p", &mut gen, None),
            Err(StoreError::NotFound(_))
        ));
    }
}
