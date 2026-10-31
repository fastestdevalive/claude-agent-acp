//! Turn 2 lane B: the session index over a hermetic projects root (list, archive,
//! rename, delete, worktrees). Ported from upstream `session-index.test.ts`,
//! driven through the public service and connection types with every wait
//! bounded.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use claude_agent_acp_rs::air::AirClient;
use claude_agent_acp_rs::session_index::connection::{IndexPaths, SessionIndexConnection};
use claude_agent_acp_rs::session_index::project_dirs::{encode_project_path, Platform};
use claude_agent_acp_rs::session_index::service::{
    RetitleOptions, ServiceDeps, SessionIndexService, SidecarMode, TitleChange,
};
use serde_json::{json, Value};
use tokio::time::timeout;

static NEXT: AtomicUsize = AtomicUsize::new(0);

const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z

fn id(n: usize) -> String {
    format!("aaaaaaaa-aaaa-4aaa-8aaa-{n:012}")
}

fn iso(ms: i64) -> String {
    claude_agent_acp_rs::session_store::iso8601(ms as u64)
}

struct Env {
    base: PathBuf,
    root: PathBuf,
    registry: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "acp-t2b-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("projects")).expect("root");
        // A canonical base, so cwd comparisons see the same spelling.
        let base = std::fs::canonicalize(&base).expect("canonical");
        let root = base.join("projects");
        let registry = base.join("sessions");
        Self {
            base,
            root,
            registry,
        }
    }

    /// A working directory that exists on disk.
    fn cwd(&self, name: &str) -> String {
        let path = self.base.join("work").join(name);
        std::fs::create_dir_all(&path).expect("cwd");
        path.to_string_lossy().into_owned()
    }

    fn project_dir(&self, cwd: &str) -> PathBuf {
        let dir = self.root.join(encode_project_path(cwd, Platform::host()));
        std::fs::create_dir_all(&dir).expect("project dir");
        dir
    }

    /// Writes a transcript and gives it `mtime_ms`.
    fn transcript(&self, cwd: &str, session: &str, lines: &[Value], mtime_ms: i64) -> PathBuf {
        let path = self.project_dir(cwd).join(format!("{session}.jsonl"));
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, text).expect("write transcript");
        set_mtime(&path, mtime_ms);
        path
    }

    fn service(&self) -> SessionIndexService {
        SessionIndexService::new(
            self.root.clone(),
            self.registry.clone(),
            ServiceDeps::new(Arc::new(|_, _| {})),
        )
    }

    fn connection(&self) -> SessionIndexConnection {
        SessionIndexConnection::new(
            IndexPaths {
                projects_root: self.root.clone(),
                registry_dir: self.registry.clone(),
            },
            Arc::new(|_, _| {}),
        )
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn set_mtime(path: &Path, ms: i64) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open");
    file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_millis(ms as u64))
        .expect("mtime");
}

fn user(session: &str, cwd: &str, text: &str, at: i64) -> Value {
    json!({"type": "user", "sessionId": session, "cwd": cwd, "timestamp": iso(at),
        "uuid": format!("u-{at}"), "parentUuid": null,
        "message": {"role": "user", "content": text}})
}

fn assistant(session: &str, at: i64) -> Value {
    json!({"type": "assistant", "sessionId": session, "timestamp": iso(at),
        "uuid": format!("a-{at}"), "parentUuid": null,
        "message": {"role": "assistant", "model": "claude-test", "stop_reason": "end_turn", "content": []}})
}

fn custom_title(session: &str, title: &str) -> Value {
    json!({"type": "custom-title", "customTitle": title, "sessionId": session})
}

