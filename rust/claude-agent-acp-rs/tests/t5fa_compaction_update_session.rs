//! T5FA — the `session.compaction` golden: a client that advertises the ACP
//! compaction contract gets `compaction_update` / `compaction_summary_chunk`
//! instead of the legacy "Compact conversation" tool call, with Node's key
//! order (`version` first in the AIR payload).
//!
//! The corpus script sends `initialize` with `session.compaction` VERBATIM
//! (`raw_capabilities`): the pinned Rust schema gates that capability behind a
//! cargo feature this workspace does not enable, so the typed recorder would
//! strip it. The recorder hook lives in
//! `porting/parity/lane-FA-wiring.patch` (now applied); with it the scenario
//! replays against the Node fixture. The frame comparison is on parsed JSON
//! values, so it does NOT check key order: the byte-level order (`version`
//! first in the AIR `_meta`) is pinned by the unit test
//! `t5fa_compaction_update_wire_key_order`.

mod common;

use common::{
    diff_frames, normalize_agent_info_version, normalize_root_paths, normalize_terminal_auth,
    parse_fixture_frames, pinned_upstream_version, repo_root, run_recorder, rust_agent_binary,
};

static NEXT_DIFF_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn run_and_diff(name: &str) {
    let root = repo_root();
    let script = root.join(format!("porting/corpus/{name}.acp.json"));
    let transcript = root.join(format!("porting/corpus/{name}.transcript.jsonl"));
    let fixture = root.join(format!("porting/fixtures/{name}.frames.jsonl"));

    assert!(script.exists(), "missing script {script:?}");
    assert!(transcript.exists(), "missing transcript {transcript:?}");
    assert!(fixture.exists(), "missing fixture {fixture:?}");

    let tmp = std::env::temp_dir().join(format!(
        "acp-diff-t5fa-{name}-{}-{}",
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

/// U-77 / U-78 / U-102 / B-109: the `compaction_update` presentation on the
/// wire — `in_progress` with the AIR `_meta`, the streamed summary chunks, then
/// the boundary's `completed` with `trigger`/token facts, `version` first.
#[test]
fn t5fa_compaction_update_session_matches_the_node_golden() {
    run_and_diff("compaction-update-session");
}
