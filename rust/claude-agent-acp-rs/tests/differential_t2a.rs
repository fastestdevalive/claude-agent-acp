//! Turn 2 / lane A differentials (T2A): Node-captured fixtures for the hazard
//! lane's wire-visible behaviour. Each test runs the Rust binary against the
//! scenario's fake-`claude` transcript and diffs the ordered ACP frames against
//! the committed Node fixture, with NO ignore paths.

mod common;

use common::{
    diff_frames, normalize_agent_info_version, normalize_root_paths, normalize_terminal_auth,
    parse_fixture_frames, pinned_upstream_version, read_json, repo_root, run_recorder,
    run_recorder_with_init, rust_agent_binary,
};

static NEXT_DIFF_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Run the Rust binary against `name`'s corpus script via the recorder +
/// fake-claude and diff its frames against the committed Node fixture.
fn run_and_diff(name: &str) {
    let root = repo_root();
    let script = root.join(format!("porting/corpus/{name}.acp.json"));
    let transcript = root.join(format!("porting/corpus/{name}.transcript.jsonl"));
    let fixture = root.join(format!("porting/fixtures/{name}.frames.jsonl"));

    assert!(script.exists(), "missing script {script:?}");
    assert!(transcript.exists(), "missing transcript {transcript:?}");
    assert!(fixture.exists(), "missing fixture {fixture:?}");

    let tmp = std::env::temp_dir().join(format!(
        "acp-diff-t2a-{name}-{}-{}",
        std::process::id(),
        NEXT_DIFF_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let out = tmp.join("out.jsonl");

    let agent_cmd = rust_agent_binary();
    assert!(
        agent_cmd.exists(),
        "rust agent binary missing: {}",
        agent_cmd.display()
    );
    let frames = run_recorder(&agent_cmd.to_string_lossy(), &script, &transcript, &out);
    let frames = normalize_terminal_auth(normalize_root_paths(frames, &root));
    let expected = normalize_terminal_auth(parse_fixture_frames(&fixture));
    let (frames, expected) =
        normalize_agent_info_version(frames, expected, &pinned_upstream_version());
    if let Err(diff) = diff_frames(&frames, &expected, &[]) {
        panic!("{name} differential failed:\n{diff}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

// ------------------------------------------------------------------ F7a

/// N-20: a `tool_progress` beat updates the call it reports on; a heartbeat id
/// falls back to its parent call; an unannounced or finished call is dropped.
#[test]
fn t2a_f7a_frame_tool_progress() {
    run_and_diff("frame-tool-progress");
}

/// T-05 / T-82: `memory_recall` frames become completed memory tool calls.
#[test]
fn t2a_f7a_frame_memory_recall() {
    run_and_diff("frame-memory-recall");
}

/// P-01 / T-81: `permission_denied` fails the surfaced call with the reason; a
/// denial for a call the client never saw is dropped.
#[test]
fn t2a_f7a_frame_permission_denied() {
    run_and_diff("frame-permission-denied");
}

/// T-31 / T-32 / C-23: a terminal client gets the command output and exit on
/// the terminal channel, the exit code from the `Exit code N` text.
#[test]
fn t2a_f7a_bash_terminal_result() {
    run_and_diff("bash-terminal-result");
}

/// T-51 / T-54: WebSearch hits and the Agent/Task report render from the
/// structured result (trailer strip, hand-back unwrap, partial-turn label).
#[test]
fn t2a_f7a_search_agent_results() {
    run_and_diff("search-agent-results");
}

// ------------------------------------------------------------------ F7b

/// N-14: a plain client gets the informational notices as transcript lines.
#[test]
fn t2a_f7b_frame_informational() {
    run_and_diff("frame-informational");
}

/// N-14: a notice client gets one notice per tool use, none for `info`, and the
/// zero-token result repeating a notice is not forwarded again.
#[test]
fn t2a_f7b_frame_informational_notice() {
    run_and_diff("frame-informational-notice");
}

/// U-13 / N-40: the refusal explanation streams ahead of the refusal response;
/// the frame's `stop_details` wins over the banner, a non-human turn seeds none.
#[test]
fn t2a_f7b_frame_refusal() {
    run_and_diff("frame-refusal");
}

/// N-39: a persistent fallback notes the swap and syncs the model; `revert` and
/// local scope keep the session model.
#[test]
fn t2a_f7b_frame_model_fallback() {
    run_and_diff("frame-model-fallback");
}

/// V-04: the CLI's synthetic login reply fails the live turn as auth_required.
#[test]
fn t2a_f7b_frame_login_synthetic() {
    run_and_diff("frame-login-synthetic");
}

/// N-44: a conversation reset clears the task plan.
#[test]
fn t2a_f7b_frame_conversation_reset() {
    run_and_diff("frame-conversation-reset");
}

/// N-38 / N-39 (AIR): the retry is a warning on the turn, the fallback an
/// advisory failure record.
#[test]
fn t2a_f7b_frame_api_retry_air() {
    run_and_diff("frame-api-retry-air");
}

// ------------------------------------------------------------------ F14

/// N-18 / N-36: a terminal `task_updated` patch (not a top-level status) settles
/// the live subagent, so the held turn drains at the next idle.
#[test]
fn t2a_f14_task_updated_patch_settles() {
    run_and_diff("task-updated-patch-settles");
}

/// N-43: a `background_tasks_changed` level without the subagent ends the hold.
#[test]
fn t2a_f14_background_tasks_level_ends_hold() {
    run_and_diff("background-tasks-level-ends-hold");
}

/// N-33: a `task_notification` of an earlier run is ignored; the hold stays for
/// the resumed run.
#[test]
fn t2a_f14_task_run_guard() {
    run_and_diff("task-run-guard");
}

/// N-43 / N-66 / N-70 (AIR): a level-recovered task stays panel-only when its
/// `task_started` arrives late, progress dedupes, the level closes a lost task as
/// `stopped` and the late notification corrects it.
#[test]
fn t2a_f14_async_task_level_recovery() {
    run_and_diff("async-task-level-recovery");
}

// ------------------------------------------------------------------ F17

/// B-08 / B-09 / B-73 (closes WB-09): the interrupted result reaches the client
/// for the call it names, and the call the stream gave no result for closes with
/// the cancelled wording before the response.
#[test]
fn t2a_f17_cancel_open_tool_closed() {
    run_and_diff("cancel-open-tool-closed");
}

/// B-08 / T-15: the tool call of a foreground subagent closes with its Agent
/// call named in the report.
#[test]
fn t2a_f17_cancel_subagent_tool_closed() {
    run_and_diff("cancel-subagent-tool-closed");
}

// ------------------------------------------------------------------ F30

/// A-13 / U-45 / B-78: a generation that yields no title releases the latch, the
/// next turn end asks again and publishes `{title, updatedAt}`, and a settled
/// session is never asked a third time.
#[test]
fn t2a_f30_session_title_retry() {
    run_and_diff("session-title-retry");
}

/// O-153: `_meta.claudeCode.options.agentProgressSummaries` reaches the
/// `initialize` control_request (needs the `agent_progress_summaries` hook in
/// `agent.rs initialize_options_for`, see `lane-A-wiring.patch`).
#[test]
fn t2a_f14_agent_progress_summaries_reaches_initialize() {
    let root = repo_root();
    let name = "initialize-agent-progress";
    let script = root.join(format!("porting/corpus/{name}.acp.json"));
    let transcript = root.join(format!("porting/corpus/{name}.transcript.jsonl"));
    let fixture = root.join(format!("porting/fixtures/{name}.init.json"));
    assert!(fixture.exists(), "missing {fixture:?}");
    let tmp = std::env::temp_dir().join(format!(
        "acp-init-t2a-{name}-{}-{}",
        std::process::id(),
        NEXT_DIFF_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let (_frames, init) = run_recorder_with_init(
        &rust_agent_binary().to_string_lossy(),
        &script,
        &transcript,
        &tmp.join("out.jsonl"),
    );
    assert_eq!(
        init,
        read_json(&fixture),
        "{name}: Rust initialize request differs from the Node fixture"
    );
    let _ = std::fs::remove_dir_all(&tmp);
    run_and_diff(name);
}
