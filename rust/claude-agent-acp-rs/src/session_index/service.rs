//! The `sessionIndex` AIR extension: the session list, rename, archive and delete
//! of a client that declared the capability (upstream
//! `session-index/service.ts`).
//!
//! Wire contract: docs/air-extensions.md, "Session index". Everything here is
//! reached only for a `sessionIndex` client, except the archive that an AIR client
//! without the capability gets in place of a delete (see
//! [`archive_instead_of_delete`]).
//!
//! The index and the live registry are plain `&mut self` structures that live on
//! one worker thread ([`IndexWorker`]): every file system call is synchronous and
//! every job runs one after the other, which serialises the file steps of a
//! mutation. The multi-step mutations (a rename that reaches the CLI between two
//! file steps) additionally hold a gate per session (upstream `exclusive`), so a
//! rename of one session never interleaves with another mutation of the same
//! session and never waits for another session's.

use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agent_client_protocol::Error;
use serde_json::{json, Value};
use tokio::sync::{watch, Semaphore};

use super::activity::OwnSessionState;
use super::archive_title::{
    agent_name_record, effective_title, is_archived_title, stored_title, title_records,
    visible_title,
};
use super::list_subscriptions::{
    ListSubscriptionDeps, ListSubscriptionTiming, ListSubscriptions, SharedForkHolds,
};
use super::live_registry::{LiveRegistryDeps, LiveSessionRegistry};
use super::project_dirs::{
    canonical_path, is_exact_project_dir, is_session_id, project_dir_matches, same_project_path,
    Platform,
};
use super::session_info::session_info_of;
use super::transcript_scan::{
    read_head_tail, read_head_tail_of, sdk_titles, tail_custom_title, transcript_agent_name,
    transcript_project_cwd,
};
use super::{
    read_sidecar_title, ArchivedFilter, ListCursor, ListQuery, SessionIndex, DEFAULT_LIST_LIMIT,
    MAX_LIST_LIMIT,
};

pub const SESSION_RENAME_METHOD: &str = "_session/rename";
pub const SESSION_ARCHIVE_METHOD: &str = "_session/archive";
pub const SESSION_UNARCHIVE_METHOD: &str = "_session/unarchive";

/// The JSON-RPC code of an unknown session (ACP `ResourceNotFound`).
const RESOURCE_NOT_FOUND: i32 = -32002;
/// The cursor format. A cursor of another version is rejected.
const CURSOR_VERSION: i64 = 5;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Sends the `_session/list/changes` params to the client.
pub type ListChangesNotifier = Arc<dyn Fn(Value) -> BoxFuture<Result<(), Error>> + Send + Sync>;

/// `rename_session` through the CLI of a session.
pub type RenameFn = Box<dyn FnOnce(String) -> BoxFuture<Result<(), Error>> + Send>;

// ---- request errors ----------------------------------------------------------

/// `RequestError.invalidParams(data, message)`.
pub fn invalid_params(data: Option<Value>, message: &str) -> Error {
    let error = Error::new(-32602, format!("Invalid params: {message}"));
    match data {
        Some(data) => error.data(data),
        None => error,
    }
}

/// A plain `Error` thrown in the handler: `-32603` with `data: {details}`.
pub fn internal_error(message: &str) -> Error {
    Error::internal_error().data(json!({ "details": message }))
}

/// `RequestError.methodNotFound(method)`.
pub fn method_not_found(method: &str) -> Error {
    Error::new(-32601, format!("\"Method not found\": {method}")).data(json!({ "method": method }))
}

pub fn session_not_found(session_id: &str) -> Error {
    Error::new(
        RESOURCE_NOT_FOUND,
        format!("Session not found: {session_id}"),
    )
    .data(json!({ "sessionId": session_id }))
}

// ---- request parsing ---------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionIdRequest {
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameSessionRequest {
    pub session_id: String,
    pub title: String,
}

pub fn parse_session_id_request(value: &Value) -> Result<SessionIdRequest, Error> {
    match value.get("sessionId").and_then(Value::as_str) {
        Some(id) if !super::js_text::js_trim(id).is_empty() => Ok(SessionIdRequest {
            session_id: super::js_text::js_trim(id).to_string(),
        }),
        _ => Err(invalid_params(None, "params require a non-empty sessionId")),
    }
}

pub fn parse_rename_session_request(value: &Value) -> Result<RenameSessionRequest, Error> {
    let SessionIdRequest { session_id } = parse_session_id_request(value)?;
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .map(crate::session_store::sanitize_title)
        .unwrap_or_default();
    if title.is_empty() {
        return Err(invalid_params(None, "title must be a non-empty string"));
    }
    Ok(RenameSessionRequest { session_id, title })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ListOptions {
    pub limit: usize,
    pub archived: ArchivedFilter,
    pub include_worktrees: bool,
}

/// What a cursor is bound to: the request values that select the rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListScope {
    pub cwd: Option<String>,
    pub archived: ArchivedFilter,
    pub include_worktrees: bool,
}

fn archived_filter(list: &Value) -> Result<ArchivedFilter, Error> {
    // Omitted or null is `unarchived`.
    match list.get("archived") {
        None | Some(Value::Null) => Ok(ArchivedFilter::Unarchived),
        Some(value) => value
            .as_str()
            .and_then(ArchivedFilter::parse)
            .ok_or_else(|| {
                invalid_params(
                    Some(json!({ "archived": value })),
                    "`_meta.jetbrains.air.list.archived` must be \"unarchived\", \"archived\" or \"all\"",
                )
            }),
    }
}

fn optional_boolean(list: &Value, key: &str) -> Result<bool, Error> {
    // Omitted or null is false.
    match list.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(other) => Err(invalid_params(
            Some(json!({ key: other })),
            &format!("`_meta.jetbrains.air.list.{key}` must be a boolean"),
        )),
    }
}

