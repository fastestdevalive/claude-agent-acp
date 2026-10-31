//! Turn 1 lane B differentials: Node-captured scenarios for the config /
//! initialize rows. Each scenario's fixtures come from
//! `bash porting/capture.sh <name>` (the Node adapter is authoritative).

mod common;

use common::{
    diff_frames, normalize_agent_info_version, normalize_root_paths, normalize_terminal_auth,
    parse_fixture_frames, pinned_upstream_version, read_json, repo_root, run_recorder,
    run_recorder_with_init, rust_agent_binary,
};

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "acp-t1b-{tag}-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Run the Rust binary against `name`'s script and diff its ordered frames
/// against the committed Node fixture.
fn run_and_diff(name: &str) {
    let root = repo_root();
    let script = root.join(format!("porting/corpus/{name}.acp.json"));
    let transcript = root.join(format!("porting/corpus/{name}.transcript.jsonl"));
    let fixture = root.join(format!("porting/fixtures/{name}.frames.jsonl"));
    assert!(fixture.exists(), "missing fixture {fixture:?}");
    let tmp = scratch(name);
    let frames = run_recorder(
        &rust_agent_binary().to_string_lossy(),
        &script,
        &transcript,
        &tmp.join("out.jsonl"),
    );
    let frames = normalize_terminal_auth(normalize_root_paths(frames, &root));
    let expected = normalize_terminal_auth(parse_fixture_frames(&fixture));
    let (frames, expected) =
        normalize_agent_info_version(frames, expected, &pinned_upstream_version());
    if let Err(diff) = diff_frames(&frames, &expected, &[]) {
        panic!("{name} differential failed:\n{diff}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The `initialize` control_request the Rust agent sends to `claude` equals the
/// Node adapter's (`<name>.init.json`).
fn init_matches_node(name: &str) {
    let root = repo_root();
    let script = root.join(format!("porting/corpus/{name}.acp.json"));
    let transcript = root.join(format!("porting/corpus/{name}.transcript.jsonl"));
    let fixture = root.join(format!("porting/fixtures/{name}.init.json"));
    assert!(fixture.exists(), "missing {fixture:?}");
    let tmp = scratch(name);
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
}

/// O-144 — `_meta.claudeCode.options.agents` reaches the `initialize`
/// control_request verbatim (needs the `agents: opts.agents` hook in
/// `agent.rs initialize_options_for`, see `lane-B-wiring.patch`).
#[test]
fn o144_agents_option_reaches_initialize() {
    init_matches_node("initialize-agents");
    run_and_diff("initialize-agents");
}

/// O-24 / O-26 — an AIR client that declared `recommendedValue` gets the
/// concrete model recommendation and the recommended effort advertised as
/// `_meta.jetbrains.air.recommendedValue` on the model and effort options.
#[test]
fn o24_o26_recommended_value_meta_on_model_and_effort_options() {
    run_and_diff("config-recommended-air");
}
