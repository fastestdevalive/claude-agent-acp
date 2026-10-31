//! T8D — `session-index.test.ts` behaviours that had no Rust proof: the
//! mid-session worktree mark, cwd recovery across the archive filter, the legacy
//! archive marker, multi-copy rename/delete, the sidecar under concurrent
//! writes, subscription rows (archive state, worktree mark, shared scope,
//! continued successors), relocation, upper-case ids and a torn last line.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use agent_client_protocol::Error;
use claude_agent_acp_rs::air::AirClient;
use claude_agent_acp_rs::session_index::connection::{IndexPaths, SessionIndexConnection};
use claude_agent_acp_rs::session_index::list_subscriptions::ListSubscriptionTiming;
use claude_agent_acp_rs::session_index::project_dirs::{encode_project_path, Platform};
use claude_agent_acp_rs::session_index::service::{
    write_custom_title_sidecar, BoxFuture, LiveTitle, RenameFn, RetitleOptions, ServiceDeps,
    SessionIndexService, SidecarMode, TitleChange,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

static NEXT: AtomicUsize = AtomicUsize::new(0);
const T0: i64 = 1_767_225_600_000;

fn id(n: usize) -> String {
    format!("aaaaaaaa-aaaa-4aaa-8aaa-{n:012}")
}

fn iso(ms: i64) -> String {
    claude_agent_acp_rs::session_store::iso8601(ms as u64)
}

fn set_mtime(path: &Path, ms: i64) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open");
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_millis(ms as u64))
        .expect("mtime");
}

fn timing() -> ListSubscriptionTiming {
    ListSubscriptionTiming {
        debounce_ms: 20,
        max_wait_ms: 100,
        rescan_ms: 400,
        settle_ms: 100,
        min_session_interval_ms: 250,
        poll_ms: 20,
    }
}

struct Env {
    base: PathBuf,
    root: PathBuf,
    registry: PathBuf,
    cwd: String,
}

impl Env {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "acp-t8d-idx-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("projects")).expect("root");
        std::fs::create_dir_all(base.join("sessions")).expect("registry");
        std::fs::create_dir_all(base.join("work/app")).expect("cwd");
        let base = std::fs::canonicalize(&base).expect("canonical");
        let cwd = base.join("work/app").to_string_lossy().into_owned();
        Self {
            root: base.join("projects"),
            registry: base.join("sessions"),
            base,
            cwd,
        }
    }

    fn dir_for(&self, cwd: &str) -> PathBuf {
        let dir = self.root.join(encode_project_path(cwd, Platform::host()));
        std::fs::create_dir_all(&dir).expect("project dir");
        dir
    }

    fn transcript_in(&self, dir_name: &str, session: &str, lines: &[Value], mtime: i64) -> PathBuf {
        let dir = self.root.join(dir_name);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(format!("{session}.jsonl"));
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, text).expect("write");
        set_mtime(&path, mtime);
        path
    }

    fn transcript(&self, cwd: &str, session: &str, lines: &[Value], mtime: i64) -> PathBuf {
        self.transcript_in(
            &encode_project_path(cwd, Platform::host()),
            session,
            lines,
            mtime,
        )
    }

    fn service(&self) -> SessionIndexService {
        SessionIndexService::new(
            self.root.clone(),
            self.registry.clone(),
            ServiceDeps::new(Arc::new(|_, _| {})),
        )
    }

    fn connection(&self, changes: mpsc::UnboundedSender<Value>) -> SessionIndexConnection {
        let mut connection = SessionIndexConnection::new(
            IndexPaths {
                projects_root: self.root.clone(),
                registry_dir: self.registry.clone(),
            },
            Arc::new(|_, _| {}),
        );
        connection.set_notifier(Arc::new(move |value| {
            let tx = changes.clone();
            Box::pin(async move {
                let _ = tx.send(value);
                Ok::<(), Error>(())
            }) as BoxFuture<Result<(), Error>>
        }));
        connection.set_subscription_timing(timing());
        let air = AirClient::from_client_capabilities(&json!({"_meta": {"jetbrains": {"air": {
            "version": 1, "capabilities": ["sessionIndex"]
        }}}}));
        connection.negotiate(air.as_ref(), false);
        connection
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn user(session: &str, cwd: &str, text: &str, at: i64) -> Value {
    json!({"type": "user", "sessionId": session, "cwd": cwd, "timestamp": iso(at),
        "message": {"role": "user", "content": text}})
}

fn assistant(session: &str, at: i64) -> Value {
    json!({"type": "assistant", "sessionId": session, "timestamp": iso(at),
        "message": {"role": "assistant", "model": "claude-test", "stop_reason": "end_turn", "content": []}})
}

fn custom_title(session: &str, title: &str) -> Value {
    json!({"type": "custom-title", "customTitle": title, "sessionId": session})
}

fn agent_name(session: &str, title: &str) -> Value {
    json!({"type": "agent-name", "agentName": title, "sessionId": session})
}

fn air(list: Value) -> Value {
    json!({"jetbrains": {"air": {"version": 1, "list": list}}})
}

async fn list(
    service: &SessionIndexService,
    params: Value,
) -> Result<Value, agent_client_protocol::Error> {
    timeout(Duration::from_secs(30), service.list(&params, &|_| None))
        .await
        .expect("list finished")
}

fn ids(response: &Value) -> Vec<String> {
    response["sessions"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r["sessionId"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn row_of<'a>(response: &'a Value, session: &str) -> Option<&'a Value> {
    response["sessions"]
        .as_array()?
        .iter()
        .find(|r| r["sessionId"] == session)
}

fn retitle_options(sidecar: SidecarMode) -> RetitleOptions {
    RetitleOptions {
        may_be_unwritten: false,
        sidecar,
    }
}

fn worktree_state(session: &str, path: &Path) -> Value {
    json!({
        "type": "worktree-state",
        "worktreeSession": {
            "originalCwd": "/repo",
            "worktreePath": path,
            "worktreeName": "made",
            "sessionId": session,
        },
        "sessionId": session,
    })
}

fn relocated(session: &str, cwd: &Path) -> Value {
    json!({"type": "relocated", "sessionId": session, "relocatedCwd": cwd})
}

async fn subscribe(connection: &SessionIndexConnection, cwd: &str) -> String {
    let operation = connection
        .subscribe_list(&json!({"cwd": cwd}))
        .expect("accepted");
    let response = timeout(Duration::from_secs(30), operation)
        .await
        .expect("subscribed in time")
        .expect("subscribed");
    response["subscriptionId"]
        .as_str()
        .expect("an id")
        .to_string()
}

async fn wait_for<F>(rx: &mut mpsc::UnboundedReceiver<Value>, mut check: F) -> bool
where
    F: FnMut(&Value) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        match timeout(left, rx.recv()).await {
            Ok(Some(change)) => {
                if check(&change) {
                    return true;
                }
            }
            _ => return false,
        }
    }
}