/// `_meta.jetbrains.air.list` of a list request.
pub fn parse_list_options(meta: Option<&Value>) -> Result<ListOptions, Error> {
    let list = meta
        .and_then(|m| m.pointer("/jetbrains/air"))
        .filter(|air| air.is_object())
        .and_then(|air| air.get("list"))
        .filter(|list| list.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    // Omitted or null is the default; an integer of at least 1 is clamped;
    // anything else is invalid.
    let limit = match list.get("limit") {
        None | Some(Value::Null) => DEFAULT_LIST_LIMIT,
        Some(raw) => {
            let integer = raw
                .as_f64()
                .filter(|n| n.is_finite() && n.fract() == 0.0 && *n >= 1.0);
            match integer {
                Some(n) => (n.min(MAX_LIST_LIMIT as f64)) as usize,
                None => {
                    return Err(invalid_params(
                        Some(json!({ "limit": raw })),
                        "`_meta.jetbrains.air.list.limit` must be an integer of at least 1",
                    ))
                }
            }
        }
    };
    Ok(ListOptions {
        limit,
        archived: archived_filter(&list)?,
        include_worktrees: optional_boolean(&list, "includeWorktrees")?,
    })
}

// ---- cursor ------------------------------------------------------------------

const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// `Buffer.from(bytes).toString("base64url")` (no padding).
fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(BASE64URL[(n >> 18) as usize & 63] as char);
        out.push(BASE64URL[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(BASE64URL[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(BASE64URL[n as usize & 63] as char);
        }
    }
    out
}

/// `Buffer.from(text, "base64url")`: lenient like Node's, both alphabets, any
/// other character skipped, decoding ends at `=`.
fn base64url_decode(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.chars() {
        let value = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '-' | '+' => 62,
            '_' | '/' => 63,
            '=' => break,
            _ => continue,
        };
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// A number as `JSON.stringify` writes it.
fn js_number_text(value: f64) -> String {
    super::js_text::js_number(value).to_string()
}

pub fn encode_list_cursor(cursor: &ListCursor, scope: &ListScope) -> String {
    // The key order of upstream's payload object.
    let payload = format!(
        "{{\"v\":{CURSOR_VERSION},\"u\":{},\"id\":{},\"cwd\":{},\"archived\":{},\"worktrees\":{}}}",
        js_number_text(cursor.order_at_ms),
        json!(cursor.session_id),
        json!(scope.cwd),
        json!(scope.archived.as_str()),
        scope.include_worktrees,
    );
    base64url_encode(payload.as_bytes())
}

/// The position a cursor names. A cursor of another cwd or filter, or one this
/// adapter did not issue, is rejected.
pub fn decode_list_cursor(cursor: &str, scope: &ListScope) -> Result<ListCursor, Error> {
    let payload: Option<Value> =
        serde_json::from_str(&String::from_utf8_lossy(&base64url_decode(cursor))).ok();
    let unknown = || invalid_params(None, &format!("Unknown session/list cursor: {cursor}"));
    let Some(payload) = payload.filter(Value::is_object) else {
        return Err(unknown());
    };
    let version_ok = payload.get("v").and_then(Value::as_f64) == Some(CURSOR_VERSION as f64);
    let order = payload.get("u").and_then(Value::as_f64);
    let id = payload.get("id").and_then(Value::as_str);
    let (true, Some(order), Some(id)) = (version_ok, order, id) else {
        return Err(unknown());
    };
    let same_cwd = match (payload.get("cwd"), &scope.cwd) {
        (Some(Value::Null), None) => true,
        (Some(Value::String(a)), Some(b)) => a == b,
        _ => false,
    };
    if !same_cwd
        || payload.get("archived").and_then(Value::as_str) != Some(scope.archived.as_str())
        || payload.get("worktrees") != Some(&Value::Bool(scope.include_worktrees))
    {
        return Err(invalid_params(
            None,
            "The session/list cursor belongs to another cwd or filter",
        ));
    }
    Ok(ListCursor {
        order_at_ms: order,
        session_id: id.to_string(),
    })
}

// ---- transcript writes ---------------------------------------------------------

/// `<projectDir>/<sessionId>/custom-title.json` of a transcript.
pub fn sidecar_path(transcript_path: &Path) -> PathBuf {
    let session_id = transcript_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    transcript_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(&session_id)
        .join("custom-title.json")
}

/// A 16-hex-character temporary suffix, like Node's
/// `randomBytes(8).toString("hex")`: `RandomState` is seeded from the OS per
/// instance, and the process counter and clock keep two calls unique even when
/// two seeds collide. (The old `uuid_v4` low bytes depended only on the
/// nanosecond clock, so concurrent writes could pick the same temporary name and
/// the second `create_new` failed with `AlreadyExists` — M-133.)
/// A 16-hex-character temporary suffix, like Node's
/// `randomBytes(8).toString("hex")`: `RandomState` is seeded from the OS per
/// instance, and the process counter and clock keep two calls unique even when
/// two seeds collide. (The old `uuid_v4` low bytes depended only on the
/// nanosecond clock, so concurrent writes could pick the same temporary name and
/// the second `create_new` failed with `AlreadyExists` — M-133.)
fn random_hex() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::AtomicU64;

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u128(nanos);
    format!("{:016x}", hasher.finish())
}

/// Writes `<projectDir>/<sessionId>/custom-title.json` the way the CLI's
/// `/rename` does: file 0600 in a 0700 directory, replaced atomically. The
/// temporary file has a random name of the CLI's `custom-title.json.tmp.*`
/// pattern, and only a temporary file this call created is removed.
pub fn write_custom_title_sidecar(transcript_path: &Path, title: &str) -> std::io::Result<()> {
    let target = sidecar_path(transcript_path);
    if let Some(dir) = target.parent() {
        crate::process::create_dir_all_private(dir)?;
    }
    let temporary = PathBuf::from(format!("{}.tmp.{}", target.display(), random_hex()));
    let mut file = crate::process::create_new_file_private(&temporary)?;
    let result = (|| {
        file.write_all(json!({ "customTitle": title }).to_string().as_bytes())?;
        drop(file);
        std::fs::rename(&temporary, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// The last two non-empty lines of a file, from its last 64 KB, and whether the
/// file ends with a newline.
fn last_lines(path: &Path) -> std::io::Result<(Vec<String>, String, bool)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let length = size.min(64 * 1024);
    file.seek(SeekFrom::Start(size - length))?;
    let mut buffer = Vec::new();
    file.take(length).read_to_end(&mut buffer)?;
    let text = String::from_utf8_lossy(&buffer).into_owned();
    let lines: Vec<String> = text
        .split('\n')
        .filter(|line| !super::js_text::js_trim(line).is_empty())
        .map(str::to_string)
        .collect();
    let last = lines.last().cloned().unwrap_or_default();
    let skip = lines.len().saturating_sub(2);
    Ok((lines[skip..].to_vec(), last, text.ends_with('\n')))
}

/// Appends the title records of `title` to a transcript whose last two records are
/// not those already, whoever else has the session open, as the CLI's `/rename`
/// does. A torn last line is closed first.
fn ensure_title_records(path: &Path, session_id: &str, title: &str) -> std::io::Result<()> {
    let records = title_records(session_id, title);
    let (lines, last, ends_with_newline) = last_lines(path)?;
    if format!("{}\n", lines.join("\n")) == records {
        return Ok(());
    }
    let complete = ends_with_newline || last.is_empty();
    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    file.write_all(format!("{}{records}", if complete { "" } else { "\n" }).as_bytes())
}

/// Appends the `agent-name` record of `title` when the copy's agent name is
/// another one, so its archive state follows `title`.
fn ensure_agent_name(path: &Path, session_id: &str, title: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)?;
    let size = file.metadata()?.len();
    let head_tail = read_head_tail_of(&mut file, size)?;
    let Some(agent_name) = transcript_agent_name(&head_tail) else {
        return Ok(());
    };
    if agent_name == title {
        return Ok(());
    }
    let complete = size == 0 || head_tail.tail.ends_with('\n');
    file.write_all(
        format!(
            "{}{}\n",
            if complete { "" } else { "\n" },
            agent_name_record(session_id, title)
        )
        .as_bytes(),
    )
}

/// The title of one transcript copy as the list reads it (see
/// [`effective_title`]): its agent name, else the title the SDK reports. It carries
/// the archive prefix only when the copy is archived: a generated title never
/// archives a session.
fn copy_title(path: &Path, session_id: &str) -> std::io::Result<Option<String>> {
    let size = std::fs::metadata(path)?.len();
    let head_tail = read_head_tail(path, size)?;
    let sidecar = if tail_custom_title(&head_tail).is_none() {
        read_sidecar_title(path, session_id)
    } else {
        None
    };
    let (title, archived) = effective_title(
        transcript_agent_name(&head_tail).as_deref(),
        &sdk_titles(&head_tail, sidecar.as_deref()),
    );
    Ok(match title {
        Some(title) if !archived && is_archived_title(Some(&title)) => Some(visible_title(&title)),
        other => other,
    })
}

/// Rewrites the sidecar of a copy whose title is `current` when the sidecar says
/// the other archive state (a sidecar write that failed after the records were
/// appended).
fn align_sidecar(
    transcript: &Path,
    current: Option<&str>,
    session_id: &str,
) -> std::io::Result<()> {
    let Some(current) = current else {
        return Ok(());
    };
    let Some(sidecar) = read_sidecar_title(transcript, session_id) else {
        return Ok(());
    };
    if is_archived_title(Some(&sidecar)) == is_archived_title(Some(current)) {
        return Ok(());
    }
    write_custom_title_sidecar(
        transcript,
        &stored_title(current, is_archived_title(Some(current)), session_id),
    )
}

/// The title a change stores for a copy whose title is `current` (`None`: none
/// yet), or `None` to leave the copy as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TitleChange {
    /// `_session/rename`: the new title, with the archive prefix on a copy that is
    /// archived, so a rename keeps the archive state.
    Rename(String),
    /// `_session/archive` and `_session/unarchive`: the current title with or
    /// without the archive prefix; a copy already in that state is left alone.
    Archive(bool),
}

impl TitleChange {
    pub fn apply(&self, current: Option<&str>, session_id: &str) -> Option<String> {
        match self {
            Self::Rename(title) => Some(if is_archived_title(current) {
                stored_title(title, true, session_id)
            } else {
                title.clone()
            }),
            Self::Archive(archived) => {
                if is_archived_title(current) == *archived {
                    None
                } else {
                    Some(stored_title(current.unwrap_or(""), *archived, session_id))
                }
            }
        }
    }
}

/// Whether `transcript` lies in the project directory of one of `paths`, the one
/// the CLI of that cwd writes: the exact encoding, or, for a long path whose name
/// the CLI hashes differently, a directory with the cut prefix whose transcript
/// belongs to the path. Another long path that shares the prefix does not count.
fn is_transcript_of(transcript: &Path, paths: &[String], platform: Platform) -> bool {
    let dir_name = transcript
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if paths
        .iter()
        .any(|cwd| is_exact_project_dir(&dir_name, cwd, platform))
    {
        return true;
    }
    if !paths
        .iter()
        .any(|cwd| project_dir_matches(&dir_name, cwd, platform))
    {
        return false;
    }
    let Ok(size) = std::fs::metadata(transcript).map(|m| m.len()) else {
        return false;
    };
    // Unreadable: not known to be the CLI's own copy.
    let Ok(head_tail) = read_head_tail(transcript, size) else {
        return false;
    };
    transcript_project_cwd(&head_tail).is_some_and(|cwd| {
        paths
            .iter()
            .any(|project_path| same_project_path(&cwd, project_path, platform))
    })
}

fn exists(target: &Path) -> bool {
    std::fs::symlink_metadata(target).is_ok()
}

// ---- the worker --------------------------------------------------------------

/// What the worker thread owns.
pub struct IndexCore {
    pub index: SessionIndex,
    pub registry: LiveSessionRegistry,
}

type Job = Box<dyn FnOnce(&mut IndexCore) + Send>;

/// The thread that owns an [`IndexCore`]. It ends when the last handle is dropped.
#[derive(Clone)]
pub struct IndexWorker {
    tx: std::sync::mpsc::Sender<Job>,
}

impl IndexWorker {
    pub fn spawn(mut core: IndexCore) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                job(&mut core);
            }
        });
        Self { tx }
    }

    /// Queues `job` and returns without waiting: jobs run in the order they are
    /// posted.
    pub fn post(&self, job: impl FnOnce(&mut IndexCore) + Send + 'static) {
        let _ = self.tx.send(Box::new(job));
    }

    /// Runs `job` on the worker and waits for its result. The wait is a blocking
    /// task, so a paused test clock does not run ahead while the job reads files.
    pub async fn run<R: Send + 'static>(
        &self,
        job: impl FnOnce(&mut IndexCore) -> R + Send + 'static,
    ) -> Result<R, Error> {
        let (reply, result) = std::sync::mpsc::channel();
        self.post(move |core| {
            let _ = reply.send(job(core));
        });
        tokio::task::spawn_blocking(move || result.recv())
            .await
            .map_err(|_| internal_error("The session index has stopped"))?
            .map_err(|_| internal_error("The session index has stopped"))
    }
}

