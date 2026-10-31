//! The live Claude Code processes on this machine, from the CLI's registry
//! (upstream `session-index/live-registry.ts`).
//!
//! Each running CLI writes `<config>/sessions/<pid>.json` with its session id,
//! kind, entrypoint and status. The format is not documented; the reader keeps
//! to the fields it needs and applies the liveness rules of Claude Desktop:
//!
//! - only `<pid>.json` files are read; `.key` files and other names are never
//!   opened, and no file is ever written or deleted;
//! - a record whose JSON does not parse is read once more, then skipped;
//! - a record of another pid domain (another machine or pid namespace) is
//!   skipped;
//! - the process must be alive: `kill(pid, 0)` succeeds or fails with `EPERM`
//!   ([`crate::process::pid_alive`]); a platform that cannot tell counts it as
//!   alive, as upstream does when it cannot tell;
//! - a recorded `procStart` must match the start time of the live process, so a
//!   reused pid does not count;
//! - a record older than 24 hours counts only when its `procStart` matches.
//!
//! The start times come from one `ps -o pid=,lstart= -p …` call for all the live
//! candidates (upstream's `defaultProcessStarts`), and the pid domain is read
//! once per registry. Where `ps` does not exist (Windows, a slim container) the
//! start times are unknown and a record counts as live by its age alone, as
//! upstream's Windows path does.
//!
//! A snapshot is cached for [`SNAPSHOT_TTL_MS`]. The records tell only the
//! `state` of a session list row; no operation is refused for them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const SNAPSHOT_TTL_MS: f64 = 5_000.0;
const STALE_RECORD_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
const PS_TIMEOUT: Duration = Duration::from_millis(1_000);
/// How long a process start time read with `ps` serves a reader that asks for
/// recent ones (see [`LiveSessionRegistry::read_files`]).
const RECENT_START_MS: f64 = 10_000.0;

#[derive(Clone, Debug, PartialEq)]
pub struct LiveRecord {
    pub pid: u32,
    pub session_id: String,
    pub cwd: Option<String>,
    pub kind: Option<String>,
    pub entrypoint: Option<String>,
    pub status: Option<String>,
    pub status_updated_at: Option<f64>,
}

pub type LiveSnapshot = HashMap<String, LiveRecord>;

/// The `ps -o lstart=` start time of each live pid.
pub type ProcessStarts = Box<dyn Fn(&[u32]) -> HashMap<u32, String> + Send>;

/// What the registry reads from the machine; each can be replaced in tests.
pub struct LiveRegistryDeps {
    /// The registry directory (`<config>/sessions`).
    pub dir: PathBuf,
    /// Epoch milliseconds.
    pub now: Box<dyn Fn() -> f64 + Send>,
    /// Whether the process is alive.
    pub is_alive: Box<dyn Fn(u32) -> bool + Send>,
    /// The `ps -o lstart=` start time of each live pid.
    pub process_starts: ProcessStarts,
    /// This machine's pid domain, as the CLI computes it.
    pub pid_domain: Box<dyn Fn() -> String + Send>,
}

impl LiveRegistryDeps {
    /// The real machine, with the registry in `dir`.
    pub fn system(dir: PathBuf) -> Self {
        Self {
            dir,
            now: Box::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
            }),
            is_alive: Box::new(default_is_alive),
            process_starts: Box::new(default_process_starts),
            pid_domain: Box::new(default_pid_domain),
        }
    }
}

/// Runs `command` for at most `timeout`; its stdout, or `None` when it could not
/// be started. A command that outlives the limit is killed (only the child this
/// function started).
fn run_with_timeout(mut command: Command, timeout: Duration) -> Option<(bool, String)> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(2));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut out, &mut stdout);
    }
    Some((status.is_some_and(|s| s.success()), stdout))
}

/// Whether the process exists: upstream's `kill(pid, 0)` (live-registry.ts:66-72),
/// with `EPERM` counted as alive. A platform that cannot tell counts the process
/// as alive, so a registry is never silently ignored.
fn default_is_alive(pid: u32) -> bool {
    crate::process::pid_alive(pid).unwrap_or(true)
}