fn link_worktree(repo: &Path, linked: &Path) {
    let meta = repo.join(".git/worktrees").join(
        linked
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "wt".to_string()),
    );
    std::fs::create_dir_all(&meta).expect("meta");
    std::fs::create_dir_all(linked).expect("linked");
    std::fs::write(
        meta.join("gitdir"),
        format!("{}\n", linked.join(".git").display()),
    )
    .expect("gitdir");
    std::fs::write(meta.join("commondir"), "../..\n").expect("commondir");
    std::fs::write(linked.join(".git"), format!("gitdir: {}\n", meta.display())).expect("dot git");
}

/// session/list > marks a session that made its worktree mid-session, and not
/// after a malformed record.
#[tokio::test]
async fn t8d_marks_a_mid_session_worktree_and_a_malformed_record() {
    let env = Env::new("mid-worktree");
    let repo = env.base.join("work/repo");
    let made = env.base.join("work/made");
    let other = env.base.join("work/other");
    std::fs::create_dir_all(&repo).expect("repo");
    std::fs::create_dir_all(&other).expect("other");
    link_worktree(&repo, &made);
    link_worktree(&repo, &other);
    let repo_str = repo.to_string_lossy().into_owned();
    let made_str = made.to_string_lossy().into_owned();
    let other_str = other.to_string_lossy().into_owned();

    let moved = id(1);
    // Started in the main checkout; EnterWorktree moved the transcript to the
    // worktree's project directory, and the relocated record names the worktree.
    env.transcript_in(
        &encode_project_path(&made_str, Platform::host()),
        &moved,
        &[
            worktree_state(&moved, &made),
            relocated(&moved, &made),
            user(&moved, &repo_str, "work in the worktree", T0),
            assistant(&moved, T0 + 1000),
        ],
        T0 + 2000,
    );

    let malformed = id(2);
    env.transcript(
        &other_str,
        &malformed,
        &[
            worktree_state(&malformed, &other),
            json!({"type": "worktree-state", "worktreeSession": "other", "sessionId": malformed}),
            user(&malformed, &other_str, "not a worktree", T0 + 5000),
            assistant(&malformed, T0 + 6000),
        ],
        T0 + 7000,
    );

    let response = list(
        &env.service(),
        json!({"cwd": repo_str, "_meta": air(json!({"includeWorktrees": true}))}),
    )
    .await
    .expect("list");
    let moved_row = row_of(&response, &moved).expect("moved row");
    assert_eq!(moved_row["cwd"].as_str(), Some(made_str.as_str()));
    assert_eq!(
        moved_row["_meta"]["jetbrains"]["air"]["agentWorktree"],
        true
    );
    let malformed_row = row_of(&response, &malformed).expect("malformed row");
    assert!(malformed_row["_meta"]["jetbrains"]["air"]
        .get("agentWorktree")
        .is_none());
}

