//! Port of `file-change-audit.ts` (Q-13, AG-3): the per-turn native file-change
//! report an AIR client negotiates with `agentFileChangeReport`.
//!
//! A prompt may carry `_meta.jetbrains.air.agentFileChangeReportRequest`
//! (`{version: 1, requestId}`). The session then previews Claude's checkpoint of
//! the turn (`rewindFiles(userMessageId, {dryRun: true})`, bounded by
//! [`REPORT_TIMEOUT`]), normalises the paths against the workspace roots, and
//! publishes one `session_info_update` whose `_meta.jetbrains.air` carries the
//! `agentFileChangeReport`. Every terminal outcome is published at most once per
//! request: a report (`reported`) or an `unavailable` reason (`cancelled`,
//! `timeout`, `invalidOutput`, `notReported`, `providerError`).
//!
//! The pure half (request parsing, path normalisation, fitting, subagent edits)
//! is separate from [`Reporter`], the stateful recorder the session actor owns.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::permission_options::resolve_lexical;

/// The AIR capability that negotiates per-turn file-change reports
/// (`AGENT_FILE_CHANGE_REPORT_CAPABILITY`, `file-change-audit.ts:6`).
pub const AGENT_FILE_CHANGE_REPORT_CAPABILITY: &str = "agentFileChangeReport";

/// The most paths one report carries (`MAX_REPORTED_PATHS`).
pub const MAX_REPORTED_PATHS: usize = 1024;
/// The longest reported path, in UTF-16 units (`MAX_REPORTED_PATH_LENGTH`).
pub const MAX_REPORTED_PATH_LENGTH: usize = 4096;
/// The serialized report's wire limit (`AGENT_FILE_CHANGE_REPORT_MAX_BYTES`).
pub const MAX_REPORT_BYTES: usize = 256 * 1024;
/// The checkpoint preview's bounded wait (`AGENT_FILE_CHANGE_REPORT_TIMEOUT_MS`).
pub const REPORT_TIMEOUT: Duration = Duration::from_millis(2_000);

/// The `_meta.jetbrains.air.agentFileChangeReportRequest.requestId` of a prompt,
/// or `None` when the request is absent or malformed (`file-change-audit.ts:72-89`):
/// an object with exactly the two keys `version` and `requestId`, `version === 1`,
/// and a request id matching `^[A-Za-z0-9._:-]{1,128}$`.
pub fn agent_file_change_report_request_id(meta: &Value) -> Option<String> {
    let request = air_extension_meta(meta)?.get("agentFileChangeReportRequest")?;
    let object = request.as_object()?;
    if object.len() != 2 || !object.contains_key("version") || !object.contains_key("requestId") {
        return None;
    }
    // `requestRecord.version !== 1`: the number 1 (also `1.0`, which JSON parses
    // to the same number), never a string or a boolean.
    if object.get("version").and_then(Value::as_f64) != Some(1.0) {
        return None;
    }
    let request_id = object.get("requestId").and_then(Value::as_str)?;
    is_request_id(request_id).then(|| request_id.to_string())
}

/// Whether a client negotiated `agentFileChangeReport`
/// (`supportsAgentFileChangeReport`, `file-change-audit.ts:91-93`): the AIR
/// extension version is at least 1 and the capability list names it.
pub fn supports_agent_file_change_report(air: Option<&crate::air::AirClient>) -> bool {
    air.is_some_and(|air| air.supports(AGENT_FILE_CHANGE_REPORT_CAPABILITY))
}

/// The `_meta` of the published report (`agentFileChangeReportMeta`): the AIR
/// extension object with `agentFileChangeReport` set to the result.
pub fn agent_file_change_report_meta(result: &Value) -> Value {
    crate::air::with_air_meta(None, AGENT_FILE_CHANGE_REPORT_CAPABILITY, result.clone())
}

