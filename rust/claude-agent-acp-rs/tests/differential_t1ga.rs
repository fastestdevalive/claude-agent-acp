//! Turn 1 fix round 2 / lane A differentials (T1GA): Node-captured fixtures for
//! process death (eviction, AIR `transport_lost`) and the early permission
//! request. Each test runs the Rust binary against the scenario's fake-`claude`
//! transcript and diffs the ordered ACP frames against the committed Node
//! fixture, with NO ignore paths.

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
        "acp-diff-t1ga-{name}-{}-{}",
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

/// V-03 (finding 1): the child dies under a prompt. The prompt is rejected, the
/// session is evicted (`session/set_mode` finds no session) and a `session/load`
/// of the same params starts a new child that serves the next prompt.
#[test]
fn t1ga_process_death_evicts_the_session_and_load_starts_fresh() {
    run_and_diff("process-death-reload");
}

/// V-03 (finding 2): the same with an AIR client that reads session failures. No
/// turn is active, so the failure is session-scoped and the prompt is rejected.
#[test]
fn t1ga_process_death_air_publishes_a_session_scoped_failure() {
    run_and_diff("process-death-reload-air");
}

/// V-03 (finding 2): with an ACTIVE turn the AIR client's prompt resolves
/// `end_turn` carrying the `transport_lost` failure; the queued prompt is
/// rejected "process exited unexpectedly".
#[test]
fn t1ga_process_death_air_active_turn_ends_with_transport_lost() {
    run_and_diff("process-death-active-air");
}

/// V-03: a plain client gets the rejection for the active and the queued prompt.
#[test]
fn t1ga_process_death_rejects_active_and_queued_for_a_plain_client() {
    run_and_diff("process-death-active");
}

/// P-50 / T-40 (finding 5): the permission request arrives BEFORE the streamed
/// tool call. The surfaced `tool_call` and the request's `toolCall` carry the
/// AIR `rawInput` (a Write without its file text), and the streamed tool use
/// that follows sends no extra `rawInput` update.
#[test]
fn t1ga_early_permission_request_tool_call_is_air_shaped() {
    run_and_diff("write-air-early-permission");
}
