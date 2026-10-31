//! Session index: port of upstream `src/session-index/*.ts` (T2 lane B). One
//! submodule per upstream file; `session-index.ts` itself is the parent module
//! (this file).
//!
//! The session list of a `sessionIndex` client. A list enumerates the
//! transcripts of the requested cwd and of every worktree of its repository: one
//! `readdir` per project directory and one `stat` per transcript. The pages
//! after a cursor reuse that enumeration for a short while
//! ([`SessionIndex::reusable_listing`]). The metadata of a transcript is cached
//! by `(path, mtime, size)` in an LRU and read on a miss from one head and tail
//! read (the titles by the SDK's rule, see `sdk_titles`; the facts, see
//! `scan_transcript_file`) and the title sidecar.
//!
//! Order is the last user activity descending, then session id: the time of the
//! last real user prompt (`lastPromptAt`), else `updatedAt`. `updatedAt` is the
//! time of the last message, capped at the transcript mtime, so a rename or
//! another metadata record does not move a session up, and the order key is
//! never later than it. Candidates are read in mtime order: a candidate's order
//! key is at most its mtime, so the scan stops as soon as the page is full and
//! the next mtime is older than the key of the last row.
//!
//! The index is a plain `&mut self` structure: it lives on one worker thread
//! (see [`service`]) and every file system call is synchronous.

pub mod activity;
pub mod archive_title;
pub mod connection;
pub mod dir_listing;
pub mod explicit_title;
pub mod first_prompt;
pub mod js_text;
pub mod list_subscriptions;
pub mod live_registry;
pub mod nfc;
mod nfc_data;
pub mod project_dirs;
pub mod service;
pub mod session_info;
pub mod transcript_scan;
pub mod worktrees;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use archive_title::{effective_title, visible_title};
use dir_listing::{stat, stat_files, DirListings, Lru};
use project_dirs::{
    canonical_path, encode_project_path, is_absolute, is_exact_project_dir, is_session_id,
    normalize_path, path_and_ancestors, project_dir_matches, project_dirs_of, real_path_string,
    same_project_path, Platform,
};
use transcript_scan::{
    continued_in_session_id, has_history, is_sidechain_transcript, last_timestamp, read_head_tail,
    relocated_cwd, scan_transcript_file, sdk_titles, tail_custom_title, transcript_agent_name,
    transcript_project_cwd, FullSearchCache, HeadTail, PreviousScan, TranscriptFacts,
};
use worktrees::{is_in_linked_worktree, worktree_counterparts};

pub const DEFAULT_LIST_LIMIT: usize = 50;
pub const MAX_LIST_LIMIT: usize = 200;
/// Entries of the metadata cache: a few hundred bytes each, so the transcripts of
/// a large history (tens of thousands) stay cached and deep pages do not read
/// them again.
const METADATA_CACHE_SIZE: usize = 50_000;
const READ_BATCH_SIZE: usize = 16;
/// Transcripts read at most per directory to recover a sibling's cwd.
const MAX_CWD_PROBES_PER_DIR: usize = 64;
/// How long a directory that gave no cwd is not probed again.
const NO_CWD_RETRY_MS: f64 = 60_000.0;
/// How long the pages after a list reuse its enumeration.
const LISTING_REUSE_MS: f64 = 30_000.0;
/// Enumerations kept for the next page of their list.
const MAX_REUSABLE_LISTINGS: usize = 8;

/// Which sessions a list holds by archive state: `unarchived` the unarchived ones
/// only, `archived` the archived ones only, `all` both in one order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchivedFilter {
    Unarchived,
    Archived,
    All,
}

impl ArchivedFilter {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "unarchived" => Some(Self::Unarchived),
            "archived" => Some(Self::Archived),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unarchived => "unarchived",
            Self::Archived => "archived",
            Self::All => "all",
        }
    }

    /// Whether the filter keeps a session of that archive state.
    pub fn keeps(self, archived: bool) -> bool {
        self == Self::All || (self == Self::Archived) == archived
    }
}

/// One transcript file found by the enumeration.
#[derive(Clone, Debug)]
pub struct TranscriptCandidate {
    pub session_id: String,
    pub file_path: PathBuf,
    pub dir_name: String,
    /// The requested path (cwd or worktree) whose project directory this is.
    pub project_path: Option<String>,
    pub mtime_ms: f64,
    pub size: u64,
    /// Tells a replaced file from one that grew.
    pub ino: u64,
}

/// The cached metadata of one transcript.
#[derive(Clone, Debug)]
pub struct TranscriptMetadata {
    /// The visible title: without the archive prefix.
    pub title: String,
    /// The custom title the SDK reports carries the archive prefix.
    pub archived: bool,
    /// The cwd read from the transcript, when it encodes to the directory name.
    pub file_cwd: Option<String>,
    /// The cwd the SDK list checks the transcript by: the last relocation, else
    /// the first cwd of the head.
    pub project_cwd: Option<String>,
    /// The session this transcript was continued in, from its tail.
    pub continued_in: Option<String>,
    pub updated_at_ms: f64,
    pub facts: TranscriptFacts,
}

#[derive(Clone, Debug)]
pub struct IndexRow {
    pub session_id: String,
    pub cwd: String,
    pub title: String,
    pub updated_at_ms: f64,
    /// The order key: the last user activity, `lastPromptAt`, else `updatedAt`.
    /// Never later than `updatedAt`, so never later than the transcript mtime.
    pub order_at_ms: f64,
    pub facts: TranscriptFacts,
    pub mtime_ms: f64,
    pub archived: bool,
    /// The session runs in a linked git worktree that Claude Code made for it:
    /// its transcript says so (`facts.agent_worktree`) and `cwd` lies in a linked
    /// worktree. See [`mark_agent_worktrees`].
    pub agent_worktree: bool,
}

/// A keyset position: the order key and the session id of the last row.
#[derive(Clone, Debug, PartialEq)]
pub struct ListCursor {
    pub order_at_ms: f64,
    pub session_id: String,
}

#[derive(Clone, Debug)]
pub struct ListQuery {
    pub cwd: Option<String>,
    /// Also list the sessions of the existing linked worktrees of `cwd`.
    pub include_worktrees: bool,
    pub limit: usize,
    pub archived: ArchivedFilter,
    pub after: Option<ListCursor>,
}

/// The transcripts of a list scope as one enumeration found them.
#[derive(Clone, Debug)]
struct Listing {
    /// The scope paths; `None` for every project.
    paths: Option<Vec<String>>,
    /// The directories read: the projects root, then each project directory.
    dirs: Vec<PathBuf>,
    /// The names each of `dirs` had, as [`DirListings`] returned them.
    names: Vec<Vec<String>>,
    /// Newest mtime first.
    candidates: Vec<TranscriptCandidate>,
    /// When it was enumerated, epoch ms.
    at: f64,
}

/// The key of the page of `query` that follows `cursor`.
fn page_key(query: &ListQuery, cursor: &ListCursor) -> String {
    serde_json::json!([
        query.cwd,
        query.include_worktrees,
        cursor.order_at_ms,
        cursor.session_id
    ])
    .to_string()
}