fn normalize_start(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The start times of `pids`, from one `ps` call, in the format the CLI records
/// (`LC_ALL=C TZ=UTC ps -o lstart=`).
fn default_process_starts(pids: &[u32]) -> HashMap<u32, String> {
    let mut result = HashMap::new();
    if pids.is_empty() || std::env::consts::OS == "windows" {
        return result;
    }
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut command = Command::new("ps");
    command
        .args(["-o", "pid=,lstart=", "-p", &list])
        .env("LC_ALL", "C")
        .env("TZ", "UTC");
    // `ps` exits 1 when one of the pids is gone; its output still holds the live ones.
    if let Some((_, stdout)) = run_with_timeout(command, PS_TIMEOUT) {
        for line in stdout.lines() {
            let line = line.trim();
            if let Some((pid, start)) = line.split_once(char::is_whitespace) {
                if let Ok(pid) = pid.parse::<u32>() {
                    result.insert(pid, normalize_start(start));
                }
            }
        }
    }
    result
}

/// The pid domain the CLI records: the platform on macOS, the machine id and pid
/// namespace on Linux, the host name on Windows.
fn default_pid_domain() -> String {
    match std::env::consts::OS {
        "windows" => {
            let host = run_with_timeout(Command::new("hostname"), PS_TIMEOUT)
                .map(|(_, out)| out.trim().to_lowercase())
                .unwrap_or_default();
            format!("win32:{host}")
        }
        "linux" => {
            let machine_id = std::fs::read_to_string("/etc/machine-id")
                .map(|v| v.trim().to_string())
                .unwrap_or_default();
            let namespace = std::fs::read_link("/proc/self/ns/pid")
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            format!("linux:{machine_id}:{namespace}")
        }
        "macos" => "darwin".to_string(),
        other => other.to_string(),
    }
}

fn read_record(path: &std::path::Path) -> Option<serde_json::Map<String, Value>> {
    for _ in 0..2 {
        let text = std::fs::read_to_string(path).ok()?;
        // The CLI may be rewriting the file; read it once more.
        if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) {
            return Some(map);
        }
        if serde_json::from_str::<Value>(&text).is_ok() {
            return None;
        }
    }
    None
}

fn finite_number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite())
}

fn optional_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

/// `^(\d+)\.json$`.
fn record_pid(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".json")?;
    if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}

pub struct LiveSessionRegistry {
    deps: LiveRegistryDeps,
    cached: Option<(f64, LiveSnapshot)>,
    /// The start times `ps` gave, by pid, and when.
    starts: HashMap<u32, (String, f64)>,
    /// This machine's pid domain, read once (upstream caches it too): on Windows
    /// computing it runs `hostname`, and on Linux it reads two files.
    domain: Option<String>,
}

impl LiveSessionRegistry {
    pub fn new(deps: LiveRegistryDeps) -> Self {
        Self {
            deps,
            cached: None,
            starts: HashMap::new(),
            domain: None,
        }
    }

    /// The live records by session id. Cached for 5 s.
    pub fn snapshot(&mut self) -> LiveSnapshot {
        let now = (self.deps.now)();
        if let Some((at, snapshot)) = &self.cached {
            if now - at < SNAPSHOT_TTL_MS {
                return snapshot.clone();
            }
        }
        let snapshot = by_session(self.read());
        self.cached = Some((now, snapshot.clone()));
        snapshot
    }