/// session/list > recovers a cwd from a sibling that the archive filter leaves out.
#[tokio::test]
async fn t8d_recovers_a_cwd_from_an_archived_sibling() {
    let env = Env::new("cwd-sibling");
    let other = env.base.join("work/other");
    std::fs::create_dir_all(&other).expect("other");
    let other_str = other.to_string_lossy().into_owned();
    let with_cwd = id(1);
    env.transcript(
        &other_str,
        &with_cwd,
        &[
            user(&with_cwd, &other_str, "with cwd", T0),
            assistant(&with_cwd, T0 + 1000),
        ],
        T0 + 2000,
    );
    let no_cwd = id(2);
    env.transcript(
        &other_str,
        &no_cwd,
        &[
            json!({"type": "user", "sessionId": no_cwd, "timestamp": iso(T0 + 3000),
                "message": {"role": "user", "content": "no cwd"}}),
            assistant(&no_cwd, T0 + 4000),
        ],
        T0 + 5000,
    );

    let service = env.service();
    service
        .retitle(
            &no_cwd,
            TitleChange::Archive(true),
            retitle_options(SidecarMode::Existing),
            None,
        )
        .await
        .expect("archived");
    let archived = list(
        &service,
        json!({"_meta": air(json!({"archived": "archived"}))}),
    )
    .await
    .expect("archived list");
    assert_eq!(ids(&archived), vec![no_cwd.clone()]);
    assert_eq!(
        archived["sessions"][0]["cwd"].as_str(),
        Some(other_str.as_str())
    );

    // The reverse: the sibling with the cwd is archived.
    service
        .retitle(
            &no_cwd,
            TitleChange::Archive(false),
            retitle_options(SidecarMode::Existing),
            None,
        )
        .await
        .expect("unarchived");
    service
        .retitle(
            &with_cwd,
            TitleChange::Archive(true),
            retitle_options(SidecarMode::Existing),
            None,
        )
        .await
        .expect("archived sibling");
    let unarchived = list(&service, json!({})).await.expect("unarchived list");
    assert_eq!(ids(&unarchived), vec![no_cwd]);
    assert_eq!(
        unarchived["sessions"][0]["cwd"].as_str(),
        Some(other_str.as_str())
    );
}

/// _session/archive > ignores a legacy archive marker file.
#[tokio::test]
async fn t8d_ignores_a_legacy_archive_marker_file() {
    let env = Env::new("legacy-marker");
    let session = id(1);
    env.transcript(
        &env.cwd,
        &session,
        &[
            user(&session, &env.cwd, "Fix it", T0),
            assistant(&session, T0 + 1000),
        ],
        T0 + 2000,
    );
    let marker_dir = env.base.join("acp/archived");
    std::fs::create_dir_all(&marker_dir).expect("marker dir");
    std::fs::write(marker_dir.join(&session), "").expect("marker");
    let page = list(&env.service(), json!({"cwd": env.cwd}))
        .await
        .expect("list");
    assert_eq!(ids(&page), vec![session.clone()]);
    assert_eq!(
        page["sessions"][0]["_meta"]["jetbrains"]["air"]["archived"],
        false
    );
    assert!(
        marker_dir.join(&session).exists(),
        "the marker is left alone"
    );
}

/// rename of a session with several transcripts > titles every copy.
#[tokio::test]
async fn t8d_rename_titles_every_copy() {
    let env = Env::new("rename-copies");
    let a = env.base.join("work/a");
    let b = env.base.join("work/b");
    std::fs::create_dir_all(&a).expect("a");
    std::fs::create_dir_all(&b).expect("b");
    let session = id(1);
    let a_str = a.to_string_lossy().into_owned();
    let b_str = b.to_string_lossy().into_owned();
    let small = env.transcript(
        &a_str,
        &session,
        &[user(&session, &a_str, "a", T0)],
        T0 + 1000,
    );
    let large = env.transcript(
        &b_str,
        &session,
        &[
            user(&session, &b_str, "b", T0 + 2000),
            assistant(&session, T0 + 3000),
        ],
        T0 + 4000,
    );
    let service = env.service();
    service
        .retitle(
            &session,
            TitleChange::Rename("Both copies".into()),
            retitle_options(SidecarMode::Always),
            None,
        )
        .await
        .expect("renamed");
    for path in [small, large] {
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("\"customTitle\":\"Both copies\""),
            "{path:?} was titled"
        );
        assert!(text.contains("\"agentName\":\"Both copies\""));
    }
    let page = list(&service, json!({})).await.expect("list");
    assert_eq!(page["sessions"][0]["title"], "Both copies");
}