fn copies_by_session(
    candidates: &[TranscriptCandidate],
) -> HashMap<String, Vec<TranscriptCandidate>> {
    let mut copies: HashMap<String, Vec<TranscriptCandidate>> = HashMap::new();
    for candidate in candidates {
        copies
            .entry(candidate.session_id.clone())
            .or_default()
            .push(candidate.clone());
    }
    copies
}

/// `candidates` as they are on disk now; one that is gone or empty is left out.
fn restat(candidates: &[TranscriptCandidate]) -> Vec<TranscriptCandidate> {
    let paths: Vec<&Path> = candidates.iter().map(|c| c.file_path.as_path()).collect();
    let stats = stat_files(&paths);
    candidates
        .iter()
        .zip(stats)
        .filter_map(|(candidate, now)| {
            let now = now.filter(|s| s.is_file && s.size > 0)?;
            let mut candidate = candidate.clone();
            candidate.mtime_ms = now.mtime_ms;
            candidate.size = now.size;
            candidate.ino = now.identity;
            Some(candidate)
        })
        .collect()
}

#[derive(Clone)]
struct CacheEntry {
    mtime_ms: f64,
    size: u64,
    ino: u64,
    metadata: Option<TranscriptMetadata>,
}

impl CacheEntry {
    fn is_current(&self, candidate: &TranscriptCandidate) -> bool {
        self.mtime_ms == candidate.mtime_ms
            && self.size == candidate.size
            && self.ino == candidate.ino
    }
}

/// Rows order: order key descending, then session id.
pub fn compare_rows(a: (f64, &str), b: (f64, &str)) -> std::cmp::Ordering {
    b.0.partial_cmp(&a.0)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.1.cmp(b.1))
}

fn is_after(order_at_ms: f64, session_id: &str, cursor: &ListCursor) -> bool {
    compare_rows(
        (order_at_ms, session_id),
        (cursor.order_at_ms, cursor.session_id.as_str()),
    ) == std::cmp::Ordering::Greater
}

fn by_mtime_descending(a: &TranscriptCandidate, b: &TranscriptCandidate) -> std::cmp::Ordering {
    b.mtime_ms
        .partial_cmp(&a.mtime_ms)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then_with(|| a.session_id.cmp(&b.session_id))
}

fn to_row(candidate: &TranscriptCandidate, metadata: &TranscriptMetadata, cwd: &str) -> IndexRow {
    let last_prompt = metadata.facts.last_prompt_at.map(|v| v as f64);
    IndexRow {
        archived: metadata.archived,
        session_id: candidate.session_id.clone(),
        cwd: cwd.to_string(),
        title: metadata.title.clone(),
        updated_at_ms: metadata.updated_at_ms,
        order_at_ms: last_prompt
            .unwrap_or(metadata.updated_at_ms)
            .min(metadata.updated_at_ms),
        facts: metadata.facts.clone(),
        mtime_ms: candidate.mtime_ms,
        agent_worktree: false,
    }
}

/// Sets `agent_worktree` on the rows whose transcript says Claude Code made their
/// worktree and whose cwd lies in a linked worktree now. Only those rows are
/// checked; the check is not cached, so a removed worktree shows at once.
pub fn mark_agent_worktrees(rows: &mut [IndexRow]) {
    for row in rows {
        if row.facts.agent_worktree == Some(true) && is_in_linked_worktree(&row.cwd) {
            row.agent_worktree = true;
        }
    }
}

/// The title of the CLI's `custom-title.json` sidecar of a transcript.
pub fn read_sidecar_title(file_path: &Path, session_id: &str) -> Option<String> {
    let text = std::fs::read_to_string(
        file_path
            .parent()?
            .join(session_id)
            .join("custom-title.json"),
    )
    .ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let title = value.get("customTitle")?.as_str()?;
    (!js_text::js_trim(title).is_empty()).then(|| title.to_string())
}

/// A path as the SDK compares it: forward slashes, in any case where the file
/// system ignores it.
fn comparable(value: &str, platform: Platform) -> String {
    let slashed = normalize_path(value, platform).replace('\\', "/");
    if platform.case_insensitive_paths() {
        slashed.to_lowercase()
    } else {
        slashed
    }
}

/// Whether `a` and `b` are different paths that encode to the same project
/// directory name (`/ws/app.v2` and `/ws/app-v2`).
fn collides(a: &str, b: &str, platform: Platform) -> bool {
    same_project_path(a, b, platform) && comparable(a, platform) != comparable(b, platform)
}

/// A path the SDK does not resolve: a `..` segment, or a UNC or device path.
fn unresolvable(value: &str) -> bool {
    let separators = |c: char| c == '/' || c == '\\';
    value.split(separators).any(|segment| segment == "..")
        || (value.len() >= 2 && value.chars().take(2).all(separators))
}

/// Which transcripts of the project directories of `paths` (every one without
/// `paths`) the SDK `listSessions` of those paths lists. It leaves out only a
/// transcript whose cwd (see [`TranscriptMetadata::project_cwd`]) is another path
/// that encodes to the same directory and still resolves on disk to such another
/// path; one whose cwd is gone, or lies in one of `paths`, is listed.
struct Scope {
    own: Option<Vec<String>>,
    platform: Platform,
    resolved: HashMap<String, bool>,
}

impl Scope {
    fn new(paths: Option<&[String]>, platform: Platform) -> Self {
        Self {
            own: paths.map(|p| p.iter().map(|v| comparable(v, platform)).collect()),
            platform,
            resolved: HashMap::new(),
        }
    }

    fn hidden(&self, cwd: &str, project_path: &str) -> bool {
        let platform = self.platform;
        if !collides(cwd, project_path, platform) || unresolvable(cwd) || unresolvable(project_path)
        {
            return false;
        }
        let Ok(real) = std::fs::canonicalize(cwd) else {
            return false;
        };
        let real = real_path_string(&real.to_string_lossy(), platform);
        collides(&real, &canonical_path(project_path, platform), platform)
    }

    fn in_scope(&mut self, candidate: &TranscriptCandidate, metadata: &TranscriptMetadata) -> bool {
        let Some(own) = &self.own else {
            return true;
        };
        let (Some(project_path), Some(project_cwd)) =
            (&candidate.project_path, &metadata.project_cwd)
        else {
            return true;
        };
        let cwd = comparable(project_cwd, self.platform);
        if own.iter().any(|own_path| {
            cwd == *own_path
                || cwd.starts_with(&if own_path.ends_with('/') {
                    own_path.clone()
                } else {
                    format!("{own_path}/")
                })
        }) {
            return true;
        }
        let key = format!("{project_cwd}\0{project_path}");
        if let Some(known) = self.resolved.get(&key) {
            return *known;
        }
        let visible = !self.hidden(project_cwd, project_path);
        self.resolved.insert(key, visible);
        visible
    }
}

