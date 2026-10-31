//! `_session/list/subscribe`: pushes the changed rows of a cwd's session list to
//! a `sessionIndex` client (upstream `session-index/list-subscriptions.ts`).
//!
//! A subscription covers the sessions of its cwd and of the same subdirectory in
//! every linked worktree (the `includeWorktrees` scope of the list), in any
//! archive state. All subscriptions of one cwd share one [`Watch`]: the project
//! directories of the scope, the stats of its transcripts, and the row of each
//! session read since. The registry and the projects root are watched once per
//! connection.
//!
//! Subscribe reads no row: it stats the transcripts and reads the registry. A row
//! is read on the first event of its session and sent unconditionally; later rows
//! are compared with it.
//!
//! - A transcript event names the file: only that file is stat'ed and, when it
//!   changed, its metadata read again (the index's metadata cache and tail scan).
//! - A registry event names the record: only that record is read, and only the
//!   sessions whose record changed get their `state` recomputed.
//! - A session this connection runs reports its SDK state at once.
//! - A session whose transcripts this connection wrote (a rename, archive,
//!   unarchive or delete) is stat'ed and read again at once, without waiting for
//!   its transcript event.
//!
//! Events are coalesced: 150 ms after the last one, at most 1 s after the first.
//! Each subscription compares a recomputed row with the last row it sent; a change
//! of `updatedAt` alone is no change. A session is sent at most once a second per
//! subscription. A rescan every 10 s stats the scope again and reads the
//! transcripts whose stat changed (or whose last read failed), which covers events
//! the event source missed; the state of a live session ages from the metadata the
//! index holds.
//!
//! The event source differs from upstream: Node's `fs.watch` becomes a stat poll
//! (the crate has no file watching dependency). The poll reports the same things
//! an event names: a transcript that appeared, changed or went, a registry
//! record, a project directory of the scope. It costs one stat per watched
//! directory per tick (the registry's included), a listing and the new entries'
//! stats only for a directory whose stat changed, and [`FILE_POLL_BUDGET`] stats
//! in rotation for the content changes a directory's stat cannot show (an
//! appended transcript). A tick never copies the baseline: the actor hands it to
//! the poll and takes it back, the poll runs on a blocking thread (never on the
//! index worker), and the next tick is a full interval after the previous one
//! ended.
//!
//! Delivery is best effort: there is no resync and no sequence number; the client
//! also reads the list now and then.
//!
//! The subscriptions are one actor task: every event is a message, every timer is
//! a deadline the task sleeps to, and the work of a pass runs to its end before the
//! next message.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::Error;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

use super::activity::OwnSessionState;
use super::live_registry::{by_session, LiveRecord, LiveSnapshot};
use super::project_dirs::{is_absolute, is_exact_project_dir, is_session_id, project_dir_matches};
use super::service::{internal_error, invalid_params, BoxFuture, IndexCore, IndexWorker};
use super::session_info::{change_signature, session_info_of};
use super::worktrees::resolve;
use super::{IndexRow, TranscriptCandidate};

pub const LIST_SUBSCRIBE_METHOD: &str = "_session/list/subscribe";
pub const LIST_UNSUBSCRIBE_METHOD: &str = "_session/list/unsubscribe";
pub const LIST_CHANGES_METHOD: &str = "_session/list/changes";

/// Subscriptions per connection.
pub const MAX_SUBSCRIPTIONS: usize = 128;
const DEBOUNCE_MS: u64 = 150;
const MAX_WAIT_MS: u64 = 1_000;
const RESCAN_MS: u64 = 10_000;
/// When a rescan follows the opening of a directory watcher.
const SETTLE_MS: u64 = 1_000;
/// The least time between two changes of one session to one subscription.
const MIN_SESSION_INTERVAL_MS: u64 = 1_000;
/// How often the watched directories are stat'ed for events.
const POLL_MS: u64 = 100;

type LogError = Arc<dyn Fn(&str, &str) + Send + Sync>;
type Notify = Arc<dyn Fn(Value) -> BoxFuture<Result<(), Error>> + Send + Sync>;
type OwnLookup = Arc<dyn Fn(&str) -> Option<OwnSessionState> + Send + Sync>;

// ---- fork holds ----------------------------------------------------------------

/// The `session/fork` requests in flight. A client must get the id of a fork from
/// the fork response before a list shows the fork. Otherwise the client sees an
/// unknown session and lists it as a session of its own.
#[derive(Clone, Debug, Default)]
pub struct ForkHolds {
    /// Source session id -> the number of its forks in flight, and the forks they
    /// copied the transcript into.
    sources: HashMap<String, (usize, HashSet<String>)>,
}

impl ForkHolds {
    pub fn started(&mut self, source_id: &str) {
        self.sources
            .entry(source_id.to_string())
            .and_modify(|(count, _)| *count += 1)
            .or_insert_with(|| (1, HashSet::new()));
    }

    /// A fork of `source_id` copied the transcript into `fork_id`.
    pub fn created(&mut self, source_id: &str, fork_id: &str) {
        if let Some((_, forks)) = self.sources.get_mut(source_id) {
            forks.insert(fork_id.to_string());
        }
    }

    /// The response of a fork of `source_id` is out. Returns whether that freed
    /// rows: the last fork of the source answered.
    pub fn answered(&mut self, source_id: &str) -> bool {
        let Some((count, _)) = self.sources.get_mut(source_id) else {
            return false;
        };
        *count -= 1;
        if *count > 0 {
            return false;
        }
        self.sources.remove(source_id);
        true
    }

    /// Whether `id` is a fork whose fork response is not out yet.
    pub fn is_held(&self, id: &str) -> bool {
        self.sources.values().any(|(_, forks)| forks.contains(id))
    }

    /// Whether the row of `id` waits. A fork that a fork in flight created waits.
    /// Before the copy returns its id, an unsent row of a fork of a source in
    /// flight waits too, by its `forkedFrom`.
    pub fn holds(&self, id: &str, sent: bool, forked_from: Option<&str>) -> bool {
        if self.is_held(id) {
            return true;
        }
        !sent && forked_from.is_some_and(|source| self.sources.contains_key(source))
    }
}

/// [`ForkHolds`] shared by the service and the subscriptions.
#[derive(Clone)]
pub struct SharedForkHolds {
    inner: Arc<watch::Sender<ForkHolds>>,
}

impl Default for SharedForkHolds {
    fn default() -> Self {
        Self {
            inner: Arc::new(watch::channel(ForkHolds::default()).0),
        }
    }
}

impl SharedForkHolds {
    pub fn started(&self, source_id: &str) {
        self.inner.send_modify(|holds| holds.started(source_id));
    }

    pub fn created(&self, source_id: &str, fork_id: &str) {
        self.inner
            .send_modify(|holds| holds.created(source_id, fork_id));
    }

    pub fn answered(&self, source_id: &str) -> bool {
        let mut freed = false;
        self.inner
            .send_modify(|holds| freed = holds.answered(source_id));
        freed
    }

    pub fn is_held(&self, id: &str) -> bool {
        self.inner.borrow().is_held(id)
    }

    pub fn holds(&self, id: &str, sent: bool, forked_from: Option<&str>) -> bool {
        self.inner.borrow().holds(id, sent, forked_from)
    }
}

// ---- requests ----------------------------------------------------------------

/// `{ cwd }`: an absolute path. Other parameters are ignored.
pub fn parse_list_subscribe_request(value: &Value) -> Result<String, Error> {
    match value.get("cwd").and_then(Value::as_str) {
        Some(cwd) if is_absolute(cwd, super::project_dirs::Platform::host()) => Ok(cwd.to_string()),
        _ => Err(invalid_params(
            Some(json!({ "cwd": value.get("cwd") })),
            "params require an absolute cwd",
        )),
    }
}

/// `{ subscriptionId }`.
pub fn parse_list_unsubscribe_request(value: &Value) -> Result<String, Error> {
    match value.get("subscriptionId").and_then(Value::as_str) {
        Some(id) => Ok(id.to_string()),
        None => Err(invalid_params(None, "params require a subscriptionId")),
    }
}

// ---- timing and dependencies -------------------------------------------------------

/// The timing of the subscriptions (tests shorten it).
#[derive(Clone, Copy, Debug)]
pub struct ListSubscriptionTiming {
    pub debounce_ms: u64,
    pub max_wait_ms: u64,
    pub rescan_ms: u64,
    pub settle_ms: u64,
    pub min_session_interval_ms: u64,
    /// How often the event source stats the watched directories.
    pub poll_ms: u64,
}

impl Default for ListSubscriptionTiming {
    fn default() -> Self {
        Self {
            debounce_ms: DEBOUNCE_MS,
            max_wait_ms: MAX_WAIT_MS,
            rescan_ms: RESCAN_MS,
            settle_ms: SETTLE_MS,
            min_session_interval_ms: MIN_SESSION_INTERVAL_MS,
            poll_ms: POLL_MS,
        }
    }
}

pub struct ListSubscriptionDeps {
    pub worker: IndexWorker,
    pub fork_holds: SharedForkHolds,
    pub projects_root: PathBuf,
    pub registry_dir: PathBuf,
    /// What this connection knows of a session it runs.
    pub own: OwnLookup,
    /// Fires when any session this connection runs changed.
    pub own_changes: Option<watch::Receiver<HashMap<String, OwnSessionState>>>,
    pub notify: Notify,
    pub log_error: LogError,
    pub timing: ListSubscriptionTiming,
}

// ---- the actor's state ---------------------------------------------------------------

struct RowEntry {
    row: IndexRow,
    info: Value,
    signature: String,
}

struct WatchedDir {
    project_path: String,
}

#[derive(Default)]
struct Subscription {
    id: String,
    watch_key: String,
    /// When it subscribed, on the sequence clock.
    since: u64,
    /// What was last sent per session: the row's signature, or `None` for a
    /// removal. A session without an entry was never sent: its next change is sent
    /// whatever it is.
    sent: HashMap<String, Option<String>>,
    sent_at: HashMap<String, f64>,
    /// Sessions to compare with `sent` at the next delivery.
    pending: HashSet<String>,
    /// Fires when a session held back by the interval may be sent.
    timer_at: Option<Instant>,
}

#[derive(Default)]
struct Watch {
    cwd: String,
    paths: Vec<String>,
    /// Project directory name -> the scope path it belongs to.
    dirs: HashMap<String, WatchedDir>,
    /// Every non-empty transcript file of the scope, as last stat'ed.
    files: HashMap<PathBuf, TranscriptCandidate>,
    /// The file paths of each session.
    files_by_id: HashMap<String, HashSet<PathBuf>>,
    /// The sessions whose first transcript appeared after the watch was ready: a
    /// subscription older than that never had them listed.
    born: HashMap<String, u64>,
    /// The current row of each session in the scope that was read since the watch
    /// started.
    rows: HashMap<String, RowEntry>,
    subscriptions: HashSet<String>,
    ready: bool,
    /// When it became ready: about when its client listed the sessions.
    ready_at: f64,
    dirty_files: HashSet<PathBuf>,
    dirty_rows: HashSet<String>,
    dirty_states: HashSet<String>,
    rescan: bool,
    /// The session each session read here is continued in, by its metadata.
    successor_of: HashMap<String, String>,
    /// Live continued sessions that a seed found hidden by their successor, with
    /// the stats of both then.
    hidden_seeds: HashMap<String, String>,
    /// The scope paths the rows were last resolved against.
    resolved_paths: String,
    /// Sessions whose transcript could not be read: kept as they were, and read
    /// again by the next rescan.
    unreadable: HashSet<String>,
    debounce_at: Option<Instant>,
    max_wait_at: Option<Instant>,
    settle_at: Option<Instant>,
    /// Work asked for: a full pass now, or a states-only pass.
    want_files: bool,
    want_states: bool,
}

enum Cmd {
    Subscribe {
        cwd: String,
        reply: oneshot::Sender<Result<String, Error>>,
    },
    Unsubscribe(String),
    OwnChanged(String),
    SessionWritten(String),
    DeliverPending,
    Dispose,
}