/// delete of a session with several transcripts > reports a copy it could not
/// delete, and a retry deletes it.
#[tokio::test]
async fn t8d_delete_reports_a_copy_it_could_not_remove_and_a_retry_finishes() {
    let env = Env::new("delete-failure");
    let a = env.base.join("work/a");
    let b = env.base.join("work/b");
    std::fs::create_dir_all(&a).expect("a");
    std::fs::create_dir_all(&b).expect("b");
    let session = id(1);
    let a_str = a.to_string_lossy().into_owned();
    let b_str = b.to_string_lossy().into_owned();
    let first = env.transcript(
        &a_str,
        &session,
        &[user(&session, &a_str, "a", T0)],
        T0 + 1000,
    );
    let second = env.transcript(
        &b_str,
        &session,
        &[user(&session, &b_str, "b", T0 + 2000)],
        T0 + 3000,
    );
    let b_dir = env.dir_for(&b_str);
    let a_dir = env.dir_for(&a_str);
    let mut perms = std::fs::metadata(&b_dir).expect("meta").permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&b_dir, perms).expect("readonly");
    let service = env.service();
    let error = service
        .delete(&session, false)
        .await
        .expect_err("a copy fails");
    assert_eq!(i32::from(error.code), -32603);
    // RR1b-02: Node rethrows the SDK delete's filesystem error, so the client
    // gets `EACCES: permission denied, unlink '<path>'` in `data.details`
    // (`session-index/service.ts:672-680`), not the "was not deleted" summary.
    assert_eq!(
        error.data.as_ref().expect("details")["details"],
        json!(format!(
            "EACCES: permission denied, unlink '{}'",
            second.display()
        )),
        "the delete failure is the filesystem error"
    );
    // RR2-02: the delete stops at the first copy it cannot remove, so only the
    // copies the scan reached before `second` are gone. The project directories
    // have no guaranteed listing order, so `first` is either already removed
    // (scanned before) or still on disk (scanned after) — never both removed.
    let remaining: Vec<&PathBuf> = [&first, &second]
        .into_iter()
        .filter(|path| path.exists())
        .collect();
    assert!(
        remaining.contains(&&second),
        "the undeletable copy always remains"
    );
    assert!(
        remaining == vec![&second] || remaining == vec![&first, &second],
        "no copy after the failure is removed"
    );
    let perms = std::fs::metadata(&a_dir).expect("meta").permissions();
    std::fs::set_permissions(&b_dir, perms).expect("writable");
    service.delete(&session, false).await.expect("retry");
    assert!(!first.exists() && !second.exists());
}

/// delete of a session with several transcripts > stops at the first copy it
/// cannot remove, like Node (`session-index/service.ts:674-680`): the copies
/// after it, the empty transcripts and the session directories stay.
#[tokio::test]
async fn t8d_delete_stops_at_the_first_copy_it_cannot_remove() {
    let env = Env::new("delete-first-failure");
    let a = env.base.join("work/a");
    let b = env.base.join("work/b");
    std::fs::create_dir_all(&a).expect("a");
    std::fs::create_dir_all(&b).expect("b");
    let session = id(1);
    let a_str = a.to_string_lossy().into_owned();
    let b_str = b.to_string_lossy().into_owned();
    // The only non-empty copy cannot be removed: the SDK delete fails on it.
    let failing = env.transcript(
        &a_str,
        &session,
        &[user(&session, &a_str, "a", T0)],
        T0 + 1000,
    );
    // The empty copy the SDK skips comes later, as do both session directories.
    let empty = env.transcript(&b_str, &session, &[], T0 + 2000);
    let a_project = env.dir_for(&a_str);
    let b_project = env.dir_for(&b_str);
    let a_session_dir = a_project.join(&session);
    let b_session_dir = b_project.join(&session);
    std::fs::create_dir_all(&a_session_dir).expect("a session dir");
    std::fs::create_dir_all(&b_session_dir).expect("b session dir");
    std::fs::write(a_session_dir.join("subagent.jsonl"), "{}").expect("a sidecar");
    std::fs::write(b_session_dir.join("subagent.jsonl"), "{}").expect("b sidecar");
    let mut perms = std::fs::metadata(&a_project).expect("meta").permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&a_project, perms).expect("readonly");
    let service = env.service();
    let error = service
        .delete(&session, false)
        .await
        .expect_err("the first copy fails");
    assert_eq!(i32::from(error.code), -32603);
    // RR1b-02: Node rethrows the SDK delete's filesystem error.
    assert_eq!(
        error.data.as_ref().expect("details")["details"],
        json!(format!(
            "EACCES: permission denied, unlink '{}'",
            failing.display()
        )),
        "the delete failure is the filesystem error of the first copy"
    );
    // RR2-02: Node stops at that failure, so the later copies and every session
    // directory are still on disk.
    assert!(failing.exists(), "the undeletable copy remains");
    assert!(empty.exists(), "the later empty copy stays");
    assert!(a_session_dir.exists(), "the first copy's session dir stays");
    assert!(b_session_dir.exists(), "the later session dir stays");
    // A retry finishes once the obstacle is gone.
    let perms = std::fs::metadata(&b_project).expect("meta").permissions();
    std::fs::set_permissions(&a_project, perms).expect("writable");
    service.delete(&session, false).await.expect("retry");
    assert!(!failing.exists() && !empty.exists());
    assert!(!a_session_dir.exists() && !b_session_dir.exists());
}