    /// Every live record, several per session when several processes hold it.
    fn read(&mut self) -> Vec<LiveRecord> {
        let Ok(read) = std::fs::read_dir(&self.deps.dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = read
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        self.read_files(&names, false)
            .into_values()
            .flatten()
            .collect()
    }

    /// The live record of each of the registry files `names`, by name: `None` for a
    /// file that is gone or holds no live record. A name that is not `<pid>.json`
    /// (a `.key` file, a temporary file) is never opened and not in the result.
    /// `recent_starts` takes a start time that `ps` gave in the last 10 s when it
    /// matches the record, so a record that changes often does not run `ps` each
    /// time.
    pub fn read_files(
        &mut self,
        names: &[String],
        recent_starts: bool,
    ) -> HashMap<String, Option<LiveRecord>> {
        let mut result: HashMap<String, Option<LiveRecord>> = HashMap::new();
        let domain = match &self.domain {
            Some(domain) => domain.clone(),
            None => {
                let domain = (self.deps.pid_domain)();
                self.domain = Some(domain.clone());
                domain
            }
        };
        let mut candidates: Vec<(String, u32, serde_json::Map<String, Value>)> = Vec::new();
        for name in names {
            let Some(pid) = record_pid(name) else {
                continue;
            };
            result.insert(name.clone(), None);
            let Some(raw) = read_record(&self.deps.dir.join(name)) else {
                continue;
            };
            if finite_number(raw.get("pid")) != Some(f64::from(pid))
                || !raw.get("sessionId").is_some_and(Value::is_string)
            {
                continue;
            }
            if raw
                .get("pidDomain")
                .and_then(Value::as_str)
                .is_some_and(|d| d != domain)
            {
                continue;
            }
            if !(self.deps.is_alive)(pid) {
                continue;
            }
            candidates.push((name.clone(), pid, raw));
        }
        let now = (self.deps.now)();
        let mut starts: HashMap<u32, String> = HashMap::new();
        let mut need_starts: Vec<u32> = Vec::new();
        for (_, pid, raw) in &candidates {
            let Some(proc_start) = raw.get("procStart").and_then(Value::as_str) else {
                continue;
            };
            let recent = self.starts.get(pid);
            // A start that differs may be a new process under the same pid.
            match recent {
                Some((start, at))
                    if recent_starts
                        && now - at < RECENT_START_MS
                        && *start == normalize_start(proc_start) =>
                {
                    starts.insert(*pid, start.clone());
                }
                _ => need_starts.push(*pid),
            }
        }
        if !need_starts.is_empty() {
            for (pid, start) in (self.deps.process_starts)(&need_starts) {
                starts.insert(pid, start.clone());
                self.starts.insert(pid, (start, now));
            }
        }
        self.starts.retain(|_, (_, at)| now - *at < RECENT_START_MS);
        for (name, pid, raw) in candidates {
            let recorded = raw
                .get("procStart")
                .and_then(Value::as_str)
                .map(normalize_start);
            let actual = starts.get(&pid);
            if let (Some(recorded), Some(actual)) = (&recorded, actual) {
                if recorded != actual {
                    continue;
                }
            }
            let proc_start_matches = recorded.is_some() && actual == recorded.as_ref();
            let last_seen = finite_number(raw.get("updatedAt"))
                .unwrap_or(0.0)
                .max(finite_number(raw.get("statusUpdatedAt")).unwrap_or(0.0))
                .max(finite_number(raw.get("startedAt")).unwrap_or(0.0));
            if now - last_seen > STALE_RECORD_MS && !proc_start_matches {
                continue;
            }
            result.insert(
                name,
                Some(LiveRecord {
                    pid,
                    session_id: optional_string(raw.get("sessionId")).unwrap_or_default(),
                    cwd: optional_string(raw.get("cwd")),
                    kind: optional_string(raw.get("kind")),
                    entrypoint: optional_string(raw.get("entrypoint")),
                    status: optional_string(raw.get("status")),
                    status_updated_at: finite_number(raw.get("statusUpdatedAt")),
                }),
            );
        }
        result
    }
}

/// One record per session: the one with the newest status.
pub fn by_session(records: impl IntoIterator<Item = LiveRecord>) -> LiveSnapshot {
    let mut result: LiveSnapshot = HashMap::new();
    for record in records {
        let replace = match result.get(&record.session_id) {
            None => true,
            Some(previous) => {
                record.status_updated_at.unwrap_or(0.0) > previous.status_updated_at.unwrap_or(0.0)
            }
        };
        if replace {
            result.insert(record.session_id.clone(), record);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const NOW: f64 = 1_767_312_000_000.0; // 2026-01-02T00:00:00Z

    fn registry_dir(tag: &str, records: &[(&str, String)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acp-t2b-lr-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        for (name, content) in records {
            std::fs::write(dir.join(name), content).expect("write");
        }
        dir
    }

    fn deps(
        dir: PathBuf,
        alive: impl Fn(u32) -> bool + Send + 'static,
        starts: HashMap<u32, String>,
    ) -> LiveRegistryDeps {
        LiveRegistryDeps {
            dir,
            now: Box::new(|| NOW),
            is_alive: Box::new(alive),
            process_starts: Box::new(move |pids| {
                pids.iter()
                    .filter_map(|p| starts.get(p).map(|s| (*p, s.clone())))
                    .collect()
            }),
            pid_domain: Box::new(|| "darwin".to_string()),
        }
    }

    /// live registry > keeps live, matching records and applies the liveness rules
    #[test]
    fn keeps_live_matching_records_and_applies_the_liveness_rules() {
        let rec = |v: Value| v.to_string();
        let dir = registry_dir(
            "rules",
            &[
                (
                    "10.json",
                    rec(
                        json!({"pid": 10, "sessionId": "live", "procStart": "Mon Jan  1 00:00:00 2026", "updatedAt": NOW}),
                    ),
                ),
                (
                    "11.json",
                    rec(json!({"pid": 11, "sessionId": "dead", "updatedAt": NOW})),
                ),
                (
                    "12.json",
                    rec(
                        json!({"pid": 12, "sessionId": "reused", "procStart": "Sun Jan 1 00:00:00 2025", "updatedAt": NOW}),
                    ),
                ),
                (
                    "13.json",
                    rec(
                        json!({"pid": 13, "sessionId": "stale", "updatedAt": NOW - 25.0 * 3_600_000.0}),
                    ),
                ),
                (
                    "14.json",
                    rec(
                        json!({"pid": 14, "sessionId": "old-but-proven", "procStart": "Tue Jan 2 00:00:00 2024", "updatedAt": 0}),
                    ),
                ),
                (
                    "15.json",
                    rec(
                        json!({"pid": 15, "sessionId": "foreign", "pidDomain": "elsewhere", "updatedAt": NOW}),
                    ),
                ),
                ("16.json", "{ not json".to_string()),
                (
                    "17.json",
                    rec(json!({"pid": 99, "sessionId": "wrong-name", "updatedAt": NOW})),
                ),
                ("18.abcdef.key", "secret".to_string()),
            ],
        );
        let starts = HashMap::from([
            (10, "Mon Jan 1 00:00:00 2026".to_string()),
            (12, "Mon Jan 1 00:00:00 2026".to_string()),
            (14, "Tue Jan 2 00:00:00 2024".to_string()),
        ]);
        let mut registry = LiveSessionRegistry::new(deps(dir.clone(), |pid| pid != 11, starts));
        let snapshot = registry.snapshot();
        let mut keys: Vec<_> = snapshot.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec!["live".to_string(), "old-but-proven".to_string()]);
        assert_eq!(snapshot["live"].pid, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// live registry > reads single records, never a .key file, and reuses a recent start time on request
    #[test]
    fn reads_single_records_and_reuses_a_recent_start_time_on_request() {
        let dir = registry_dir(
            "single",
            &[
                ("10.json", json!({"pid": 10, "sessionId": "live", "procStart": "Mon Jan 1 00:00:00 2026", "updatedAt": NOW}).to_string()),
                ("10.json.tmp", "{}".to_string()),
                ("18.abcdef.key", "secret".to_string()),
            ],
        );
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = asked.clone();
        let mut registry = LiveSessionRegistry::new(LiveRegistryDeps {
            dir: dir.clone(),
            now: Box::new(|| NOW),
            is_alive: Box::new(|_| true),
            process_starts: Box::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                HashMap::from([(10, "Mon Jan 1 00:00:00 2026".to_string())])
            }),
            pid_domain: Box::new(|| "darwin".to_string()),
        });
        let names: Vec<String> = ["10.json", "10.json.tmp", "18.abcdef.key", "11.json"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let read = registry.read_files(&names, false);
        let mut keys: Vec<_> = read.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec!["10.json".to_string(), "11.json".to_string()]);
        assert_eq!(
            read["10.json"].as_ref().map(|r| r.session_id.as_str()),
            Some("live")
        );
        assert_eq!(read["11.json"], None);
        registry.read_files(&["10.json".to_string()], true);
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        // Without recent starts, `ps` is asked again.
        registry.read_files(&["10.json".to_string()], false);
        assert_eq!(asked.load(Ordering::SeqCst), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// live registry > is empty when the registry does not exist
    #[test]
    fn is_empty_when_the_registry_does_not_exist() {
        let mut registry = LiveSessionRegistry::new(LiveRegistryDeps::system(PathBuf::from(
            "/nonexistent/registry",
        )));
        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn keeps_the_record_with_the_newest_status_per_session() {
        let record = |pid, at| LiveRecord {
            pid,
            session_id: "s".into(),
            cwd: None,
            kind: None,
            entrypoint: None,
            status: None,
            status_updated_at: at,
        };
        let snapshot = by_session([record(1, Some(5.0)), record(2, Some(9.0)), record(3, None)]);
        assert_eq!(snapshot["s"].pid, 2);
    }

    #[test]
    fn the_snapshot_is_cached_for_five_seconds() {
        let dir = registry_dir(
            "cache",
            &[(
                "10.json",
                json!({"pid": 10, "sessionId": "a", "updatedAt": NOW}).to_string(),
            )],
        );
        let mut registry = LiveSessionRegistry::new(deps(dir.clone(), |_| true, HashMap::new()));
        assert_eq!(registry.snapshot().len(), 1);
        std::fs::remove_file(dir.join("10.json")).expect("remove");
        assert_eq!(registry.snapshot().len(), 1, "served from the cache");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
