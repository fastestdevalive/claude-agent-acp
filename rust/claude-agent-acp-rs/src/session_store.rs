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
//! sidecar title store, the `isSidechain`-first-line archive checks of
//! `pF`, command-tag prompt filtering of `vi`, `compactMetadata` /
//! attachment uuid remapping and `upToMessageId` of `forkSession`.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};

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
    let config = env
        .get_path("CLAUDE_CONFIG_DIR")
        .or_else(|| env.get_path("HOME").map(|h| h.join(".claude")))
        .unwrap_or_default();
    config.join("projects")
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

/// `sanitizePath`: every non-ASCII-alphanumeric character becomes `-`.
fn sanitize(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The SDK's string hash over UTF-16 code units, base 36 of its absolute value.
fn path_hash(path: &str) -> String {
    let mut h: i32 = 0;
    for unit in path.encode_utf16() {
        h = h
            .wrapping_shl(5)
            .wrapping_sub(h)
            .wrapping_add(i32::from(unit));
    }
    let mut n = i64::from(h).unsigned_abs();
    if n == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::new();
    while n > 0 {
        digits.push(char::from_digit((n % 36) as u32, 36).unwrap_or('0'));
        n /= 36;
    }
    digits.iter().rev().collect()
}

/// The project directory name of `path` (`_u`): the sanitized path, cut at 200
/// characters plus `-<hash>` when longer.
pub fn project_slug(path: &str) -> String {
    let slug = sanitize(path);
    if slug.chars().count() <= MAX_SLUG_LENGTH {
        return slug;
    }
    let cut: String = slug.chars().take(MAX_SLUG_LENGTH).collect();
    format!("{cut}-{}", path_hash(path))
}

/// The existing project directories for `dir`: the exact slug, plus (for a long
/// path, whose hash the CLI computes differently) the directories sharing its
/// 200-character prefix.
fn project_dirs_for(root: &Path, dir: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let exact = root.join(project_slug(dir));
    if exact.is_dir() {
        out.push(exact.clone());
    }
    let slug = sanitize(dir);
    if slug.chars().count() > MAX_SLUG_LENGTH {
        let prefix: String = slug.chars().take(MAX_SLUG_LENGTH).collect();
        let prefix = format!("{prefix}-");
        if let Ok(read) = std::fs::read_dir(root) {
            let mut names: Vec<_> = read.flatten().collect();
            names.sort_by_key(std::fs::DirEntry::file_name);
            for entry in names {
                let path = entry.path();
                if path.is_dir()
                    && entry.file_name().to_string_lossy().starts_with(&prefix)
                    && path != exact
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
    for line in head.lines() {
        if !line.contains("\"type\":\"user\"") && !line.contains("\"type\": \"user\"") {
            continue;
        }
        if line.contains("\"tool_result\"")
            || line.contains("\"isMeta\":true")
            || line.contains("\"isCompactSummary\":true")
        {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let content = value.pointer("/message/content");
        let mut texts: Vec<&str> = Vec::new();
        match content {
            Some(Value::String(s)) => texts.push(s),
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                        texts.clear();
                        break;
                    }
                    if block.get("type").and_then(Value::as_str) == Some("text") {
                        if let Some(t) = block.get("text").and_then(Value::as_str) {
                            texts.push(t);
                        }
                    }
                }
            }
            _ => {}
        }
        for text in texts {
            let one_line = text.replace('\n', " ");
            let trimmed = one_line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.chars().count() > 200 {
                let cut: String = trimmed.chars().take(200).collect();
                return Some(format!("{}…", cut.trim()));
            }
            return Some(trimmed.to_string());
        }
    }
    None
}

/// `sanitizeTitle` (`session-titles.js`): collapse whitespace, cap at 256.
pub fn sanitize_title(text: &str) -> String {
    let collapse = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > TITLE_SCAN_LENGTH {
        let head: String = text.chars().take(TITLE_SCAN_LENGTH).collect();
        let head = head.split_whitespace().collect::<Vec<_>>().join(" ");
        if head.chars().count() >= MAX_TITLE_LENGTH + 2 {
            let cut: String = head.chars().take(MAX_TITLE_LENGTH - 1).collect();
            return format!("{cut}…");
        }
    }
    let sanitized = collapse(text);
    if sanitized.chars().count() <= MAX_TITLE_LENGTH {
        return sanitized;
    }
    let cut: String = sanitized.chars().take(MAX_TITLE_LENGTH - 1).collect();
    format!("{cut}…")
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
    let summary = custom
        .or_else(|| last_string_field(&tail, "lastPrompt").filter(|s| !s.is_empty()))
        .or_else(|| last_string_field(&tail, "summary").filter(|s| !s.is_empty()))
        .or_else(|| first_prompt(&head).filter(|s| !s.is_empty()))
        .or_else(|| {
            head.lines().find_map(|l| {
                if !l.contains("\"type\":\"user\"") || l.contains("\"tool_result\"") {
                    None
                } else if l.contains("\"type\":\"image\"") {
                    Some("Image".to_string())
                } else if l.contains("\"type\":\"document\"") {
                    Some("Document".to_string())
                } else {
                    None
                }
            })
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

/// Copy the latest transcript of `source_id` (found under `dir`'s project) into
/// a new session, as SDK `forkSession` without `upToMessageId`: a fresh uuid per
/// entry (`progress` entries dropped, ancestors remapped past them),
/// `sessionId` rewritten, `forkedFrom` recorded and a closing `custom-title`
/// `"<title> (fork)"`. Returns the new session id.
pub fn fork(
    root: &Path,
    source_id: &str,
    dir: &str,
    new_uuid: &mut dyn FnMut() -> String,
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
        }
    }
    transcript.retain(|e| e.get("isSidechain").and_then(Value::as_bool) != Some(true));
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
    let by_uuid: std::collections::HashMap<String, &Value> =
        transcript.iter().map(|e| (uuid_of(e), e)).collect();
    let fork_id = new_uuid();
    let now = iso8601(now_ms());
    let mut out: Vec<String> = Vec::new();
    let last = kept.len() - 1;
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
    let head: String = text.chars().take(WINDOW as usize).collect();
    let tail: String = {
        let all: Vec<char> = text.chars().collect();
        all[all.len().saturating_sub(WINDOW as usize)..]
            .iter()
            .collect()
    };
    let title = last_string_field(&tail, "customTitle")
        .filter(|s| !s.is_empty())
        .or_else(|| last_string_field(&head, "customTitle").filter(|s| !s.is_empty()))
        .or_else(|| last_string_field(&tail, "aiTitle").filter(|s| !s.is_empty()))
        .or_else(|| last_string_field(&head, "aiTitle").filter(|s| !s.is_empty()))
        .or_else(|| first_prompt(&head).filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "Forked session".to_string());
    out.push(
        json!({"type": "custom-title", "sessionId": fork_id,
            "customTitle": format!("{title} (fork)"), "uuid": new_uuid(), "timestamp": now})
        .to_string(),
    );
    let mut body = out.join("\n");
    body.push('\n');
    std::fs::write(project_dir.join(format!("{fork_id}.jsonl")), body)?;
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
        let fork_id = fork(&root, A, &real, &mut gen).expect("fork");
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

    #[test]
    fn inv_52_fork_errors() {
        let tmp = TempDir::new();
        let root = tmp.0.join("projects");
        let mut gen = || "x".to_string();
        assert!(matches!(
            fork(&root, "bad", "/p", &mut gen),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            fork(&root, A, "/p", &mut gen),
            Err(StoreError::NotFound(_))
        ));
    }
}