/// _session/list/subscribe > sends the archive state by the agent name first,
/// else the custom title, and marks the change rows of a worktree Claude Code
/// made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t8d_subscription_rows_take_the_agent_name_first_and_mark_agent_worktrees() {
    let env = Env::new("sub-rows");
    let repo = env.base.join("work/repo");
    let made = env.base.join("work/made");
    std::fs::create_dir_all(&repo).expect("repo");
    link_worktree(&repo, &made);
    let repo_str = repo.to_string_lossy().into_owned();
    let made_str = made.to_string_lossy().into_owned();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let connection = env.connection(tx);
    let _subscription = subscribe(&connection, &repo_str).await;

    let by_agent_name = id(1);
    env.transcript(
        &repo_str,
        &by_agent_name,
        &[
            custom_title(&by_agent_name, "Open"),
            agent_name(&by_agent_name, "[archived] Open"),
            user(&by_agent_name, &repo_str, "x", T0),
            assistant(&by_agent_name, T0 + 1000),
        ],
        T0 + 2000,
    );
    let by_title = id(2);
    env.transcript(
        &repo_str,
        &by_title,
        &[
            agent_name(&by_title, "Named"),
            custom_title(&by_title, "[archived] Named"),
            user(&by_title, &repo_str, "y", T0 + 3000),
            assistant(&by_title, T0 + 4000),
        ],
        T0 + 5000,
    );
    let in_made = id(3);
    env.transcript_in(
        &encode_project_path(&made_str, Platform::host()),
        &in_made,
        &[
            worktree_state(&in_made, &made),
            user(&in_made, &made_str, "z", T0 + 6000),
            assistant(&in_made, T0 + 7000),
        ],
        T0 + 8000,
    );

    let mut seen: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let wanted = |seen: &std::collections::HashMap<String, Value>| {
        let agent = seen.get(&by_agent_name);
        let title = seen.get(&by_title);
        let worktree = seen.get(&in_made);
        agent.is_some_and(|row| {
            row["title"] == "Open" && row["_meta"]["jetbrains"]["air"]["archived"] == true
        }) && title.is_some_and(|row| {
            row["title"] == "Named" && row["_meta"]["jetbrains"]["air"]["archived"] == false
        }) && worktree.is_some_and(|row| row["_meta"]["jetbrains"]["air"]["agentWorktree"] == true)
    };
    let found = wait_for(&mut rx, |change| {
        for row in change["sessions"].as_array().into_iter().flatten() {
            if let Some(session) = row["sessionId"].as_str() {
                seen.insert(session.to_string(), row.clone());
            }
        }
        wanted(&seen)
    })
    .await;
    assert!(found, "the expected subscription rows arrived: {seen:?}");
    connection.dispose();
}

/// _session/list/subscribe > shares one watcher between two subscriptions of a
/// cwd, each notified.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t8d_two_subscriptions_share_one_scope_and_are_both_notified() {
    let env = Env::new("sub-shared");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let connection = env.connection(tx);
    let first = subscribe(&connection, &env.cwd).await;
    let second = subscribe(&connection, &env.cwd).await;
    let session = id(1);
    env.transcript(
        &env.cwd,
        &session,
        &[
            user(&session, &env.cwd, "first", T0),
            assistant(&session, T0 + 1000),
        ],
        T0 + 2000,
    );
    let mut seen = std::collections::HashSet::new();
    let found = wait_for(&mut rx, |change| {
        if change["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|row| row["sessionId"].as_str() == Some(session.as_str()))
        {
            if let Some(id) = change["subscriptionId"].as_str() {
                seen.insert(id.to_string());
            }
        }
        seen.contains(&first) && seen.contains(&second)
    })
    .await;
    assert!(found, "both subscriptions were notified: {seen:?}");
    // Both subscriptions of one cwd share one watch.
    let handles = connection
        .subscription_handles()
        .await
        .expect("subscriptions exist");
    assert_eq!(handles["watches"], 1, "one watch for the scope: {handles}");
    assert_eq!(handles["subscriptions"], 2);

    // Unsubscribing the first leaves one watch and only the second notified.
    connection
        .unsubscribe_list(&json!({"subscriptionId": first}))
        .expect("unsubscribed");
    let handles = connection
        .subscription_handles()
        .await
        .expect("subscriptions exist");
    assert_eq!(
        handles["watches"], 1,
        "the remaining one watches: {handles}"
    );
    assert_eq!(handles["subscriptions"], 1);
    let second_session = id(2);
    env.transcript(
        &env.cwd,
        &second_session,
        &[
            user(&second_session, &env.cwd, "second", T0 + 3000),
            assistant(&second_session, T0 + 4000),
        ],
        T0 + 5000,
    );
    let mut after = std::collections::HashSet::new();
    let found = wait_for(&mut rx, |change| {
        if let Some(id) = change["subscriptionId"].as_str() {
            after.insert(id.to_string());
        }
        after.contains(&second)
    })
    .await;
    assert!(found, "the remaining subscription is notified");
    assert!(
        !after.contains(&first),
        "the unsubscribed one is not notified: {after:?}"
    );

    // Unsubscribing the last drops the watch.
    connection
        .unsubscribe_list(&json!({"subscriptionId": second}))
        .expect("unsubscribed");
    let handles = connection
        .subscription_handles()
        .await
        .expect("subscriptions exist");
    assert_eq!(handles["watches"], 0, "no watch is left: {handles}");
    assert_eq!(handles["subscriptions"], 0);
    connection.dispose();
}