struct CollectOptions<'a> {
    archived: ArchivedFilter,
    after: Option<&'a ListCursor>,
    wanted: usize,
    /// The transcripts not read (by path), which may supply a sibling's cwd.
    unread: &'a dyn Fn(&HashSet<PathBuf>) -> Vec<TranscriptCandidate>,
    restat: bool,
}

/// The session index of one connection.
pub struct SessionIndex {
    root: PathBuf,
    platform: Platform,
    listings: DirListings,
    metadata: Lru<PathBuf, CacheEntry>,
    /// A cwd that encodes to each project directory name, learned from its
    /// transcripts; it recovers the cwd of a sibling that has none.
    dir_cwds: HashMap<String, String>,
    /// The project cwd of the transcripts of prefix-matched long directories.
    transcript_cwds: Lru<PathBuf, (f64, u64, Option<String>)>,
    /// Directories whose transcripts gave no cwd, by the directory mtime and when
    /// that was learned.
    dirs_without_cwd: HashMap<String, (f64, f64)>,
    /// The enumeration of each list with a next page, by [`page_key`].
    next_pages: Lru<String, Listing>,
    scan_cache: FullSearchCache,
    clock: Box<dyn Fn() -> f64 + Send>,
    /// Transcripts read so far (metadata cache misses).
    reads: usize,
}

fn system_now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

impl SessionIndex {
    /// An index over the projects root `root`, with the host's path rules.
    pub fn new(root: PathBuf) -> Self {
        Self::with_platform(root, Platform::host())
    }

    pub fn with_platform(root: PathBuf, platform: Platform) -> Self {
        Self {
            root,
            platform,
            listings: DirListings::default(),
            metadata: Lru::new(METADATA_CACHE_SIZE),
            dir_cwds: HashMap::new(),
            transcript_cwds: Lru::new(METADATA_CACHE_SIZE),
            dirs_without_cwd: HashMap::new(),
            next_pages: Lru::new(MAX_REUSABLE_LISTINGS),
            scan_cache: FullSearchCache::default(),
            clock: Box::new(system_now_ms),
            reads: 0,
        }
    }

    /// Replaces the clock (epoch ms) the index measures its reuse windows by.
    pub fn set_clock(&mut self, clock: Box<dyn Fn() -> f64 + Send>) {
        self.clock = clock;
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn platform(&self) -> Platform {
        self.platform
    }

    /// How many transcripts the index has read, for tests and diagnostics.
    pub fn transcripts_read(&self) -> usize {
        self.reads
    }

    /// How many times a directory was actually read.
    pub fn directory_reads(&self) -> usize {
        self.listings.reads()
    }

    /// The paths whose sessions a list of `cwd` shows: `cwd`, and with
    /// `include_worktrees` the same subdirectory of every other existing worktree
    /// of its repository (the worktree roots for a cwd at the root).
    pub fn listed_paths(&self, cwd: &str, include_worktrees: bool) -> Vec<String> {
        let canonical = canonical_path(cwd, self.platform);
        if !include_worktrees {
            return vec![canonical];
        }
        let mut paths = vec![canonical.clone()];
        for counterpart in worktree_counterparts(&canonical, self.platform) {
            if !paths.contains(&counterpart) {
                paths.push(counterpart);
            }
        }
        paths
    }

    /// On macOS the SDK opens `<projects>/<encoded cwd>` through the file system,
    /// which ignores case on the usual volumes: a directory whose name differs in
    /// case only is the project directory then. Returns that entry of
    /// `root_entries` when the file system resolves the exact name to it.
    fn case_insensitive_project_dir(
        &self,
        project_path: &str,
        root_entries: &[String],
    ) -> Option<String> {
        if self.platform != Platform::Darwin {
            return None;
        }
        let exact = encode_project_path(project_path, self.platform);
        if root_entries.contains(&exact) {
            return None;
        }
        let lower = exact.to_lowercase();
        let variant = root_entries
            .iter()
            .find(|name| name.to_lowercase() == lower)?;
        stat(&self.root.join(&exact))
            .is_some_and(|s| s.is_dir)
            .then(|| variant.clone())
    }

    /// Whether the encoding of `cwd` names the directory `dir_name` on disk.
    fn same_dir_on_disk(&self, dir_name: &str, cwd: &str) -> bool {
        let listed = stat(&self.root.join(dir_name));
        let resolved = stat(&self.root.join(encode_project_path(cwd, self.platform)));
        match (listed, resolved) {
            (Some(a), Some(b)) => a.is_dir && b.is_dir && a.identity == b.identity,
            _ => false,
        }
    }

    /// The project directories (names under the projects root) of `paths`. A long
    /// path is matched by its cut prefix, which other long paths may share: such a
    /// directory counts only when one of its transcripts belongs to the path, as
    /// the SDK checks.
    pub fn project_dirs(&mut self, paths: &[String]) -> Vec<(String, String)> {
        let root_entries = self.listings.names(&self.root.clone());
        let mut seen: HashSet<String> = HashSet::new();
        let mut result = Vec::new();
        for project_path in paths {
            let mut names = project_dirs_of(project_path, &root_entries, self.platform);
            let case_variant = self.case_insensitive_project_dir(project_path, &root_entries);
            if let Some(variant) = &case_variant {
                if !names.contains(variant) {
                    names.insert(0, variant.clone());
                }
            }
            for dir_name in names {
                if seen.contains(&dir_name) {
                    continue;
                }
                if case_variant.as_deref() != Some(dir_name.as_str())
                    && dir_name != encode_project_path(project_path, self.platform)
                    && !self.long_dir_belongs_to(&dir_name, project_path)
                {
                    continue;
                }
                seen.insert(dir_name.clone());
                result.push((dir_name, project_path.clone()));
            }
        }
        result
    }

    /// Whether a transcript of the prefix-matched `dir_name` belongs to
    /// `project_path`. The cwd of each transcript is cached by its
    /// `(path, mtime, size)`, so a transcript that changes is read again.
    fn long_dir_belongs_to(&mut self, dir_name: &str, project_path: &str) -> bool {
        let dir = self.root.join(dir_name);
        for name in self.listings.names(&dir) {
            if !name.ends_with(".jsonl") {
                continue;
            }
            let file_path = dir.join(&name);
            let Some(stats) = stat(&file_path).filter(|s| s.is_file) else {
                continue;
            };
            let cached = self
                .transcript_cwds
                .get(&file_path)
                .filter(|(mtime, size, _)| *mtime == stats.mtime_ms && *size == stats.size)
                .cloned();
            let cwd = match cached {
                Some((_, _, cwd)) => cwd,
                None => {
                    let Ok(head_tail) = read_head_tail(&file_path, stats.size) else {
                        continue;
                    };
                    let cwd = transcript_project_cwd(&head_tail);
                    self.transcript_cwds
                        .set(file_path.clone(), (stats.mtime_ms, stats.size, cwd.clone()));
                    cwd
                }
            };
            if cwd.is_some_and(|c| same_project_path(&c, project_path, self.platform)) {
                return true;
            }
        }
        false
    }

    /// Every non-empty transcript file of `paths` (without, of all projects),
    /// every copy of a session: a list shows the newest one that has a row.
    pub fn enumerate_files(&mut self, paths: Option<&[String]>) -> Vec<TranscriptCandidate> {
        self.enumerate(paths).2
    }

    /// [`enumerate_files`](Self::enumerate_files), with the directory listings it
    /// was made from.
    fn enumerate(
        &mut self,
        paths: Option<&[String]>,
    ) -> (Vec<PathBuf>, Vec<Vec<String>>, Vec<TranscriptCandidate>) {
        let root = self.root.clone();
        let root_names = self.listings.names(&root);
        let dirs: Vec<(String, Option<String>)> = match paths {
            Some(paths) => self
                .project_dirs(paths)
                .into_iter()
                .map(|(dir, path)| (dir, Some(path)))
                .collect(),
            None => root_names.iter().map(|d| (d.clone(), None)).collect(),
        };
        // All directories' names first, then one bounded run of stats.
        let dir_paths: Vec<PathBuf> = dirs.iter().map(|(d, _)| root.join(d)).collect();
        let listed = self.listings.names_of_all(&dir_paths);
        let mut files = Vec::new();
        for (i, (dir_name, project_path)) in dirs.iter().enumerate() {
            for name in &listed[i] {
                if let Some(stem) = name.strip_suffix(".jsonl") {
                    if is_session_id(stem) {
                        files.push((
                            stem.to_string(),
                            root.join(dir_name).join(name),
                            dir_name.clone(),
                            project_path.clone(),
                        ));
                    }
                }
            }
        }
        let stats = stat_files(&files.iter().map(|f| f.1.as_path()).collect::<Vec<_>>());
        let mut all = Vec::new();
        for ((session_id, file_path, dir_name, project_path), stats) in files.into_iter().zip(stats)
        {
            let Some(stats) = stats.filter(|s| s.is_file && s.size > 0) else {
                continue;
            };
            all.push(TranscriptCandidate {
                session_id,
                file_path,
                dir_name,
                project_path,
                mtime_ms: stats.mtime_ms,
                size: stats.size,
                ino: stats.identity,
            });
        }
        let mut all_dirs = vec![root];
        all_dirs.extend(dir_paths);
        let mut all_names = vec![root_names];
        all_names.extend(listed);
        (all_dirs, all_names, all)
    }

    /// Every transcript (with its size) and every `<sessionId>/` directory
    /// (sidecar, subagent transcripts) of the session, in any project directory,
    /// by the exact id. One pass over the (cached) project directory listings.
    pub fn scan_session(&mut self, session_id: &str) -> (Vec<(PathBuf, u64)>, Vec<PathBuf>) {
        if !is_session_id(session_id) {
            return (Vec::new(), Vec::new());
        }
        let root = self.root.clone();
        let dirs: Vec<PathBuf> = self
            .listings
            .names(&root)
            .into_iter()
            .map(|d| root.join(d))
            .collect();
        let listed = self.listings.names_of_all(&dirs);
        let transcript_name = format!("{session_id}.jsonl");
        let mut transcripts = Vec::new();
        let mut session_dirs = Vec::new();
        for (dir, names) in dirs.iter().zip(&listed) {
            if names.contains(&transcript_name) {
                let path = dir.join(&transcript_name);
                if let Some(stats) = stat(&path).filter(|s| s.is_file) {
                    transcripts.push((path, stats.size));
                }
            }
            if names.iter().any(|n| n == session_id) {
                let path = dir.join(session_id);
                // `lstat`: a symbolic link to a directory is not removed through.
                if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                    session_dirs.push(path);
                }
            }
        }
        (transcripts, session_dirs)
    }