// ---- retitle -----------------------------------------------------------------

/// How a retitle reaches the CLI of a session that runs here.
pub struct LiveTitle {
    pub cwd: String,
    /// `rename_session` through the session's CLI.
    pub rename: RenameFn,
    /// The title last stored through the CLI, read once the session's earlier
    /// title changes are done.
    pub stored: Box<dyn FnOnce() -> Option<String> + Send>,
    /// Records a title the CLI took, before the next change starts.
    pub remember: Box<dyn FnOnce(String) + Send>,
    /// The title last shown for a session that has neither a stored title nor a
    /// transcript.
    pub shown: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidecarMode {
    /// Writes the CLI title sidecar next to every titled copy.
    Always,
    /// Rewrites only one that is there already.
    Existing,
}

#[derive(Clone, Copy, Debug)]
pub struct RetitleOptions {
    /// A session without a transcript is not unknown: a new session this
    /// connection runs, or a live one being renamed.
    pub may_be_unwritten: bool,
    pub sidecar: SidecarMode,
}

/// Appends the title records `change` gives each copy, then its sidecar. A copy
/// already in the requested state gets nothing, but its sidecar is brought in line
/// with it. Returns the title the first titled copy got.
fn title_copies(
    session_id: &str,
    transcripts: &[PathBuf],
    change: &TitleChange,
    sidecar: SidecarMode,
) -> std::io::Result<Option<String>> {
    let mut first = None;
    for transcript in transcripts {
        let current = copy_title(transcript, session_id)?;
        let Some(title) = change.apply(current.as_deref(), session_id) else {
            align_sidecar(transcript, current.as_deref(), session_id)?;
            continue;
        };
        ensure_title_records(transcript, session_id, &title)?;
        if first.is_none() {
            first = Some(title.clone());
        }
        if sidecar == SidecarMode::Always || exists(&sidecar_path(transcript)) {
            write_custom_title_sidecar(transcript, &title)?;
        }
    }
    Ok(first)
}

struct Prepared {
    cli: Option<PathBuf>,
    others: Vec<PathBuf>,
    current: Option<String>,
    stored: Option<String>,
}

fn io_error(error: &std::io::Error) -> Error {
    internal_error(&error.to_string())
}

// ---- the service -------------------------------------------------------------

type OwnLookup = Arc<dyn Fn(&str) -> Option<OwnSessionState> + Send + Sync>;
type LogError = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// What the service takes from its connection.
#[derive(Clone)]
pub struct ServiceDeps {
    /// What this connection knows of a session it runs.
    pub own_session_state: Option<OwnLookup>,
    /// Fires when a session this connection runs changed (the list subscriptions
    /// send its row at once).
    pub own_changes: Option<watch::Receiver<HashMap<String, OwnSessionState>>>,
    /// Sends `_session/list/changes`.
    pub notify_list_changes: Option<ListChangesNotifier>,
    /// The timing of the list subscriptions (tests shorten it).
    pub subscription_timing: ListSubscriptionTiming,
    pub log_error: LogError,
    /// Epoch milliseconds.
    pub now: Arc<dyn Fn() -> f64 + Send + Sync>,
    /// How long the `rename_session` of a live session may take (tests shorten
    /// it). Upstream waits for the CLI's reply without a limit.
    pub live_mutation_timeout: Duration,
}

impl ServiceDeps {
    pub fn new(log_error: LogError) -> Self {
        Self {
            own_session_state: None,
            own_changes: None,
            notify_list_changes: None,
            subscription_timing: ListSubscriptionTiming::default(),
            log_error,
            now: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
            }),
            live_mutation_timeout: LIVE_MUTATION_TIMEOUT,
        }
    }
}

