//! Turn 3 lane B (O-66): the auth probe runs in the session's settings context:
//! its cwd, `--settings <override>` and the launch env on top of the process env.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use claude_agent_acp_rs::auth_status::{probe_cli_in, ProbeOutcome, ProbeRequest};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use serde_json::json;

fn temp_dir(tag: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-t3b-probe-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    tmp
}

/// A stand-in `claude` that records how it was invoked and answers a Pro login.
fn recording_claude(tmp: &std::path::Path) -> (PathBuf, PathBuf) {
    let record = tmp.join("record.txt");
    let script = tmp.join("claude");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{record}'\npwd >> '{record}'\nprintf 'X=%s\\n' \"$T3B_PROBE_X\" >> '{record}'\n\
             echo '{{\"loggedIn\":true,\"apiProvider\":\"firstParty\",\"subscriptionType\":\"pro\",\"email\":\"a@b.c\"}}'\n",
            record = record.display()
        ),
    )
    .expect("write script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    (script, record)
}

/// O-66: `--settings`, the cwd and the launch env reach the probe
#[tokio::test]
async fn t3b_the_probe_runs_in_the_session_settings_context() {
    let tmp = temp_dir("context");
    let cwd = tmp.join("work");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let (script, record) = recording_claude(&tmp);
    let request = ProbeRequest {
        cwd: cwd.clone(),
        settings_override: Some(json!({"apiKeyHelper": "my-helper"})),
        launch_env: Some(vec![("T3B_PROBE_X".into(), "1".into())]),
        logger: None,
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        probe_cli_in(Some(&script), &request, &[], &EnvSnapshot::empty()),
    )
    .await
    .expect("the probe finished");
    match outcome {
        ProbeOutcome::Status(status) => assert_eq!(status.kind, "account"),
        other => panic!("unexpected {other:?}"),
    }
    let recorded = std::fs::read_to_string(&record).expect("record");
    let lines: Vec<&str> = recorded.lines().collect();
    assert_eq!(
        &lines[..4],
        ["auth", "status", "--json", "--settings"],
        "{recorded}"
    );
    assert_eq!(lines[4], r#"{"apiKeyHelper":"my-helper"}"#);
    let canonical = cwd.canonicalize().expect("canonical cwd");
    assert_eq!(lines[5], canonical.to_string_lossy());
    assert_eq!(lines[6], "X=1");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Without a settings context the probe is the plain `auth status --json`.
#[tokio::test]
async fn t3b_the_probe_without_a_context_is_plain() {
    let tmp = temp_dir("plain");
    let (script, record) = recording_claude(&tmp);
    let request = ProbeRequest::in_dir(&tmp);
    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        probe_cli_in(Some(&script), &request, &[], &EnvSnapshot::empty()),
    )
    .await
    .expect("the probe finished");
    assert!(matches!(outcome, ProbeOutcome::Status(_)));
    let recorded = std::fs::read_to_string(&record).expect("record");
    assert_eq!(
        recorded.lines().take(3).collect::<Vec<_>>(),
        ["auth", "status", "--json"]
    );
    assert!(!recorded.contains("--settings"));
    let _ = std::fs::remove_dir_all(&tmp);
}