/// _session/list/subscribe > shows a session continued in a successor deleted
/// after it was listed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t8d_subscription_shows_a_continued_session_after_its_successor_is_deleted() {
    let env = Env::new("sub-continued");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let connection = env.connection(tx);
    let subscription = subscribe(&connection, &env.cwd).await;
    let successor = id(2);
    let predecessor = id(1);
    env.transcript(
        &env.cwd,
        &predecessor,
        &[
            user(&predecessor, &env.cwd, "start", T0),
            assistant(&predecessor, T0 + 1000),
            json!({"type": "continued-in", "continuedInSessionId": successor}),
        ],
        T0 + 2000,
    );
    let successor_file = env.transcript(
        &env.cwd,
        &successor,
        &[custom_title(&successor, "carry on")],
        T0 + 3000,
    );

    let shown = |change: &Value| {
        change["sessions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|row| row["sessionId"].as_str() == Some(predecessor.as_str()))
    };
    assert!(
        wait_for(&mut rx, shown).await,
        "the predecessor is listed while its successor has no history"
    );

    // The successor gets history: the predecessor is hidden.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&successor_file)
        .expect("open");
    use std::io::Write;
    writeln!(
        file,
        "{}",
        json!({"type": "user", "parentUuid": null, "sessionId": successor,
            "cwd": env.cwd, "timestamp": iso(T0 + 4000),
            "message": {"role": "user", "content": "carry on"}})
    )
    .expect("append");
    drop(file);
    let removed = |change: &Value| {
        change["removed"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|id| id.as_str() == Some(predecessor.as_str()))
    };
    assert!(
        wait_for(&mut rx, removed).await,
        "the predecessor is removed once its successor has history"
    );

    // The successor goes: the predecessor shows again.
    std::fs::remove_file(&successor_file).expect("remove");
    let shown_again = |change: &Value| {
        change["subscriptionId"].as_str() == Some(subscription.as_str())
            && change["sessions"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|row| row["sessionId"].as_str() == Some(predecessor.as_str()))
    };
    assert!(
        wait_for(&mut rx, shown_again).await,
        "the predecessor shows again once its successor is gone"
    );
    connection.dispose();
}

/// a session relocated to a path with the same project directory > is listed
/// under the cwd it was moved to.
#[tokio::test]
async fn t8d_lists_a_relocated_session_under_the_cwd_it_moved_to() {
    let env = Env::new("relocated");
    let from = env.base.join("work/app.v2");
    let to = env.base.join("work/app-v2");
    std::fs::create_dir_all(&from).expect("from");
    std::fs::create_dir_all(&to).expect("to");
    let from_str = from.to_string_lossy().into_owned();
    let to_str = to.to_string_lossy().into_owned();
    let session = id(1);
    env.transcript(
        &from_str,
        &session,
        &[
            user(&session, &from_str, "moved", T0),
            assistant(&session, T0 + 1000),
            relocated(&session, &to),
        ],
        T0 + 2000,
    );
    let service = env.service();
    let under_to = list(&service, json!({"cwd": to_str})).await.expect("to");
    assert_eq!(ids(&under_to), vec![session.clone()]);
    assert_eq!(
        under_to["sessions"][0]["cwd"].as_str(),
        Some(to_str.as_str())
    );
    let under_from = list(&service, json!({"cwd": from_str}))
        .await
        .expect("from");
    assert!(ids(&under_from).is_empty());
}

