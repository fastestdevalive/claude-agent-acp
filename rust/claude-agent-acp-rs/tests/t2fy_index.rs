//! Turn 2 fix round lane Y: the mutations of the session index (rename, archive,
//! unarchive, delete) run one at a time *per session* (upstream `exclusive`), and
//! a `rename_session` through a session's CLI is bounded, so one wedged CLI can
//! no longer hold up the mutations of every other session.
//!
//! Every wait is bounded; the transcripts live under a temporary projects root.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use agent_client_protocol::Error;
use claude_agent_acp_rs::session_index::project_dirs::{encode_project_path, Platform};
use claude_agent_acp_rs::session_index::service::{
    BoxFuture, LiveTitle, RenameFn, RetitleOptions, ServiceDeps, SessionIndexService, SidecarMode,
    TitleChange,
};
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::time::timeout;

static NEXT: AtomicUsize = AtomicUsize::new(0);

const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
/// How long a test waits for a mutation that must finish.
const WAIT: Duration = Duration::from_secs(5);

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

struct Env {
    base: PathBuf,
    root: PathBuf,
    registry: PathBuf,
    cwd: String,
}

impl Env {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "acp-t2fy-index-{tag}-{}-{}",
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

    fn project_dir(&self) -> PathBuf {
        let dir = self
            .root
            .join(encode_project_path(&self.cwd, Platform::host()));
        std::fs::create_dir_all(&dir).expect("project dir");
        dir
    }

    fn user(&self, session: &str, text: &str, at: i64) -> Value {
        json!({"type": "user", "sessionId": session, "cwd": self.cwd, "timestamp": iso(at),
            "uuid": format!("u-{at}"), "parentUuid": null,
            "message": {"role": "user", "content": text}})
    }

    fn assistant(&self, session: &str, at: i64) -> Value {
        json!({"type": "assistant", "sessionId": session, "timestamp": iso(at),
            "uuid": format!("a-{at}"), "parentUuid": null,
            "message": {"role": "assistant", "model": "claude-test", "stop_reason": "end_turn",
                "content": []}})
    }

    fn transcript(&self, session: &str, at: i64) -> PathBuf {
        let path = self.project_dir().join(format!("{session}.jsonl"));
        let text = format!(
            "{}\n{}\n",
            self.user(session, &format!("prompt of {session}"), at),
            self.assistant(session, at + 1000)
        );
        std::fs::write(&path, text).expect("write transcript");
        set_mtime(&path, at + 2000);
        path
    }

    fn service(&self, deps: ServiceDeps) -> SessionIndexService {
        SessionIndexService::new(self.root.clone(), self.registry.clone(), deps)
    }

    fn deps(&self) -> ServiceDeps {
        ServiceDeps::new(Arc::new(|_, _| {}))
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn code(error: &Error) -> i32 {
    i32::from(error.code)
}

fn rename_options() -> RetitleOptions {
    RetitleOptions {
        may_be_unwritten: false,
        sidecar: SidecarMode::Existing,
    }
}

/// A `rename_session` that never answers until the returned sender fires, and
/// tells the test when the CLI was reached.
fn stuck_rename() -> (RenameFn, oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let rename: RenameFn = Box::new(move |_title| {
        Box::pin(async move {
            let _ = entered_tx.send(());
            let _ = release_rx.await;
            Ok(())
        }) as BoxFuture<Result<(), Error>>
    });
    (rename, entered_rx, release_tx)
}

/// A `rename_session` that answers at once.
fn quick_rename() -> RenameFn {
    Box::new(|_title| Box::pin(async move { Ok(()) }) as BoxFuture<Result<(), Error>>)
}

fn live_title(env: &Env, rename: RenameFn) -> (LiveTitle, Arc<AtomicBool>) {
    let remembered = Arc::new(AtomicBool::new(false));
    let flag = remembered.clone();
    let title = LiveTitle {
        cwd: env.cwd.clone(),
        rename,
        stored: Box::new(|| None),
        remember: Box::new(move |_| flag.store(true, Ordering::SeqCst)),
        shown: None,
    };
    (title, remembered)
}

/// A rename of one session whose CLI is wedged does not hold up the rename of
/// another session (the gate is per session, as upstream's `exclusive`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2fy_a_wedged_cli_rename_does_not_block_another_session() {
    let env = Env::new("wedged");
    let stuck = id(1);
    let other = id(2);
    env.transcript(&stuck, T0);
    env.transcript(&other, T0 + 60_000);
    let service = env.service(env.deps());

    let (rename, entered, release) = stuck_rename();
    let stuck_service = service.clone();
    let stuck_id = stuck.clone();
    let (stuck_title, stuck_remembered) = live_title(&env, rename);
    let stuck_task = tokio::spawn(async move {
        stuck_service
            .retitle(
                &stuck_id,
                TitleChange::Rename("Stuck title".to_string()),
                rename_options(),
                Some(stuck_title),
            )
            .await
    });
    // Wait until the wedged CLI holds its session's gate.
    timeout(WAIT, entered)
        .await
        .expect("the CLI of the first session was reached")
        .expect("the signal");

    // The other session's rename must not wait for it.
    let (other_title, other_remembered) = live_title(&env, quick_rename());
    let renamed = timeout(
        WAIT,
        service.retitle(
            &other,
            TitleChange::Rename("Other title".to_string()),
            rename_options(),
            Some(other_title),
        ),
    )
    .await
    .expect("the other session's rename did not wait for the wedged CLI")
    .expect("renamed");
    assert_eq!(renamed.as_deref(), Some("Other title"));
    // The CLI took the title: the mutation really ran.
    assert!(
        other_remembered.load(Ordering::SeqCst),
        "the CLI of the other session got its title"
    );
    assert!(!stuck_remembered.load(Ordering::SeqCst), "not yet");

    // The first one is still waiting for its CLI, and finishes when it answers.
    assert!(!stuck_task.is_finished(), "the wedged CLI still holds it");
    release.send(()).expect("release the CLI");
    let stuck_result = timeout(WAIT, stuck_task)
        .await
        .expect("the first rename finished once its CLI answered")
        .expect("no panic");
    assert_eq!(
        stuck_result.expect("renamed").as_deref(),
        Some("Stuck title")
    );
}

/// A CLI that never answers `rename_session` fails that session's mutation in
/// bounded time, with the error a failed CLI rename gives, and leaves the gate
/// free for the next mutation of the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2fy_a_cli_that_never_answers_rename_fails_the_mutation() {
    let env = Env::new("timeout");
    let session = id(1);
    let path = env.transcript(&session, T0);
    let mut deps = env.deps();
    deps.live_mutation_timeout = Duration::from_millis(50);
    let service = env.service(deps);