/// `recordSubagentFileEdits` (`file-change-audit.ts:101-133`): add the raw
/// `file_path` (`notebook_path` for NotebookEdit) of each successful file-tool
/// edit among a subagent message's tool results. Root messages
/// (`parent_tool_use_id == None`) are skipped — the checkpoint already tracks
/// their files — and so are turns without a requested report or already settled.
///
/// `tool_uses` maps a tool-use id to `(name, input)` (the session's cache).
pub fn record_subagent_file_edits(
    parent_tool_use_id: Option<&str>,
    content: &Value,
    tool_uses: &std::collections::HashMap<String, (String, Value)>,
    edited_paths: &mut std::collections::BTreeSet<String>,
) {
    if parent_tool_use_id.is_none() {
        return;
    }
    let Some(blocks) = content.as_array() else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_result")
            || block.get("is_error") == Some(&Value::Bool(true))
        {
            continue;
        }
        let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
            continue;
        };
        let Some((name, input)) = tool_uses.get(tool_use_id) else {
            continue;
        };
        if !is_file_edit_tool(name) {
            continue;
        }
        let key = if name == "NotebookEdit" {
            "notebook_path"
        } else {
            "file_path"
        };
        if let Some(path) = input
            .get(key)
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
        {
            edited_paths.insert(path.to_string());
        }
    }
}

/// The file-tool edits a checkpoint does not track (`FILE_EDIT_TOOLS`).
pub fn is_file_edit_tool(name: &str) -> bool {
    matches!(name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
}

/// A normalised workspace (`normalizeWorkspace`, `file-change-audit.ts:250-266`):
/// the canonical `cwd` and the canonical additional roots, deduplicated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
}

impl Workspace {
    pub fn new(cwd: &Path, additional_directories: &[PathBuf]) -> Self {
        let cwd = canonicalize_workspace_root(&resolve_lexical(cwd, ""));
        let mut seen = HashSet::from([path_key(&cwd)]);
        let mut roots = Vec::new();
        for directory in additional_directories {
            let directory = directory.to_string_lossy();
            let normalized = canonicalize_workspace_root(&resolve_lexical(&cwd, &directory));
            let key = path_key(&normalized);
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            roots.push(normalized);
        }
        Self {
            cwd,
            additional_directories: roots,
        }
    }

    fn roots(&self) -> Vec<&Path> {
        std::iter::once(self.cwd.as_path())
            .chain(self.additional_directories.iter().map(PathBuf::as_path))
            .collect()
    }
}

/// `normalizeReportedPaths` (`file-change-audit.ts:268-320`): resolve each path
/// against `cwd`, canonicalise it, reject control characters, paths longer than
/// [`MAX_REPORTED_PATH_LENGTH`], paths outside the workspace roots and the roots
/// themselves, dedupe, and cap the list at [`MAX_REPORTED_PATHS`] entries /
/// [`MAX_REPORT_BYTES`] bytes. Anything dropped sets `truncated`.
pub fn normalize_reported_paths(
    reported_paths: &[String],
    workspace: &Workspace,
) -> (Vec<String>, bool) {
    let roots = workspace.roots();
    let mut seen: HashSet<String> = HashSet::new();
    let mut result: Vec<String> = Vec::new();
    let mut total_bytes = 0usize;
    let mut truncated = false;
    for reported in reported_paths {
        if reported.trim().is_empty()
            || reported.encode_utf16().count() > MAX_REPORTED_PATH_LENGTH
            || has_control_character(reported)
        {
            truncated = true;
            continue;
        }
        let resolved = resolve_lexical(&workspace.cwd, reported);
        let Some(normalized) = canonicalize_reported_path(&resolved) else {
            truncated = true;
            continue;
        };
        let normalized = normalized.to_string_lossy().to_string();
        if normalized.encode_utf16().count() > MAX_REPORTED_PATH_LENGTH
            || has_control_character(&normalized)
        {
            truncated = true;
            continue;
        }
        if !roots
            .iter()
            .any(|root| is_within_root(Path::new(&normalized), root))
        {
            truncated = true;
            continue;
        }
        let key = path_key(Path::new(&normalized));
        if roots.iter().any(|root| path_key(root) == key) {
            truncated = true;
            continue;
        }
        if !seen.insert(key) {
            continue;
        }
        let path_bytes = normalized.len();
        if result.len() >= MAX_REPORTED_PATHS || total_bytes + path_bytes > MAX_REPORT_BYTES {
            truncated = true;
            continue;
        }
        result.push(normalized);
        total_bytes += path_bytes;
    }
    (result, truncated)
}