/// a transcript named by an upper-case id > is renamed, archived and deleted
/// under the listed id and its own spelling.
#[tokio::test]
async fn t8d_upper_case_transcript_id_is_mutated_under_its_own_spelling() {
    let env = Env::new("upper-id");
    let upper = "AAAAAAAA-AAAA-4AAA-8AAA-000000000001";
    let transcript = env.transcript(
        &env.cwd,
        upper,
        &[
            user(upper, &env.cwd, "upper", T0),
            assistant(upper, T0 + 1000),
        ],
        T0 + 2000,
    );
    let service = env.service();
    let page = list(&service, json!({"cwd": env.cwd})).await.expect("list");
    assert_eq!(ids(&page), vec![upper.to_string()]);

    service
        .retitle(
            upper,
            TitleChange::Rename("Upper".into()),
            retitle_options(SidecarMode::Always),
            None,
        )
        .await
        .expect("renamed");
    let text = std::fs::read_to_string(&transcript).expect("read");
    assert!(text.contains(&format!("\"sessionId\":\"{upper}\"")));
    service
        .retitle(
            upper,
            TitleChange::Archive(true),
            retitle_options(SidecarMode::Existing),
            None,
        )
        .await
        .expect("archived");
    let all = list(
        &service,
        json!({"cwd": env.cwd, "_meta": air(json!({"archived": "all"}))}),
    )
    .await
    .expect("all");
    assert_eq!(
        all["sessions"][0]["_meta"]["jetbrains"]["air"]["archived"],
        true
    );

    service.delete(upper, false).await.expect("deleted");
    assert!(!transcript.exists());
}

/// a title record after a torn last line > follows a complete line when no
/// process writes the transcript.
#[tokio::test]
async fn t8d_rename_closes_a_torn_last_line() {
    let env = Env::new("torn-line");
    let session = id(1);
    let transcript = env.transcript(
        &env.cwd,
        &session,
        &[
            user(&session, &env.cwd, "Fix it", T0),
            assistant(&session, T0 + 1000),
        ],
        T0 + 2000,
    );
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .expect("open");
    use std::io::Write;
    write!(file, "{{\"type\":\"assistant\",\"mess").expect("torn line");
    drop(file);
    env.service()
        .retitle(
            &session,
            TitleChange::Rename("Later".into()),
            retitle_options(SidecarMode::Always),
            None,
        )
        .await
        .expect("renamed");
    let text = std::fs::read_to_string(&transcript).expect("read");
    assert!(
        text.contains("{\"type\":\"assistant\",\"mess\n{\"type\":\"custom-title\""),
        "the torn line is closed before the records: {text}"
    );
}

/// rename of a running session with several transcripts > titles the other
/// copies here and leaves the CLI's own copy to the CLI.
#[tokio::test]
async fn t8d_rename_running_session_titles_the_other_copies() {
    let env = Env::new("rename-live");
    let own_cwd = env.cwd.clone();
    let moved = env.base.join("work/moved");
    std::fs::create_dir_all(&moved).expect("moved");
    let moved_str = moved.to_string_lossy().into_owned();
    let session = id(1);
    let own = env.transcript(
        &own_cwd,
        &session,
        &[
            user(&session, &own_cwd, "own", T0),
            assistant(&session, T0 + 1000),
        ],
        T0 + 2000,
    );
    let other = env.transcript(
        &moved_str,
        &session,
        &[
            user(&session, &moved_str, "other", T0 + 3000),
            assistant(&session, T0 + 4000),
        ],
        T0 + 5000,
    );
    let own_before = std::fs::read_to_string(&own).expect("read own");
    let rename: RenameFn = Box::new(move |title| {
        Box::pin(async move {
            assert_eq!(title, "Live title");
            Ok(())
        }) as BoxFuture<Result<(), Error>>
    });
    let live = LiveTitle {
        cwd: own_cwd.clone(),
        rename,
        stored: Box::new(|| None),
        remember: Box::new(|_| {}),
        shown: None,
    };
    env.service()
        .retitle(
            &session,
            TitleChange::Rename("Live title".into()),
            retitle_options(SidecarMode::Always),
            Some(live),
        )
        .await
        .expect("renamed");
    assert_eq!(
        std::fs::read_to_string(&own).expect("own"),
        own_before,
        "the CLI's own transcript is the CLI's"
    );
    let other_text = std::fs::read_to_string(&other).expect("other");
    assert!(other_text.contains("\"customTitle\":\"Live title\""));
    let sidecar = other
        .parent()
        .expect("dir")
        .join(&session)
        .join("custom-title.json");
    assert_eq!(
        std::fs::read_to_string(sidecar).expect("sidecar"),
        "{\"customTitle\":\"Live title\"}"
    );
}

/// archive in AIR's title format > brings an existing sidecar in line with a
/// transcript already archived.
#[tokio::test]
async fn t8d_archive_aligns_an_existing_sidecar() {
    let env = Env::new("archive-sidecar");
    let session = id(1);
    let transcript = env.transcript(
        &env.cwd,
        &session,
        &[
            user(&session, &env.cwd, "Fix it", T0),
            assistant(&session, T0 + 1000),
            custom_title(&session, "[archived] Done"),
            agent_name(&session, "[archived] Done"),
        ],
        T0 + 2000,
    );
    write_custom_title_sidecar(&transcript, "Done").expect("sidecar");
    let before = std::fs::read_to_string(&transcript).expect("read");
    env.service()
        .retitle(
            &session,
            TitleChange::Archive(true),
            retitle_options(SidecarMode::Existing),
            None,
        )
        .await
        .expect("archived");
    assert_eq!(
        std::fs::read_to_string(&transcript).expect("read"),
        before,
        "an already archived transcript is not rewritten"
    );
    let sidecar = transcript
        .parent()
        .expect("dir")
        .join(&session)
        .join("custom-title.json");
    assert_eq!(
        std::fs::read_to_string(sidecar).expect("sidecar"),
        "{\"customTitle\":\"[archived] Done\"}"
    );
}