    /// Every transcript file of `session_id` (see [`scan_session`](Self::scan_session)).
    /// Empty files only with `include_empty`.
    pub fn find_transcripts(&mut self, session_id: &str, include_empty: bool) -> Vec<PathBuf> {
        self.scan_session(session_id)
            .0
            .into_iter()
            .filter(|(_, size)| *size > 0 || include_empty)
            .map(|(path, _)| path)
            .collect()
    }

    /// Every `<sessionId>/` directory of the session.
    pub fn find_session_dirs(&mut self, session_id: &str) -> Vec<PathBuf> {
        self.scan_session(session_id).1
    }

    /// Whether `candidate` was read at its stat: its metadata is cached, also when
    /// it holds no row. False after a failed read.
    pub fn is_read(&self, candidate: &TranscriptCandidate) -> bool {
        self.metadata
            .peek(&candidate.file_path)
            .is_some_and(|c| c.is_current(candidate))
    }

    /// The session that the cached metadata of `file_path` says it was continued in.
    pub fn continued_in(&self, file_path: &Path) -> Option<String> {
        self.metadata
            .peek(&file_path.to_path_buf())?
            .metadata
            .as_ref()?
            .continued_in
            .clone()
    }

    /// Drops the cached metadata of `file_paths`.
    pub fn invalidate(&mut self, file_paths: &[PathBuf]) {
        for path in file_paths {
            self.metadata.delete(path);
        }
    }

    /// One page of rows, plus whether more rows follow the page.
    pub fn list(&mut self, query: &ListQuery) -> (Vec<IndexRow>, bool) {
        let after = query.after.clone();
        let reused = after
            .as_ref()
            .and_then(|cursor| self.reusable_listing(query, cursor));
        let was_reused = reused.is_some();
        let listing = match reused {
            Some(listing) => listing,
            None => self.fresh_listing(query),
        };
        // A page after a cursor skips, without reading them, the sessions whose
        // newest transcript has a cached order key before the cursor. The archive
        // state comes from the title, so the archive filter applies once a
        // transcript is read.
        let mut skipped: HashSet<String> = HashSet::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for candidate in &listing.candidates {
            if !seen.insert(candidate.session_id.as_str()) {
                continue;
            }
            if let Some(cursor) = &after {
                if self.cached_before(candidate, cursor) {
                    skipped.insert(candidate.session_id.clone());
                }
            }
        }
        let candidates: Vec<TranscriptCandidate> = listing
            .candidates
            .iter()
            .filter(|c| !skipped.contains(&c.session_id))
            .cloned()
            .collect();
        let enumerated = &listing.candidates;
        let unread = |read: &HashSet<PathBuf>| -> Vec<TranscriptCandidate> {
            enumerated
                .iter()
                .filter(|c| !read.contains(&c.file_path))
                .cloned()
                .collect()
        };
        let mut scope = Scope::new(listing.paths.as_deref(), self.platform);
        // One row more than the page tells whether a next page exists, so a
        // cursor never leads to an empty page.
        let rows = self.collect_rows(
            &candidates,
            &mut scope,
            &CollectOptions {
                archived: query.archived,
                after: after.as_ref(),
                wanted: query.limit + 1,
                unread: &unread,
                restat: was_reused,
            },
        );
        let has_more = rows.len() > query.limit;
        let page: Vec<IndexRow> = rows.into_iter().take(query.limit).collect();
        if has_more {
            if let Some(last) = page.last() {
                let key = page_key(
                    query,
                    &ListCursor {
                        order_at_ms: last.order_at_ms,
                        session_id: last.session_id.clone(),
                    },
                );
                self.next_pages.set(key, listing);
            }
        }
        (page, has_more)
    }