#[derive(Clone)]
pub struct SessionIndexService {
    worker: IndexWorker,
    deps: ServiceDeps,
    /// One mutation at a time per session: a rename, archive, unarchive or delete
    /// of a session waits for the previous one of that session, and for none of
    /// another (upstream `exclusive`).
    gates: MutationGates,
    /// The forks whose rows wait for their fork response. Here and not in the
    /// subscriptions, which start with the first subscribe: a subscribe during a
    /// fork holds that fork back too.
    fork_holds: SharedForkHolds,
    /// Started by the first subscribe.
    subscriptions: Arc<OnceLock<ListSubscriptions>>,
    /// Set for good by [`dispose`](Self::dispose): no subscription is made after it.
    disposed: Arc<AtomicBool>,
    projects_root: PathBuf,
    registry_dir: PathBuf,
}

/// How long the `rename_session` reply of a live session's CLI may take. The CLI
/// is a local child and answers in milliseconds; a wedged one would hold that
/// session's mutations for the rest of the connection (upstream waits without a
/// limit, which one stuck CLI can do to itself only).
const LIVE_MUTATION_TIMEOUT: Duration = Duration::from_secs(30);

/// The mutations in flight per session (upstream `exclusive`, service.ts:518-527):
/// each session has its own gate, so a rename of session A that waits for a
/// wedged CLI never holds up the rename of session B. A gate is dropped once no
/// mutation holds or waits for it, so the map holds only busy sessions.
#[derive(Clone)]
struct MutationGates {
    gates: Arc<watch::Sender<HashMap<String, Arc<Semaphore>>>>,
}

impl MutationGates {
    fn new() -> Self {
        Self {
            gates: Arc::new(watch::channel(HashMap::new()).0),
        }
    }

    /// The gate of `session_id`, created on its first mutation. Two mutations of
    /// one session get the same gate and run in call order (tokio's semaphore is
    /// FIFO), as upstream chains them.
    fn gate(&self, session_id: &str) -> Arc<Semaphore> {
        let mut gate = None;
        self.gates.send_modify(|gates| {
            gate = Some(
                gates
                    .entry(session_id.to_string())
                    .or_insert_with(|| Arc::new(Semaphore::new(1)))
                    .clone(),
            );
        });
        gate.unwrap_or_else(|| Arc::new(Semaphore::new(1)))
    }

    /// Drops the gate of `session_id` when no mutation holds or waits for it.
    fn release(&self, session_id: &str) {
        self.gates.send_modify(|gates| {
            let idle = gates
                .get(session_id)
                .is_some_and(|gate| Arc::strong_count(gate) == 1 && gate.available_permits() == 1);
            if idle {
                gates.remove(session_id);
            }
        });
    }

    /// Waits for the session's previous mutation to end and holds its gate until
    /// the returned guard is dropped — also when the mutation is cancelled.
    async fn hold(&self, session_id: &str) -> Result<GateHold, Error> {
        let gate = self.gate(session_id);
        let permit = gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| internal_error("The session index has stopped"))?;
        Ok(GateHold {
            gates: self.clone(),
            session_id: session_id.to_string(),
            permit: Some(permit),
            gate: Some(gate),
        })
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.gates.borrow().len()
    }
}

/// A mutation's hold on its session's gate: dropping it (or being cancelled)
/// releases the gate, and drops the session's entry when nobody else holds or
/// waits for it.
struct GateHold {
    gates: MutationGates,
    session_id: String,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    gate: Option<Arc<Semaphore>>,
}

impl Drop for GateHold {
    fn drop(&mut self) {
        drop(self.permit.take());
        drop(self.gate.take());
        self.gates.release(&self.session_id);
    }
}

impl SessionIndexService {
    /// A service over the projects root `root` and the live registry `registry_dir`.
    pub fn new(root: PathBuf, registry_dir: PathBuf, deps: ServiceDeps) -> Self {
        Self::with_parts(
            SessionIndex::new(root.clone()),
            LiveSessionRegistry::new(LiveRegistryDeps::system(registry_dir.clone())),
            root,
            registry_dir,
            deps,
        )
    }

    pub fn with_parts(
        index: SessionIndex,
        registry: LiveSessionRegistry,
        projects_root: PathBuf,
        registry_dir: PathBuf,
        deps: ServiceDeps,
    ) -> Self {
        Self {
            worker: IndexWorker::spawn(IndexCore { index, registry }),
            deps,
            gates: MutationGates::new(),
            fork_holds: SharedForkHolds::default(),
            subscriptions: Arc::new(OnceLock::new()),
            disposed: Arc::new(AtomicBool::new(false)),
            projects_root,
            registry_dir,
        }
    }

    pub fn worker(&self) -> &IndexWorker {
        &self.worker
    }

    pub fn deps(&self) -> &ServiceDeps {
        &self.deps
    }

