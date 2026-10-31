//! T5B lane B — O-125: the SDK's `forkSession`, `resumeSessionAt` and
//! `resumeDropsTurn` options become `--fork-session`,
//! `--resume-session-at=<v>` and `--resume-drops-turn=<v>`
//! (`sdk.mjs@1197635-1197799`), in that order before `--session-id`.

use claude_agent_acp_rs::process::{build_argv, SpawnOptions};
use claude_agent_acp_rs::sdk_options::claude_options;
use serde_json::json;

fn spawn_with(options: serde_json::Value) -> Vec<String> {
    let params = json!({
        "cwd": "/project",
        "mcpServers": [],
        "_meta": {"claudeCode": {"options": options}},
    });
    let claude = claude_options(&params);
    let spawn = SpawnOptions {
        session_id: Some("sess".to_string()),
        fork_session: claude.fork_session,
        resume_session_at: claude.resume_session_at,
        resume_drops_turn: claude.resume_drops_turn,
        ..SpawnOptions::default()
    };
    build_argv(&spawn)
}

#[test]
fn o125_fork_argv_flags() {
    let argv = spawn_with(json!({
        "forkSession": true,
        "resumeSessionAt": "uuid-1",
        "resumeDropsTurn": false,
    }));
    let index = |token: &str| argv.iter().position(|arg| arg == token);
    let fork = index("--fork-session").expect("--fork-session");
    let at = index("--resume-session-at=uuid-1").expect("--resume-session-at");
    let drops = index("--resume-drops-turn=false").expect("--resume-drops-turn");
    let session = index("--session-id=sess").expect("--session-id");
    assert!(fork < at && at < drops && drops < session, "{argv:?}");
}

#[test]
fn o125_fork_argv_absent_and_truthy_values() {
    // Absent options emit no flag.
    let argv = spawn_with(json!({}));
    assert!(!argv.iter().any(|a| a == "--fork-session"));
    assert!(!argv.iter().any(|a| a.starts_with("--resume-session-at")));
    assert!(!argv.iter().any(|a| a.starts_with("--resume-drops-turn")));

    // `forkSession: false` and an empty `resumeSessionAt` are falsy; a present
    // `resumeDropsTurn` is emitted even when false (`!== undefined`).
    let argv = spawn_with(json!({
        "forkSession": false,
        "resumeSessionAt": "",
        "resumeDropsTurn": true,
    }));
    assert!(!argv.iter().any(|a| a == "--fork-session"));
    assert!(!argv.iter().any(|a| a.starts_with("--resume-session-at")));
    assert!(argv.iter().any(|a| a == "--resume-drops-turn=true"));

    // A `null` value is `String(null)`.
    let argv = spawn_with(json!({"resumeDropsTurn": null}));
    assert!(argv.iter().any(|a| a == "--resume-drops-turn=null"));
}