/// The handle of the subscriptions of one connection.
#[derive(Clone)]
pub struct ListSubscriptions {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl ListSubscriptions {
    pub fn spawn(deps: ListSubscriptionDeps) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let actor = Actor::new(deps);
        tokio::spawn(actor.run(rx));
        Self { tx }
    }

    /// Subscribes to the sessions of `cwd`. Changes are tracked from the return
    /// on. Reads no row.
    pub async fn subscribe(&self, cwd: &str) -> Result<Value, Error> {
        let (reply, result) = oneshot::channel();
        self.tx
            .send(Cmd::Subscribe {
                cwd: cwd.to_string(),
                reply,
            })
            .map_err(|_| internal_error("The connection is closed"))?;
        let id = result
            .await
            .map_err(|_| internal_error("The connection is closed"))??;
        Ok(json!({ "subscriptionId": id }))
    }

    /// Idempotent: an unknown id is no error.
    pub fn unsubscribe(&self, subscription_id: &str) {
        let _ = self.tx.send(Cmd::Unsubscribe(subscription_id.to_string()));
    }

    /// A session this connection runs changed its state, turn end or cost.
    pub fn own_session_changed(&self, session_id: &str) {
        let _ = self.tx.send(Cmd::OwnChanged(session_id.to_string()));
    }

    /// This connection wrote the transcripts of a session: a rename, archive,
    /// unarchive or delete.
    pub fn session_written(&self, session_id: &str) {
        let _ = self.tx.send(Cmd::SessionWritten(session_id.to_string()));
    }

    /// Sends the pending sessions now: [`ForkHolds`] held some back.
    pub fn deliver_pending(&self) {
        let _ = self.tx.send(Cmd::DeliverPending);
    }

    pub fn dispose(&self) {
        let _ = self.tx.send(Cmd::Dispose);
    }
}

fn scope_key(paths: &[String]) -> String {
    let mut sorted = paths.to_vec();
    sorted.sort();
    sorted.join("\0")
}

/// The session id of a transcript file name, else `None`.
fn transcript_id(filename: &str) -> Option<String> {
    let id = filename.strip_suffix(".jsonl")?;
    is_session_id(id).then(|| id.to_string())
}

fn is_record_file(name: &str) -> bool {
    name.strip_suffix(".json")
        .is_some_and(|stem| !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()))
}

/// The files of a scope as one enumeration found them, with what the index knows
/// of each.
struct Enumerated {
    paths: Vec<String>,
    dirs: Vec<(String, String)>,
    files: Vec<FileInfo>,
}

struct FileInfo {
    candidate: TranscriptCandidate,
    continued_in: Option<String>,
    is_read: bool,
}

/// A polled file as the event source compares it: mtime, size, and the identity
/// that tells a file replaced by another apart from one that grew.
type StatKey = (f64, u64, u64);

/// A directory's stat: its mtime, size and identity. An entry added, removed or
/// renamed changes it; appending to a transcript does not.
type DirSig = StatKey;

/// The file system reads of the event source. A test replaces it to count the
/// stats of a tick.
trait PollFs {
    fn stat(&self, path: &Path) -> Option<super::dir_listing::StatInfo>;
    fn read_dir(&self, dir: &Path) -> Vec<String>;
}

/// The real file system.
struct RealFs;

impl PollFs for RealFs {
    fn stat(&self, path: &Path) -> Option<super::dir_listing::StatInfo> {
        super::dir_listing::stat(path)
    }