    /// Enumerates the scope of `query`, newest mtime first.
    fn fresh_listing(&mut self, query: &ListQuery) -> Listing {
        let at = (self.clock)();
        let paths = query
            .cwd
            .as_deref()
            .filter(|cwd| !cwd.is_empty())
            .map(|cwd| self.listed_paths(cwd, query.include_worktrees));
        let (dirs, names, mut candidates) = self.enumerate(paths.as_deref());
        candidates.sort_by(by_mtime_descending);
        Listing {
            paths,
            dirs,
            names,
            candidates,
            at,
        }
    }

    /// The enumeration of the earlier page of `query` that ended at `cursor`, while
    /// it is recent and no directory of it has changed its entries: a transcript
    /// was neither added nor removed (see [`DirListings`], which returns the same
    /// names until then). Which transcripts the page reads is stat'ed again (see
    /// `collect_rows`), so a page made from it is as fresh as one from a new
    /// enumeration.
    fn reusable_listing(&mut self, query: &ListQuery, cursor: &ListCursor) -> Option<Listing> {
        let key = page_key(query, cursor);
        let listing = self.next_pages.peek(&key)?.clone();
        self.next_pages.delete(&key);
        if (self.clock)() - listing.at > LISTING_REUSE_MS {
            return None;
        }
        let names = self.listings.names_of_all(&listing.dirs);
        (names == listing.names).then_some(listing)
    }

    /// The rows of `candidates`, one per session (its newest transcript that has a
    /// row), any archive state, as a list of `paths` shows them: a candidate
    /// without a row (no title, a sidechain, continued elsewhere, another path that
    /// shares the project directory) is left out. `siblings` are the other
    /// transcripts of the candidates' directories, read only when a candidate has
    /// no cwd of its own. The metadata cache makes an unchanged transcript free.
    pub fn rows_of(
        &mut self,
        paths: &[String],
        candidates: &[TranscriptCandidate],
        siblings: &dyn Fn() -> Vec<TranscriptCandidate>,
    ) -> Vec<IndexRow> {
        let unread = |read: &HashSet<PathBuf>| -> Vec<TranscriptCandidate> {
            siblings()
                .into_iter()
                .filter(|c| !read.contains(&c.file_path))
                .collect()
        };
        let mut scope = Scope::new(Some(paths), self.platform);
        self.collect_rows(
            candidates,
            &mut scope,
            &CollectOptions {
                archived: ArchivedFilter::All,
                after: None,
                wanted: usize::MAX,
                unread: &unread,
                restat: false,
            },
        )
    }

    /// The rows of `candidates` (as [`rows_of`](Self::rows_of)) that the metadata
    /// cache alone gives, read from it at once, and the ids of the sessions it
    /// gives no answer for: a transcript of it has nothing cached at its stat, no
    /// cwd known, or is continued in another session (whether the successor hides
    /// it depends on the successor now). Reads no transcript. The rows carry
    /// `agent_worktree` unless `mark` is false.
    pub fn cached_rows_of(
        &mut self,
        paths: &[String],
        candidates: &[TranscriptCandidate],
        mark: bool,
    ) -> (Vec<IndexRow>, HashSet<String>) {
        let mut unknown: HashSet<String> = HashSet::new();
        let mut resolved: Vec<(TranscriptCandidate, TranscriptMetadata, String)> = Vec::new();
        for candidate in candidates {
            let id = &candidate.session_id;
            let cached = self.metadata.peek(&candidate.file_path).cloned();
            let Some(cached) = cached.filter(|c| {
                c.is_current(candidate)
                    && !c
                        .metadata
                        .as_ref()
                        .is_some_and(|m| m.continued_in.is_some())
            }) else {
                unknown.insert(id.clone());
                continue;
            };
            let Some(metadata) = cached.metadata else {
                continue;
            };
            let cwd = metadata
                .file_cwd
                .clone()
                .or_else(|| self.fallback_cwd(candidate));
            match cwd {
                Some(cwd) => resolved.push((candidate.clone(), metadata, cwd)),
                None => {
                    unknown.insert(id.clone());
                }
            }
        }
        let mut scope = Scope::new(Some(paths), self.platform);
        let mut newest: HashMap<String, (IndexRow, f64)> = HashMap::new();
        for (candidate, metadata, cwd) in &resolved {
            let id = &candidate.session_id;
            if unknown.contains(id) || !scope.in_scope(candidate, metadata) {
                continue;
            }
            if newest
                .get(id)
                .is_some_and(|(_, mtime)| *mtime >= candidate.mtime_ms)
            {
                continue;
            }
            newest.insert(
                id.clone(),
                (to_row(candidate, metadata, cwd), candidate.mtime_ms),
            );
        }
        let mut rows: Vec<IndexRow> = newest.into_values().map(|(row, _)| row).collect();
        if mark {
            mark_agent_worktrees(&mut rows);
        }
        (rows, unknown)
    }

