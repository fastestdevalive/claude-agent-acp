//! rust-air-gaps Phase 1 (G3/G5): the `initialize` control_request the Rust
//! agent sends to `claude` for a client `systemPrompt` / AIR
//! `customInstructions` equals the one the Node adapter sends
//! (`porting/fixtures/<name>.init.json`, captured with `bash porting/capture.sh`).

mod common;

use common::{read_json, repo_root, run_recorder_with_init, rust_agent_binary};

/// Scripts that record their own `initialize` control_request.
const INIT_SCRIPTS: [&str; 7] = [
    "system-prompt-append",
    "system-prompt-override",
    "system-prompt-load",
    "custom-instructions",
    "system-prompt-combined",
    "system-prompt-meta-string",
    "system-prompt-meta-preset",
];

/// 1.3 / 3.1 (INV-39) — for every system-prompt script the Rust initialize request carries
/// exactly the Node's initialize control_request including `hooks` and `promptSuggestions`.
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
            init,
            read_json(&fixture),
            "{name}: Rust initialize request differs from the Node fixture"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
