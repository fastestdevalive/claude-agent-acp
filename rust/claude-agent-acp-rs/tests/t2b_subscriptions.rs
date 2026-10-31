//! Turn 2 lane B: the list subscriptions (`_session/list/subscribe`,
//! `_session/list/changes`), ported from upstream `session-index.test.ts`
//! ("_session/list/subscribe"). The timing is shortened; every wait is bounded.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use agent_client_protocol::Error;
use claude_agent_acp_rs::air::AirClient;
use claude_agent_acp_rs::session_index::activity::ActivityState;
use claude_agent_acp_rs::session_index::connection::{IndexPaths, SessionIndexConnection};
use claude_agent_acp_rs::session_index::list_subscriptions::ListSubscriptionTiming;
use claude_agent_acp_rs::session_index::project_dirs::{encode_project_path, Platform};
use claude_agent_acp_rs::session_index::service::BoxFuture;
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

struct Env {
    base: PathBuf,
    root: PathBuf,
    registry: PathBuf,
    cwd: String,
    changes: mpsc::UnboundedReceiver<Value>,
    connection: SessionIndexConnection,
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

impl Env {
    async fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "acp-t2b-sub-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("projects")).expect("root");
        std::fs::create_dir_all(base.join("sessions")).expect("registry");
        std::fs::create_dir_all(base.join("work/app")).expect("cwd");
        let base = std::fs::canonicalize(&base).expect("canonical");
        let cwd = base.join("work/app").to_string_lossy().into_owned();
        let mut connection = SessionIndexConnection::new(
            IndexPaths {
                projects_root: base.join("projects"),
                registry_dir: base.join("sessions"),
            },
            Arc::new(|_, _| {}),
        );
        let (tx, changes) = mpsc::unbounded_channel();
        connection.set_notifier(Arc::new(move |value| {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.send(value);
                Ok::<(), Error>(())
            }) as BoxFuture<Result<(), Error>>
        }));
        connection.set_subscription_timing(timing());
        let air = AirClient::from_client_capabilities(&json!({"_meta": {"jetbrains": {"air": {
            "version": 1, "capabilities": ["sessionIndex"]
        }}}}));
        connection.negotiate(air.as_ref());
        Self {
            root: base.join("projects"),
            registry: base.join("sessions"),
            base,
            cwd,
            changes,
            connection,
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
            "message": {"role": "user", "content": text}})
    }

    fn assistant(&self, session: &str, at: i64) -> Value {
        json!({"type": "assistant", "sessionId": session, "timestamp": iso(at),
            "message": {"role": "assistant", "model": "m", "stop_reason": "end_turn", "content": []}})
    }

    fn write(&self, session: &str, lines: &[Value]) -> PathBuf {
        let path = self.project_dir().join(format!("{session}.jsonl"));
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, text).expect("write");
        path
    }

    fn append(&self, path: &Path, line: &Value) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open");
        writeln!(file, "{line}").expect("append");
    }

    async fn subscribe(&self) -> String {
        let operation = self
            .connection
            .subscribe_list(&json!({"cwd": self.cwd}))
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

    /// The next `_session/list/changes` params, bounded.
    async fn next(&mut self) -> Value {
        timeout(Duration::from_secs(10), self.changes.recv())
            .await
            .expect("a change arrived in time")
            .expect("the channel is open")
    }

    /// Whether no change arrives within `wait`.
    async fn quiet(&mut self, wait: Duration) -> bool {
        timeout(wait, self.changes.recv()).await.is_err()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.connection.dispose();
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn session_ids(change: &Value) -> Vec<String> {
    change["sessions"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r["sessionId"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn removed_ids(change: &Value) -> Vec<String> {
    change["removed"]
        .as_array()
        .map(|ids| {
            ids.iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// sends a created session, a prompt and a turn end, but not updatedAt alone
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2b_subscription_sends_a_created_session_a_prompt_and_a_turn_end() {
    let mut env = Env::new("created").await;
    let subscription = env.subscribe().await;
    let session = id(1);
    let path = env.write(&session, &[env.user(&session, "first prompt", T0)]);
    let created = env.next().await;
    assert_eq!(created["subscriptionId"], subscription.as_str());
    assert_eq!(session_ids(&created), vec![session.clone()]);
    assert_eq!(created["sessions"][0]["title"], "first prompt");
    assert_eq!(created["removed"], json!([]));
    // A turn end changes the row (lastTurnEndedAt, state).
    env.append(&path, &env.assistant(&session, T0 + 1000));
    let ended = env.next().await;
    assert_eq!(
        ended["sessions"][0]["_meta"]["jetbrains"]["air"]["lastTurnEndedAt"],
        iso(T0 + 1000)
    );
    // Only the mtime moves (a metadata-only record): not a change.
    env.append(
        &path,
        &json!({"type": "ai-title", "aiTitle": "first prompt"}),
    );
    set_mtime(&path, T0 + 60_000);
    assert!(
        env.quiet(Duration::from_millis(500)).await,
        "updatedAt alone is no change"
    );
}

/// removes a session deleted after the first read took its row
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2b_subscription_removes_a_deleted_session() {
    let mut env = Env::new("removed").await;
    let session = id(1);
    let path = env.write(
        &session,
        &[
            env.user(&session, "doomed", T0),
            env.assistant(&session, T0 + 1000),
        ],
    );
    env.subscribe().await;
    // The first event of the session reads its row and sends it in full.
    env.append(
        &path,
        &json!({"type": "ai-title", "aiTitle": "doomed work"}),
    );
    let first = env.next().await;
    assert_eq!(first["sessions"][0]["title"], "doomed work");
    std::fs::remove_file(&path).expect("remove");
    let gone = env.next().await;
    assert_eq!(removed_ids(&gone), vec![session]);
    assert_eq!(gone["sessions"], json!([]));
}

/// coalesces the changes of a session to one a second (here: per interval)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2b_subscription_sends_a_session_at_most_once_per_interval() {
    let mut env = Env::new("interval").await;
    let session = id(1);
    let path = env.write(&session, &[env.user(&session, "one", T0)]);
    env.subscribe().await;
    env.append(&path, &env.assistant(&session, T0 + 1000));
    let first = env.next().await;
    assert_eq!(session_ids(&first), vec![session.clone()]);
    let sent_at = std::time::Instant::now();
    env.append(&path, &env.user(&session, "two", T0 + 2000));
    let second = env.next().await;
    assert!(
        sent_at.elapsed() >= Duration::from_millis(200),
        "the second change waits for the session's interval: {:?}",
        sent_at.elapsed()
    );
    assert_eq!(
        second["sessions"][0]["_meta"]["jetbrains"]["air"]["lastPromptAt"],
        iso(T0 + 2000)
    );
}

/// sends a rename, archive and unarchive at once (the connection wrote them)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2b_subscription_sends_a_rename_and_an_archive() {
    let mut env = Env::new("mutations").await;
    let session = id(1);
    let other = id(2);
    env.write(
        &session,
        &[
            env.user(&session, "work", T0),
            env.assistant(&session, T0 + 1000),
        ],
    );
    env.write(
        &other,
        &[
            env.user(&other, "other", T0 + 5000),
            env.assistant(&other, T0 + 6000),
        ],
    );
    env.subscribe().await;
    let publish: Arc<dyn Fn(Value) + Send + Sync> = Arc::new(|_| {});
    let rename = env
        .connection
        .rename(
            &json!({"sessionId": session, "title": "Renamed"}),
            None,
            publish.clone(),
        )
        .expect("accepted");
    rename.await.expect("renamed");
    let renamed = env.next().await;
    assert_eq!(renamed["sessions"][0]["title"], "Renamed");
    let archive = env
        .connection
        .archive(&json!({"sessionId": other}), None, publish.clone())
        .expect("accepted");
    archive.await.expect("archived");
    let archived = env.next().await;
    assert_eq!(session_ids(&archived), vec![other.clone()]);
    assert_eq!(
        archived["sessions"][0]["_meta"]["jetbrains"]["air"]["archived"],
        true
    );
    // The rows of the sessions other than the changed one stay put.
    assert!(env.quiet(Duration::from_millis(300)).await);
}

/// holds a fork back until the response of its session/fork is out
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2b_subscription_holds_a_fork_back_until_the_fork_response_is_out() {
    let mut env = Env::new("fork").await;
    let source = id(1);
    env.write(
        &source,
        &[
            env.user(&source, "source", T0),
            env.assistant(&source, T0 + 1000),
        ],
    );
    env.subscribe().await;
    env.connection.fork_started(&source);
    let fork = id(2);
    env.connection.on_fork_created(&source, &fork);
    env.write(
        &fork,
        &[
            json!({"type": "user", "sessionId": fork, "cwd": env.cwd, "timestamp": iso(T0 + 5000),
                "forkedFrom": {"sessionId": source, "messageUuid": "m"},
                "message": {"role": "user", "content": "forked work"}}),
            env.assistant(&fork, T0 + 6000),
        ],
    );
    assert!(
        env.quiet(Duration::from_millis(500)).await,
        "the fork waits for its response"
    );
    env.connection.fork_answered(&source);
    let released = env.next().await;
    assert_eq!(session_ids(&released), vec![fork]);
}

/// refuses the 129th subscription of a connection
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2b_subscription_refuses_the_129th() {
    let env = Env::new("limit").await;
    for _ in 0..128 {
        env.subscribe().await;
    }
    let operation = env
        .connection
        .subscribe_list(&json!({"cwd": env.cwd}))
        .expect("accepted");
    let error = timeout(Duration::from_secs(30), operation)
        .await
        .expect("answered")
        .expect_err("refused");
    assert_eq!(i32::from(error.code), -32602);
    assert_eq!(
        error.message,
        "Invalid params: At most 128 session list subscriptions per connection"
    );
    assert_eq!(
        error.data,
        Some(json!({"reason": "too_many_subscriptions", "max": 128}))
    );
}

/// unsubscribe is idempotent, and nothing is sent after it
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2b_unsubscribe_is_idempotent_and_stops_the_pushes() {
    let mut env = Env::new("unsubscribe").await;
    let session = id(1);
    let path = env.write(&session, &[env.user(&session, "one", T0)]);
    let subscription = env.subscribe().await;
    for _ in 0..2 {
        let result = env
            .connection
            .unsubscribe_list(&json!({"subscriptionId": subscription}))
            .expect("accepted");
        assert_eq!(result, json!({}));
    }
    env.connection
        .unsubscribe_list(&json!({"subscriptionId": "never-issued"}))
        .expect("an unknown id is no error");
    env.append(&path, &env.assistant(&session, T0 + 1000));
    assert!(env.quiet(Duration::from_millis(500)).await);
}

/// sends the SDK state of a session this connection runs at once
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2b_subscription_sends_the_state_of_a_session_that_runs_here() {
    let mut env = Env::new("own").await;
    let session = id(1);
    let path = env.write(
        &session,
        &[
            env.user(&session, "work", T0),
            env.assistant(&session, T0 + 1000),
        ],
    );
    env.subscribe().await;
    // The first event of the session reads its row.
    env.append(&path, &json!({"type": "ai-title", "aiTitle": "work"}));
    let first = env.next().await;
    assert_eq!(
        first["sessions"][0]["_meta"]["jetbrains"]["air"]["state"],
        "idle"
    );
    env.connection
        .board()
        .note_session_state(&session, ActivityState::Running, 1.0);
    let running = env.next().await;
    assert_eq!(
        running["sessions"][0]["_meta"]["jetbrains"]["air"]["state"],
        "running"
    );
    env.connection.board().note_cost(&session, 1.5);
    let costly = env.next().await;
    assert_eq!(
        costly["sessions"][0]["_meta"]["jetbrains"]["air"]["cost"],
        json!({"amount": 1.5, "currency": "USD"})
    );
    // ...and its close: the row shows the transcript's state again, without the
    // cost of the query that ended (`closeQueryStream` → `onOwnSessionChanged`;
    // upstream "sends the SDK state of a session this connection runs at once,
    // and its close").
    env.connection.on_teardown(&session);
    let closed = env.next().await;
    let air = &closed["sessions"][0]["_meta"]["jetbrains"]["air"];
    assert_eq!(air["state"], "idle");
    assert!(
        air.get("cost").is_none(),
        "a closed session keeps no cost: {air:?}"
    );
}

/// sends a state flip of the registry
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2b_subscription_sends_a_registry_state_flip() {
    let mut env = Env::new("registry").await;
    let session = id(1);
    let path = env.write(
        &session,
        &[
            env.user(&session, "work", T0),
            env.assistant(&session, T0 + 1000),
        ],
    );
    // The transcript is older than the registry record that follows.
    set_mtime(&path, T0 + 2000);
    env.subscribe().await;
    env.append(&path, &json!({"type": "ai-title", "aiTitle": "work"}));
    set_mtime(&path, T0 + 2000);
    let first = env.next().await;
    assert_eq!(
        first["sessions"][0]["_meta"]["jetbrains"]["air"]["state"],
        "idle"
    );
    // An interactive CLI (this very process, so it is alive) reports `waiting`.
    let pid = std::process::id();
    let record = json!({
        "pid": pid, "sessionId": session, "kind": "interactive", "entrypoint": "cli",
        "status": "waiting", "statusUpdatedAt": 4_000_000_000_000i64, "updatedAt": 4_000_000_000_000i64,
        "startedAt": 4_000_000_000_000i64
    });
    std::fs::write(env.registry.join(format!("{pid}.json")), record.to_string()).expect("record");
    let waiting = env.next().await;
    assert_eq!(
        waiting["sessions"][0]["_meta"]["jetbrains"]["air"]["state"],
        "requires_action"
    );
}

/// leaves no work behind after the connection closes: subscribing again fails
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t2b_subscribe_after_dispose_fails() {
    let env = Env::new("dispose").await;
    env.subscribe().await;
    env.connection.dispose();
    let operation = env
        .connection
        .subscribe_list(&json!({"cwd": env.cwd}))
        .expect("accepted");
    let error = timeout(Duration::from_secs(10), operation)
        .await
        .expect("answered")
        .expect_err("closed");
    assert_eq!(i32::from(error.code), -32603);
}

/// answers method-not-found without sessionIndex
#[tokio::test]
async fn t2b_subscribe_needs_the_capability() {
    let base = std::env::temp_dir().join(format!("acp-t2b-sub-nocap-{}", std::process::id()));
    let connection = SessionIndexConnection::new(
        IndexPaths {
            projects_root: base.join("projects"),
            registry_dir: base.join("sessions"),
        },
        Arc::new(|_, _| {}),
    );
    let error = connection
        .subscribe_list(&json!({"cwd": "/work"}))
        .err()
        .expect("no service");
    assert_eq!(i32::from(error.code), -32601);
    let error = connection
        .unsubscribe_list(&json!({"subscriptionId": "x"}))
        .expect_err("no service");
    assert_eq!(i32::from(error.code), -32601);
    // The params are checked first, as the router parses before the handler.
    let error = connection
        .subscribe_list(&json!({"cwd": "relative"}))
        .err()
        .expect("invalid");
    assert_eq!(i32::from(error.code), -32602);
}
