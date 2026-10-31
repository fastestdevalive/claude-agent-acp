//! T4A3 STEP 5 (F24: O-143/O-162) — `_meta.claudeCode.options.skills` reaches
//! the `initialize` control request verbatim (upstream `acp-agent.ts:1601-1640`);
//! `verbatimPrompts` is an SDK Query option, never an initialize field (FA3-03).

mod common;

use std::path::PathBuf;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::time::Duration;

/// O-143/O-162 — the new-session `_meta.claudeCode.options` fields are set on
/// the initialize request, verbatim.
#[tokio::test]
async fn t4a3_init_options_carry_skills_and_verbatim_prompts() {
    let tmp = std::env::temp_dir().join(format!("t4a3-init-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let transcript: PathBuf = tmp.join("init.transcript.jsonl");
    std::fs::write(
        &transcript,
        r#"{"expect":{"type":"control_request","request":{"subtype":"initialize"}},"emit":[{"type":"control_response","response":{"subtype":"success","request_id":"$REQ","response":{"models":[],"account":{},"commands":[]}}}]}"#,
    )
    .expect("write transcript");
    let init_out = tmp.join("init.json");
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                transcript.to_string_lossy().into_owned(),
            ),
            (
                "FAKE_CLAUDE_INIT_OUT".to_string(),
                init_out.to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(repo_root()),
        env: EnvSnapshot::empty(),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let cwd_for_client = cwd.clone();
    let skills = json!([{ "name": "commits", "path": "/skills/commits/SKILL.md" }]);
    let skills_for_client = skills.clone();
    let result = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| {
                let cwd = cwd_for_client.clone();
                let skills = skills_for_client.clone();
                async move {
                    connection
                        .send_request(UntypedMessage::new(
                            "initialize",
                            json!({ "protocolVersion": 1 }),
                        )?)
                        .block_task()
                        .await?;
                    connection
                        .send_request(UntypedMessage::new(
                            "session/new",
                            json!({
                                "cwd": cwd.to_string_lossy(),
                                "_meta": {
                                    "claudeCode": {
                                        "options": {
                                            "skills": skills,
                                            "verbatimPrompts": true,
                                        }
                                    }
                                }
                            }),
                        )?)
                        .block_task()
                        .await?;
                    Ok::<(), agent_client_protocol::Error>(())
                }
            },
        )
        .await;
    assert!(result.is_ok(), "session/new resolves: {result:?}");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !init_out.exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let raw: Value = serde_json::from_str(
        &std::fs::read_to_string(&init_out).expect("initialize control_request captured"),
    )
    .expect("parse init");
    assert_eq!(raw["request"]["subtype"], "initialize");
    assert_eq!(
        raw["request"]["skills"], skills,
        "skills reach the initialize request verbatim: {raw}"
    );
    // FA3-03: `verbatimPrompts` is not an initialize field (Node's
    // `buildInitializeRequest` has none); it only marks user frames
    // `client_composed` in the SDK. The unit tests in control.rs cover that.
    assert!(
        raw["request"].get("verbatimPrompts").is_none(),
        "verbatimPrompts is not forwarded to initialize: {raw}"
    );

    drop(serve_task);
    let _ = std::fs::remove_dir_all(&tmp);
}