/// empty transcripts on delete > archives for an AIR client without sessionIndex
/// only what the SDK delete would find.
#[tokio::test]
async fn t8d_air_archive_of_an_empty_transcript_fails() {
    let env = Env::new("air-empty");
    let session = id(1);
    let transcript = env.transcript(&env.cwd, &session, &[], T0 + 1000);
    assert_eq!(std::fs::metadata(&transcript).expect("meta").len(), 0);
    let mut connection = env.connection(mpsc::unbounded_channel().0);
    let plain_air = AirClient::from_client_capabilities(
        &json!({"_meta": {"jetbrains": {"air": {"version": 1, "capabilities": []}}}}),
    );
    connection.negotiate(plain_air.as_ref(), false);
    let operation = connection
        .delete_session(session.clone(), false, true)
        .expect("an AIR client archives");
    let error = timeout(Duration::from_secs(30), operation)
        .await
        .expect("finished")
        .expect_err("an empty transcript is no SDK delete");
    assert_eq!(i32::from(error.code), -32603);
    assert_eq!(std::fs::metadata(&transcript).expect("meta").len(), 0);
}

/// `session-index.test.ts` > "sessionIndex negotiation > is not advertised under
/// ACP v2": a v2 AIR client that declared `sessionIndex` gets none of the index
/// capabilities, while the same client on v1 gets all five.
#[test]
fn t8d_session_index_is_not_advertised_under_acp_v2() {
    let env = Env::new("v2-negotiation");
    let mut connection = SessionIndexConnection::new(
        IndexPaths {
            projects_root: env.root.clone(),
            registry_dir: env.registry.clone(),
        },
        Arc::new(|_, _| {}),
    );
    let air = AirClient::from_client_capabilities(&json!({"_meta": {"jetbrains": {"air": {
        "version": 1, "capabilities": ["sessionIndex"]
    }}}}));
    let index_capabilities = [
        "sessionIndex",
        "sessionArchive",
        "sessionRename",
        "sessionListSubscribe",
        "sessionWorktrees",
    ];

    connection.negotiate(air.as_ref(), true);
    assert!(
        connection.capabilities().is_empty(),
        "ACP v2 enables nothing: {:?}",
        connection.capabilities()
    );
    for capability in index_capabilities {
        assert!(!connection.capabilities().contains(&capability));
    }

    // The same declaration on v1 still advertises the index.
    connection.negotiate(air.as_ref(), false);
    assert_eq!(
        connection.capabilities(),
        index_capabilities.to_vec(),
        "v1 advertises the index capabilities"
    );
}

/// `session-index.test.ts` > "custom title sidecar > survives concurrent writes
/// and leaves other temporary files alone": 20 concurrent writes all succeed,
/// one of them wins, and a foreign `custom-title.json.tmp.*` is left untouched.
#[test]
fn t8d_sidecar_survives_concurrent_writes_and_leaves_foreign_temps_alone() {
    use std::sync::Barrier;

    let env = Env::new("sidecar-concurrent");
    let session = id(1);
    let transcript = env.transcript(
        &env.cwd,
        &session,
        &[
            user(&session, &env.cwd, "hi", T0),
            assistant(&session, T0 + 1000),
        ],
        T0 + 1000,
    );
    let dir = transcript.parent().expect("project dir").join(&session);
    std::fs::create_dir_all(&dir).expect("sidecar dir");
    let foreign = dir.join("custom-title.json.tmp.foreign");
    std::fs::write(&foreign, "{}").expect("foreign temp");

    let titles: Vec<String> = (0..20).map(|i| format!("Title {i}")).collect();
    let barrier = Barrier::new(titles.len());
    std::thread::scope(|scope| {
        for title in &titles {
            let path = transcript.clone();
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                write_custom_title_sidecar(&path, title).expect("concurrent write");
            });
        }
    });

    let written: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("custom-title.json")).expect("sidecar"),
    )
    .expect("sidecar json");
    let winner = written["customTitle"].as_str().unwrap_or("");
    assert!(
        titles.iter().any(|title| title == winner),
        "one concurrent title won: {winner}"
    );
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("read dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "custom-title.json".to_string(),
            "custom-title.json.tmp.foreign".to_string()
        ],
        "no temporary file of this call is left behind"
    );
}
