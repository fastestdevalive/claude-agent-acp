//! Turn 1 fix round / lane A differentials: Node-captured fixtures for the
//! findings of the Opus review (T1FA). Each test runs the Rust binary against the
//! scenario's fake-`claude` transcript and diffs the ordered ACP frames against
//! the committed Node fixture, with NO ignore paths.

mod common;

use common::{
    diff_frames, normalize_agent_info_version, normalize_root_paths, normalize_terminal_auth,
    parse_fixture_frames, pinned_upstream_version, repo_root, run_recorder, run_recorder_with_argv,
    rust_agent_binary,
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
        "acp-diff-t1fa-{name}-{}-{}",
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

/// V-08 (finding 9): `session/close` with a prompt in flight. The cancelled
/// prompt's response leaves BEFORE the close reply, and close replies exactly
/// once (the recorder keeps the ordered wire, so the order is part of the diff).
#[test]
fn t1fa_v08_close_with_prompt_in_flight() {
    run_and_diff("close-prompt-in-flight");
}

/// Run `name` and compare the argv of the LAST `claude` spawn with the Node
/// fixture (the frames alone cannot tell which mode the recreated process got).
fn run_and_diff_argv(name: &str) {
    let root = repo_root();
    let script = root.join(format!("porting/corpus/{name}.acp.json"));
    let transcript = root.join(format!("porting/corpus/{name}.transcript.jsonl"));
    let tmp = std::env::temp_dir().join(format!(
        "acp-argv-t1fa-{name}-{}-{}",
        std::process::id(),
        NEXT_DIFF_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let out = tmp.join("out.jsonl");
    let agent_cmd = rust_agent_binary();
    let (_frames, rust_argv) =
        run_recorder_with_argv(&agent_cmd.to_string_lossy(), &script, &transcript, &out);
    let expected = common::read_json(&root.join(format!("porting/fixtures/{name}.argv.json")));
    assert_eq!(
        rust_argv, expected,
        "{name} argv diverges from the Node fixture"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// B-117 (finding 12): a changed-params `session/resume` of a session whose stream
/// ended recreates it with its LIVE permission mode (`plan`, set before the
/// stream died), not the mode it was created with.
#[test]
fn t1fa_b117_recreate_after_stream_end_keeps_the_live_mode() {
    run_and_diff("resume-live-mode-after-eof");
    run_and_diff_argv("resume-live-mode-after-eof");
}

/// B-117 (finding 11): a changed-params `session/resume` rejected for its `cwd`
/// (the live session was already torn down) must leave nothing behind: a later
/// resume of the same id starts in the transcript's mode, not the old live one.
#[test]
fn t1fa_b117_rejected_recreate_leaves_no_stale_mode() {
    run_and_diff("resume-recreate-rejected-then-load");
    run_and_diff_argv("resume-recreate-rejected-then-load");
}

/// M-38 (finding 14): a `set_config_option` whose `value` is neither a string nor
/// `{type: "boolean", value: <bool>}` fails the SDK's schema check: `-32602`
/// `Invalid params` with the zod-shaped `data`, not `-32603`.
#[test]
fn t1fa_m38_set_config_option_bad_value() {
    run_and_diff("config-set-option-bad-value");
}
