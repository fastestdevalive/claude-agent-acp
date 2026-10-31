//! rust-air-gaps Phase 1 (G3/G5): the `initialize` control_request the Rust
//! agent sends to `claude` for a client `systemPrompt` / AIR
//! `customInstructions` equals the one the Node adapter sends
//! (`porting/fixtures/<name>.init.json`, captured with `bash porting/capture.sh`).

mod common;

use common::{read_json, repo_root, run_recorder_with_init, rust_agent_binary};
use serde_json::Value;

/// Scripts that record their own `initialize` control_request.
const INIT_SCRIPTS: [&str; 5] = [
    "system-prompt-append",
    "system-prompt-override",
    "system-prompt-load",
    "custom-instructions",
    "system-prompt-combined",
];

/// Drop the two request keys the Rust agent deliberately never sends: the SDK
/// `hooks` payload (PARITY: "Hooks ... skipped-deliberate") and
/// `promptSuggestions` (`InitializeOptions::prompt_suggestions`, left `None` by
/// the live agent). Every other key is compared exactly.
fn without_skipped_keys(mut init: Value) -> Value {
    if let Some(request) = init.get_mut("request").and_then(Value::as_object_mut) {
        request.remove("hooks");
        request.remove("promptSuggestions");
    }
    init
}

/// 1.3 — for every system-prompt script the Rust initialize request carries
/// exactly the Node's `systemPrompt` / `appendSystemPrompt` (and every other
/// non-skipped key).
#[test]
fn inv_39_initialize_matches_node() {
    let root = repo_root();
    for name in INIT_SCRIPTS {
        let script = root.join(format!("porting/corpus/{name}.acp.json"));
        let transcript = root.join(format!("porting/corpus/{name}.transcript.jsonl"));
        let fixture = root.join(format!("porting/fixtures/{name}.init.json"));
        assert!(fixture.exists(), "missing {fixture:?}");
        let tmp = std::env::temp_dir().join(format!("acp-sysprompt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let (_frames, init) = run_recorder_with_init(
            &rust_agent_binary().to_string_lossy(),
            &script,
            &transcript,
            &tmp.join("out.jsonl"),
        );
        assert_eq!(
            without_skipped_keys(init),
            without_skipped_keys(read_json(&fixture)),
            "{name}: Rust initialize request differs from the Node fixture"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
