//! Turn 3 / lane A differentials (T3A): Node-captured fixtures for the hazard
//! lane's wire-visible behaviour. Each test runs the Rust binary against the
//! scenario's fake-`claude` transcript and diffs the ordered ACP frames against
//! the committed Node fixture, with NO ignore paths.

mod common;

use common::{
    diff_frames, normalize_agent_info_version, normalize_root_paths, normalize_terminal_auth,
    parse_fixture_frames, pinned_upstream_version, repo_root, run_recorder, rust_agent_binary,
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
        "acp-diff-t3a-{name}-{}-{}",
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

// ------------------------------------------------------------------ F25

/// V-06: `worker_shutting_down` then a clean end under the active turn ends the
/// turn `end_turn` with a `worker_shutdown` failure on the response.
#[test]
fn t3a_f25_session_failure_worker_shutdown() {
    run_and_diff("session-failure-worker-shutdown");
}

/// V-06: with no turn in flight the signal publishes the session-scoped failure.
#[test]
fn t3a_f25_session_failure_worker_shutdown_idle() {
    run_and_diff("session-failure-worker-shutdown-idle");
}

/// V-06: a client without AIR session failures gets the `worker_shutdown`
/// internal error.
#[test]
fn t3a_f25_session_failure_worker_shutdown_plain() {
    run_and_diff("session-failure-worker-shutdown-plain");
}

/// N-53 / V-15: `session/load` replays each synthetic usage-limit message as a
/// typed failure at its transcript position, named after its prompt.
#[test]
fn t3a_f25_session_failure_replay_usage_limit() {
    run_and_diff("session-failure-replay-usage-limit");
}

// ------------------------------------------------------------------ F21

/// B-76 / P-02: "No, keep planning" interrupts the cycle; the error-shaped
/// diagnostic paired with the recorded answer ends the prompt `cancelled`, the
/// deny carries the keep-planning message, and the next prompt runs normally.
#[test]
fn t3a_f21_exit_plan_reject_interrupt() {
    run_and_diff("exit-plan-reject-interrupt");
}

/// P-34: the clear-context options carry the "(N% used)" share of the measured
/// context window.
#[test]
fn t3a_f21_exit_plan_clear_context_label() {
    run_and_diff("exit-plan-clear-context-label");
}

// ------------------------------------------------------------------ F22

/// B-74 / B-14: approving the plan with a cleared context closes the first
/// child, starts a fresh one, publishes the mode, and continues the same prompt
/// in the fresh context with the usage of both contexts on its response.
#[test]
fn t3a_f22_exit_plan_clear_context() {
    run_and_diff("exit-plan-clear-context");
}

// ------------------------------------------------------------------ F15

/// N-62 / T-11 / T-34 / T-95 / T-96: an AIR client gets the background Bash task
/// announced once the structured tool result proves the command went to the
/// background (with the output path read from the result text), and the Bash
/// card carries the `backgrounded` marker.
#[test]
fn t3a_f15_bash_background_air() {
    run_and_diff("bash-background-air");
}

/// N-69: a terminal edge that came before the Bash result is kept as a
/// tombstone; the result then announces the task and its end.
#[test]
fn t3a_f15_bash_background_late_result() {
    run_and_diff("bash-background-late-result");
}

/// T-06 / T-11 / T-33 / T-34: a terminal client keeps the command's display
/// terminal running (no `terminal_exit` with the result) until the task ends,
/// then ends it with a null exit code.
#[test]
fn t3a_f15_bash_background_terminal() {
    run_and_diff("bash-background-terminal");
}

/// N-61: a task started by a `Monitor` tool call is not shown.
#[test]
fn t3a_f15_async_task_monitor_tool() {
    run_and_diff("async-task-monitor-tool");
}

/// N-64: the state change of a held task is sent after its spawn, in order.
#[test]
fn t3a_f15_async_task_held_update() {
    run_and_diff("async-task-held-update");
}

// ------------------------------------------------------------------ M-65

/// M-65: the session list shows a session waiting on a permission prompt as
/// `requires_action`, an idle one with its turn end, and a session whose last
/// turn failed as `error`.
#[test]
fn t3a_m65_session_index_activity() {
    run_and_diff("session-index-activity");
}

/// M-65: a session whose CLI died mid-turn keeps the `error` row.
#[test]
fn t3a_m65_session_index_query_failed() {
    run_and_diff("session-index-query-failed");
}

// ------------------------------------------------------------------ Opus review gaps

/// A `SandboxNetworkAccess` approval through the real permission handler: the
/// call is surfaced and asked about, but the CLI never sends a tool result for
/// its synthetic id, so the turn must still end `end_turn` (B-64).
#[test]
fn t3a_gap_network_permission() {
    run_and_diff("network-permission");
}