    /// `session/list` of a `sessionIndex` client. `own` reports the sessions that
    /// this connection runs.
    pub async fn list(
        &self,
        params: &Value,
        own: &(dyn Fn(&str) -> Option<OwnSessionState> + Sync),
    ) -> Result<Value, Error> {
        let ListOptions {
            limit,
            archived,
            include_worktrees,
        } = parse_list_options(params.get("_meta"))?;
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .map(str::to_string);
        let scope = ListScope {
            cwd: cwd.clone(),
            archived,
            include_worktrees,
        };
        let after = match params.get("cursor") {
            None | Some(Value::Null) => None,
            Some(Value::String(cursor)) => Some(decode_list_cursor(cursor, &scope)?),
            // A cursor that is no string cannot be one this adapter issued.
            Some(other) => {
                return Err(invalid_params(
                    None,
                    &format!("Unknown session/list cursor: {other}"),
                ))
            }
        };
        let query = ListQuery {
            cwd,
            include_worktrees,
            limit,
            archived,
            after,
        };
        let (rows, has_more, live) = self
            .worker
            .run(move |core| {
                // The live registry is read while the transcripts are.
                let live = core.registry.snapshot();
                let (rows, has_more) = core.index.list(&query);
                (rows, has_more, live)
            })
            .await?;
        let now = (self.deps.now)();
        let sessions: Vec<Value> = rows
            .iter()
            // A fork whose fork response is not out yet is left out, as in the
            // list subscriptions.
            .filter(|row| !self.fork_holds.is_held(&row.session_id))
            .map(|row| {
                session_info_of(
                    row,
                    own(&row.session_id).as_ref(),
                    live.get(&row.session_id),
                    now,
                )
            })
            .collect();
        let mut response = json!({ "sessions": sessions });
        if let (true, Some(last)) = (has_more, rows.last()) {
            response["nextCursor"] = json!(encode_list_cursor(
                &ListCursor {
                    order_at_ms: last.order_at_ms,
                    session_id: last.session_id.clone(),
                },
                &scope
            ));
        }
        Ok(response)
    }

    /// Retitles every transcript of a session with `change`: rename, archive and
    /// unarchive. Returns the title stored through the CLI that runs here, else
    /// the title of the first copy that got one; `None` when nothing was stored.
    ///
    /// - The CLI runs here (`live`): it titles its own transcript and sidecar
    ///   (`rename_session`), the only writer that cannot be overtaken by the title
    ///   the CLI holds. The other copies get the records and the sidecar here, as
    ///   a best effort once the CLI has the title: a failure is logged, not
    ///   returned.
    /// - Otherwise every transcript gets the records (and the sidecar, see
    ///   [`SidecarMode`]).
    ///
    /// As the CLI's `/rename`, the records are written whether or not another
    /// process has the session open. A session without a transcript is unknown
    /// (`-32002`), unless [`RetitleOptions::may_be_unwritten`].
    pub async fn retitle(
        &self,
        session_id: &str,
        change: TitleChange,
        options: RetitleOptions,
        live: Option<LiveTitle>,
    ) -> Result<Option<String>, Error> {
        let _hold = self.gates.hold(session_id).await?;
        let result = self.retitle_locked(session_id, change, options, live).await;
        // Whatever was written, also by a change that failed part way.
        self.session_written(session_id);
        result
    }

    /// `rename_session` through the CLI of a live session, bounded: a CLI that
    /// does not answer fails that session's mutation instead of holding it (and,
    /// before the per-session gates, every other session's) forever.
    ///
    /// The bound covers the `rename_session` request only. A rename or archive of
    /// a loaded session first waits behind a `generate_session_title` still in
    /// flight (the session actor parks the change), and that request has no limit:
    /// a CLI that never answers it holds the mutation. Upstream behaves the same
    /// (accepted parity, see `porting/parity/lane-C-notes.md`).
    async fn rename_through_cli(
        &self,
        rename: RenameFn,
        title: String,
        session_id: &str,
    ) -> Result<(), Error> {
        match tokio::time::timeout(self.deps.live_mutation_timeout, rename(title)).await {
            Ok(result) => result,
            Err(_) => Err(internal_error(&format!(
                "The CLI of {session_id} did not answer rename_session in time"
            ))),
        }
    }

    async fn retitle_locked(
        &self,
        session_id: &str,
        change: TitleChange,
        options: RetitleOptions,
        live: Option<LiveTitle>,
    ) -> Result<Option<String>, Error> {
        if !is_session_id(session_id) {
            let Some(live) = live.filter(|_| options.may_be_unwritten) else {
                return Err(session_not_found(session_id));
            };
            let current = (live.stored)().or(live.shown.clone());
            let title = change.apply(current.as_deref(), session_id);
            if let Some(title) = &title {
                self.rename_through_cli(live.rename, title.clone(), session_id)
                    .await?;
                (live.remember)(title.clone());
            }
            return Ok(title);
        }
        let id = session_id.to_string();
        let LiveTitleParts {
            cwd,
            rename,
            remember,
            stored_before,
            shown,
        } = match live {
            Some(live) => {
                let stored_before = (live.stored)();
                LiveTitleParts {
                    cwd: Some(live.cwd),
                    rename: Some(live.rename),
                    remember: Some(live.remember),
                    stored_before,
                    shown: live.shown,
                }
            }
            None => LiveTitleParts::default(),
        };
        let prepare_id = id.clone();
        let prepare_change = change.clone();
        let prepared = self
            .worker
            .run(
                move |core| -> Result<Result<Prepared, Vec<PathBuf>>, Error> {
                    let transcripts = core.index.find_transcripts(&prepare_id, false);
                    if transcripts.is_empty() && !options.may_be_unwritten {
                        return Err(session_not_found(&prepare_id));
                    }
                    // The cache is left as it is: the appended records change the
                    // size, and the next read keeps what the earlier scans found (a
                    // last prompt beyond the tail search).
                    let Some(cwd) = cwd else {
                        return Ok(Err(transcripts));
                    };
                    let platform = core.index.platform();
                    let canonical = canonical_path(&cwd, platform);
                    let mut paths = vec![cwd.clone()];
                    if canonical != cwd {
                        paths.push(canonical.clone());
                    }
                    let own: Vec<&PathBuf> = transcripts
                        .iter()
                        .filter(|t| is_transcript_of(t, &paths, platform))
                        .collect();
                    // The CLI writes the transcript of the resolved cwd.
                    let cli = own
                        .iter()
                        .find(|t| {
                            let dir_name = t
                                .parent()
                                .and_then(Path::file_name)
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            is_exact_project_dir(&dir_name, &canonical, platform)
                        })
                        .or_else(|| own.first())
                        .map(|t| (*t).clone());
                    let others: Vec<PathBuf> = transcripts
                        .iter()
                        .filter(|t| Some(*t) != cli.as_ref())
                        .cloned()
                        .collect();
                    // The transcript holds the current title, also one changed
                    // otherwise (a `/rename`); without one, the title last given to
                    // the CLI.
                    let primary = cli.clone().or_else(|| others.first().cloned());
                    let read = match &primary {
                        Some(primary) => {
                            copy_title(primary, &prepare_id).map_err(|e| io_error(&e))?
                        }
                        None => None,
                    };
                    let current = read.or(stored_before).or(shown);
                    let stored = prepare_change.apply(current.as_deref(), &prepare_id);
                    Ok(Ok(Prepared {
                        cli,
                        others,
                        current,
                        stored,
                    }))
                },
            )
            .await??;
        let prepared = match prepared {
            Err(transcripts) => {
                // Not a live session: every transcript gets the records.
                let sidecar = options.sidecar;
                return self
                    .worker
                    .run(move |_| title_copies(&id, &transcripts, &change, sidecar))
                    .await?
                    .map_err(|e| io_error(&e));
            }
            Ok(prepared) => prepared,
        };
        let (Some(rename), Some(remember)) = (rename, remember) else {
            return Ok(None);
        };
        let Prepared {
            cli,
            others,
            current,
            stored,
        } = prepared;
        if let Some(title) = &stored {
            self.rename_through_cli(rename, title.clone(), &id).await?;
            remember(title.clone());
        }
        let log = self.deps.log_error.clone();
        let sidecar = options.sidecar;
        let stored_for_job = stored.clone();
        let job_id = id.clone();
        self.worker
            .run(move |_| {
                // The CLI writes the custom title before it answers, the agent name
                // only later. An older agent name decides the archive state first,
                // so the copy gets the new one now. A best effort: the CLI took the
                // title, and writes the agent name itself too.
                match (&stored_for_job, &cli) {
                    (Some(title), Some(cli)) => {
                        if let Err(e) = ensure_agent_name(cli, &job_id, title) {
                            log(
                                &format!("writing the agent name of {job_id} failed"),
                                &e.to_string(),
                            );
                        }
                    }
                    (None, Some(cli)) => {
                        let _ = align_sidecar(cli, current.as_deref(), &job_id);
                    }
                    _ => {}
                }
                if let Err(e) = title_copies(&job_id, &others, &change, sidecar) {
                    log(
                        &format!("titling the other transcripts of {job_id} failed"),
                        &e.to_string(),
                    );
                }
            })
            .await?;
        Ok(stored)
    }

