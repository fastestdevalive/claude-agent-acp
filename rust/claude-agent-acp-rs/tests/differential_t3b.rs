//! Turn 3 lane B differentials: Node-captured fixtures for providers, the legacy
//! gateway `authenticate` and `logout`. Each test runs the Rust binary against the
//! scenario's script and diffs the ordered ACP frames against the committed Node
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
        "acp-diff-t3b-{name}-{}-{}",
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

/// M-128/M-129/M-15/C-49/O-72/WB-01: `providers/list`, the four `providers/set`
/// validation errors, a listing that fails (refused connection; base URL with
/// credentials), `providers/disable`.
#[test]
fn t3b_providers_validation() {
    run_and_diff("providers-validation");
}

/// M-12: `authenticate` validation, the unknown method, and the gateway status
/// pushed before the response; the legacy gateway then shows in `providers/list`.
#[test]
fn t3b_authenticate_gateway() {
    run_and_diff("authenticate-gateway");
}

/// M-13/M-12: `logout` clears the legacy gateway, and the store is re-read.
#[test]
fn t3b_authenticate_logout() {
    run_and_diff("authenticate-logout");
}