    /// Reads `candidates` in order into rows, one per session, sorted. A session
    /// copied to several transcripts shows its newest one that has a row, as the
    /// SDK list does. With a finite `wanted`, stops once `wanted` rows are certain
    /// to come first (a candidate's order key is at most its mtime, so `candidates`
    /// must be in mtime order then).
    ///
    /// With `restat`, `candidates` are an earlier enumeration: each is stat'ed
    /// again just before it is read, together with the other copies of its session,
    /// any of which may be its newest one now.
    fn collect_rows(
        &mut self,
        candidates: &[TranscriptCandidate],
        scope: &mut Scope,
        options: &CollectOptions<'_>,
    ) -> Vec<IndexRow> {
        let wanted = options.wanted;
        // The row of each session (None: left out by the archive filter or the
        // cursor), and the mtime of the transcript it is from.
        let mut by_session: HashMap<String, (Option<IndexRow>, f64)> = HashMap::new();
        let sorted_rows = |by_session: &HashMap<String, (Option<IndexRow>, f64)>| {
            let mut rows: Vec<IndexRow> = by_session
                .values()
                .filter_map(|(row, _)| row.clone())
                .collect();
            rows.sort_by(|a, b| {
                compare_rows(
                    (a.order_at_ms, &a.session_id),
                    (b.order_at_ms, &b.session_id),
                )
            });
            rows
        };
        // Read transcripts without a cwd of their own: a sibling of the same
        // directory may supply it, whichever batch it is read in.
        let mut pending: Vec<(TranscriptCandidate, TranscriptMetadata)> = Vec::new();
        let accept = |by_session: &mut HashMap<String, (Option<IndexRow>, f64)>,
                      scope: &mut Scope,
                      candidate: &TranscriptCandidate,
                      metadata: &TranscriptMetadata,
                      cwd: &str| {
            // The SDK leaves out another path that encodes to the same project
            // directory (see Scope).
            if !scope.in_scope(candidate, metadata) {
                return;
            }
            if by_session
                .get(&candidate.session_id)
                .is_some_and(|(_, mtime)| *mtime >= candidate.mtime_ms)
            {
                return;
            }
            let row = to_row(candidate, metadata, cwd);
            // A newer copy that the filter or the cursor leaves out still hides an
            // older one.
            let kept = options.archived.keeps(row.archived)
                && options
                    .after
                    .is_none_or(|cursor| is_after(row.order_at_ms, &row.session_id, cursor));
            by_session.insert(
                candidate.session_id.clone(),
                (kept.then_some(row), candidate.mtime_ms),
            );
        };
        let mut read: HashSet<PathBuf> = HashSet::new();
        // With `restat`: the copies of each session.
        let copies = options.restat.then(|| copies_by_session(candidates));
        let mut index = 0usize;
        while index < candidates.len() {
            if by_session.len() >= wanted {
                let sorted = sorted_rows(&by_session);
                // A candidate's order key is at most its mtime.
                if sorted.len() >= wanted
                    && candidates[index].mtime_ms < sorted[wanted - 1].order_at_ms
                {
                    break;
                }
            }
            let end = (index + READ_BATCH_SIZE).min(candidates.len());
            let next = &candidates[index..end];
            index = end;
            let batch: Vec<TranscriptCandidate> = match &copies {
                Some(copies) => {
                    let mut wanted_copies: Vec<TranscriptCandidate> = Vec::new();
                    let mut taken: HashSet<PathBuf> = HashSet::new();
                    for candidate in next {
                        for copy in copies.get(&candidate.session_id).into_iter().flatten() {
                            if !read.contains(&copy.file_path)
                                && taken.insert(copy.file_path.clone())
                            {
                                wanted_copies.push(copy.clone());
                            }
                        }
                    }
                    restat(&wanted_copies)
                }
                None => next.to_vec(),
            };
            let mut resolved = Vec::new();
            for candidate in batch {
                read.insert(candidate.file_path.clone());
                let Some(metadata) = self.metadata_of(&candidate) else {
                    continue;
                };
                if self.continued_elsewhere(&candidate, &metadata) {
                    continue;
                }
                resolved.push((candidate, metadata));
            }
            for (candidate, metadata) in resolved {
                let cwd = metadata
                    .file_cwd
                    .clone()
                    .or_else(|| self.fallback_cwd(&candidate));
                match cwd {
                    Some(cwd) => accept(&mut by_session, scope, &candidate, &metadata, &cwd),
                    None => pending.push((candidate, metadata)),
                }
            }
            self.settle_pending(&mut pending, &mut by_session, scope, &accept);
        }
        if !pending.is_empty() {
            // A row recovered after the scan stopped still sorts into the page: it
            // was read before the stop, and more rows only raise the bound. A
            // sibling that the archive filter or the cursor leaves out may still
            // supply the cwd.
            let dir_names: HashSet<String> =
                pending.iter().map(|(c, _)| c.dir_name.clone()).collect();
            let unread = (options.unread)(&read);
            self.learn_dir_cwds(&dir_names, &unread, options.restat);
            self.settle_pending(&mut pending, &mut by_session, scope, &accept);
        }
        let mut rows = sorted_rows(&by_session);
        mark_agent_worktrees(&mut rows);
        rows
    }

    fn settle_pending<F>(
        &self,
        pending: &mut Vec<(TranscriptCandidate, TranscriptMetadata)>,
        by_session: &mut HashMap<String, (Option<IndexRow>, f64)>,
        scope: &mut Scope,
        accept: &F,
    ) where
        F: Fn(
            &mut HashMap<String, (Option<IndexRow>, f64)>,
            &mut Scope,
            &TranscriptCandidate,
            &TranscriptMetadata,
            &str,
        ),
    {
        let mut left = Vec::new();
        for (candidate, metadata) in pending.drain(..) {
            match self.fallback_cwd(&candidate) {
                Some(cwd) => accept(by_session, scope, &candidate, &metadata, &cwd),
                None => left.push((candidate, metadata)),
            }
        }
        *pending = left;
    }

    /// Whether the cached metadata of `candidate`, still current, places it at or
    /// before `cursor`.
    fn cached_before(&self, candidate: &TranscriptCandidate, cursor: &ListCursor) -> bool {
        let Some(cached) = self.metadata.peek(&candidate.file_path) else {
            return false;
        };
        let Some(metadata) = cached
            .metadata
            .as_ref()
            .filter(|_| cached.is_current(candidate))
        else {
            return false;
        };
        let order_at_ms = metadata
            .facts
            .last_prompt_at
            .map_or(metadata.updated_at_ms, |v| v as f64)
            .min(metadata.updated_at_ms);
        !is_after(order_at_ms, &candidate.session_id, cursor)
    }

    /// Reads the unread transcripts of `dir_names` until each directory has a
    /// known cwd, at most [`MAX_CWD_PROBES_PER_DIR`] per directory. A directory that
    /// gives none is not read again for a while, unless it changes.
    fn learn_dir_cwds(
        &mut self,
        dir_names: &HashSet<String>,
        unread: &[TranscriptCandidate],
        restat_first: bool,
    ) {
        let now = (self.clock)();
        let mut mtimes: HashMap<String, f64> = HashMap::new();
        for dir_name in dir_names {
            let Some(stats) = stat(&self.root.join(dir_name)) else {
                continue;
            };
            if let Some((known_mtime, at)) = self.dirs_without_cwd.get(dir_name) {
                if *known_mtime == stats.mtime_ms && now - at < NO_CWD_RETRY_MS {
                    continue;
                }
            }
            mtimes.insert(dir_name.clone(), stats.mtime_ms);
        }
        let mut probes: HashMap<String, usize> = HashMap::new();
        let mut remaining: Vec<TranscriptCandidate> = unread
            .iter()
            .filter(|candidate| {
                if !mtimes.contains_key(&candidate.dir_name) {
                    return false;
                }
                let count = probes.entry(candidate.dir_name.clone()).or_insert(0);
                if *count >= MAX_CWD_PROBES_PER_DIR {
                    return false;
                }
                *count += 1;
                true
            })
            .cloned()
            .collect();
        while !remaining.is_empty() {
            remaining.retain(|c| !self.dir_cwds.contains_key(&c.dir_name));
            let take = READ_BATCH_SIZE.min(remaining.len());
            let next: Vec<TranscriptCandidate> = remaining.drain(..take).collect();
            let batch = if restat_first { restat(&next) } else { next };
            for candidate in &batch {
                let _ = self.metadata_of(candidate);
            }
        }
        for (dir_name, mtime_ms) in mtimes {
            if !self.dir_cwds.contains_key(&dir_name) {
                self.dirs_without_cwd.insert(dir_name, (mtime_ms, now));
            }
        }
    }

    /// The requested path of the directory, else a sibling's cwd.
    fn fallback_cwd(&self, candidate: &TranscriptCandidate) -> Option<String> {
        // A directory found for the requested path belongs to it, also one that
        // differs in case only (see `case_insensitive_project_dir`).
        if let Some(project_path) = &candidate.project_path {
            if project_dir_matches(&candidate.dir_name, project_path, self.platform)
                || is_exact_project_dir(&candidate.dir_name, project_path, self.platform)
            {
                return Some(project_path.clone());
            }
        }
        self.dir_cwds.get(&candidate.dir_name).cloned()
    }