    fn read_dir(&self, dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|read| {
                read.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// How many known files a tick stats for a content change (an appended
/// transcript). The files rotate, so every one of them is stat'ed again after
/// `ceil(files / FILE_POLL_BUDGET)` ticks, and the 10 s rescan covers what the
/// rotation has not reached yet. A directory's own stat, and the stats of the
/// entries that appeared in it, are outside this budget.
const FILE_POLL_BUDGET: usize = 256;

/// A file that changed stays in the hot set (stat'ed every tick) for this many
/// ticks (30 s at the 100 ms poll) after its last change.
const HOT_TICKS: u64 = 300;

/// The hot set holds at most this many files.
const HOT_MAX: usize = 256;

/// What one tick of the event source found.
#[derive(Default)]
struct Polled {
    /// The transcripts and registry records that appeared, changed or went, with
    /// their stat now (`None` when gone).
    changed: Vec<(PathBuf, Option<StatKey>)>,
    /// The project directory names made or removed under the projects root.
    root_changed: Vec<String>,
}

/// A watched directory as the last tick found it.
struct DirBaseline {
    /// `None` for a directory that does not exist (yet).
    sig: Option<DirSig>,
    /// Every entry of the directory.
    names: HashSet<String>,
    /// The polled files of the directory (transcripts, or registry records).
    files: HashSet<PathBuf>,
}

/// The projects root as the last tick found it: its entries are project
/// directories, so only their names matter.
struct RootBaseline {
    sig: Option<DirSig>,
    names: HashSet<String>,
}

/// What the event source compares against: each watched directory's stat and
/// entries, the projects root's listing, and the rotation over the files it
/// knows. The actor hands it to the poll and takes it back, so a tick never
/// copies a map.
#[derive(Default)]
struct PollBaseline {
    dirs: HashMap<PathBuf, DirBaseline>,
    root: Option<RootBaseline>,
    /// Every polled file (transcripts and registry records), as last stat'ed.
    files: HashMap<PathBuf, StatKey>,
    /// The files in rotation order. A file that went stays here until the order
    /// is rebuilt, and is skipped when the rotation reaches it.
    order: Vec<PathBuf>,
    cursor: usize,
    /// Ticks the rotation has run, the clock of `hot`.
    tick: u64,
    /// Files that changed lately, by the tick of their last change.
    hot: HashMap<PathBuf, u64>,
}

impl PollBaseline {
    /// Stats `dir` and reads its entries when its stat changed, reporting the
    /// polled files that appeared or went. A directory seen for the first time
    /// only becomes the baseline: the watch stat'ed its transcripts itself.
    fn scan_dir(
        &mut self,
        fs: &dyn PollFs,
        dir: &Path,
        registry: bool,
        polled: &mut Polled,
        reported: &mut HashSet<PathBuf>,
    ) {
        let sig = dir_sig(fs, dir);
        let first = !self.dirs.contains_key(dir);
        let before_names: HashSet<String> = self
            .dirs
            .get(dir)
            .map(|d| d.names.clone())
            .unwrap_or_default();
        let before_files: HashSet<PathBuf> = self
            .dirs
            .get(dir)
            .map(|d| d.files.clone())
            .unwrap_or_default();
        let mut names: HashSet<String> = HashSet::new();
        let mut files: HashSet<PathBuf> = HashSet::new();
        if sig.is_some() {
            for name in fs.read_dir(dir) {
                let polled_name = if registry {
                    is_record_file(&name)
                } else {
                    transcript_id(&name).is_some()
                };
                names.insert(name.clone());
                if !polled_name {
                    continue;
                }
                let path = dir.join(&name);
                if !first && before_names.contains(&name) && self.files.contains_key(&path) {
                    // Its content is the rotation's business: keep the stat the
                    // baseline holds, so the next tick does not report it twice.
                    // A name whose stat failed before is not in `files`, so it
                    // falls through and is stat'ed again.
                    files.insert(path);
                    continue;
                }
                let Some(stat) = fs.stat(&path) else {
                    continue;
                };
                if !stat.is_file {
                    continue;
                }
                let key = (stat.mtime_ms, stat.size, stat.identity);
                files.insert(path.clone());
                let known = self.files.insert(path.clone(), key);
                if known.is_none() {
                    // A file that appeared after the first scan joins the
                    // rotation at once, so its appends are seen as fast as a
                    // pre-existing file's.
                    self.order.push(path.clone());
                }
                if !first && known != Some(key) && reported.insert(path.clone()) {
                    self.mark_hot(&path);
                    polled.changed.push((path, Some(key)));
                }
            }
        }
        if !first {
            for path in before_files.difference(&files) {
                self.files.remove(path);
                if reported.insert(path.clone()) {
                    polled.changed.push((path.clone(), None));
                }
            }
        }
        self.dirs
            .insert(dir.to_path_buf(), DirBaseline { sig, names, files });
    }

    /// Stats up to `budget` known files, wrapping around, and reports the ones
    /// that changed. A directory whose stat changed was scanned already; its
    /// files are not stat'ed again here.
    fn rotate(
        &mut self,
        fs: &dyn PollFs,
        budget: usize,
        registry_dir: &Path,
        polled: &mut Polled,
        reported: &mut HashSet<PathBuf>,
    ) {
        self.tick = self.tick.wrapping_add(1);
        // The hot set: files that changed lately are stat'ed every tick, so an
        // active session is noticed as fast as `fs.watch` would, whatever the
        // size of the rotation. Expired entries leave first.
        let tick = self.tick;
        self.hot.retain(|_, at| tick.wrapping_sub(*at) <= HOT_TICKS);
        let mut hot: Vec<PathBuf> = self.hot.keys().cloned().collect();
        hot.sort();
        for path in hot {
            if reported.contains(&path) || !self.files.contains_key(&path) {
                continue;
            }
            self.check_file(fs, path, polled, reported);
        }
        // The live registry records are stat'ed every tick, hot or not: there is one
        // per live CLI, and a session quiet for minutes that wakes (idle -> busy) is
        // noticed as fast as upstream's `fs.watch` would, not a rotation later.
        let mut records: Vec<PathBuf> = self
            .dirs
            .get(registry_dir)
            .map(|dir| dir.files.iter().cloned().collect())
            .unwrap_or_default();
        records.sort();
        for path in records {
            if reported.contains(&path) || !self.files.contains_key(&path) {
                continue;
            }
            self.check_file(fs, path, polled, reported);
        }
        if self.order.is_empty() {
            return;
        }
        // A file that went leaves a hole; rebuild once the holes outnumber the
        // files. Sorting keeps a tick's files, and so a test's counts, stable.
        if self.order.len() > self.files.len() * 2 + 8 {
            self.order = self.files.keys().cloned().collect();
            self.order.sort();
            self.cursor = 0;
        }
        if self.order.is_empty() {
            return;
        }
        let mut left = budget;
        while left > 0 {
            let len = self.order.len();
            let path = self.order[self.cursor % len].clone();
            self.cursor = self.cursor.wrapping_add(1);
            left -= 1;
            if reported.contains(&path) || !self.files.contains_key(&path) {
                continue;
            }
            self.check_file(fs, path, polled, reported);
        }
    }

    /// Stats one known file and reports it when it changed or went.
    fn check_file(
        &mut self,
        fs: &dyn PollFs,
        path: PathBuf,
        polled: &mut Polled,
        reported: &mut HashSet<PathBuf>,
    ) {
        match fs.stat(&path).filter(|s| s.is_file) {
            Some(stat) => {
                let key = (stat.mtime_ms, stat.size, stat.identity);
                if self.files.get(&path) != Some(&key) {
                    self.files.insert(path.clone(), key);
                    self.mark_hot(&path);
                    polled.changed.push((path.clone(), Some(key)));
                    reported.insert(path);
                }
            }
            None => {
                self.files.remove(&path);
                self.hot.remove(&path);
                polled.changed.push((path.clone(), None));
                reported.insert(path);
            }
        }
    }

    /// Puts a file that just changed into the hot set, dropping the stalest
    /// entry when the set is full.
    fn mark_hot(&mut self, path: &Path) {
        if self.hot.len() >= HOT_MAX && !self.hot.contains_key(path) {
            if let Some(oldest) = self
                .hot
                .iter()
                .min_by_key(|(p, at)| (**at, (*p).clone()))
                .map(|(p, _)| p.clone())
            {
                self.hot.remove(&oldest);
            }
        }
        self.hot.insert(path.to_path_buf(), self.tick);
    }

    /// Reads the projects root's listing when its stat changed.
    fn scan_root(&mut self, fs: &dyn PollFs, root: &Path, polled: &mut Polled) {
        let sig = dir_sig(fs, root);
        match &self.root {
            None => {
                let names = fs.read_dir(root).into_iter().collect();
                self.root = Some(RootBaseline { sig, names });
            }
            Some(previous) if previous.sig == sig => {}
            Some(previous) => {
                let names: HashSet<String> = fs.read_dir(root).into_iter().collect();
                let mut changed: Vec<String> = names
                    .symmetric_difference(&previous.names)
                    .cloned()
                    .collect();
                changed.sort();
                polled.root_changed = changed;
                self.root = Some(RootBaseline { sig, names });
            }
        }
    }

    /// Drops the directories no watch watches any more, and their files.
    fn retain_dirs(&mut self, wanted: &HashSet<PathBuf>) {
        let gone: Vec<PathBuf> = self
            .dirs
            .keys()
            .filter(|dir| !wanted.contains(*dir))
            .cloned()
            .collect();
        for dir in gone {
            if let Some(entry) = self.dirs.remove(&dir) {
                for path in entry.files {
                    self.files.remove(&path);
                }
            }
        }
    }
}

fn dir_sig(fs: &dyn PollFs, dir: &Path) -> Option<DirSig> {
    fs.stat(dir)
        .filter(|s| s.is_dir)
        .map(|s| (s.mtime_ms, s.size, s.identity))
}

/// One tick of the event source (the replacement for `fs.watch`): each watched
/// directory's stat first, a listing and the new entries' stats only for the
/// directories that changed, then [`FILE_POLL_BUDGET`] known files in rotation
/// (an appended transcript), and the projects root's listing when it changed.
fn poll_once(
    fs: &dyn PollFs,
    root: &Path,
    registry_dir: &Path,
    dirs: &[PathBuf],
    mut baseline: PollBaseline,
) -> (Polled, PollBaseline) {
    let mut polled = Polled::default();
    let mut reported: HashSet<PathBuf> = HashSet::new();
    let wanted: HashSet<PathBuf> = dirs.iter().cloned().collect();
    baseline.retain_dirs(&wanted);
    for dir in dirs {
        let registry = dir == registry_dir;
        let changed = match baseline.dirs.get(dir) {
            None => true,
            Some(entry) => entry.sig != dir_sig(fs, dir),
        };
        if changed {
            baseline.scan_dir(fs, dir, registry, &mut polled, &mut reported);
        }
    }
    baseline.rotate(
        fs,
        FILE_POLL_BUDGET,
        registry_dir,
        &mut polled,
        &mut reported,
    );
    baseline.scan_root(fs, root, &mut polled);
    (polled, baseline)
}

struct Actor {
    deps: ListSubscriptionDeps,
    subscriptions: HashMap<String, Subscription>,
    /// Sessions whose row shows this connection's SDK state: the rescan presents
    /// them again, in case a change of it was not reported.
    own_shown: HashSet<String>,
    watches: HashMap<String, Watch>,
    rescan_at: Option<Instant>,
    poll_at: Option<Instant>,
    /// The live records by registry file name.
    live_files: HashMap<String, LiveRecord>,
    live: LiveSnapshot,
    registry_loaded: bool,
    registry_names: HashSet<String>,
    registry_full: bool,
    registry_debounce_at: Option<Instant>,
    registry_max_wait_at: Option<Instant>,
    registry_now: bool,
    disposed: bool,
    /// Orders subscriptions and session births.
    seq: u64,
    /// The event source's baseline: each watched directory's stat and entries,
    /// the projects root's listing, and the rotation over the files it knows.
    /// Handed to the poll and taken back, so a tick copies no map.
    poll_baseline: PollBaseline,
    started_at: Instant,
    epoch_at_start: f64,
    own_seen: HashMap<String, OwnSessionState>,
}

impl Actor {
    fn new(deps: ListSubscriptionDeps) -> Self {
        let epoch_at_start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64() * 1000.0);
        let own_seen = deps
            .own_changes
            .as_ref()
            .map(|rx| rx.borrow().clone())
            .unwrap_or_default();
        Self {
            deps,
            subscriptions: HashMap::new(),
            own_shown: HashSet::new(),
            watches: HashMap::new(),
            rescan_at: None,
            poll_at: None,
            live_files: HashMap::new(),
            live: HashMap::new(),
            registry_loaded: false,
            registry_names: HashSet::new(),
            registry_full: false,
            registry_debounce_at: None,
            registry_max_wait_at: None,
            registry_now: false,
            disposed: false,
            seq: 0,
            poll_baseline: PollBaseline::default(),
            started_at: Instant::now(),
            epoch_at_start,
            own_seen,
        }
    }

    /// Epoch milliseconds, advancing with the (possibly paused) runtime clock.
    fn now(&self) -> f64 {
        self.epoch_at_start + self.started_at.elapsed().as_secs_f64() * 1000.0
    }

    fn ms(&self, ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    fn log(&self, message: &str, error: &str) {
        (self.deps.log_error)(message, error);
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        let mut own_changes = self.deps.own_changes.clone();
        loop {
            self.run_ready().await;
            if self.disposed {
                return;
            }
            let deadline = self.next_deadline();
            tokio::select! {
                biased;
                cmd = rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd).await,
                    None => {
                        self.dispose();
                        return;
                    }
                },
                changed = async {
                    match own_changes.as_mut() {
                        Some(rx) => rx.changed().await.is_ok(),
                        None => std::future::pending().await,
                    }
                } => {
                    if changed {
                        self.own_board_changed();
                    } else {
                        own_changes = None;
                    }
                },
                () = async {
                    match deadline {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => self.fire_due().await,
            }
        }
    }

    async fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Subscribe { cwd, reply } => {
                let result = self.subscribe(&cwd).await;
                let _ = reply.send(result);
            }
            Cmd::Unsubscribe(id) => self.unsubscribe(&id),
            Cmd::OwnChanged(id) => self.own_session_changed(&id),
            Cmd::SessionWritten(id) => self.session_written(&id),
            Cmd::DeliverPending => {
                let ids: Vec<String> = self
                    .subscriptions
                    .iter()
                    .filter(|(_, s)| !s.pending.is_empty())
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in ids {
                    self.deliver(&id);
                }
            }
            Cmd::Dispose => self.dispose(),
        }
    }

    /// The sessions whose own state differs from the last time the board was read.
    fn own_board_changed(&mut self) {
        let Some(rx) = self.deps.own_changes.as_ref() else {
            return;
        };
        let current = rx.borrow().clone();
        let mut changed: Vec<String> = Vec::new();
        for (id, state) in &current {
            if self.own_seen.get(id) != Some(state) {
                changed.push(id.clone());
            }
        }
        for id in self.own_seen.keys() {
            if !current.contains_key(id) {
                changed.push(id.clone());
            }
        }
        self.own_seen = current;
        for id in changed {
            self.own_session_changed(&id);
        }
    }

    // --- subscribe ------------------------------------------------------------

    async fn subscribe(&mut self, cwd: &str) -> Result<String, Error> {
        if self.disposed {
            return Err(internal_error("The connection is closed"));
        }
        if self.subscriptions.len() >= MAX_SUBSCRIPTIONS {
            return Err(invalid_params(
                Some(json!({ "reason": "too_many_subscriptions", "max": MAX_SUBSCRIPTIONS })),
                &format!("At most {MAX_SUBSCRIPTIONS} session list subscriptions per connection"),
            ));
        }
        let base = std::env::current_dir().unwrap_or_default();
        let key = resolve(&base, Path::new(cwd))
            .to_string_lossy()
            .into_owned();
        if !self.watches.contains_key(&key) {
            self.watches.insert(
                key.clone(),
                Watch {
                    cwd: key.clone(),
                    ..Watch::default()
                },
            );
            self.ensure_shared().await;
            if let Err(error) = self.start_watch(&key).await {
                self.close_watch(&key);
                return Err(error);
            }
        }
        self.seq += 1;
        let id = crate::agent::uuid_v4();
        self.subscriptions.insert(
            id.clone(),
            Subscription {
                id: id.clone(),
                watch_key: key.clone(),
                since: self.seq,
                ..Subscription::default()
            },
        );
        if let Some(watch) = self.watches.get_mut(&key) {
            watch.subscriptions.insert(id.clone());
        }
        Ok(id)
    }

    /// Idempotent: an unknown id is no error.
    fn unsubscribe(&mut self, id: &str) {
        let Some(subscription) = self.subscriptions.remove(id) else {
            return;
        };
        let key = subscription.watch_key;
        let empty = self.watches.get_mut(&key).is_some_and(|watch| {
            watch.subscriptions.remove(id);
            watch.subscriptions.is_empty()
        });
        if empty {
            self.close_watch(&key);
        }
    }

    fn dispose(&mut self) {
        self.disposed = true;
        let ids: Vec<String> = self.subscriptions.keys().cloned().collect();
        for id in ids {
            self.unsubscribe(&id);
        }
        self.stop_shared();
    }

    fn close_watch(&mut self, key: &str) {
        let Some(watch) = self.watches.remove(key) else {
            return;
        };
        for id in &watch.subscriptions {
            self.subscriptions.remove(id);
        }
        if self.watches.is_empty() {
            self.stop_shared();
        }
    }

    /// The registry and the projects root, polled once for all cwds, and the
    /// rescan.
    async fn ensure_shared(&mut self) {
        if self.disposed || self.watches.is_empty() {
            return;
        }
        if !self.registry_loaded {
            self.registry_loaded = true;
            if let Err(error) = self.read_registry(true).await {
                self.log("session list registry read failed", &error.message);
            }
            // The projects root and the registry as they are now: only later
            // changes are events.
            self.poll().await;
        }
        if self.rescan_at.is_none() {
            self.rescan_at = Some(Instant::now() + self.ms(self.deps.timing.rescan_ms));
        }
        if self.poll_at.is_none() {
            self.poll_at = Some(Instant::now() + self.ms(self.deps.timing.poll_ms));
        }
    }

    fn stop_shared(&mut self) {
        self.own_shown.clear();
        self.rescan_at = None;
        self.poll_at = None;
        self.registry_debounce_at = None;
        self.registry_max_wait_at = None;
        self.registry_now = false;
        self.registry_loaded = false;
        self.registry_names.clear();
        self.registry_full = false;
        self.live_files.clear();
        self.live.clear();
        self.poll_baseline = PollBaseline::default();
    }

    // --- scheduling -----------------------------------------------------------

    fn next_deadline(&self) -> Option<Instant> {
        let mut deadlines: Vec<Instant> = Vec::new();
        deadlines.extend(self.rescan_at);
        deadlines.extend(self.poll_at);
        deadlines.extend(self.registry_debounce_at);
        deadlines.extend(self.registry_max_wait_at);
        for watch in self.watches.values() {
            deadlines.extend(watch.debounce_at);
            deadlines.extend(watch.max_wait_at);
            deadlines.extend(watch.settle_at);
        }
        for subscription in self.subscriptions.values() {
            deadlines.extend(subscription.timer_at);
        }
        deadlines.into_iter().min()
    }

    async fn fire_due(&mut self) {
        let now = Instant::now();
        let due = |at: &Option<Instant>| at.is_some_and(|at| at <= now);
        let keys: Vec<String> = self.watches.keys().cloned().collect();
        for key in &keys {
            let Some(watch) = self.watches.get_mut(key) else {
                continue;
            };
            if due(&watch.debounce_at) || due(&watch.max_wait_at) {
                self.schedule(key, Mode::Now);
            }
            if let Some(watch) = self.watches.get_mut(key) {
                if due(&watch.settle_at) {
                    watch.settle_at = None;
                    watch.rescan = true;
                    self.schedule(key, Mode::Now);
                }
            }
        }
        if due(&self.registry_debounce_at) || due(&self.registry_max_wait_at) {
            self.registry_debounce_at = None;
            self.registry_max_wait_at = None;
            self.registry_now = true;
        }
        let timers: Vec<String> = self
            .subscriptions
            .iter()
            .filter(|(_, s)| due(&s.timer_at))
            .map(|(id, _)| id.clone())
            .collect();
        for id in timers {
            self.deliver(&id);
        }
        if due(&self.rescan_at) {
            self.rescan_at = Some(Instant::now() + self.ms(self.deps.timing.rescan_ms));
            self.rescan_all().await;
        }
        if due(&self.poll_at) {
            // No back-to-back ticks: the next one is a full interval after this
            // one ends, however long it took.
            self.poll_at = None;
            self.poll().await;
            if !self.watches.is_empty() && !self.disposed {
                self.poll_at = Some(Instant::now() + self.ms(self.deps.timing.poll_ms));
            }
        }
    }

    fn schedule(&mut self, key: &str, mode: Mode) {
        let debounce = self.ms(self.deps.timing.debounce_ms);
        let max_wait = self.ms(self.deps.timing.max_wait_ms);
        let Some(watch) = self.watches.get_mut(key) else {
            return;
        };
        match mode {
            Mode::States => watch.want_states = true,
            Mode::Now => {
                watch.debounce_at = None;
                watch.max_wait_at = None;
                watch.want_files = true;
            }
            Mode::Debounced => {
                watch.debounce_at = Some(Instant::now() + debounce);
                if watch.max_wait_at.is_none() {
                    watch.max_wait_at = Some(Instant::now() + max_wait);
                }
            }
        }
    }

    fn schedule_registry(&mut self, now: bool) {
        if self.disposed || self.watches.is_empty() {
            return;
        }
        if now {
            self.registry_debounce_at = None;
            self.registry_max_wait_at = None;
            self.registry_now = true;
            return;
        }
        self.registry_debounce_at = Some(Instant::now() + self.ms(self.deps.timing.debounce_ms));
        if self.registry_max_wait_at.is_none() {
            self.registry_max_wait_at =
                Some(Instant::now() + self.ms(self.deps.timing.max_wait_ms));
        }
    }

    /// Runs the work that was asked for: the registry, then each watch.
    async fn run_ready(&mut self) {
        if self.registry_now {
            self.registry_now = false;
            self.registry_work().await;
        }
        loop {
            let next = self
                .watches
                .iter()
                .find(|(_, w)| w.want_files || w.want_states)
                .map(|(key, w)| (key.clone(), w.want_files));
            let Some((key, files)) = next else {
                break;
            };
            if let Some(watch) = self.watches.get_mut(&key) {
                watch.want_files = false;
                watch.want_states = false;
                if !watch.ready {
                    // Before the transcripts are stat'ed, the events wait.
                    continue;
                }
            }
            if let Err(error) = self.pass(&key, files).await {
                let cwd = self
                    .watches
                    .get(&key)
                    .map(|w| w.cwd.clone())
                    .unwrap_or_default();
                self.log(
                    &format!("session list change check of {cwd} failed"),
                    &error.message,
                );
            }
        }
    }

    // --- events ---------------------------------------------------------------

    fn own_session_changed(&mut self, id: &str) {
        let keys: Vec<String> = self.watches.keys().cloned().collect();
        for key in keys {
            let Some(watch) = self.watches.get_mut(&key) else {
                continue;
            };
            if watch.ready && watch.rows.contains_key(id) {
                watch.dirty_states.insert(id.to_string());
                self.schedule(&key, Mode::States);
            } else if !watch.ready || watch.files_by_id.contains_key(id) {
                // Its first event here: the row is read.
                watch.dirty_rows.insert(id.to_string());
                self.schedule(&key, Mode::Now);
            }
        }
    }

    fn session_written(&mut self, id: &str) {
        let keys: Vec<String> = self.watches.keys().cloned().collect();
        for key in keys {
            let Some(watch) = self.watches.get_mut(&key) else {
                continue;
            };
            if !watch.ready {
                // Read when the watch is ready, as the events of its start.
                watch.dirty_rows.insert(id.to_string());
                continue;
            }
            let Some(files) = watch.files_by_id.get(id).cloned() else {
                continue;
            };
            watch.dirty_files.extend(files);
            self.schedule(&key, Mode::Now);
        }
    }

    async fn rescan_all(&mut self) {
        // Directories that did not exist before may now.
        self.ensure_shared().await;
        self.registry_full = true;
        self.schedule_registry(true);
        let keys: Vec<String> = self.watches.keys().cloned().collect();
        for key in keys {
            if self.watches.get(&key).is_some_and(|w| w.ready) {
                if let Some(watch) = self.watches.get_mut(&key) {
                    watch.rescan = true;
                }
                self.schedule(&key, Mode::Now);
            }
        }
    }

    /// The event source: stats the watched directories, the projects root and the
    /// registry, and turns what changed into the events `fs.watch` would have
    /// reported. A tick costs one stat per directory, a listing and the new
    /// entries' stats for the directories that changed, and
    /// [`FILE_POLL_BUDGET`] stats for the content changes the rotation reaches.
    async fn poll(&mut self) {
        if self.watches.is_empty() {
            return;
        }
        let root = self.deps.projects_root.clone();
        let registry_dir = self.deps.registry_dir.clone();
        // The project directories of every ready watch, once, and the registry.
        let mut dirs: Vec<PathBuf> = vec![registry_dir.clone()];
        let mut seen: HashSet<PathBuf> = HashSet::from([registry_dir.clone()]);
        for watch in self.watches.values().filter(|w| w.ready) {
            for name in watch.dirs.keys() {
                let dir = root.join(name);
                if seen.insert(dir.clone()) {
                    dirs.push(dir);
                }
            }
        }
        let baseline = std::mem::take(&mut self.poll_baseline);
        let poll_registry = registry_dir.clone();
        // Off the index worker: a poll never queues a list behind it.
        let polled = tokio::task::spawn_blocking(move || {
            poll_once(&RealFs, &root, &poll_registry, &dirs, baseline)
        })
        .await;
        let Ok((polled, baseline)) = polled else {
            self.log("session list poll failed", "the poll task panicked");
            return;
        };
        self.poll_baseline = baseline;
        // Project directories made or removed.
        for name in polled.root_changed {
            let keys: Vec<String> = self.watches.keys().cloned().collect();
            for key in keys {
                let matches = self.watches.get(&key).is_some_and(|w| {
                    w.paths.iter().any(|cwd| {
                        let platform = super::project_dirs::Platform::host();
                        is_exact_project_dir(&name, cwd, platform)
                            || project_dir_matches(&name, cwd, platform)
                    })
                });
                if matches {
                    if let Some(watch) = self.watches.get_mut(&key) {
                        watch.rescan = true;
                    }
                    self.schedule(&key, Mode::Debounced);
                }
            }
        }
        // Registry records: only the changed ones are read again.
        let mut registry_changed = false;
        for (path, _) in &polled.changed {
            if path.parent() != Some(registry_dir.as_path()) {
                continue;
            }
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            if is_record_file(&name) {
                self.registry_names.insert(name);
                registry_changed = true;
            }
        }
        if registry_changed {
            self.schedule_registry(false);
        }
        // Transcripts: the events name the file, and each watch of its directory
        // gets it once.
        let mut touched: HashSet<String> = HashSet::new();
        for (path, stat) in polled.changed {
            if path.parent() == Some(registry_dir.as_path()) {
                continue;
            }
            let Some(dir_name) = path
                .parent()
                .and_then(Path::file_name)
                .map(|n| n.to_string_lossy().into_owned())
            else {
                continue;
            };
            // An empty transcript is no transcript: the watch drops it.
            let stat = stat.filter(|(_, size, _)| *size > 0);
            for (key, watch) in &mut self.watches {
                if !watch.ready || !watch.dirs.contains_key(&dir_name) {
                    continue;
                }
                let known = watch.files.get(&path);
                let changed = match (stat, known) {
                    (Some((mtime_ms, size, ino)), Some(known)) => {
                        known.mtime_ms != mtime_ms || known.size != size || known.ino != ino
                    }
                    (Some(_), None) | (None, Some(_)) => true,
                    (None, None) => false,
                };
                if changed {
                    watch.dirty_files.insert(path.clone());
                    touched.insert(key.clone());
                }
            }
        }
        for key in touched {
            self.schedule(&key, Mode::Debounced);
        }
    }

    // --- registry -------------------------------------------------------------

    async fn registry_work(&mut self) {
        while self.registry_full || !self.registry_names.is_empty() {
            let full = self.registry_full;
            self.registry_full = false;
            match self.read_registry(full).await {
                Ok(affected) => self.on_live_changed(&affected),
                Err(error) => {
                    self.log("session list registry read failed", &error.message);
                    break;
                }
            }
        }
    }

    /// Reads the changed registry records (all with `full`) and returns the ids of
    /// the sessions whose record changed.
    async fn read_registry(&mut self, full: bool) -> Result<HashSet<String>, Error> {
        let names: Option<Vec<String>> = if full {
            self.registry_names.clear();
            None
        } else {
            let names = self.registry_names.drain().collect();
            Some(names)
        };
        let dir = self.deps.registry_dir.clone();
        let read = self
            .deps
            .worker
            .run(move |core: &mut IndexCore| {
                let names = names.unwrap_or_else(|| {
                    std::fs::read_dir(&dir)
                        .map(|read| {
                            read.flatten()
                                .map(|e| e.file_name().to_string_lossy().into_owned())
                                .collect()
                        })
                        .unwrap_or_default()
                });
                core.registry.read_files(&names, true)
            })
            .await?;
        let key = |record: Option<&LiveRecord>| -> String {
            record.map_or_else(String::new, |r| {
                format!(
                    "{}\0{:?}\0{:?}\0{:?}\0{:?}",
                    r.session_id, r.status, r.status_updated_at, r.kind, r.entrypoint
                )
            })
        };
        let mut affected = HashSet::new();
        let mut next: HashMap<String, LiveRecord> = if full {
            HashMap::new()
        } else {
            self.live_files.clone()
        };
        let before = std::mem::take(&mut self.live_files);
        let changed_names: HashSet<String> = if full {
            before.keys().chain(read.keys()).cloned().collect()
        } else {
            read.keys().cloned().collect()
        };
        for name in changed_names {
            let previous = before.get(&name);
            let record = read.get(&name).and_then(Option::as_ref);
            match record {
                Some(record) => {
                    next.insert(name.clone(), record.clone());
                }
                None => {
                    next.remove(&name);
                }
            }
            if key(previous) != key(record) {
                if let Some(previous) = previous {
                    affected.insert(previous.session_id.clone());
                }
                if let Some(record) = record {
                    affected.insert(record.session_id.clone());
                }
            }
        }
        self.live = by_session(next.values().cloned());
        self.live_files = next;
        Ok(affected)
    }

    fn on_live_changed(&mut self, affected: &HashSet<String>) {
        if affected.is_empty() {
            return;
        }
        let keys: Vec<String> = self.watches.keys().cloned().collect();
        for key in keys {
            let mut states = false;
            let mut rows = false;
            if let Some(watch) = self.watches.get_mut(&key) {
                for id in affected {
                    if watch.ready && watch.rows.contains_key(id) {
                        watch.dirty_states.insert(id.clone());
                        states = true;
                    } else if !watch.ready || watch.files_by_id.contains_key(id) {
                        // Its first event here: the row is read.
                        watch.dirty_rows.insert(id.clone());
                        rows = true;
                    }
                }
            }
            if states {
                self.schedule(&key, Mode::States);
            }
            if rows {
                self.schedule(&key, Mode::Debounced);
            }
        }
    }

    // --- scope work -----------------------------------------------------------

    fn apply_dirs(&mut self, key: &str, paths: Vec<String>, dirs: Vec<(String, String)>) {
        let settle = self.ms(self.deps.timing.settle_ms);
        let Some(watch) = self.watches.get_mut(key) else {
            return;
        };
        watch.paths = paths;
        let wanted: HashMap<String, String> = dirs.into_iter().collect();
        watch.dirs.retain(|name, _| wanted.contains_key(name));
        let mut opened = false;
        for (dir_name, project_path) in wanted {
            match watch.dirs.get_mut(&dir_name) {
                Some(entry) => entry.project_path = project_path,
                None => {
                    watch.dirs.insert(dir_name, WatchedDir { project_path });
                    opened = true;
                }
            }
        }
        if opened && watch.settle_at.is_none() {
            watch.settle_at = Some(Instant::now() + settle);
        }
    }

    /// Everything a start or a rescan reads of the scope: its paths, project
    /// directories and transcripts, with what the index holds of each.
    async fn enumerate(&mut self, key: &str) -> Result<Enumerated, Error> {
        let cwd = self
            .watches
            .get(key)
            .map(|w| w.cwd.clone())
            .unwrap_or_default();
        self.deps
            .worker
            .run(move |core: &mut IndexCore| {
                let paths = core.index.listed_paths(&cwd, true);
                let dirs = core.index.project_dirs(&paths);
                let candidates = core.index.enumerate_files(Some(&paths));
                let files = candidates
                    .into_iter()
                    .map(|candidate| FileInfo {
                        continued_in: core.index.continued_in(&candidate.file_path),
                        is_read: core.index.is_read(&candidate),
                        candidate,
                    })
                    .collect();
                Enumerated { paths, dirs, files }
            })
            .await
    }

    /// Opens the watch: its directories, the stats of its transcripts and the
    /// registry. Reads no row.
    async fn start_watch(&mut self, key: &str) -> Result<(), Error> {
        let enumerated = self.enumerate(key).await?;
        if !self.watches.contains_key(key) {
            return Ok(());
        }
        let Enumerated { paths, dirs, files } = enumerated;
        self.apply_dirs(key, paths, dirs);
        self.set_files(key, &files);
        self.learn_successors(key, &files);
        if let Some(watch) = self.watches.get_mut(key) {
            watch.resolved_paths = scope_key(&watch.paths);
        }
        // The rows of the live sessions a list read: their state ages from now.
        let seeded = self.live_seeds(key, &HashSet::new()).await?;
        let ready_at = self.now();
        let mut present: Vec<(String, RowEntry)> = Vec::new();
        for row in seeded {
            let id = row.session_id.clone();
            present.push((id, self.present(row, ready_at)));
        }
        let mut held_events = false;
        if let Some(watch) = self.watches.get_mut(key) {
            watch.ready = true;
            watch.ready_at = ready_at;
            for (id, entry) in present {
                watch.rows.insert(id, entry);
            }
            // Events of the start, held until now.
            held_events = !watch.dirty_files.is_empty()
                || !watch.dirty_rows.is_empty()
                || !watch.dirty_states.is_empty()
                || watch.rescan;
        }
        if held_events {
            self.schedule(key, Mode::Debounced);
        }
        Ok(())
    }

    fn set_files(&mut self, key: &str, files: &[FileInfo]) {
        let Some(watch) = self.watches.get_mut(key) else {
            return;
        };
        let before: HashSet<String> = watch.files_by_id.keys().cloned().collect();
        watch.files = HashMap::new();
        watch.files_by_id = HashMap::new();
        for file in files {
            let candidate = file.candidate.clone();
            Self::put_file_into(&mut self.seq, watch, candidate, Some(&before));
        }
    }

    /// `known`: the sessions with a transcript before; another one is born.
    fn put_file_into(
        seq: &mut u64,
        watch: &mut Watch,
        file: TranscriptCandidate,
        known: Option<&HashSet<String>>,
    ) {
        let id = file.session_id.clone();
        let path = file.file_path.clone();
        let known_before = match known {
            Some(known) => known.contains(&id),
            None => watch.files_by_id.contains_key(&id),
        };
        watch.files.insert(path.clone(), file);
        if watch.ready && !known_before {
            *seq += 1;
            watch.born.insert(id.clone(), *seq);
        }
        watch.files_by_id.entry(id).or_default().insert(path);
    }

    fn drop_file(watch: &mut Watch, path: &Path, id: &str) {
        watch.files.remove(path);
        if let Some(paths) = watch.files_by_id.get_mut(id) {
            paths.remove(path);
            if paths.is_empty() {
                watch.files_by_id.remove(id);
            }
        }
    }

    /// Records which session each transcript is continued in, by the metadata the
    /// index holds now (a list read it): kept when the index evicts it.
    fn learn_successors(&mut self, key: &str, files: &[FileInfo]) {
        let Some(watch) = self.watches.get_mut(key) else {
            return;
        };
        for file in files {
            if let Some(successor) = &file.continued_in {
                watch
                    .successor_of
                    .insert(file.candidate.session_id.clone(), successor.clone());
            }
        }
    }

    fn present(&mut self, row: IndexRow, now: f64) -> RowEntry {
        let own = (self.deps.own)(&row.session_id);
        if own.is_some() {
            self.own_shown.insert(row.session_id.clone());
        } else {
            self.own_shown.remove(&row.session_id);
        }
        let info = session_info_of(&row, own.as_ref(), self.live.get(&row.session_id), now);
        let signature = change_signature(&info);
        RowEntry {
            row,
            info,
            signature,
        }
    }

    /// The transcripts of a session the watch knows.
    fn files_of(watch: &Watch, id: &str) -> Vec<TranscriptCandidate> {
        watch
            .files_by_id
            .get(id)
            .into_iter()
            .flatten()
            .filter_map(|path| watch.files.get(path).cloned())
            .collect()
    }

    /// The rows of the live sessions of the scope without a row here (and not in
    /// `except`), from the metadata the index holds (a list read them), not from
    /// their transcripts; one it holds nothing for the client did not list. Reads
    /// no transcript: a continued one only checks its successor.
    async fn live_seeds(
        &mut self,
        key: &str,
        except: &HashSet<String>,
    ) -> Result<Vec<IndexRow>, Error> {
        let Some(watch) = self.watches.get(key) else {
            return Ok(Vec::new());
        };
        let mut read: Vec<TranscriptCandidate> = Vec::new();
        for id in self.live.keys() {
            if !watch.rows.contains_key(id) && !except.contains(id) {
                read.extend(Self::files_of(watch, id));
            }
        }
        if read.is_empty() {
            return Ok(Vec::new());
        }
        let paths = watch.paths.clone();
        let files = watch.files.clone();
        let hidden = watch.hidden_seeds.clone();
        let (rows, hidden_updates) = self
            .deps
            .worker
            .run(move |core: &mut IndexCore| live_seeds_job(core, &paths, &read, &files, &hidden))
            .await?;
        if let Some(watch) = self.watches.get_mut(key) {
            for (id, update) in hidden_updates {
                match update {
                    Some(stat_key) => {
                        watch.hidden_seeds.insert(id, stat_key);
                    }
                    None => {
                        watch.hidden_seeds.remove(&id);
                    }
                }
            }
        }
        Ok(rows)
    }

    /// One pass of the pending work of a watch. `files`: the transcript events and
    /// the rescan too, not only the states.
    async fn pass(&mut self, key: &str, files: bool) -> Result<(), Error> {
        let Some(watch) = self.watches.get_mut(key) else {
            return Ok(());
        };
        let rescan = files && watch.rescan;
        let dirty_files: Vec<PathBuf> = if files {
            watch.dirty_files.iter().cloned().collect()
        } else {
            Vec::new()
        };
        let mut row_ids: HashSet<String> = if files {
            watch.dirty_rows.clone()
        } else {
            HashSet::new()
        };
        let state_ids: HashSet<String> = watch.dirty_states.clone();
        // Sessions whose state may have aged: sent only when it changed.
        let mut aged_ids: HashSet<String> = HashSet::new();
        // The scope paths and the transcripts of each session read again, as they
        // were before this pass.
        let prior_paths = watch.paths.clone();
        let mut prior_files: HashMap<String, Vec<TranscriptCandidate>> = HashMap::new();
        if files {
            watch.rescan = false;
            watch.dirty_files.clear();
            watch.dirty_rows.clear();
        }
        watch.dirty_states.clear();

        let mut resolved: HashMap<String, IndexRow> = HashMap::new();
        let mut seeded: Vec<IndexRow> = Vec::new();
        let mut shown_before: HashSet<String> = HashSet::new();

        let outcome: Result<(), Error> = async {
            if rescan {
                let enumerated = self.enumerate(key).await?;
                let Enumerated {
                    paths,
                    dirs,
                    files: found,
                } = enumerated;
                if !self.watches.contains_key(key) {
                    return Ok(());
                }
                self.apply_dirs(key, paths, dirs);
                let watch = self
                    .watches
                    .get_mut(key)
                    .ok_or_else(|| internal_error("closed"))?;
                // The sessions of a transcript that is new, changed or gone are read
                // again; one whose stat did not change is not read.
                let mut before = watch.files.clone();
                for file in &found {
                    let old = before.remove(&file.candidate.file_path);
                    let candidate = &file.candidate;
                    let changed = old.is_none_or(|old| {
                        old.mtime_ms != candidate.mtime_ms
                            || old.size != candidate.size
                            || old.ino != candidate.ino
                    });
                    if changed {
                        row_ids.insert(candidate.session_id.clone());
                    }
                }
                for gone in before.values() {
                    row_ids.insert(gone.session_id.clone());
                }
                for id in &row_ids {
                    prior_files
                        .entry(id.clone())
                        .or_insert_with(|| Self::files_of(watch, id));
                }
                self.set_files(key, &found);
                self.learn_successors(key, &found);
                let watch = self
                    .watches
                    .get_mut(key)
                    .ok_or_else(|| internal_error("closed"))?;
                if scope_key(&watch.paths) != watch.resolved_paths {
                    // A worktree came or went: a transcript that did not change may
                    // now be in the scope or out of it (another path of the same
                    // project directory). Resolved again: the sessions read here,
                    // and those whose metadata the index holds (a list read them);
                    // both without reading a transcript.
                    row_ids.extend(watch.rows.keys().cloned());
                    for file in &found {
                        if file.is_read {
                            row_ids.insert(file.candidate.session_id.clone());
                        }
                    }
                }
                // A transcript that could not be read before.
                row_ids.extend(watch.unreadable.drain());
                // Only the state of a session that a live process holds depends on
                // the time (an unfinished turn ages).
                for id in self.live.keys() {
                    if watch.rows.contains_key(id) {
                        aged_ids.insert(id.clone());
                    }
                }
                seeded = self.live_seeds(key, &row_ids).await?;
                if let Some(watch) = self.watches.get(key) {
                    for id in &self.own_shown {
                        if watch.rows.contains_key(id) {
                            aged_ids.insert(id.clone());
                        }
                    }
                }
            } else if !dirty_files.is_empty() {
                let stats: Vec<Option<(f64, u64, u64, bool)>> = {
                    let paths = dirty_files.clone();
                    self.deps
                        .worker
                        .run(move |_| {
                            paths
                                .iter()
                                .map(|path| {
                                    super::dir_listing::stat(path)
                                        .map(|s| (s.mtime_ms, s.size, s.identity, s.is_file))
                                })
                                .collect()
                        })
                        .await?
                };
                let watch = self
                    .watches
                    .get_mut(key)
                    .ok_or_else(|| internal_error("closed"))?;
                for (path, stat) in dirty_files.iter().zip(stats) {
                    let filename = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let dir_name = path
                        .parent()
                        .and_then(Path::file_name)
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let Some(id) = transcript_id(&filename) else {
                        continue;
                    };
                    row_ids.insert(id.clone());
                    prior_files
                        .entry(id.clone())
                        .or_insert_with(|| Self::files_of(watch, &id));
                    let dir = watch.dirs.get(&dir_name);
                    match (dir, stat) {
                        (Some(dir), Some((mtime_ms, size, ino, true))) if size > 0 => {
                            let project_path = dir.project_path.clone();
                            Self::put_file_into(
                                &mut self.seq,
                                watch,
                                TranscriptCandidate {
                                    session_id: id,
                                    file_path: path.clone(),
                                    dir_name,
                                    project_path: Some(project_path),
                                    mtime_ms,
                                    size,
                                    ino,
                                },
                                None,
                            );
                        }
                        _ => Self::drop_file(watch, path, &id),
                    }
                }
            }

            if !row_ids.is_empty() {
                // A session continued in a changed one may be shown or hidden now.
                // Which sessions those are is known from the reads here and from
                // the metadata the index holds: a session that no list or event
                // read was not shown.
                let successors: HashSet<String> = row_ids.clone();
                let watch = self
                    .watches
                    .get(key)
                    .ok_or_else(|| internal_error("closed"))?;
                for (id, successor) in &watch.successor_of {
                    if successors.contains(successor) {
                        row_ids.insert(id.clone());
                    }
                }
                let all_files: Vec<PathBuf> = watch.files.keys().cloned().collect();
                let continued: HashMap<PathBuf, String> = self
                    .deps
                    .worker
                    .run(move |core: &mut IndexCore| {
                        all_files
                            .into_iter()
                            .filter_map(|p| core.index.continued_in(&p).map(|s| (p, s)))
                            .collect()
                    })
                    .await?;
                let watch = self
                    .watches
                    .get(key)
                    .ok_or_else(|| internal_error("closed"))?;
                for (path, successor) in &continued {
                    if successors.contains(successor) {
                        if let Some(file) = watch.files.get(path) {
                            row_ids.insert(file.session_id.clone());
                        }
                    }
                }
                for id in &row_ids {
                    prior_files
                        .entry(id.clone())
                        .or_insert_with(|| Self::files_of(watch, id));
                }
                // Before the read below replaces the metadata the index holds.
                shown_before = self
                    .shown_before(key, &prior_paths, &row_ids, &prior_files)
                    .await?;
                let watch = self
                    .watches
                    .get(key)
                    .ok_or_else(|| internal_error("closed"))?;
                let mut read: Vec<TranscriptCandidate> = Vec::new();
                for id in &row_ids {
                    read.extend(Self::files_of(watch, id));
                }
                let paths = watch.paths.clone();
                let siblings: Vec<TranscriptCandidate> = watch.files.values().cloned().collect();
                let read_for_job = read.clone();
                let (rows, info) = self
                    .deps
                    .worker
                    .run(move |core: &mut IndexCore| {
                        let rows = core
                            .index
                            .rows_of(&paths, &read_for_job, &|| siblings.clone());
                        let info: Vec<(PathBuf, Option<String>, bool)> = read_for_job
                            .iter()
                            .map(|file| {
                                (
                                    file.file_path.clone(),
                                    core.index.continued_in(&file.file_path),
                                    core.index.is_read(file),
                                )
                            })
                            .collect();
                        (rows, info)
                    })
                    .await?;
                resolved = rows
                    .into_iter()
                    .map(|row| (row.session_id.clone(), row))
                    .collect();
                let watch = self
                    .watches
                    .get_mut(key)
                    .ok_or_else(|| internal_error("closed"))?;
                for (file, (_, successor, is_read)) in read.iter().zip(&info) {
                    let id = &file.session_id;
                    match successor {
                        Some(successor) => {
                            watch.successor_of.insert(id.clone(), successor.clone());
                        }
                        None if *is_read => {
                            watch.successor_of.remove(id);
                        }
                        None => {}
                    }
                }
                // A transcript there whose read failed is no removal: it keeps its
                // row until a rescan reads it.
                for (file, (_, _, is_read)) in read.iter().zip(&info) {
                    let id = &file.session_id;
                    if !resolved.contains_key(id) && !is_read {
                        watch.unreadable.insert(id.clone());
                        row_ids.remove(id);
                    }
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = outcome {
            // What this pass took is left for the next one: the file stats may have
            // moved on, so the sessions are resolved again by id.
            if let Some(watch) = self.watches.get_mut(key) {
                watch.dirty_files.extend(dirty_files);
                watch.dirty_rows.extend(row_ids);
                watch.dirty_states.extend(state_ids);
                if rescan {
                    watch.rescan = true;
                }
            }
            return Err(error);
        }
        if rescan {
            if let Some(watch) = self.watches.get_mut(key) {
                watch.resolved_paths = scope_key(&watch.paths);
            }
        }

        // From here on synchronous: the rows change and are delivered at once.
        let now = self.now();
        // The changed sessions.
        let mut changed: HashSet<String> = HashSet::new();
        // Sessions without a row now.
        let mut absent: HashSet<String> = HashSet::new();
        for id in &row_ids {
            if let Some(row) = resolved.remove(id) {
                let entry = self.present(row, now);
                if let Some(watch) = self.watches.get_mut(key) {
                    watch.rows.insert(id.clone(), entry);
                }
                changed.insert(id.clone());
                continue;
            }
            let previous = self
                .watches
                .get_mut(key)
                .and_then(|watch| watch.rows.remove(id));
            if previous.is_none() && !shown_before.contains(id) {
                continue;
            }
            changed.insert(id.clone());
            absent.insert(id.clone());
        }
        for row in seeded {
            let id = row.session_id.clone();
            let known = row_ids.contains(&id)
                || self
                    .watches
                    .get(key)
                    .is_some_and(|w| w.rows.contains_key(&id));
            if known {
                continue;
            }
            // As the client listed it, about when the watch became ready: a state
            // that aged since is sent below.
            let ready_at = self.watches.get(key).map_or(now, |w| w.ready_at);
            let entry = self.present(row, ready_at);
            if let Some(watch) = self.watches.get_mut(key) {
                watch.rows.insert(id.clone(), entry);
            }
            aged_ids.insert(id);
        }
        let mut state_and_aged: HashSet<String> = state_ids.clone();
        state_and_aged.extend(aged_ids);
        for id in state_and_aged {
            if row_ids.contains(&id) {
                continue;
            }
            let Some(current_row) = self
                .watches
                .get(key)
                .and_then(|w| w.rows.get(&id))
                .map(|entry| (entry.row.clone(), entry.signature.clone()))
            else {
                continue;
            };
            let next = self.present(current_row.0, now);
            let signature_changed = next.signature != current_row.1;
            if let Some(watch) = self.watches.get_mut(key) {
                watch.rows.insert(id.clone(), next);
            }
            // An event of the session, or a state that aged.
            if state_ids.contains(&id) || signature_changed {
                changed.insert(id);
            }
        }
        let subscription_ids: Vec<String> = self
            .watches
            .get(key)
            .map(|w| w.subscriptions.iter().cloned().collect())
            .unwrap_or_default();
        for sub_id in &subscription_ids {
            for id in &changed {
                let born = self.watches.get(key).and_then(|w| w.born.get(id).copied());
                let Some(subscription) = self.subscriptions.get_mut(sub_id) else {
                    continue;
                };
                if absent.contains(id)
                    && !subscription.sent.contains_key(id)
                    && born.is_some_and(|born| born > subscription.since)
                {
                    // Born after the subscription and never sent: nothing to remove.
                    subscription.pending.remove(id);
                    continue;
                }
                subscription.pending.insert(id.clone());
            }
            self.deliver(sub_id);
        }
        // A session without a transcript now, shown or not: a removal already sent
        // for it is forgotten too.
        for id in &row_ids {
            let has_files = self
                .watches
                .get(key)
                .is_some_and(|w| w.files_by_id.contains_key(id));
            if has_files {
                continue;
            }
            if let Some(watch) = self.watches.get_mut(key) {
                watch.born.remove(id);
                watch.successor_of.remove(id);
                watch.hidden_seeds.remove(id);
            }
            for sub_id in &subscription_ids {
                if let Some(subscription) = self.subscriptions.get_mut(sub_id) {
                    if subscription.sent.get(id) == Some(&None)
                        && !subscription.pending.contains(id)
                    {
                        subscription.sent.remove(id);
                    }
                }
            }
        }
        Ok(())
    }

    /// Which of `ids`, without a row read here, may have had a row in the scope
    /// before: those that the metadata the index holds for their earlier
    /// transcripts (a list read them) gives a row, and those it gives no answer for
    /// (unknown: a removal is sent once). Not a session without a transcript
    /// before, nor one known to have had no row (another path of the project
    /// directory, a hidden session). Reads no transcript.
    async fn shown_before(
        &mut self,
        key: &str,
        paths: &[String],
        ids: &HashSet<String>,
        prior_files: &HashMap<String, Vec<TranscriptCandidate>>,
    ) -> Result<HashSet<String>, Error> {
        let Some(watch) = self.watches.get(key) else {
            return Ok(HashSet::new());
        };
        let mut candidates: Vec<TranscriptCandidate> = Vec::new();
        for id in ids {
            if !watch.rows.contains_key(id) {
                candidates.extend(prior_files.get(id).cloned().unwrap_or_default());
            }
        }
        if candidates.is_empty() {
            return Ok(HashSet::new());
        }
        let paths = paths.to_vec();
        self.deps
            .worker
            .run(move |core: &mut IndexCore| {
                // Only the ids count: no worktree check.
                let (rows, mut unknown) = core.index.cached_rows_of(&paths, &candidates, false);
                for row in rows {
                    unknown.insert(row.session_id);
                }
                unknown
            })
            .await
    }

    /// Sends the pending sessions of a subscription that differ from what it last
    /// sent (a session never sent differs), each at most once per interval, in one
    /// notification.
    fn deliver(&mut self, sub_id: &str) {
        let min_interval = self.deps.timing.min_session_interval_ms as f64;
        let now_ms = self.now();
        let now = Instant::now();
        let Some(watch_key) = self.subscriptions.get(sub_id).map(|s| s.watch_key.clone()) else {
            return;
        };
        let Some(watch) = self.watches.get(&watch_key) else {
            return;
        };
        let Some(subscription) = self.subscriptions.get_mut(sub_id) else {
            return;
        };
        subscription.timer_at = None;
        let mut sessions: Vec<Value> = Vec::new();
        let mut removed: Vec<String> = Vec::new();
        let mut next_at: Option<f64> = None;
        let pending: Vec<String> = subscription.pending.iter().cloned().collect();
        for id in pending {
            let current = watch.rows.get(&id);
            let sent = subscription.sent.get(&id);
            let unchanged = match current {
                Some(current) => sent == Some(&Some(current.signature.clone())),
                None => sent == Some(&None),
            };
            if unchanged {
                subscription.pending.remove(&id);
                continue;
            }
            // A fork whose fork response is not out yet stays pending.
            if self.deps.fork_holds.holds(
                &id,
                subscription.sent.contains_key(&id),
                current.and_then(|c| c.row.facts.forked_from.as_deref()),
            ) {
                continue;
            }
            let due_at = subscription
                .sent_at
                .get(&id)
                .map_or(f64::NEG_INFINITY, |at| at + min_interval);
            if due_at > now_ms {
                next_at = Some(next_at.map_or(due_at, |n| n.min(due_at)));
                continue;
            }
            subscription.pending.remove(&id);
            subscription.sent_at.insert(id.clone(), now_ms);
            match current {
                None => {
                    removed.push(id.clone());
                    // A session with a transcript but no row (hidden, or of another
                    // path) is remembered as removed; one without a transcript is
                    // forgotten.
                    if watch.files_by_id.contains_key(&id) {
                        subscription.sent.insert(id, None);
                    } else {
                        subscription.sent.remove(&id);
                    }
                }
                Some(current) => {
                    sessions.push(current.info.clone());
                    subscription
                        .sent
                        .insert(id, Some(current.signature.clone()));
                }
            }
        }
        // Drops send times older than the interval: they hold nothing back.
        if subscription.sent_at.len() >= 1024 {
            subscription
                .sent_at
                .retain(|_, at| now_ms - *at < min_interval);
        }
        if let Some(next_at) = next_at {
            subscription.timer_at =
                Some(now + Duration::from_secs_f64(((next_at - now_ms) / 1000.0).max(0.0)));
        }
        if sessions.is_empty() && removed.is_empty() {
            return;
        }
        let changes = json!({
            "subscriptionId": subscription.id,
            "sessions": sessions,
            "removed": removed,
        });
        let notify = self.deps.notify.clone();
        let log = self.deps.log_error.clone();
        tokio::spawn(async move {
            if let Err(error) = notify(changes).await {
                log("session list change notification failed", &error.message);
            }
        });
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Debounced,
    Now,
    States,
}

/// The worker's half of [`Actor::live_seeds`]: the rows the index's metadata gives
/// for `read`, and for a continued one whose successor holds no history yet, a read
/// of the one transcript. `hidden` maps a session to the stats it was found hidden
/// at; the updates say which to set or drop.
type HiddenUpdates = Vec<(String, Option<String>)>;

fn live_seeds_job(
    core: &mut IndexCore,
    paths: &[String],
    read: &[TranscriptCandidate],
    files: &HashMap<PathBuf, TranscriptCandidate>,
    hidden: &HashMap<String, String>,
) -> (Vec<IndexRow>, HiddenUpdates) {
    let (cached_rows, unknown) = core.index.cached_rows_of(paths, read, true);
    // A continued one is shown while its successor has no history: its cached
    // metadata and that check, made again only when the stat of either changed.
    let stat_of = |file: Option<&TranscriptCandidate>| {
        file.map_or_else(
            || "-".to_string(),
            |f| format!("{}:{}:{}", f.mtime_ms, f.size, f.ino),
        )
    };
    let mut continued: Vec<(TranscriptCandidate, String)> = Vec::new();
    for file in read {
        let id = &file.session_id;
        let successor = core.index.continued_in(&file.file_path);
        let Some(successor) = successor else {
            continue;
        };
        if !unknown.contains(id) || !core.index.is_read(file) {
            continue;
        }
        let successor_path = file
            .file_path
            .parent()
            .map(|dir| dir.join(format!("{successor}.jsonl")));
        let successor_file = successor_path.as_ref().and_then(|p| files.get(p));
        let key = format!("{}|{}", stat_of(Some(file)), stat_of(successor_file));
        if hidden.get(id) != Some(&key) {
            continued.push((file.clone(), key));
        }
    }
    if continued.is_empty() {
        return (cached_rows, Vec::new());
    }
    let candidates: Vec<TranscriptCandidate> = continued.iter().map(|(f, _)| f.clone()).collect();
    let rows = core.index.rows_of(paths, &candidates, &Vec::new);
    let shown: HashSet<String> = rows.iter().map(|r| r.session_id.clone()).collect();
    let mut updates = Vec::new();
    for (file, key) in continued {
        let id = file.session_id;
        if shown.contains(&id) {
            updates.push((id, None));
        } else {
            updates.push((id, Some(key)));
        }
    }
    let mut all = cached_rows;
    all.extend(rows);
    (all, updates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fork_is_held_until_the_last_fork_of_its_source_answers() {
        let mut holds = ForkHolds::default();
        holds.started("src");
        holds.started("src");
        holds.created("src", "fork-1");
        assert!(holds.is_held("fork-1"));
        assert!(!holds.is_held("other"));
        assert!(!holds.answered("src"), "one fork is still in flight");
        assert!(holds.is_held("fork-1"));
        assert!(holds.answered("src"));
        assert!(!holds.is_held("fork-1"));
        assert!(!holds.answered("src"), "nothing to free");
    }

    #[test]
    fn an_unsent_fork_row_waits_by_its_forked_from_before_the_copy_returns() {
        let mut holds = ForkHolds::default();
        holds.started("src");
        assert!(holds.holds("fork", false, Some("src")));
        assert!(!holds.holds("fork", true, Some("src")));
        assert!(!holds.holds("fork", false, Some("elsewhere")));
        assert!(!holds.holds("fork", false, None));
    }

    /// _session/list/subscribe > takes an absolute cwd and ignores other parameters
    #[test]
    fn subscribe_takes_an_absolute_cwd_and_ignores_other_parameters() {
        assert_eq!(
            parse_list_subscribe_request(&json!({"cwd": "/repo", "other": 1})).expect("ok"),
            "/repo"
        );
        for params in [json!({}), json!({"cwd": "relative"}), json!({"cwd": 7})] {
            let error = parse_list_subscribe_request(&params).expect_err("invalid");
            assert_eq!(i32::from(error.code), -32602);
            assert_eq!(
                error.message,
                "Invalid params: params require an absolute cwd"
            );
        }
        assert_eq!(
            parse_list_unsubscribe_request(&json!({"subscriptionId": "x"})).expect("ok"),
            "x"
        );
        assert_eq!(
            parse_list_unsubscribe_request(&json!({}))
                .expect_err("invalid")
                .message,
            "Invalid params: params require a subscriptionId"
        );
    }

    // ---- the event source (the replacement for fs.watch) --------------------

    use std::cell::Cell;

    /// A file system in memory, counting the reads of a tick.
    #[derive(Default)]
    struct VirtualFs {
        /// The files: transcripts and registry records.
        files: HashMap<PathBuf, super::super::dir_listing::StatInfo>,
        /// The directories: their stat and their entries.
        dirs: HashMap<PathBuf, (DirSig, Vec<String>)>,
        stats: Cell<usize>,
        file_stats: Cell<usize>,
        listings: Cell<usize>,
    }

    fn stat_info(
        is_file: bool,
        size: u64,
        mtime_ms: f64,
        identity: u64,
    ) -> super::super::dir_listing::StatInfo {
        super::super::dir_listing::StatInfo {
            is_file,
            is_dir: !is_file,
            size,
            mtime_ms,
            identity,
        }
    }

    /// A session id for the `i`th transcript of a test.
    fn session_name(i: usize) -> String {
        format!("aaaaaaaa-aaaa-4aaa-8aaa-{i:012x}.jsonl")
    }

    impl VirtualFs {
        fn add_dir(&mut self, path: &Path, mtime_ms: f64) {
            self.dirs
                .insert(path.to_path_buf(), ((mtime_ms, 0, 0), Vec::new()));
            // A directory appears in its parent: the parent's stat changes.
            if let Some(parent) = path.parent() {
                if let Some(entry) = self.dirs.get_mut(parent) {
                    let name = path
                        .file_name()
                        .expect("name")
                        .to_string_lossy()
                        .into_owned();
                    entry.1.push(name);
                    entry.0 .0 += 1.0;
                    entry.0 .1 += 1;
                }
            }
        }

        /// A transcript appears: the directory's stat changes with it.
        fn add_file(&mut self, path: &Path, size: u64, mtime_ms: f64, identity: u64) {
            let parent = path.parent().expect("parent").to_path_buf();
            let name = path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            let entry = self.dirs.get_mut(&parent).expect("the directory exists");
            entry.1.push(name);
            entry.0 .0 += 1.0;
            entry.0 .1 += 1;
            self.files.insert(
                path.to_path_buf(),
                stat_info(true, size, mtime_ms, identity),
            );
        }

        /// A transcript is written: only its stat changes, not its directory's.
        fn write_file(&mut self, path: &Path, size: u64, mtime_ms: f64) {
            let file = self.files.get_mut(path).expect("the file exists");
            file.size = size;
            file.mtime_ms = mtime_ms;
        }

        fn remove_file(&mut self, path: &Path) {
            let parent = path.parent().expect("parent").to_path_buf();
            let name = path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            let entry = self.dirs.get_mut(&parent).expect("the directory exists");
            entry.1.retain(|n| *n != name);
            entry.0 .0 += 1.0;
            entry.0 .1 -= 1;
            self.files.remove(path);
        }

        fn reset_counts(&self) {
            self.stats.set(0);
            self.file_stats.set(0);
            self.listings.set(0);
        }
    }

    impl PollFs for VirtualFs {
        fn stat(&self, path: &Path) -> Option<super::super::dir_listing::StatInfo> {
            self.stats.set(self.stats.get() + 1);
            if let Some((sig, _)) = self.dirs.get(path) {
                return Some(stat_info(false, sig.1, sig.0, sig.2));
            }
            self.file_stats.set(self.file_stats.get() + 1);
            self.files.get(path).copied()
        }

        fn read_dir(&self, dir: &Path) -> Vec<String> {
            self.listings.set(self.listings.get() + 1);
            self.dirs
                .get(dir)
                .map(|(_, names)| names.clone())
                .unwrap_or_default()
        }
    }

    /// The tree of the poll tests: a projects root with one project directory of
    /// `n` transcripts, and an empty registry.
    fn tree(n: usize) -> (VirtualFs, PathBuf, PathBuf, PathBuf) {
        let root = PathBuf::from("/projects");
        let registry = PathBuf::from("/sessions");
        let project = root.join("proj");
        let mut fs = VirtualFs::default();
        fs.add_dir(&root, 1.0);
        fs.add_dir(&registry, 1.0);
        fs.add_dir(&project, 1.0);
        for i in 0..n {
            fs.add_file(
                &project.join(session_name(i)),
                100,
                1_000.0 + i as f64,
                10_000 + i as u64,
            );
        }
        (fs, root, registry, project)
    }

    /// the event source > stats a directory once and a bounded slice of its files per tick, whatever the size of the history
    #[tokio::test(start_paused = true)]
    async fn a_tick_stats_a_directory_once_and_a_bounded_slice_of_its_files() {
        const N: usize = 10_000;
        let (mut fs, root, registry, project) = tree(N);
        let dirs = vec![project.clone(), registry.clone()];
        // The first tick only takes the baseline.
        let (polled, mut baseline) =
            poll_once(&fs, &root, &registry, &dirs, PollBaseline::default());
        assert!(polled.changed.is_empty(), "a first tick reports nothing");
        assert!(polled.root_changed.is_empty());
        // A quiet tick: three directory stats (the two watched directories and the
        // projects root) and the rotation's slice, not 10k stats.
        tokio::time::advance(Duration::from_millis(POLL_MS)).await;
        fs.reset_counts();
        let (polled, next) = poll_once(&fs, &root, &registry, &dirs, baseline);
        baseline = next;
        assert!(polled.changed.is_empty(), "nothing changed");
        assert_eq!(
            fs.file_stats.get(),
            FILE_POLL_BUDGET,
            "a quiet tick stats a bounded slice of the known files"
        );
        assert!(
            fs.stats.get() <= FILE_POLL_BUDGET + 3,
            "a quiet tick costs a slice and one stat per directory, not 10k: {}",
            fs.stats.get()
        );
        // One transcript is written: the rotation reaches it, and only it is
        // reported, within `ceil(N / budget)` ticks.
        let written = project.join(session_name(0));
        fs.write_file(&written, 200, 9_000.0);
        let mut found = 0;
        for _ in 0..(N / FILE_POLL_BUDGET + 2) {
            tokio::time::advance(Duration::from_millis(POLL_MS)).await;
            fs.reset_counts();
            let (polled, next) = poll_once(&fs, &root, &registry, &dirs, baseline);
            baseline = next;
            assert!(
                fs.file_stats.get() <= FILE_POLL_BUDGET + 1,
                "every tick stays within the budget and the one hot file"
            );
            assert!(fs.stats.get() <= FILE_POLL_BUDGET + 4);
            for (path, stat) in polled.changed {
                assert_eq!(path, written, "only the written transcript changed");
                assert_eq!(stat, Some((9_000.0, 200, 10_000)));
                found += 1;
            }
        }
        assert_eq!(found, 1, "the one change is reported once");
    }

    /// the event source > a transcript that appears costs one stat and a listing, not a stat per file of its directory
    #[tokio::test(start_paused = true)]
    async fn a_transcript_that_appears_costs_one_stat_and_a_listing() {
        const N: usize = 1_000;
        let (mut fs, root, registry, project) = tree(N);
        let dirs = vec![project.clone(), registry.clone()];
        let (_, mut baseline) = poll_once(&fs, &root, &registry, &dirs, PollBaseline::default());
        let appeared = project.join(session_name(N));
        fs.add_file(&appeared, 300, 5_000.0, 50_000);
        tokio::time::advance(Duration::from_millis(POLL_MS)).await;
        fs.reset_counts();
        let (polled, next) = poll_once(&fs, &root, &registry, &dirs, baseline);
        baseline = next;
        assert_eq!(
            polled.changed,
            vec![(appeared.clone(), Some((5_000.0, 300, 50_000)))]
        );
        assert_eq!(fs.listings.get(), 1, "only the changed directory is listed");
        assert!(
            fs.file_stats.get() <= FILE_POLL_BUDGET + 1,
            "only the entry that appeared is stat'ed beyond the rotation: {}",
            fs.file_stats.get()
        );
        // A transcript that goes is reported without a stat of the others.
        fs.remove_file(&project.join(session_name(1)));
        tokio::time::advance(Duration::from_millis(POLL_MS)).await;
        fs.reset_counts();
        let (polled, _) = poll_once(&fs, &root, &registry, &dirs, baseline);
        assert_eq!(
            polled.changed,
            vec![(project.join(session_name(1)), None)],
            "the removed transcript"
        );
        assert!(fs.file_stats.get() <= FILE_POLL_BUDGET + 1);
    }

    /// Ticks until `path` is reported with `size`, or panics after `max` ticks.
    fn ticks_until_reported(
        fs: &VirtualFs,
        root: &Path,
        registry: &Path,
        dirs: &[PathBuf],
        baseline: &mut PollBaseline,
        path: &Path,
        max: usize,
    ) -> usize {
        for tick in 1..=max {
            let (polled, next) = poll_once(fs, root, registry, dirs, std::mem::take(baseline));
            *baseline = next;
            if polled.changed.iter().any(|(p, s)| p == path && s.is_some()) {
                return tick;
            }
        }
        panic!("{} was not reported in {max} ticks", path.display());
    }

    /// the event source > a transcript that appears after the first scan is polled like the others: its appends are seen at once while hot and within a rotation when cold
    #[tokio::test(start_paused = true)]
    async fn a_transcript_that_appears_later_joins_the_rotation() {
        const N: usize = 1_000;
        let (mut fs, root, registry, project) = tree(N);
        let dirs = vec![project.clone(), registry.clone()];
        let (_, mut baseline) = poll_once(&fs, &root, &registry, &dirs, PollBaseline::default());
        let appeared = project.join(session_name(N));
        fs.add_file(&appeared, 300, 5_000.0, 50_000);
        let first = ticks_until_reported(&fs, &root, &registry, &dirs, &mut baseline, &appeared, 2);
        assert_eq!(first, 1, "the new transcript is reported by the next tick");
        // It grows right away: the hot set sees it on the very next tick.
        fs.write_file(&appeared, 400, 6_000.0);
        let hot = ticks_until_reported(&fs, &root, &registry, &dirs, &mut baseline, &appeared, 3);
        assert_eq!(hot, 1, "an append to a new transcript is seen at once");
        // Quiet for longer than the hot window, then it grows again: the
        // rotation must still reach it (it is not only in the hot set).
        for _ in 0..(HOT_TICKS as usize + 5) {
            let (polled, next) =
                poll_once(&fs, &root, &registry, &dirs, std::mem::take(&mut baseline));
            baseline = next;
            assert!(polled.changed.is_empty());
        }
        assert!(baseline.hot.is_empty(), "the hot window expired");
        fs.write_file(&appeared, 500, 7_000.0);
        let cold = ticks_until_reported(
            &fs,
            &root,
            &registry,
            &dirs,
            &mut baseline,
            &appeared,
            N / FILE_POLL_BUDGET + 2,
        );
        assert!(cold <= N / FILE_POLL_BUDGET + 2);
    }

    /// the event source > a registry record that appears after the first scan has its status rewrites noticed
    #[tokio::test(start_paused = true)]
    async fn a_registry_record_that_appears_later_joins_the_rotation() {
        let (mut fs, root, registry, project) = tree(3);
        let dirs = vec![project.clone(), registry.clone()];
        let (_, mut baseline) = poll_once(&fs, &root, &registry, &dirs, PollBaseline::default());
        let record = registry.join("4242.json");
        fs.add_file(&record, 80, 5_000.0, 77);
        let first = ticks_until_reported(&fs, &root, &registry, &dirs, &mut baseline, &record, 2);
        assert_eq!(first, 1);
        for _ in 0..(HOT_TICKS as usize + 5) {
            let (_, next) = poll_once(&fs, &root, &registry, &dirs, std::mem::take(&mut baseline));
            baseline = next;
        }
        fs.write_file(&record, 90, 6_000.0);
        let cold = ticks_until_reported(&fs, &root, &registry, &dirs, &mut baseline, &record, 3);
        assert!(cold <= 3, "the rewritten record is seen within a rotation");
    }

    /// the event source > a file whose stat failed once is stat'ed again by the next scan of its directory
    #[tokio::test(start_paused = true)]
    async fn a_name_whose_stat_failed_is_retried() {
        let (mut fs, root, registry, project) = tree(2);
        let dirs = vec![project.clone(), registry.clone()];
        let (_, mut baseline) = poll_once(&fs, &root, &registry, &dirs, PollBaseline::default());
        let late = project.join(session_name(9));
        // Listed but not stat-able: a directory entry racing its creation.
        fs.dirs
            .get_mut(&project)
            .expect("project")
            .1
            .push(session_name(9));
        fs.dirs.get_mut(&project).expect("project").0 = (2.0, 0, 0);
        let (polled, next) = poll_once(&fs, &root, &registry, &dirs, baseline);
        baseline = next;
        assert!(polled.changed.is_empty());
        fs.files
            .insert(late.clone(), stat_info(true, 10, 3_000.0, 5));
        fs.dirs.get_mut(&project).expect("project").0 = (3.0, 0, 0);
        let found = ticks_until_reported(&fs, &root, &registry, &dirs, &mut baseline, &late, 1);
        assert_eq!(found, 1);
    }

    /// the event source > a change to a recently changed file is seen on the next tick at 10k transcripts, a quiet tick stays within the budget, and a cold file is found within a rotation
    #[tokio::test(start_paused = true)]
    async fn a_hot_file_is_stat_ed_every_tick_and_a_cold_one_within_a_rotation() {
        const N: usize = 10_000;
        let (mut fs, root, registry, project) = tree(N);
        let dirs = vec![project.clone(), registry.clone()];
        let (_, mut baseline) = poll_once(&fs, &root, &registry, &dirs, PollBaseline::default());
        // A cold file found by the rotation becomes hot.
        let session = project.join(session_name(N - 1));
        fs.write_file(&session, 200, 9_000.0);
        let cold = ticks_until_reported(
            &fs,
            &root,
            &registry,
            &dirs,
            &mut baseline,
            &session,
            N / FILE_POLL_BUDGET + 2,
        );
        assert!(cold <= N.div_ceil(FILE_POLL_BUDGET) + 1);
        // Appends to it are now seen on the very next tick, not up to 4 s later.
        for size in [300_u64, 400, 500] {
            fs.write_file(&session, size, 9_000.0 + size as f64);
            let ticks =
                ticks_until_reported(&fs, &root, &registry, &dirs, &mut baseline, &session, 1);
            assert_eq!(ticks, 1);
        }
        // A quiet tick stays small: the rotation slice plus the hot set.
        fs.reset_counts();
        let (polled, _) = poll_once(&fs, &root, &registry, &dirs, std::mem::take(&mut baseline));
        assert!(polled.changed.is_empty());
        assert!(
            fs.file_stats.get() <= FILE_POLL_BUDGET + HOT_MAX,
            "a quiet tick is bounded: {}",
            fs.file_stats.get()
        );
        assert!(fs.file_stats.get() < N / 10);
    }

    /// the event source > T3B 0c: every live registry record is stat'ed on every tick at 10k transcripts, so a quiet session that wakes is seen on the next tick
    #[tokio::test(start_paused = true)]
    async fn registry_records_are_stat_ed_every_tick_at_10k_transcripts() {
        const N: usize = 10_000;
        const RECORDS: usize = 40;
        let (mut fs, root, registry, project) = tree(N);
        for i in 0..RECORDS {
            fs.add_file(
                &registry.join(format!("{}.json", 4_000 + i)),
                50,
                2_000.0 + i as f64,
                70_000 + i as u64,
            );
        }
        let dirs = vec![project.clone(), registry.clone()];
        let (_, mut baseline) = poll_once(&fs, &root, &registry, &dirs, PollBaseline::default());
        // A quiet tick: the rotation slice plus one stat per record.
        tokio::time::advance(Duration::from_millis(POLL_MS)).await;
        fs.reset_counts();
        let (polled, next) = poll_once(&fs, &root, &registry, &dirs, baseline);
        baseline = next;
        assert!(polled.changed.is_empty());
        assert_eq!(
            fs.file_stats.get(),
            FILE_POLL_BUDGET + RECORDS,
            "every record is stat'ed on a quiet tick, on top of the slice"
        );
        // A record nobody touched for a long time changes: the very next tick sees
        // it, whichever record it is and wherever the rotation stands.
        for _ in 0..300 {
            let (_, next) = poll_once(&fs, &root, &registry, &dirs, std::mem::take(&mut baseline));
            baseline = next;
        }
        for i in [0, RECORDS / 2, RECORDS - 1] {
            let record = registry.join(format!("{}.json", 4_000 + i));
            fs.write_file(&record, 90 + i as u64, 8_000.0 + i as f64);
            let ticks =
                ticks_until_reported(&fs, &root, &registry, &dirs, &mut baseline, &record, 1);
            assert_eq!(ticks, 1);
        }
    }

    /// the event source > reports a project directory made or removed, and nothing while the root is unchanged
    #[tokio::test(start_paused = true)]
    async fn the_projects_root_is_listed_only_when_it_changed() {
        let (mut fs, root, registry, project) = tree(3);
        let dirs = vec![project.clone(), registry.clone()];
        let (polled, mut baseline) =
            poll_once(&fs, &root, &registry, &dirs, PollBaseline::default());
        assert!(polled.root_changed.is_empty(), "the first tick baselines");
        // A quiet tick reads no listing of the root.
        tokio::time::advance(Duration::from_millis(POLL_MS)).await;
        fs.reset_counts();
        let (polled, next) = poll_once(&fs, &root, &registry, &dirs, baseline);
        baseline = next;
        assert!(polled.root_changed.is_empty());
        assert_eq!(fs.listings.get(), 0, "no directory listing at all");
        // A project directory appears.
        fs.add_dir(&root.join("other"), 20.0);
        tokio::time::advance(Duration::from_millis(POLL_MS)).await;
        let (polled, _) = poll_once(&fs, &root, &registry, &dirs, baseline);
        assert_eq!(polled.root_changed, vec!["other".to_string()]);
    }

    /// the event source > drops the directories no watch watches any more
    #[test]
    fn the_baseline_drops_the_directories_of_a_closed_watch() {
        let (fs, root, registry, project) = tree(3);
        let dirs = vec![project.clone(), registry.clone()];
        let (_, baseline) = poll_once(&fs, &root, &registry, &dirs, PollBaseline::default());
        assert_eq!(baseline.dirs.len(), 2);
        assert_eq!(baseline.files.len(), 3);
        // The watch closed: only the registry is polled now.
        let (_, pruned) = poll_once(
            &fs,
            &root,
            &registry,
            std::slice::from_ref(&registry),
            baseline,
        );
        assert_eq!(pruned.dirs.len(), 1);
        assert_eq!(pruned.files.len(), 0, "its files left with it");
    }

    /// the event source > a rotation whose files all went is rebuilt empty, and the next tick is quiet
    #[tokio::test(start_paused = true)]
    async fn the_rotation_survives_every_transcript_going() {
        let (mut fs, root, registry, project) = tree(20);
        let dirs = vec![project.clone(), registry.clone()];
        let (_, mut baseline) = poll_once(&fs, &root, &registry, &dirs, PollBaseline::default());
        for i in 0..20 {
            fs.remove_file(&project.join(session_name(i)));
        }
        tokio::time::advance(Duration::from_millis(POLL_MS)).await;
        let (polled, next) = poll_once(&fs, &root, &registry, &dirs, baseline);
        baseline = next;
        assert_eq!(polled.changed.len(), 20, "every transcript went");
        assert!(polled.changed.iter().all(|(_, stat)| stat.is_none()));
        // The rotation order is now longer than the files it knows: rebuilt, and
        // the tick after it must not panic or report anything.
        tokio::time::advance(Duration::from_millis(POLL_MS)).await;
        let (polled, _) = poll_once(&fs, &root, &registry, &dirs, baseline);
        assert!(polled.changed.is_empty(), "nothing left to report");
    }
}
