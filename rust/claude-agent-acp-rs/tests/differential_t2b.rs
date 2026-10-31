//! Turn 2 lane B differentials: Node-captured fixtures for the session index
//! (list, archive, rename, delete). Each test runs the Rust binary against the
//! scenario's script (seeded transcripts under `CLAUDE_CONFIG_DIR`) and diffs the
//! ordered ACP frames against the committed Node fixture, with NO ignore paths.

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
        "acp-diff-t2b-{name}-{}-{}",
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

/// M-07/M-58..M-64/M-77/C-13: the index-backed list of a `sessionIndex` client:
/// order, page size, opaque cursor, archive filter, foreign and mis-scoped
/// cursors, option validation, and the capabilities advertised at `initialize`.
#[test]
fn t2b_session_index_list() {
    run_and_diff("session-index-list");
}

/// M-70/M-81/M-82/M-83/M-84/M-85: `_session/rename`, `_session/archive` and
/// `_session/unarchive` of sessions that are not loaded, and their errors.
#[test]
fn t2b_session_index_mutate() {
    run_and_diff("session-index-mutate");
}

/// M-81/M-82: a session loaded here is retitled through its CLI
/// (`rename_session`), published (`session_info_update` title), archived with an
/// interrupt, torn down and reported (`_meta.jetbrains.air.archived`).
#[test]
fn t2b_session_index_loaded() {
    run_and_diff("session-index-loaded");
}

/// M-78/M-79/U-44: `_session/list/subscribe` pushes the row a rename and an
/// archive changed (`_session/list/changes`), `_session/list/unsubscribe` is
/// idempotent, and the params are checked.
#[test]
fn t2b_session_index_subscribe() {
    run_and_diff("session-index-subscribe");
}

/// B-77/M-64: a fork is listed with `forkedFrom` once its response is out, and a
/// rename of the loaded fork goes through its CLI only (the CLI's own transcript
/// copy is left to the CLI).
#[test]
fn t2b_session_index_fork() {
    run_and_diff("session-index-fork");
}

/// M-86: `session/delete` of a `sessionIndex` client removes every copy.
#[test]
fn t2b_session_index_delete() {
    run_and_diff("session-index-delete");
}

/// M-87 / WB-02: `session/delete` of an AIR client without `sessionIndex`
/// archives instead, and the old list hides the archived session (M-89).
#[test]
fn t2b_session_delete_air_archive() {
    run_and_diff("session-delete-air-archive");
}

/// T2FX item 1: a closed session's row shows what the registry and the transcript
/// tell again, without the cost of the query that ended.
#[test]
fn t2fx_session_index_close_state() {
    run_and_diff("session-index-close-state");
}

/// T2FX item 2: a failed `_session/rename` leaves the session loaded and usable,
/// and its title open to generation.
#[test]
fn t2fx_session_index_rename_failure() {
    run_and_diff("session-index-rename-failure");
}

/// T2FX item 2: a failed `_session/archive` (its CLI rename refused) leaves the
/// session loaded and usable instead of orphaning it.
#[test]
fn t2fx_session_index_archive_failure() {
    run_and_diff("session-index-archive-failure");
}

/// T2FX item 3: a client rename settles the title, so the next turn end neither
/// generates one nor publishes a second `session_info_update`.
#[test]
fn t2fx_session_index_rename_settles() {
    run_and_diff("session-index-rename-settles");
}