    /// Deletes every transcript of the session, whoever else has it open, as the
    /// CLI does. `known`: the session was loaded here (and is torn down already),
    /// so a missing transcript is no error.
    ///
    /// The SDK deletes the first non-empty transcript it finds and its
    /// `<sessionId>/` directory, one copy per call; empty transcripts, which it
    /// skips, are removed here.
    pub async fn delete(&self, session_id: &str, known: bool) -> Result<(), Error> {
        if !is_session_id(session_id) {
            return Err(session_not_found(session_id));
        }
        let gate = self.gates.hold(session_id).await?;
        let id = session_id.to_string();
        let result = self
            .worker
            .run(move |core| delete_session(core, &id, known))
            .await;
        drop(gate);
        self.session_written(session_id);
        result?
    }

    /// `session/delete` of an AIR client without `sessionIndex`, which uses delete
    /// to mark a session done: archives it instead, so the transcript survives. A
    /// session without a transcript fails as the SDK delete did.
    pub async fn archive_instead_of_delete(&self, session_id: &str) -> Result<(), Error> {
        if !is_session_id(session_id) {
            return Err(sdk_delete_error(session_id));
        }
        // The SDK delete skips empty transcripts, so they do not count.
        let id = session_id.to_string();
        let found = self
            .worker
            .run(move |core| core.index.find_transcripts(&id, false))
            .await?;
        if found.is_empty() {
            return Err(sdk_delete_error(session_id));
        }
        self.retitle(
            session_id,
            TitleChange::Archive(true),
            RetitleOptions {
                may_be_unwritten: false,
                sidecar: SidecarMode::Existing,
            },
            None,
        )
        .await?;
        Ok(())
    }

    /// The transcripts of the session were written here (a rename, archive,
    /// unarchive or delete): the list subscriptions read its row again at once.
    fn session_written(&self, session_id: &str) {
        if let Some(subscriptions) = self.subscriptions.get() {
            subscriptions.session_written(session_id);
        }
    }

    /// A `session/fork` of `source_id` started (see [`ForkHolds`]).
    pub fn fork_started(&self, source_id: &str) {
        self.fork_holds.started(source_id);
    }

    /// The `session/fork` of `source_id` copied the transcript into `fork_id`.
    pub fn fork_created(&self, source_id: &str, fork_id: &str) {
        self.fork_holds.created(source_id, fork_id);
    }

    /// The response of a `session/fork` of `source_id` is out: the list
    /// Diagnostics of the list subscriptions (upstream `handles()`): one shared
    /// watch per scope, the subscriptions, and their scheduled timers. `None`
    /// until a subscription is created.
    pub async fn subscription_handles(&self) -> Option<Value> {
        match self.subscriptions.get() {
            Some(subscriptions) => Some(subscriptions.handles().await),
            None => None,
        }
    }

    /// subscriptions send the rows they held back.
    pub fn fork_answered(&self, source_id: &str) {
        if self.fork_holds.answered(source_id) {
            if let Some(subscriptions) = self.subscriptions.get() {
                subscriptions.deliver_pending();
            }
        }
    }

    /// `_session/list/subscribe`: pushes the changed rows of `cwd` (an absolute
    /// path) until [`unsubscribe_list`](Self::unsubscribe_list) or
    /// [`dispose`](Self::dispose).
    pub async fn subscribe_list(&self, cwd: &str) -> Result<Value, Error> {
        if self.disposed.load(Ordering::SeqCst) {
            return Err(internal_error("The connection is closed"));
        }
        let subscriptions = self.subscriptions.get_or_init(|| {
            let deps = &self.deps;
            let own = deps.own_session_state.clone();
            ListSubscriptions::spawn(ListSubscriptionDeps {
                worker: self.worker.clone(),
                fork_holds: self.fork_holds.clone(),
                projects_root: self.projects_root.clone(),
                registry_dir: self.registry_dir.clone(),
                own: Arc::new(move |id| own.as_ref().and_then(|own| own(id))),
                own_changes: deps.own_changes.clone(),
                notify: deps.notify_list_changes.clone().unwrap_or_else(|| {
                    Arc::new(|_| {
                        Box::pin(async { Ok::<(), Error>(()) }) as BoxFuture<Result<(), Error>>
                    })
                }),
                log_error: deps.log_error.clone(),
                timing: deps.subscription_timing,
            })
        });
        subscriptions.subscribe(cwd).await
    }

    /// `_session/list/unsubscribe`. Idempotent.
    pub fn unsubscribe_list(&self, subscription_id: &str) {
        if let Some(subscriptions) = self.subscriptions.get() {
            subscriptions.unsubscribe(subscription_id);
        }
    }