    fn metadata_of(&mut self, candidate: &TranscriptCandidate) -> Option<TranscriptMetadata> {
        let cached = self.metadata.get(&candidate.file_path).cloned();
        if let Some(cached) = &cached {
            if cached.is_current(candidate) {
                if let Some(cwd) = cached.metadata.as_ref().and_then(|m| m.file_cwd.clone()) {
                    self.dir_cwds.insert(candidate.dir_name.clone(), cwd);
                }
                return cached.metadata.clone();
            }
        }
        self.read_and_cache(candidate, cached)
    }

    fn read_and_cache(
        &mut self,
        candidate: &TranscriptCandidate,
        cached: Option<CacheEntry>,
    ) -> Option<TranscriptMetadata> {
        // The transcript grew since: an earlier scan may still know its last prompt.
        let previous =
            cached
                .as_ref()
                .filter(|c| c.ino == candidate.ino)
                .map(|c| match &c.metadata {
                    Some(m) => PreviousScan::of(&m.facts, c.size),
                    None => PreviousScan {
                        size: c.size,
                        ..PreviousScan::default()
                    },
                });
        // Unreadable now (deleted, permissions): skip it and retry next time.
        let metadata = self.read_metadata(candidate, previous.as_ref()).ok()?;
        self.metadata.set(
            candidate.file_path.clone(),
            CacheEntry {
                mtime_ms: candidate.mtime_ms,
                size: candidate.size,
                ino: candidate.ino,
                metadata: metadata.clone(),
            },
        );
        metadata
    }

    fn read_metadata(
        &mut self,
        candidate: &TranscriptCandidate,
        previous: Option<&PreviousScan>,
    ) -> std::io::Result<Option<TranscriptMetadata>> {
        self.reads += 1;
        let head_tail = read_head_tail(&candidate.file_path, candidate.size)?;
        if is_sidechain_transcript(&head_tail.head) {
            return Ok(None);
        }
        let facts = scan_transcript_file(
            &candidate.file_path,
            candidate.size,
            Some(head_tail.clone()),
            previous,
            Some(candidate.ino),
            &mut self.scan_cache,
        )?;
        if !facts.has_messages {
            return Ok(None);
        }
        // The last relocation names the session's cwd, as the SDK reads it; the
        // messages before it keep the old one.
        let mut cwd_candidates: Vec<Option<String>> =
            vec![relocated_cwd(&head_tail.tail), facts.head_cwd.clone()];
        if let Some(tail_cwd) = &facts.tail_cwd {
            cwd_candidates.extend(
                path_and_ancestors(tail_cwd, self.platform)
                    .into_iter()
                    .map(Some),
            );
        }
        let file_cwd = self.recover_cwd(&candidate.dir_name, &cwd_candidates);
        if let Some(cwd) = &file_cwd {
            self.dir_cwds
                .insert(candidate.dir_name.clone(), cwd.clone());
        }
        let sdk = self.titles_of(candidate, &head_tail);
        // No title at all: the SDK does not list it either.
        let Some(summary) = sdk.summary.clone() else {
            return Ok(None);
        };
        // Archived by its name alone: the agent name, else the custom title.
        let (title, archived) = effective_title(transcript_agent_name(&head_tail).as_deref(), &sdk);
        let title = title.unwrap_or(summary);
        // A last message longer than the tail search still ends with its
        // timestamp; the mtime moves with every metadata record.
        let last_message_at = match facts.last_message_at {
            Some(at) => at as f64,
            None => last_timestamp(&candidate.file_path, candidate.size)?
                .map_or(candidate.mtime_ms, |at| at as f64),
        };
        let continued_in = continued_in_session_id(&head_tail.tail);
        let project_cwd = transcript_project_cwd(&head_tail);
        Ok(Some(TranscriptMetadata {
            title: crate::session_store::sanitize_title(&if archived {
                visible_title(&title)
            } else {
                title
            }),
            archived,
            file_cwd,
            project_cwd,
            continued_in,
            updated_at_ms: last_message_at.min(candidate.mtime_ms),
            facts,
        }))
    }

    /// The titles the SDK gives the listed transcript (`customTitle` and `summary`
    /// of its `getSessionInfo`), by the SDK's rule from the head and tail read here
    /// and the sidecar. Not through the SDK itself: its lookup of the transcript
    /// reads the whole project directory on every call.
    fn titles_of(
        &self,
        candidate: &TranscriptCandidate,
        head_tail: &HeadTail,
    ) -> archive_title::SdkTitles {
        let sidecar = if tail_custom_title(head_tail).is_none() {
            read_sidecar_title(&candidate.file_path, &candidate.session_id)
        } else {
            None
        };
        sdk_titles(head_tail, sidecar.as_deref())
    }

    /// Whether a successor of a continued transcript holds history: the SDK list
    /// then hides the predecessor.
    fn continued_elsewhere(
        &self,
        candidate: &TranscriptCandidate,
        metadata: &TranscriptMetadata,
    ) -> bool {
        let Some(continued_in) = &metadata.continued_in else {
            return false;
        };
        candidate
            .file_path
            .parent()
            .is_some_and(|dir| has_history(&dir.join(format!("{continued_in}.jsonl"))))
    }