fn agent_name(session: &str, title: &str) -> Value {
    json!({"type": "agent-name", "agentName": title, "sessionId": session})
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

fn air(list: Value) -> Value {
    json!({"jetbrains": {"air": {"version": 1, "list": list}}})
}

fn code(error: &agent_client_protocol::Error) -> i32 {
    i32::from(error.code)
}

fn air_client() -> AirClient {
    AirClient::from_client_capabilities(&json!({"_meta": {"jetbrains": {"air": {
        "version": 1,
        "capabilities": ["sessionIndex"]
    }}}}))
    .expect("air client")
}

fn simple_session(env: &Env, cwd: &str, n: usize, at: i64) -> String {
    let session = id(n);
    env.transcript(
        cwd,
        &session,
        &[
            user(&session, cwd, &format!("prompt {n}"), at),
            assistant(&session, at + 1000),
        ],
        at + 2000,
    );
    session
}

/// session/list of a sessionIndex client > orders by the last message time, honours the limit, and pages with a cursor
#[tokio::test]
async fn t2b_list_orders_newest_first_and_pages_with_an_opaque_cursor() {
    let env = Env::new("order");
    let cwd = env.cwd("app");
    for n in 1..=5 {
        simple_session(&env, &cwd, n, T0 + n as i64 * 60_000);
    }
    let service = env.service();
    let first = list(
        &service,
        json!({"cwd": cwd, "_meta": air(json!({"limit": 2}))}),
    )
    .await
    .expect("page 1");
    assert_eq!(ids(&first), vec![id(5), id(4)]);
    let cursor = first["nextCursor"]
        .as_str()
        .expect("a next page")
        .to_string();
    assert!(!cursor.starts_with("offset:"), "the cursor is opaque");
    let second = list(
        &service,
        json!({"cwd": cwd, "cursor": cursor, "_meta": air(json!({"limit": 2}))}),
    )
    .await
    .expect("page 2");
    assert_eq!(ids(&second), vec![id(3), id(2)]);
    let third = list(
        &service,
        json!({"cwd": cwd, "cursor": second["nextCursor"], "_meta": air(json!({"limit": 2}))}),
    )
    .await
    .expect("page 3");
    assert_eq!(ids(&third), vec![id(1)]);
    assert!(
        third.get("nextCursor").is_none(),
        "never a cursor to an empty page"
    );
    // The default page size is 50.
    let all = list(&service, json!({"cwd": cwd})).await.expect("all");
    assert_eq!(ids(&all).len(), 5);
}

/// list request parsing: a foreign cursor and a cursor of another scope are -32602
#[tokio::test]
async fn t2b_list_rejects_a_foreign_cursor_and_one_of_another_scope() {
    let env = Env::new("cursor");
    let cwd = env.cwd("app");
    for n in 1..=3 {
        simple_session(&env, &cwd, n, T0 + n as i64 * 60_000);
    }
    let service = env.service();
    let page = list(
        &service,
        json!({"cwd": cwd, "_meta": air(json!({"limit": 1}))}),
    )
    .await
    .expect("page");
    let foreign = list(&service, json!({"cwd": cwd, "cursor": "offset:1"}))
        .await
        .expect_err("foreign");
    assert_eq!(code(&foreign), -32602);
    assert_eq!(
        foreign.message,
        "Invalid params: Unknown session/list cursor: offset:1"
    );
    let other = list(
        &service,
        json!({"cwd": cwd, "cursor": page["nextCursor"], "_meta": air(json!({"limit": 1, "archived": "all"}))}),
    )
    .await
    .expect_err("another filter");
    assert_eq!(code(&other), -32602);
    assert_eq!(
        other.message,
        "Invalid params: The session/list cursor belongs to another cwd or filter"
    );
    let bad_limit = list(
        &service,
        json!({"cwd": cwd, "_meta": air(json!({"limit": 0}))}),
    )
    .await
    .expect_err("limit");
    assert_eq!(code(&bad_limit), -32602);
}

/// order by the last user activity > ranks by the last prompt, else updatedAt
#[tokio::test]
async fn t2b_list_ranks_by_the_last_prompt_not_the_last_message() {
    let env = Env::new("prompt-order");
    let cwd = env.cwd("app");
    // `a`: prompted early, answered late. `b`: prompted later, answered early.
    let a = id(1);
    env.transcript(
        &cwd,
        &a,
        &[
            user(&a, &cwd, "early prompt", T0),
            assistant(&a, T0 + 600_000),
        ],
        T0 + 700_000,
    );
    let b = id(2);
    env.transcript(
        &cwd,
        &b,
        &[
            user(&b, &cwd, "later prompt", T0 + 100_000),
            assistant(&b, T0 + 110_000),
        ],
        T0 + 120_000,
    );
    let response = list(&env.service(), json!({"cwd": cwd}))
        .await
        .expect("list");
    assert_eq!(ids(&response), vec![id(2), id(1)]);
    // updatedAt is the last message, capped at the mtime.
    assert_eq!(response["sessions"][1]["updatedAt"], iso(T0 + 600_000));
    assert_eq!(
        response["sessions"][1]["_meta"]["jetbrains"]["air"]["lastPromptAt"],
        iso(T0)
    );
}

/// session/list > lists unarchived sessions by default, archived ones only, or all in one order
#[tokio::test]
async fn t2b_list_filters_by_archive_state() {
    let env = Env::new("filter");
    let cwd = env.cwd("app");
    let live = simple_session(&env, &cwd, 1, T0 + 60_000);
    let archived = id(2);
    env.transcript(
        &cwd,
        &archived,
        &[
            user(&archived, &cwd, "old work", T0 + 120_000),
            assistant(&archived, T0 + 121_000),
            custom_title(&archived, "[archived] Old work"),
            agent_name(&archived, "[archived] Old work"),
        ],
        T0 + 122_000,
    );
    let service = env.service();
    let default = list(&service, json!({"cwd": cwd})).await.expect("default");
    assert_eq!(ids(&default), vec![live.clone()]);
    let only = list(
        &service,
        json!({"cwd": cwd, "_meta": air(json!({"archived": "archived"}))}),
    )
    .await
    .expect("archived");
    assert_eq!(ids(&only), vec![archived.clone()]);
    assert_eq!(
        only["sessions"][0]["title"], "Old work",
        "the title is shown without the prefix"
    );
    assert_eq!(
        only["sessions"][0]["_meta"]["jetbrains"]["air"]["archived"],
        true
    );
    let all = list(
        &service,
        json!({"cwd": cwd, "_meta": air(json!({"archived": "all"}))}),
    )
    .await
    .expect("all");
    assert_eq!(ids(&all), vec![archived, live]);
}

/// session/list > lists a session copied to two project directories once, from its newest copy
#[tokio::test]
async fn t2b_list_shows_a_session_copied_to_two_directories_once() {
    let env = Env::new("copies");
    let cwd = env.cwd("app");
    let session = id(1);
    let old = env.transcript(
        &cwd,
        &session,
        &[user(&session, &cwd, "old copy", T0)],
        T0 + 1000,
    );
    let moved = env.cwd("moved");
    let newer = env.project_dir(&moved).join(format!("{session}.jsonl"));
    std::fs::write(
        &newer,
        format!("{}\n", user(&session, &moved, "new copy", T0 + 5000)),
    )
    .expect("write");
    set_mtime(&newer, T0 + 9000);
    assert!(old.exists());
    let response = list(&env.service(), json!({}))
        .await
        .expect("list all projects");
    assert_eq!(ids(&response), vec![session]);
    assert_eq!(response["sessions"][0]["title"], "new copy");
    assert_eq!(response["sessions"][0]["cwd"], moved.as_str());
}

/// session/list > includes the sessions of existing linked worktrees on request, with their own cwd
#[tokio::test]
async fn t2b_list_includes_linked_worktrees_on_request() {
    let env = Env::new("worktrees");
    let main = env.cwd("main");
    let linked = env.cwd("linked");
    std::fs::create_dir_all(Path::new(&main).join(".git/worktrees/linked")).expect("git");
    std::fs::write(
        Path::new(&main).join(".git/worktrees/linked/gitdir"),
        format!("{linked}/.git\n"),
    )
    .expect("gitdir");
    std::fs::write(
        Path::new(&main).join(".git/worktrees/linked/commondir"),
        "../..\n",
    )
    .expect("commondir");
    std::fs::write(
        Path::new(&linked).join(".git"),
        format!("gitdir: {main}/.git/worktrees/linked\n"),
    )
    .expect("dot git");
    let in_main = simple_session(&env, &main, 1, T0 + 60_000);
    let in_linked = simple_session(&env, &linked, 2, T0 + 120_000);
    let service = env.service();
    let alone = list(&service, json!({"cwd": main})).await.expect("alone");
    assert_eq!(ids(&alone), vec![in_main.clone()]);
    let both = list(
        &service,
        json!({"cwd": main, "_meta": air(json!({"includeWorktrees": true}))}),
    )
    .await
    .expect("both");
    assert_eq!(ids(&both), vec![in_linked.clone(), in_main]);
    let rows = both["sessions"].as_array().expect("rows");
    assert_eq!(rows[0]["cwd"], linked.as_str(), "a row keeps its own cwd");
}

/// session/list > marks the rows of sessions in a worktree Claude Code made (agentWorktree)
#[tokio::test]
async fn t2b_list_marks_a_worktree_claude_code_made() {
    let env = Env::new("agent-worktree");
    let main = env.cwd("main");
    let linked = env.cwd("linked");
    std::fs::create_dir_all(Path::new(&main).join(".git/worktrees/linked")).expect("git");
    std::fs::write(
        Path::new(&main).join(".git/worktrees/linked/commondir"),
        "../..\n",
    )
    .expect("commondir");
    std::fs::write(
        Path::new(&linked).join(".git"),
        format!("gitdir: {main}/.git/worktrees/linked\n"),
    )
    .expect("dot git");
    let made = id(1);
    env.transcript(
        &linked,
        &made,
        &[
            json!({"type": "worktree-state", "worktreeSession": {"worktreeName": "w"}}),
            user(&made, &linked, "work in the worktree", T0),
            assistant(&made, T0 + 1000),
        ],
        T0 + 2000,
    );
    let entered = id(2);
    env.transcript(
        &linked,
        &entered,
        &[
            json!({"type": "worktree-state", "worktreeSession": {"enteredExisting": true}}),
            user(&entered, &linked, "entered an existing one", T0 + 5000),
            assistant(&entered, T0 + 6000),
        ],
        T0 + 7000,
    );
    let response = list(&env.service(), json!({"cwd": linked}))
        .await
        .expect("list");
    let rows = response["sessions"].as_array().expect("rows");
    let air_of = |session: &str| {
        rows.iter()
            .find(|r| r["sessionId"] == session)
            .map(|r| r["_meta"]["jetbrains"]["air"].clone())
            .expect("row")
    };
    assert_eq!(air_of(&made)["agentWorktree"], true);
    assert!(air_of(&entered).get("agentWorktree").is_none());
}

/// session/list > reports the row fields of the RFDs, flat
#[tokio::test]
async fn t2b_list_reports_the_row_fields() {
    let env = Env::new("fields");
    let cwd = env.cwd("app");
    let source = id(9);
    let fork = id(1);
    env.transcript(
        &cwd,
        &fork,
        &[
            json!({"type": "user", "sessionId": fork, "cwd": cwd, "timestamp": iso(T0),
                "forkedFrom": {"sessionId": source, "messageUuid": "m"},
                "message": {"role": "user", "content": "forked work"}}),
            assistant(&fork, T0 + 1000),
        ],
        T0 + 2000,
    );
    let response = list(&env.service(), json!({"cwd": cwd}))
        .await
        .expect("list");
    let row = &response["sessions"][0];
    let air = &row["_meta"]["jetbrains"]["air"];
    assert_eq!(air["version"], 1);
    assert_eq!(air["archived"], false);
    assert_eq!(air["model"], "claude-test");
    assert_eq!(air["forkedFrom"], source.as_str());
    assert_eq!(air["lastPromptAt"], iso(T0));
    assert_eq!(air["state"], "idle");
    assert_eq!(air["lastTurnEndedAt"], iso(T0 + 1000));
    assert!(
        air.get("cost").is_none(),
        "cost only for a session that runs here"
    );
    assert_eq!(row["cwd"], cwd.as_str());
}

/// title of a listed copy that starts with a slash command > is the first real prompt, as the SDK titles it
#[tokio::test]
async fn t2b_list_titles_by_the_first_real_prompt_after_a_slash_command() {
    let env = Env::new("slash");
    let cwd = env.cwd("app");
    let session = id(1);
    env.transcript(
        &cwd,
        &session,
        &[
            user(&session, &cwd, "<command-name>/init</command-name>", T0),
            user(&session, &cwd, "the real prompt", T0 + 1000),
            assistant(&session, T0 + 2000),
        ],
        T0 + 3000,
    );
    let response = list(&env.service(), json!({"cwd": cwd}))
        .await
        .expect("list");
    assert_eq!(response["sessions"][0]["title"], "the real prompt");
}

/// session/list > recovers the cwd of a session whose head has none
#[tokio::test]
async fn t2b_list_uses_the_requested_path_for_a_row_without_a_cwd() {
    let env = Env::new("no-cwd");
    let cwd = env.cwd("app");
    let session = id(1);
    env.transcript(
        &cwd,
        &session,
        &[
            json!({"type": "user", "sessionId": session, "timestamp": iso(T0), "message": {"role": "user", "content": "no cwd here"}}),
            json!({"type": "assistant", "sessionId": session, "timestamp": iso(T0 + 1000), "message": {"stop_reason": "end_turn"}}),
        ],
        T0 + 2000,
    );
    let response = list(&env.service(), json!({"cwd": cwd}))
        .await
        .expect("list");
    assert_eq!(response["sessions"][0]["cwd"], cwd.as_str());
}

/// project directories: another path that encodes to the same directory is left out
#[tokio::test]
async fn t2b_list_leaves_out_another_path_that_shares_the_project_directory() {
    let env = Env::new("collide");
    let a = env.cwd("app.v2");
    let b = env.cwd("app-v2");
    // Both encode to the same directory; the listing of `a` shows only its own.
    let mine = id(1);
    let theirs = id(2);
    env.transcript(
        &a,
        &mine,
        &[user(&mine, &a, "mine", T0), assistant(&mine, T0 + 1000)],
        T0 + 2000,
    );
    env.transcript(
        &b,
        &theirs,
        &[
            user(&theirs, &b, "theirs", T0 + 5000),
            assistant(&theirs, T0 + 6000),
        ],
        T0 + 7000,
    );
    let service = env.service();
    assert_eq!(
        ids(&list(&service, json!({"cwd": a})).await.expect("a")),
        vec![mine]
    );
    assert_eq!(
        ids(&list(&service, json!({"cwd": b})).await.expect("b")),
        vec![theirs]
    );
}

/// _session/rename > renames a session that is not loaded: title records and the sidecar
#[tokio::test]
async fn t2b_rename_writes_the_records_and_the_private_sidecar() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("rename");
    let cwd = env.cwd("app");
    let session = simple_session(&env, &cwd, 1, T0);
    let service = env.service();
    let stored = service
        .retitle(
            &session,
            TitleChange::Rename("Better name".into()),
            RetitleOptions {
                may_be_unwritten: false,
                sidecar: SidecarMode::Always,
            },
            None,
        )
        .await
        .expect("renamed");
    assert_eq!(stored.as_deref(), Some("Better name"));
    let transcript = env.project_dir(&cwd).join(format!("{session}.jsonl"));
    let text = std::fs::read_to_string(&transcript).expect("read");
    let lines: Vec<&str> = text.lines().collect();
    let tail = &lines[lines.len() - 2..];
    assert_eq!(
        tail[0],
        format!("{{\"type\":\"custom-title\",\"customTitle\":\"Better name\",\"sessionId\":\"{session}\"}}")
    );
    assert_eq!(
        tail[1],
        format!(
            "{{\"type\":\"agent-name\",\"agentName\":\"Better name\",\"sessionId\":\"{session}\"}}"
        )
    );
    let sidecar = env
        .project_dir(&cwd)
        .join(&session)
        .join("custom-title.json");
    assert_eq!(
        std::fs::read_to_string(&sidecar).expect("sidecar"),
        "{\"customTitle\":\"Better name\"}"
    );
    let file_mode = std::fs::metadata(&sidecar)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    let dir_mode = std::fs::metadata(sidecar.parent().expect("dir"))
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!((file_mode, dir_mode), (0o600, 0o700));
    // No temporary file stays behind, and the list shows the new title.
    let leftovers: Vec<_> = std::fs::read_dir(sidecar.parent().expect("dir"))
        .expect("read dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers, vec!["custom-title.json".to_string()]);
    let response = list(&service, json!({"cwd": cwd})).await.expect("list");
    assert_eq!(response["sessions"][0]["title"], "Better name");
}

/// _session/rename > refuses an unknown session (-32002)
#[tokio::test]
async fn t2b_rename_and_archive_refuse_an_unknown_session() {
    let env = Env::new("unknown");
    let service = env.service();
    let error = service
        .retitle(
            &id(7),
            TitleChange::Rename("x".into()),
            RetitleOptions {
                may_be_unwritten: false,
                sidecar: SidecarMode::Always,
            },
            None,
        )
        .await
        .expect_err("unknown");
    assert_eq!(code(&error), -32002);
    assert_eq!(error.message, format!("Session not found: {}", id(7)));
    let not_a_uuid = service
        .retitle(
            "../etc/passwd",
            TitleChange::Archive(true),
            RetitleOptions {
                may_be_unwritten: false,
                sidecar: SidecarMode::Existing,
            },
            None,
        )
        .await
        .expect_err("not an id");
    assert_eq!(code(&not_a_uuid), -32002);
}

/// _session/archive and _session/unarchive > append AIR's archive title records, are idempotent and keep updatedAt
#[tokio::test]
async fn t2b_archive_and_unarchive_are_idempotent_and_keep_updated_at() {
    let env = Env::new("archive");
    let cwd = env.cwd("app");
    let session = simple_session(&env, &cwd, 1, T0);
    let service = env.service();
    let archive = |state: bool| {
        let service = service.clone();
        let session = session.clone();
        async move {
            service
                .retitle(
                    &session,
                    TitleChange::Archive(state),
                    RetitleOptions {
                        may_be_unwritten: false,
                        sidecar: SidecarMode::Existing,
                    },
                    None,
                )
                .await
                .expect("retitled")
        }
    };
    let before = list(&service, json!({"cwd": cwd})).await.expect("before");
    let updated_at = before["sessions"][0]["updatedAt"].clone();
    assert_eq!(archive(true).await.as_deref(), Some("[archived] prompt 1"));
    assert_eq!(archive(true).await, None, "idempotent");
    let hidden = list(&service, json!({"cwd": cwd})).await.expect("hidden");
    assert!(ids(&hidden).is_empty());
    let archived = list(
        &service,
        json!({"cwd": cwd, "_meta": air(json!({"archived": "archived"}))}),
    )
    .await
    .expect("archived");
    assert_eq!(
        archived["sessions"][0]["updatedAt"], updated_at,
        "archiving moves nothing"
    );
    assert_eq!(archived["sessions"][0]["title"], "prompt 1");
    assert_eq!(archive(false).await.as_deref(), Some("prompt 1"));
    assert_eq!(archive(false).await, None, "idempotent");
    assert_eq!(
        ids(&list(&service, json!({"cwd": cwd})).await.expect("back")),
        vec![session]
    );
}

/// _session/rename > a rename keeps the archive state
#[tokio::test]
async fn t2b_rename_of_an_archived_session_keeps_it_archived() {
    let env = Env::new("rename-archived");
    let cwd = env.cwd("app");
    let session = id(1);
    env.transcript(
        &cwd,
        &session,
        &[
            user(&session, &cwd, "work", T0),
            assistant(&session, T0 + 1000),
            custom_title(&session, "[archived] Work"),
            agent_name(&session, "[archived] Work"),
        ],
        T0 + 2000,
    );
    let service = env.service();
    let stored = service
        .retitle(
            &session,
            TitleChange::Rename("Renamed".into()),
            RetitleOptions {
                may_be_unwritten: false,
                sidecar: SidecarMode::Always,
            },
            None,
        )
        .await
        .expect("renamed");
    assert_eq!(stored.as_deref(), Some("[archived] Renamed"));
    let archived = list(
        &service,
        json!({"cwd": cwd, "_meta": air(json!({"archived": "archived"}))}),
    )
    .await
    .expect("archived");
    assert_eq!(archived["sessions"][0]["title"], "Renamed");
}

/// archive titles > cut the archived title to the CLI's 200 characters
#[tokio::test]
async fn t2b_archive_cuts_the_title_to_200_characters() {
    let env = Env::new("archive-long");
    let cwd = env.cwd("app");
    let session = id(1);
    env.transcript(
        &cwd,
        &session,
        &[
            user(&session, &cwd, "work", T0),
            assistant(&session, T0 + 1000),
            custom_title(&session, &"y".repeat(250)),
        ],
        T0 + 2000,
    );
    let stored = env
        .service()
        .retitle(
            &session,
            TitleChange::Archive(true),
            RetitleOptions {
                may_be_unwritten: false,
                sidecar: SidecarMode::Existing,
            },
            None,
        )
        .await
        .expect("archived")
        .expect("stored");
    assert_eq!(stored.len(), 200);
    assert!(stored.starts_with("[archived] yyy"));
}

/// session/delete per client > deletes the transcript for a sessionIndex client, every copy and the session directory
#[tokio::test]
async fn t2b_delete_removes_every_copy_and_the_session_directory() {
    let env = Env::new("delete");
    let cwd = env.cwd("app");
    let session = simple_session(&env, &cwd, 1, T0);
    let sidecar_dir = env.project_dir(&cwd).join(&session);
    std::fs::create_dir_all(&sidecar_dir).expect("session dir");
    std::fs::write(sidecar_dir.join("custom-title.json"), "{}").expect("sidecar");
    // A second copy in another project, and an empty copy in a third.
    let second = env.base.join("projects/-copy-two");
    std::fs::create_dir_all(&second).expect("copy two");
    std::fs::write(
        second.join(format!("{session}.jsonl")),
        format!("{}\n", user(&session, "/x", "copy", T0)),
    )
    .expect("copy");
    let third = env.base.join("projects/-copy-three");
    std::fs::create_dir_all(&third).expect("copy three");
    std::fs::write(third.join(format!("{session}.jsonl")), "").expect("empty copy");
    let service = env.service();
    timeout(Duration::from_secs(30), service.delete(&session, false))
        .await
        .expect("finished")
        .expect("deleted");
    assert!(!env
        .project_dir(&cwd)
        .join(format!("{session}.jsonl"))
        .exists());
    assert!(!second.join(format!("{session}.jsonl")).exists());
    assert!(!third.join(format!("{session}.jsonl")).exists());
    assert!(!sidecar_dir.exists());
    // Deleting again: unknown, unless it was loaded here.
    let again = service.delete(&session, false).await.expect_err("unknown");
    assert_eq!(code(&again), -32002);
    service
        .delete(&session, true)
        .await
        .expect("a loaded session may have no transcript");
}

/// empty transcripts on delete > deletes a session that has only an empty transcript, and its session directory
#[tokio::test]
async fn t2b_delete_handles_a_session_with_only_an_empty_transcript() {
    let env = Env::new("delete-empty");
    let cwd = env.cwd("app");
    let session = id(1);
    let dir = env.project_dir(&cwd);
    std::fs::write(dir.join(format!("{session}.jsonl")), "").expect("empty");
    std::fs::create_dir_all(dir.join(&session)).expect("session dir");
    env.service()
        .delete(&session, false)
        .await
        .expect("deleted");
    assert!(!dir.join(format!("{session}.jsonl")).exists());
    assert!(!dir.join(&session).exists());
}

/// session/delete of an AIR client without sessionIndex > archives instead of deleting
#[tokio::test]
async fn t2b_air_client_without_the_index_archives_on_delete() {
    let env = Env::new("air-delete");
    let cwd = env.cwd("app");
    let session = simple_session(&env, &cwd, 1, T0);
    let mut connection = env.connection();
    // An AIR client that did not declare `sessionIndex`.
    let plain_air = AirClient::from_client_capabilities(
        &json!({"_meta": {"jetbrains": {"air": {"version": 1, "capabilities": []}}}}),
    );
    connection.negotiate(plain_air.as_ref());
    assert!(!connection.has_service());
    let operation = connection
        .delete_session(session.clone(), false, true)
        .expect("an AIR client archives");
    timeout(Duration::from_secs(30), operation)
        .await
        .expect("finished")
        .expect("archived");
    let transcript = env.project_dir(&cwd).join(format!("{session}.jsonl"));
    assert!(transcript.exists(), "the transcript survives");
    let text = std::fs::read_to_string(&transcript).expect("read");
    assert!(text.contains("\"customTitle\":\"[archived] prompt 1\""));
    // A session without a transcript fails as the SDK delete did.
    let missing = connection
        .delete_session(id(5), false, true)
        .expect("archive path");
    let error = missing.await.expect_err("unknown");
    assert_eq!(code(&error), -32603);
    assert_eq!(
        error.data,
        Some(json!({"details": format!("Session {} not found in any project directory", id(5))}))
    );
    // Another client gets the SDK delete (the agent's old path).
    assert!(connection.delete_session(id(1), false, false).is_none());
}

/// sessionIndex negotiation > is advertised with sessionArchive and sessionRename only to an AIR client that declares it
#[tokio::test]
async fn t2b_capabilities_are_advertised_only_after_negotiation() {
    let env = Env::new("caps");
    let mut connection = env.connection();
    assert!(connection.capabilities().is_empty());
    connection.negotiate(Some(&air_client()));
    assert_eq!(
        connection.capabilities(),
        vec![
            "sessionIndex",
            "sessionArchive",
            "sessionRename",
            "sessionListSubscribe",
            "sessionWorktrees"
        ]
    );
    // A later initialize without the capability drops the index.
    connection.negotiate(None);
    assert!(connection.capabilities().is_empty());
    // Without the capability the new methods answer method-not-found.
    let publish: Arc<dyn Fn(Value) + Send + Sync> = Arc::new(|_| {});
    let error = connection
        .rename(
            &json!({"sessionId": id(1), "title": "x"}),
            None,
            publish.clone(),
        )
        .err()
        .expect("no service");
    assert_eq!(code(&error), -32601);
    assert!(connection
        .archive(&json!({"sessionId": id(1)}), None, publish.clone())
        .is_err());
    assert!(connection
        .unarchive(&json!({"sessionId": id(1)}), None, publish)
        .is_err());
    assert!(connection.list(json!({})).is_none());
}

/// _session/archive: params check (-32602) once the capability is negotiated
#[tokio::test]
async fn t2b_archive_checks_its_params() {
    let env = Env::new("params");
    let mut connection = env.connection();
    connection.negotiate(Some(&air_client()));
    let publish: Arc<dyn Fn(Value) + Send + Sync> = Arc::new(|_| {});
    for params in [
        json!({}),
        json!({"sessionId": ""}),
        json!({"sessionId": "   "}),
        json!({"sessionId": 7}),
    ] {
        let error = connection
            .archive(&params, None, publish.clone())
            .err()
            .expect("invalid");
        assert_eq!(code(&error), -32602);
        assert_eq!(
            error.message,
            "Invalid params: params require a non-empty sessionId"
        );
    }
    let rename = connection
        .rename(&json!({"sessionId": id(1), "title": "  "}), None, publish)
        .err()
        .expect("invalid title");
    assert_eq!(
        error_message(&rename),
        "Invalid params: title must be a non-empty string"
    );
}

fn error_message(error: &agent_client_protocol::Error) -> String {
    error.message.clone()
}

/// the connection-level archive of a session that is not loaded: no update, the list hides it
#[tokio::test]
async fn t2b_connection_archive_of_an_unloaded_session_reports_nothing() {
    let env = Env::new("conn-archive");
    let cwd = env.cwd("app");
    let session = simple_session(&env, &cwd, 1, T0);
    let mut connection = env.connection();
    connection.negotiate(Some(&air_client()));
    let updates = Arc::new(AtomicUsize::new(0));
    let counter = updates.clone();
    let publish: Arc<dyn Fn(Value) + Send + Sync> = Arc::new(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    let operation = connection
        .archive(&json!({"sessionId": session}), None, publish.clone())
        .expect("accepted");
    assert_eq!(
        timeout(Duration::from_secs(30), operation)
            .await
            .expect("finished")
            .expect("archived"),
        json!({})
    );
    assert_eq!(
        updates.load(Ordering::SeqCst),
        0,
        "a session not loaded here is not reported"
    );
    let listing = connection.list(json!({"cwd": cwd})).expect("index list");
    let response = timeout(Duration::from_secs(30), listing)
        .await
        .expect("finished")
        .expect("list");
    assert!(ids(&response).is_empty());
    let unknown = connection
        .unarchive(&json!({"sessionId": id(8)}), None, publish)
        .expect("accepted");
    let error = unknown.await.expect_err("unknown");
    assert_eq!(code(&error), -32002);
}

/// forks held back from the list until the fork response is out
#[tokio::test]
async fn t2b_a_fork_is_held_back_from_the_list_until_its_response_is_out() {
    let env = Env::new("fork-hold");
    let cwd = env.cwd("app");
    let source = simple_session(&env, &cwd, 1, T0);
    let fork = simple_session(&env, &cwd, 2, T0 + 60_000);
    let service = env.service();
    service.fork_started(&source);
    service.fork_created(&source, &fork);
    assert_eq!(
        ids(&list(&service, json!({"cwd": cwd})).await.expect("held")),
        vec![source.clone()]
    );
    service.fork_answered(&source);
    assert_eq!(
        ids(&list(&service, json!({"cwd": cwd})).await.expect("released")),
        vec![fork, source]
    );
}

/// project directory: the row of a long path is found by its cut prefix, whatever the hash
#[tokio::test]
async fn t2b_list_finds_a_long_path_by_its_cut_prefix() {
    let env = Env::new("long");
    let name = "d".repeat(210);
    let cwd = env.cwd(&name);
    let session = id(1);
    // The CLI (Bun) hashes the long name differently: a directory with the cut
    // prefix and another suffix.
    let encoded = encode_project_path(&cwd, Platform::host());
    let prefix = &encoded[..200];
    let dir = env.root.join(format!("{prefix}-cliHash"));
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join(format!("{session}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n",
            user(&session, &cwd, "long path work", T0),
            assistant(&session, T0 + 1000)
        ),
    )
    .expect("write");
    set_mtime(&path, T0 + 2000);
    let response = list(&env.service(), json!({"cwd": cwd}))
        .await
        .expect("list");
    assert_eq!(ids(&response), vec![session]);
    assert_eq!(response["sessions"][0]["cwd"], cwd.as_str());
}

/// list cursor scope > does not bind the limit
#[tokio::test]
async fn t2b_list_cursor_does_not_bind_the_limit() {
    let env = Env::new("cursor-limit");
    let cwd = env.cwd("app");
    for n in 1..=5 {
        simple_session(&env, &cwd, n, T0 + n as i64 * 60_000);
    }
    let service = env.service();
    let first = list(
        &service,
        json!({"cwd": cwd, "_meta": air(json!({"limit": 2}))}),
    )
    .await
    .expect("page 1");
    let second = list(
        &service,
        json!({"cwd": cwd, "cursor": first["nextCursor"], "_meta": air(json!({"limit": 1}))}),
    )
    .await
    .expect("a cursor works with another limit");
    assert_eq!(ids(&second), vec![id(3)]);
}