    /// A session this connection runs changed what its row shows.
    pub fn own_session_changed(&self, session_id: &str) {
        if let Some(subscriptions) = self.subscriptions.get() {
            subscriptions.own_session_changed(session_id);
        }
    }

    pub fn dispose(&self) {
        self.disposed.store(true, Ordering::SeqCst);
        if let Some(subscriptions) = self.subscriptions.get() {
            subscriptions.dispose();
        }
    }
}

#[derive(Default)]
struct LiveTitleParts {
    cwd: Option<String>,
    rename: Option<RenameFn>,
    remember: Option<Box<dyn FnOnce(String) + Send>>,
    stored_before: Option<String>,
    shown: Option<String>,
}

/// The error the SDK `deleteSession` gives for a session id it rejects or does not
/// find, which a client without `sessionIndex` got before.
fn sdk_delete_error(session_id: &str) -> Error {
    internal_error(&if is_session_id(session_id) {
        format!("Session {session_id} not found in any project directory")
    } else {
        format!("Invalid sessionId: {session_id}")
    })
}

/// The text Node's `fs.rm`/`fs.unlink` failure carries: libuv's
/// `<CODE>: <strerror>, <syscall> '<path>'` (`EACCES: permission denied,
/// unlink '/…'`). The delete rethrows every SDK delete failure that is not
/// "not found with the copy gone" (`session-index/service.ts:672-680`), so the
/// client gets this filesystem text, not the "was not deleted" summary.
fn node_fs_error(error: &std::io::Error, syscall: &str, path: &Path) -> String {
    let (code, text) = match error.kind() {
        std::io::ErrorKind::PermissionDenied => ("EACCES", "permission denied"),
        std::io::ErrorKind::NotFound => ("ENOENT", "no such file or directory"),
        std::io::ErrorKind::AlreadyExists => ("EEXIST", "file already exists"),
        std::io::ErrorKind::InvalidInput => ("EINVAL", "invalid argument"),
        std::io::ErrorKind::WouldBlock => ("EAGAIN", "resource temporarily unavailable"),
        std::io::ErrorKind::TimedOut => ("ETIMEDOUT", "connection timed out"),
        std::io::ErrorKind::BrokenPipe => ("EPIPE", "broken pipe"),
        std::io::ErrorKind::ConnectionRefused => ("ECONNREFUSED", "connection refused"),
        std::io::ErrorKind::Interrupted => ("EINTR", "interrupted system call"),
        _ => ("EIO", "i/o error"),
    };
    format!("{code}: {text}, {syscall} '{}'", path.display())
}

/// Removes one transcript copy as the SDK delete does. A copy removed meanwhile
/// (`NotFound`) is not a failure (`isSdkNotFound(error) && !(await exists(...))`);
/// any other failure is the filesystem error Node rethrows.
fn remove_transcript_copy(file_path: &Path) -> Result<(), Error> {
    match std::fs::remove_file(file_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(internal_error(&node_fs_error(&error, "unlink", file_path))),
    }
}