    /// The first candidate that encodes to `dir_name`, or, on macOS, that encodes to
    /// a name the file system resolves to `dir_name` (it differs in case only). A
    /// directory name is never decoded.
    fn recover_cwd(&self, dir_name: &str, candidates: &[Option<String>]) -> Option<String> {
        for candidate in candidates.iter().flatten() {
            if candidate.is_empty() || !is_absolute(candidate, self.platform) {
                continue;
            }
            if project_dir_matches(dir_name, candidate, self.platform) {
                return Some(candidate.clone());
            }
            if is_exact_project_dir(dir_name, candidate, self.platform)
                && self.same_dir_on_disk(dir_name, candidate)
            {
                return Some(candidate.clone());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    const T0: i64 = 1_767_225_600_000;

    fn id(n: usize) -> String {
        format!("aaaaaaaa-aaaa-4aaa-8aaa-{n:012}")
    }

    struct Fixture {
        base: PathBuf,
        root: PathBuf,
        cwd: String,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let base =
                std::env::temp_dir().join(format!("acp-t2b-idx-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(base.join("projects")).expect("root");
            std::fs::create_dir_all(base.join("work/app")).expect("cwd");
            let base = std::fs::canonicalize(&base).expect("canonical");
            let cwd = base.join("work/app").to_string_lossy().into_owned();
            Self {
                root: base.join("projects"),
                base,
                cwd,
            }
        }

        fn dir(&self) -> PathBuf {
            let dir = self
                .root
                .join(encode_project_path(&self.cwd, Platform::host()));
            std::fs::create_dir_all(&dir).expect("project dir");
            dir
        }

        fn write(&self, session: &str, lines: &[Value], mtime_ms: i64) -> PathBuf {
            let path = self.dir().join(format!("{session}.jsonl"));
            let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
            std::fs::write(&path, text).expect("write");
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("open");
            file.set_modified(
                std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_millis(mtime_ms as u64),
            )
            .expect("mtime");
            path
        }

        fn prompt(&self, session: &str, text: &str, at: i64) -> Value {
            json!({"type": "user", "sessionId": session, "cwd": self.cwd,
                "timestamp": js_text::to_iso_string(at), "parentUuid": null,
                "message": {"role": "user", "content": text}})
        }

        fn answer(&self, session: &str, at: i64) -> Value {
            json!({"type": "assistant", "sessionId": session, "timestamp": js_text::to_iso_string(at),
                "message": {"role": "assistant", "stop_reason": "end_turn", "content": []}})
        }

        fn query(&self, limit: usize, after: Option<ListCursor>) -> ListQuery {
            ListQuery {
                cwd: Some(self.cwd.clone()),
                include_worktrees: false,
                limit,
                archived: ArchivedFilter::All,
                after,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// session id lookup > matches a session id exactly, in no other case
    #[test]
    fn a_session_id_matches_exactly_in_no_other_case() {
        let fx = Fixture::new("exact-id");
        let session = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let dir = fx.dir();
        std::fs::create_dir_all(dir.join(session)).expect("session dir");
        let file = fx.write(session, &[fx.prompt(session, "hi", T0)], T0);
        let mut index = SessionIndex::new(fx.root.clone());
        assert_eq!(index.find_transcripts(session, false), vec![file.clone()]);
        let (transcripts, dirs) = index.scan_session(session);
        assert_eq!(transcripts.len(), 1);
        assert_eq!(transcripts[0].0, file);
        assert_eq!(dirs, vec![dir.join(session)]);
        let upper = session.to_uppercase();
        assert!(index.find_transcripts(&upper, false).is_empty());
        assert_eq!(index.scan_session(&upper), (Vec::new(), Vec::new()));
    }

    /// session index cost > pages through a project reading each transcript once
    #[test]
    fn paging_reads_each_transcript_once() {
        let fx = Fixture::new("paging");
        for n in 1..=5 {
            let session = id(n);
            let at = T0 + n as i64 * 60_000;
            fx.write(
                &session,
                &[
                    fx.prompt(&session, &format!("p{n}"), at),
                    fx.answer(&session, at + 1000),
                ],
                at + 2000,
            );
        }
        let mut index = SessionIndex::new(fx.root.clone());
        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let (rows, has_more) = index.list(&fx.query(2, after.clone()));
            seen.extend(rows.iter().map(|r| r.session_id.clone()));
            let Some(last) = rows.last().filter(|_| has_more) else {
                break;
            };
            after = Some(ListCursor {
                order_at_ms: last.order_at_ms,
                session_id: last.session_id.clone(),
            });
        }
        assert_eq!(seen, vec![id(5), id(4), id(3), id(2), id(1)]);
        assert!(
            index.transcripts_read() <= 6,
            "each transcript is read once (plus the page-full probe): {}",
            index.transcripts_read()
        );
        // The same list again reads nothing: the metadata cache holds.
        let before = index.transcripts_read();
        index.list(&fx.query(50, None));
        assert_eq!(index.transcripts_read(), before);
    }

    /// a continued session > is hidden once its successor has history, as in the SDK list
    #[test]
    fn a_continued_session_is_hidden_once_its_successor_has_history() {
        let fx = Fixture::new("continued");
        let (old, new) = (id(1), id(2));
        fx.write(
            &old,
            &[
                fx.prompt(&old, "start", T0),
                fx.answer(&old, T0 + 1000),
                json!({"type": "continued-in", "continuedInSessionId": new}),
            ],
            T0 + 2000,
        );
        let mut index = SessionIndex::new(fx.root.clone());
        let (rows, _) = index.list(&fx.query(10, None));
        assert_eq!(
            rows.iter()
                .map(|r| r.session_id.clone())
                .collect::<Vec<_>>(),
            vec![old.clone()],
            "no successor yet: the predecessor is listed"
        );
        let mut successor = fx.prompt(&new, "carry on", T0 + 5000);
        if let Some(map) = successor.as_object_mut() {
            map.insert("parentUuid".into(), json!("u1"));
        }
        fx.write(&new, &[successor, fx.answer(&new, T0 + 6000)], T0 + 7000);
        let mut fresh = SessionIndex::new(fx.root.clone());
        let (rows, _) = fresh.list(&fx.query(10, None));
        assert_eq!(
            rows.iter()
                .map(|r| r.session_id.clone())
                .collect::<Vec<_>>(),
            vec![new],
            "the successor has history: the predecessor is hidden"
        );
    }

    /// cwd recovery from a sibling transcript > does not depend on the batch the sibling is read in
    #[test]
    fn a_row_without_a_cwd_takes_the_requested_path_of_its_directory() {
        let fx = Fixture::new("sibling");
        let session = id(1);
        fx.write(
            &session,
            &[
                json!({"type": "user", "sessionId": session, "timestamp": js_text::to_iso_string(T0),
                    "message": {"role": "user", "content": "no cwd"}}),
                fx.answer(&session, T0 + 1000),
            ],
            T0 + 2000,
        );
        let mut index = SessionIndex::new(fx.root.clone());
        let (rows, _) = index.list(&fx.query(10, None));
        assert_eq!(rows[0].cwd, fx.cwd);
    }

    /// is the project directory of the cwd on a case-insensitive volume
    #[test]
    fn a_differing_case_directory_is_the_project_directory_on_darwin_only() {
        let fx = Fixture::new("case");
        let session = id(1);
        let upper_dir = fx
            .root
            .join(encode_project_path(&fx.cwd, Platform::Other).to_uppercase());
        std::fs::create_dir_all(&upper_dir).expect("dir");
        let path = upper_dir.join(format!("{session}.jsonl"));
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                fx.prompt(&session, "cased", T0),
                fx.answer(&session, T0 + 1000)
            ),
        )
        .expect("write");
        // Case-sensitive platforms do not see the directory...
        let mut other = SessionIndex::with_platform(fx.root.clone(), Platform::Other);
        let (rows, _) = other.list(&fx.query(10, None));
        let case_sensitive_disk =
            std::fs::metadata(fx.root.join(encode_project_path(&fx.cwd, Platform::Other))).is_err();
        if case_sensitive_disk {
            assert!(rows.is_empty());
            // ...darwin rules need the file system to resolve the exact name to the
            // directory, which a case-sensitive volume does not.
            let mut darwin = SessionIndex::with_platform(fx.root.clone(), Platform::Darwin);
            assert!(darwin.list(&fx.query(10, None)).0.is_empty());
        } else {
            let mut darwin = SessionIndex::with_platform(fx.root.clone(), Platform::Darwin);
            let (rows, _) = darwin.list(&fx.query(10, None));
            assert_eq!(
                rows.len(),
                1,
                "the differing-case directory is the project directory"
            );
        }
    }
}