/// The `reported` report body (`fitReportedAgentFileChangeReport`,
/// `file-change-audit.ts:322-341`): while the serialized report is over the wire
/// limit, drop the last path and mark it truncated. `Null` when even an empty
/// path list cannot fit (Node throws; the caller logs).
pub fn reported_report(request_id: &str, mut paths: Vec<String>, truncated: bool) -> Value {
    let mut fitted = json!({
        "version": 1,
        "requestId": request_id,
        "status": "reported",
        "paths": paths.clone(),
        "declaredComplete": false,
        "truncated": truncated,
    });
    loop {
        if serde_json::to_string(&fitted).map_or(usize::MAX, |text| text.len()) <= MAX_REPORT_BYTES
        {
            return fitted;
        }
        if paths.is_empty() {
            return Value::Null;
        }
        paths.pop();
        fitted = json!({
            "version": 1,
            "requestId": request_id,
            "status": "reported",
            "paths": paths.clone(),
            "declaredComplete": false,
            "truncated": true,
        });
    }
}

/// The `unavailable` report body (`finishUnavailable`, `file-change-audit.ts:143-155`).
pub fn unavailable_report(request_id: &str, reason: &str) -> Value {
    json!({
        "version": 1,
        "requestId": request_id,
        "status": "unavailable",
        "reason": reason,
    })
}

/// The checkpoint preview a `rewindFiles` dry run returned
/// (`RewindFilesResult`): usable only when `canRewind` is true and
/// `filesChanged` is an array (`file-change-audit.ts:180-186`). The flag is
/// `true` when a non-string entry was dropped: Node's
/// `normalizeReportedPaths` sets `truncated` for it
/// (`typeof reportedPath !== "string"`).
pub fn preview_files_changed(preview: &Value) -> Option<(Vec<String>, bool)> {
    if preview.get("canRewind") != Some(&Value::Bool(true)) {
        return None;
    }
    preview
        .get("filesChanged")
        .and_then(Value::as_array)
        .map(|paths| {
            let mut truncated = false;
            let mut out = Vec::with_capacity(paths.len());
            for path in paths {
                match path.as_str() {
                    Some(path) => out.push(String::from(path)),
                    None => truncated = true,
                }
            }
            (out, truncated)
        })
}

/// One accepted report request. The `finished` flag is the only state a spawned
/// report task shares with the actor: the first terminal outcome to swap it
/// publishes, every later one is a no-op (`publish`'s `phase === "finished"`
/// check). Lock-free, so a cancel never waits for the report task.
#[derive(Debug, Clone)]
pub struct ReportHandle {
    finished: Arc<std::sync::atomic::AtomicBool>,
}

