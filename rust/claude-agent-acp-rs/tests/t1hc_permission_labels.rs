//! Turn 1 fix round 3, lane C+D: the shell-directory label with trailing
//! backslashes. Expected labels come from the Node oracle
//! (`nodeh/opts3.mjs`, `buildClaudePermissionOptions`), not from hand writing.

use std::path::PathBuf;

use claude_agent_acp_rs::permission_options::{
    build_options, ChangeSet, OptionsContext, OPTION_ALLOW_WITH_UPDATES,
};
use serde_json::json;

fn label(directories: &[&str]) -> Option<String> {
    let ctx = OptionsContext {
        tool_name: "Bash".to_string(),
        display_name: None,
        input: json!({ "command": "ls" }),
        cwd: PathBuf::from("/repo"),
        change_set: Some(ChangeSet {
            updates: vec![json!({
                "type": "addDirectories",
                "destination": "session",
                "directories": directories,
            })],
        }),
        no_persistent_rule: false,
        default_to_no: false,
        available_modes: vec!["default".into(), "acceptEdits".into()],
        auto_unavailable: false,
        pre_plan_mode: None,
        context_used_percent: None,
        home_dir: None,
    };
    build_options(&ctx)
        .into_iter()
        .find(|o| o["optionId"] == OPTION_ALLOW_WITH_UPDATES)
        .and_then(|o| o["name"].as_str().map(String::from))
}

#[test]
fn trailing_backslash_does_not_merge_with_trailing_slash() {
    // Node: "a\b\/ and a\b\/" (two entries); the old Rust merged them into one.
    assert_eq!(
        label(&["a\\b\\", "a\\b\\/"]).as_deref(),
        Some("Yes, and always allow access to a\\b\\/ and a\\b\\/ from this project")
    );
    assert_eq!(
        label(&["/x/y\\", "/x/y\\/"]).as_deref(),
        Some("Yes, and always allow access to /x/y\\/ and /x/y\\/ from this project")
    );
    // Single-path forms (unchanged by the fix).
    assert_eq!(
        label(&["q\\"]).as_deref(),
        Some("Yes, and always allow access to q\\/ from this project")
    );
    assert_eq!(
        label(&["/x/y\\"]).as_deref(),
        Some("Yes, and always allow access to y\\/ from this project")
    );
    assert_eq!(
        label(&["c:\\x\\"]).as_deref(),
        Some("Yes, and always allow access to c:\\x\\/ from this project")
    );
}