    let (rename, _entered, _release) = stuck_rename();
    let (title, _remembered) = live_title(&env, rename);
    let error = timeout(
        Duration::from_secs(2),
        service.retitle(
            &session,
            TitleChange::Rename("Never".to_string()),
            rename_options(),
            Some(title),
        ),
    )
    .await
    .expect("the wait for the CLI is bounded")
    .expect_err("the CLI did not answer");
    assert_eq!(code(&error), -32603);
    assert!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("details"))
            .and_then(Value::as_str)
            .is_some_and(|details| details.contains("did not answer rename_session")),
        "the failure names the CLI call: {error:?}"
    );
    // The failed mutation released the session's gate: the next one runs.
    let (later_title, later_remembered) = live_title(&env, quick_rename());
    let renamed = timeout(
        WAIT,
        service.retitle(
            &session,
            TitleChange::Rename("Later".to_string()),
            rename_options(),
            Some(later_title),
        ),
    )
    .await
    .expect("the gate is free after the failed mutation")
    .expect("renamed");
    assert_eq!(renamed.as_deref(), Some("Later"));
    assert!(later_remembered.load(Ordering::SeqCst));
    // The transcript is untouched: the CLI of a live session writes its own
    // records, and the port only keeps the agent name in line.
    let written = std::fs::read_to_string(&path).expect("read");
    assert!(!written.contains("Later"), "the records are the CLI's");
}

/// Mutations of one session run in call order, one at a time (the same gate).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2fy_mutations_of_one_session_run_in_call_order() {
    let env = Env::new("order");
    let session = id(1);
    let path = env.transcript(&session, T0);
    let service = env.service(env.deps());

    let (rename, entered, release) = stuck_rename();
    let first_service = service.clone();
    let first_id = session.clone();
    let (first_title, first_remembered) = live_title(&env, rename);
    let first = tokio::spawn(async move {
        first_service
            .retitle(
                &first_id,
                TitleChange::Rename("First".to_string()),
                rename_options(),
                Some(first_title),
            )
            .await
    });
    timeout(WAIT, entered)
        .await
        .expect("the first mutation reached its CLI")
        .expect("the signal");
    // The second mutation of the same session waits for the first.
    let second_service = service.clone();
    let second_id = session.clone();
    let (second_title, second_remembered) = live_title(&env, quick_rename());
    let second = tokio::spawn(async move {
        second_service
            .retitle(
                &second_id,
                TitleChange::Rename("Second".to_string()),
                rename_options(),
                Some(second_title),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !second.is_finished(),
        "the second mutation of the session waits for the first"
    );
    assert!(!second_remembered.load(Ordering::SeqCst), "not reached yet");
    release.send(()).expect("release the CLI");
    timeout(WAIT, first)
        .await
        .expect("the first finished")
        .expect("no panic")
        .expect("renamed");
    assert!(first_remembered.load(Ordering::SeqCst));
    let renamed = timeout(WAIT, second)
        .await
        .expect("the second ran after the first")
        .expect("no panic")
        .expect("renamed");
    assert_eq!(renamed.as_deref(), Some("Second"));
    assert!(second_remembered.load(Ordering::SeqCst));
    let written = std::fs::read_to_string(&path).expect("read");
    assert!(!written.contains("Second"), "the records are the CLI's");
}

/// `session/delete` of one session does not wait for a rename of another.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2fy_a_delete_of_another_session_does_not_wait_for_a_wedged_rename() {
    let env = Env::new("delete");
    let stuck = id(1);
    let doomed = id(2);
    env.transcript(&stuck, T0);
    let doomed_path = env.transcript(&doomed, T0 + 60_000);
    let service = env.service(env.deps());

    let (rename, entered, release) = stuck_rename();
    let stuck_service = service.clone();
    let stuck_id = stuck.clone();
    let (stuck_title, _remembered) = live_title(&env, rename);
    let stuck_task = tokio::spawn(async move {
        stuck_service
            .retitle(
                &stuck_id,
                TitleChange::Rename("Stuck".to_string()),
                rename_options(),
                Some(stuck_title),
            )
            .await
    });
    timeout(WAIT, entered)
        .await
        .expect("the wedged CLI was reached")
        .expect("the signal");
    timeout(WAIT, service.delete(&doomed, false))
        .await
        .expect("the delete did not wait for the wedged rename")
        .expect("deleted");
    assert!(!doomed_path.exists(), "the transcript is gone");
    release.send(()).expect("release the CLI");
    timeout(WAIT, stuck_task)
        .await
        .expect("the rename finished")
        .expect("no panic")
        .expect("renamed");
}