impl ReportHandle {
    fn new() -> Self {
        Self {
            finished: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Whether a terminal outcome already went out for this request.
    pub fn is_finished(&self) -> bool {
        self.finished.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The session's recorder (`createNativeFileChangeReporter`), owned by the actor.
/// `request` and `finish` run on the actor; [`run_report`] runs in a spawned,
/// bounded task.
pub struct Reporter {
    workspace: Workspace,
    request_ids: HashSet<String>,
    handles: std::collections::HashMap<String, ReportHandle>,
    timeout: Duration,
}

impl Reporter {
    pub fn new(cwd: &Path, additional_directories: &[PathBuf]) -> Self {
        Self {
            workspace: Workspace::new(cwd, additional_directories),
            request_ids: HashSet::new(),
            handles: std::collections::HashMap::new(),
            timeout: REPORT_TIMEOUT,
        }
    }

    /// Override the checkpoint preview's bounded wait (tests).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// `request(meta)` (`file-change-audit.ts:225-233`): accept a prompt's
    /// request once per session, or `None` when absent, malformed or repeated.
    pub fn request(&mut self, meta: &Value) -> Option<(String, ReportHandle)> {
        let request_id = agent_file_change_report_request_id(meta)?;
        if !self.request_ids.insert(request_id.clone()) {
            return None;
        }
        let handle = ReportHandle::new();
        self.handles.insert(request_id.clone(), handle.clone());
        Some((request_id, handle))
    }

    /// `finish(state, reason)` (`file-change-audit.ts:235-238`): publish the
    /// `unavailable` report unless a terminal outcome already went out.
    pub fn finish(&self, handle: &ReportHandle, request_id: &str, reason: &str, publish: &Publish) {
        publish_once(handle, publish, &unavailable_report(request_id, reason));
    }

    /// Drop a settled turn's request bookkeeping.
    pub fn forget(&mut self, request_id: &str) {
        self.handles.remove(request_id);
    }
}

/// The publish callback of a report: the session's update channel. `Fn` so a
/// spawned task and the actor share it.
pub type Publish = Arc<dyn Fn(Value) + Send + Sync>;

/// The logger of a report failure (Node `options.logError`).
pub type LogError = Arc<dyn Fn(&str) + Send + Sync>;

fn publish_once(handle: &ReportHandle, publish: &Publish, report: &Value) {
    if handle
        .finished
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    publish(agent_file_change_report_meta(report));
}

/// One turn's report job: everything [`run_report`] needs, bundled so the
/// spawned task takes one value.
pub struct ReportJob {
    pub workspace: Workspace,
    pub handle: ReportHandle,
    pub request_id: String,
    pub edited_paths: Vec<String>,
    pub publish: Publish,
    pub log_error: LogError,
    pub timeout: Duration,
}

/// `report(turn, query)` (`file-change-audit.ts:160-198`): preview the
/// checkpoint with a bounded wait, then publish the reported paths. A
/// cancellation that settles meanwhile wins: its `unavailable` already went out
/// and this publishes nothing.
///
/// `preview` is the caller's `rewindFiles(userMessageId, {dryRun:true})` control
/// request; its error text is logged as the `providerError` cause.
pub async fn run_report<F>(job: ReportJob, preview: F)
where
    F: Future<Output = Result<Value, String>>,
{
    let ReportJob {
        workspace,
        handle,
        request_id,
        edited_paths,
        publish,
        log_error,
        timeout,
    } = job;
    // `Promise.race([preview, timeout])` (`file-change-audit.ts:168-179`): the
    // bounded wait settles `timeout`; a rejected preview is `providerError`.
    match tokio::time::timeout(timeout, preview).await {
        Err(_) => finish_without_preview(
            &workspace,
            &handle,
            &request_id,
            &edited_paths,
            "timeout",
            &publish,
        ),
        Ok(Err(error)) => {
            log_error(&format!(
                "Failed to inspect Claude file checkpoint {request_id}: {error}"
            ));
            finish_without_preview(
                &workspace,
                &handle,
                &request_id,
                &edited_paths,
                "providerError",
                &publish,
            );
        }
        Ok(Ok(value)) => match preview_files_changed(&value) {
            Some((paths, non_string)) => {
                let mut reported = paths;
                reported.extend(edited_paths.iter().cloned());
                let (paths, truncated) = normalize_reported_paths(&reported, &workspace);
                publish_reported(
                    &handle,
                    &request_id,
                    paths,
                    truncated || non_string,
                    &publish,
                );
            }
            None => finish_without_preview(
                &workspace,
                &handle,
                &request_id,
                &edited_paths,
                "invalidOutput",
                &publish,
            ),
        },
    }
}

/// `finishWithoutPreview` (`file-change-audit.ts:157-164`): the subagent edits
/// seen in the stream are still worth reporting; without them the outcome is
/// `unavailable`.
fn finish_without_preview(
    workspace: &Workspace,
    handle: &ReportHandle,
    request_id: &str,
    edited_paths: &[String],
    reason: &str,
    publish: &Publish,
) {
    if edited_paths.is_empty() {
        publish_once(handle, publish, &unavailable_report(request_id, reason));
        return;
    }
    let (paths, truncated) = normalize_reported_paths(edited_paths, workspace);
    publish_reported(handle, request_id, paths, truncated, publish);
}

fn publish_reported(
    handle: &ReportHandle,
    request_id: &str,
    paths: Vec<String>,
    truncated: bool,
    publish: &Publish,
) {
    let report = reported_report(request_id, paths, truncated);
    if report.is_null() {
        // Node throws here; the actor logs it and publishes nothing more.
        return;
    }
    publish_once(handle, publish, &report);
}

fn air_extension_meta(meta: &Value) -> Option<&Value> {
    let air = meta.get("jetbrains")?.get("air")?;
    air.is_object().then_some(air)
}

fn is_request_id(value: &str) -> bool {
    let length = value.encode_utf16().count();
    (1..=128).contains(&length)
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

fn has_control_character(value: &str) -> bool {
    value
        .chars()
        .any(|c| (c as u32) <= 0x1f || (0x7f..=0x9f).contains(&(c as u32)))
}

fn is_within_root(candidate: &Path, root: &Path) -> bool {
    candidate
        .strip_prefix(root)
        .is_ok_and(|relative| relative.components().next() != Some(std::path::Component::ParentDir))
}

/// `pathKey`: identity. Node lowercases on Windows; this crate cannot detect the
/// platform outside `process.rs` (guard G9), so the case-insensitive Windows key
/// is documented as a known divergence instead of a `cfg` read.
fn path_key(value: &Path) -> String {
    value.to_string_lossy().to_string()
}

/// `canonicalizeWorkspaceRoot`: the real path of the deepest existing ancestor,
/// with the missing segments re-appended (`file-change-audit.ts:360-392`).
fn canonicalize_workspace_root(value: &Path) -> PathBuf {
    canonicalize_from_existing_ancestor(value)
}

/// `canonicalizeReportedPath`: the real parent plus the file name, so a file
/// that does not exist yet still resolves inside its canonical directory.
fn canonicalize_reported_path(value: &Path) -> Option<PathBuf> {
    let parent = value.parent()?;
    let parent = canonicalize_from_existing_ancestor(parent);
    Some(resolve_lexical(
        &parent,
        &value.file_name()?.to_string_lossy(),
    ))
}

fn canonicalize_from_existing_ancestor(value: &Path) -> PathBuf {
    let original = value.to_path_buf();
    let mut current = original.clone();
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&current) {
            let mut resolved = canonical;
            for segment in missing.iter().rev() {
                resolved.push(segment);
            }
            return resolved;
        }
        let Some(parent) = current.parent() else {
            return original;
        };
        if parent == current {
            return original;
        }
        if let Some(name) = current.file_name() {
            missing.push(name.to_os_string());
        }
        current = parent.to_path_buf();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn request_meta(request_id: &str) -> Value {
        json!({"jetbrains": {"air": {
            "agentFileChangeReportRequest": {"version": 1, "requestId": request_id}
        }}})
    }

    /// review-T6INT finding 4: Node's `normalizeReportedPaths` sets `truncated`
    /// for an entry that is not a string (`typeof reportedPath !== "string"`);
    /// the preview must not silently drop it.
    #[tokio::test]
    async fn a_non_string_files_changed_entry_marks_the_report_truncated() {
        let dir = std::env::temp_dir().join(format!("fca-nonstring-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).expect("create dir");
        std::fs::write(dir.join("src/a.ts"), "x").expect("write file");
        let mut reporter = Reporter::new(&dir, &[]);
        let (request_id, handle) = reporter
            .request(&request_meta("r-nonstring"))
            .expect("a request");
        let (tx, rx) = mpsc::channel();
        let publish: Publish = Arc::new(move |value| {
            let _ = tx.send(value);
        });
        let log_error: LogError = Arc::new(|_| {});
        run_report(
            ReportJob {
                workspace: Workspace::new(&dir, &[]),
                handle,
                request_id,
                edited_paths: Vec::new(),
                publish,
                log_error,
                timeout: Duration::from_millis(500),
            },
            async {
                Ok(json!({
                    "canRewind": true,
                    "filesChanged": ["src/a.ts", 7],
                }))
            },
        )
        .await;
        let meta = rx.try_recv().expect("a report was published");
        let report = &meta["jetbrains"]["air"]["agentFileChangeReport"];
        assert_eq!(report["status"].as_str(), Some("reported"), "{report:?}");
        assert_eq!(report["truncated"], json!(true), "{report:?}");
        assert_eq!(
            report["paths"].as_array().map(Vec::len),
            Some(1),
            "{report:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