fn delete_session(core: &mut IndexCore, session_id: &str, known: bool) -> Result<(), Error> {
    // A session directory can outlive its transcript: a delete that removed the
    // transcript and then failed on the directory.
    let (transcripts, session_dirs) = core.index.scan_session(session_id);
    let all: Vec<PathBuf> = transcripts.iter().map(|(p, _)| p.clone()).collect();
    if all.is_empty() && session_dirs.is_empty() {
        // Unknown, unless it was loaded here.
        return if known {
            Ok(())
        } else {
            Err(session_not_found(session_id))
        };
    }
    let outcome = (|| -> Result<(), Error> {
        // Node's `deleteSession` rethrows the SDK delete's failure before the
        // next copy (`session-index/service.ts:674-680`): the copies after the
        // failing one, the empty transcripts and the session directories stay on
        // disk, and a retry finishes once the obstacle is gone.
        for (file_path, _) in transcripts.iter().filter(|(_, size)| *size > 0) {
            remove_transcript_copy(file_path)?;
        }
        // Empty transcripts, which the SDK skips, and session directories without
        // a transcript.
        for (file_path, size) in &transcripts {
            if *size == 0 {
                remove_transcript_copy(file_path)?;
            }
        }
        for session_dir in &session_dirs {
            if exists(&PathBuf::from(format!("{}.jsonl", session_dir.display()))) {
                continue;
            }
            let _ = std::fs::remove_dir_all(session_dir);
        }
        let (left_transcripts, left_dirs) = core.index.scan_session(session_id);
        let remaining: Vec<String> = left_transcripts
            .iter()
            .map(|(p, _)| p.display().to_string())
            .chain(left_dirs.iter().map(|p| p.display().to_string()))
            .collect();
        if !remaining.is_empty() {
            // A copy that vanished and came back leaves no `io::Error` to
            // rethrow: Node's "was not deleted" summary is the fallback.
            return Err(internal_error(&format!(
                "Session {session_id} was not deleted: {} remain",
                remaining.join(", ")
            )));
        }
        Ok(())
    })();
    core.index.invalidate(&all);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

    fn meta(list: Value) -> Value {
        json!({"jetbrains": {"air": {"version": 1, "list": list}}})
    }

    fn code(error: &Error) -> i32 {
        i32::from(error.code)
    }

    /// mutations > a session's gate is dropped once no mutation holds or waits for it
    #[tokio::test]
    async fn a_gate_is_dropped_once_its_session_is_idle() {
        let gates = MutationGates::new();
        let gate = gates.gate("a");
        let permit = gate.clone().acquire_owned().await.expect("the permit");
        assert_eq!(gates.len(), 1, "a busy session holds a gate");
        drop(permit);
        drop(gate);
        gates.release("a");
        assert_eq!(gates.len(), 0, "an idle session's gate is dropped");

        // A waiter keeps the gate: the next mutation of the session must queue
        // behind the running one, not start beside it.
        let gate = gates.gate("b");
        let permit = gate.clone().acquire_owned().await.expect("the permit");
        let waiter = gates.gate("b");
        assert!(Arc::ptr_eq(&gate, &waiter), "one gate per session");
        drop(permit);
        drop(gate);
        gates.release("b");
        assert_eq!(gates.len(), 1, "the waiting mutation keeps the gate");
        drop(waiter);
        gates.release("b");
        assert_eq!(gates.len(), 0);

        // A hold releases the gate when it is dropped (also when the mutation is
        // cancelled before it ends).
        let hold = gates.hold("c").await.expect("held");
        assert_eq!(gates.len(), 1);
        drop(hold);
        assert_eq!(gates.len(), 0, "the hold released and dropped the gate");
    }

    /// list request parsing > defaults and clamps the limit
    #[test]
    fn defaults_and_clamps_the_limit() {
        let defaults = parse_list_options(None).expect("defaults");
        assert_eq!(
            defaults,
            ListOptions {
                limit: 50,
                archived: ArchivedFilter::Unarchived,
                include_worktrees: false
            }
        );
        let limit =
            |v: Value| parse_list_options(Some(&meta(json!({"limit": v})))).map(|o| o.limit);
        assert_eq!(limit(Value::Null).ok(), Some(50));
        assert_eq!(limit(json!(1)).ok(), Some(1));
        assert_eq!(limit(json!(1000)).ok(), Some(200));
        for bad in [
            json!("1"),
            json!(1.5),
            json!(0),
            json!(-3),
            json!(true),
            json!({}),
        ] {
            let error = limit(bad).expect_err("invalid limit");
            assert_eq!(code(&error), -32602);
        }
    }

    /// list request parsing > takes includeWorktrees as a boolean and archived as a filter value, null as the default
    #[test]
    fn takes_include_worktrees_and_archived() {
        let parse = |list: Value| parse_list_options(Some(&meta(list)));
        assert!(
            parse(json!({"includeWorktrees": true}))
                .expect("ok")
                .include_worktrees
        );
        assert!(
            !parse(json!({"includeWorktrees": null}))
                .expect("ok")
                .include_worktrees
        );
        assert_eq!(
            code(&parse(json!({"includeWorktrees": "yes"})).expect_err("bad")),
            -32602
        );
        for (text, filter) in [
            ("unarchived", ArchivedFilter::Unarchived),
            ("archived", ArchivedFilter::Archived),
            ("all", ArchivedFilter::All),
        ] {
            assert_eq!(
                parse(json!({"archived": text})).expect("ok").archived,
                filter
            );
        }
        assert_eq!(
            parse(json!({"archived": null})).expect("ok").archived,
            ArchivedFilter::Unarchived
        );
        assert_eq!(
            parse(json!({})).expect("ok").archived,
            ArchivedFilter::Unarchived
        );
        for bad in [
            json!(true),
            json!(false),
            json!("only"),
            json!("ALL"),
            json!("Archived"),
            json!(""),
            json!(1),
            json!("true"),
            json!({}),
            json!([]),
        ] {
            assert_eq!(
                code(&parse(json!({"archived": bad})).expect_err("bad")),
                -32602
            );
        }
    }

    /// list request parsing > round-trips a cursor and rejects one of another scope
    #[test]
    fn round_trips_a_cursor_and_rejects_one_of_another_scope() {
        let scope = ListScope {
            cwd: Some("/repo".into()),
            archived: ArchivedFilter::Unarchived,
            include_worktrees: false,
        };
        let cursor = ListCursor {
            order_at_ms: 5.0,
            session_id: SESSION.into(),
        };
        let encoded = encode_list_cursor(&cursor, &scope);
        assert_eq!(
            decode_list_cursor(&encoded, &scope).expect("round trip"),
            cursor
        );
        for archived in [ArchivedFilter::Archived, ArchivedFilter::All] {
            let other_scope = ListScope {
                archived,
                ..scope.clone()
            };
            assert_eq!(
                code(&decode_list_cursor(&encoded, &other_scope).expect_err("scope")),
                -32602
            );
            let other = encode_list_cursor(&cursor, &other_scope);
            assert_eq!(
                decode_list_cursor(&other, &other_scope).expect("ok"),
                cursor
            );
            assert_eq!(
                code(&decode_list_cursor(&other, &scope).expect_err("scope")),
                -32602
            );
        }
        let worktrees = ListScope {
            include_worktrees: true,
            ..scope.clone()
        };
        assert_eq!(
            code(&decode_list_cursor(&encoded, &worktrees).expect_err("scope")),
            -32602
        );
        let elsewhere = ListScope {
            cwd: Some("/other".into()),
            ..scope.clone()
        };
        assert_eq!(
            code(&decode_list_cursor(&encoded, &elsewhere).expect_err("scope")),
            -32602
        );
        assert_eq!(
            code(&decode_list_cursor("offset:1000", &scope).expect_err("foreign")),
            -32602
        );
        // Cursors of earlier format versions.
        for v in [3, 4] {
            let legacy = base64url_encode(
                json!({"v": v, "u": 5, "id": SESSION, "cwd": "/repo", "archived": false, "worktrees": false})
                    .to_string()
                    .as_bytes(),
            );
            assert_eq!(
                code(&decode_list_cursor(&legacy, &scope).expect_err("old")),
                -32602
            );
        }
        // The payload is the bytes upstream writes.
        let payload = String::from_utf8(base64url_decode(&encoded)).expect("utf8");
        assert_eq!(
            payload,
            format!("{{\"v\":5,\"u\":5,\"id\":\"{SESSION}\",\"cwd\":\"/repo\",\"archived\":\"unarchived\",\"worktrees\":false}}")
        );
    }

    /// list request parsing > validates and truncates a rename title
    #[test]
    fn validates_and_truncates_a_rename_title() {
        for params in [
            json!({"sessionId": SESSION, "title": "  "}),
            json!({"sessionId": SESSION}),
        ] {
            assert_eq!(
                code(&parse_rename_session_request(&params).expect_err("bad")),
                -32602
            );
        }
        let ok =
            parse_rename_session_request(&json!({"sessionId": SESSION, "title": "x".repeat(300)}))
                .expect("ok");
        assert_eq!(super::super::js_text::utf16_len(&ok.title), 256);
        let error = parse_session_id_request(&json!({"sessionId": " "})).expect_err("blank");
        assert_eq!(code(&error), -32602);
        assert_eq!(
            error.message,
            "Invalid params: params require a non-empty sessionId"
        );
        assert_eq!(
            parse_session_id_request(&json!({"sessionId": "  abc "}))
                .expect("ok")
                .session_id,
            "abc"
        );
    }

    #[test]
    fn base64url_matches_nodes_encoding() {
        assert_eq!(base64url_encode(b"ab"), "YWI");
        assert_eq!(base64url_encode(b"abc"), "YWJj");
        assert_eq!(base64url_encode(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64url_decode("-_8"), vec![0xfb, 0xff]);
        assert_eq!(base64url_decode("YWI="), b"ab".to_vec());
    }

    #[test]
    fn a_title_change_follows_the_archive_state() {
        let rename = TitleChange::Rename("New".into());
        assert_eq!(rename.apply(Some("Old"), SESSION).as_deref(), Some("New"));
        assert_eq!(
            rename.apply(Some("[archived] Old"), SESSION).as_deref(),
            Some("[archived] New")
        );
        let archive = TitleChange::Archive(true);
        assert_eq!(
            archive.apply(Some("Old"), SESSION).as_deref(),
            Some("[archived] Old")
        );
        assert_eq!(archive.apply(Some("[archived] Old"), SESSION), None);
        assert_eq!(
            archive.apply(None, SESSION).as_deref(),
            Some("[archived] Session aaaaaaaa")
        );
        let unarchive = TitleChange::Archive(false);
        assert_eq!(
            unarchive.apply(Some("[archived] Old"), SESSION).as_deref(),
            Some("Old")
        );
        assert_eq!(unarchive.apply(Some("Old"), SESSION), None);
    }
}
