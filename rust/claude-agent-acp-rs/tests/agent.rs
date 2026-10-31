//! Agent-level integration tests (8.T2, 8.T5, 8.T6) driving `serve` over an
//! in-memory `Channel::duplex()` transport with a real `Client`.
//!
//! Requests are sent as [`UntypedMessage`]s so the tests assert directly against
//! the raw wire values (the agent's `handle_request` routes untyped methods and
//! the response bodies are the JSON-RPC `result` objects).

mod common;

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use common::{fake_claude_binary, read_json, repo_root, rust_agent_binary, wait_for_file};
use serde_json::json;
use std::io::Write;
use tokio::time::timeout;

/// A minimal transcript for the argv/session tests: only an initialize step.
fn init_only_transcript(tmp: &std::path::Path) -> PathBuf {
    let transcript = tmp.join("init-only.transcript.jsonl");
    std::fs::write(
        &transcript,
        "{\"expect\":{\"type\":\"control_request\",\"request\":{\"subtype\":\"initialize\"}},\"emit\":[{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{\"models\":[{\"value\":\"claude-sonnet-4-5\"}],\"commands\":[],\"account\":{}}}}]}\n",
    )
    .expect("write transcript");
    transcript
}

/// The error code from a JSON-RPC error, as the raw wire number.
fn error_code(err: &agent_client_protocol::Error) -> i64 {
    serde_json::to_value(err)
        .ok()
        .and_then(|v| v.get("code").cloned())
        .and_then(|c| c.as_i64())
        .unwrap_or(i64::MAX)
}

/// Build a `ServeOptions` pointing at fake-claude for a given transcript.
fn fake_opts(
    root: &std::path::Path,
    transcript: &std::path::Path,
    argv_out: &std::path::Path,
) -> ServeOptions {
    ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                transcript.to_string_lossy().into_owned(),
            ),
            (
                "FAKE_CLAUDE_ARGV_OUT".to_string(),
                argv_out.to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(root.to_path_buf()),
        ..Default::default()
    }
}

/// Read the recorded argv from fake-claude.
fn read_argv(argv_out: &std::path::Path) -> Vec<String> {
    wait_for_file(argv_out, Duration::from_secs(10)).expect("argv recorded");
    let captured = read_json(argv_out);
    captured["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

/// 8.T2 — an unhandled method returns `-32601`, never hangs or panics.
#[tokio::test]
async fn inv_unhandled_method_returns_method_not_found() {
    let opts = ServeOptions::default();
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let result = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                let resp = connection
                    .send_request(UntypedMessage::new("session/bogus", json!({}))?)
                    .block_task()
                    .await;
                let err = resp.expect_err("unhandled method must error");
                assert_eq!(
                    error_code(&err),
                    -32601,
                    "unhandled method must return -32601"
                );
                Ok::<(), agent_client_protocol::Error>(())
            },
        )
        .await;
    let ok = result.is_ok();
    drop(result);
    let _ = timeout(Duration::from_secs(5), serve_task).await;
    assert!(ok);
}

/// 8.T5 — `session/new` with a model and permission mode yields
/// `--model` / `--permission-mode` / `--session-id=<uuid>` in the spawned argv,
/// and the returned `sessionId` equals that uuid.
#[tokio::test]
async fn inv_session_new_model_permission_and_uuid() {
    let root = repo_root();
    let tmp = std::env::temp_dir().join(format!("acp-agent-t5-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let transcript = init_only_transcript(&tmp);
    let argv_out = tmp.join("argv.json");

    let opts = fake_opts(&root, &transcript, &argv_out);
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let root_for_client = root.clone();
    let result = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| {
                let root_for_client = root_for_client.clone();
                async move {
                    let params = json!({
                        "cwd": root_for_client.to_string_lossy(),
                        "_meta": {
                            "claudeCode": {
                                "options": {
                                    "model": "claude-sonnet-4-5",
                                    "permissionMode": "acceptEdits",
                                }
                            }
                        }
                    });
                    let resp = connection
                        .send_request(UntypedMessage::new("session/new", params)?)
                        .block_task()
                        .await?;
                    let session_id = resp["sessionId"].as_str().unwrap_or("").to_string();
                    assert_eq!(session_id.len(), 36, "sessionId is a uuid: {session_id}");
                    Ok::<String, agent_client_protocol::Error>(session_id)
                }
            },
        )
        .await;
    let session_id = result.clone().expect("session/new resolves");
    drop(result);
    let _ = timeout(Duration::from_secs(5), serve_task).await;

    let argv = read_argv(&argv_out);
    assert!(
        argv.windows(2)
            .any(|w| w == ["--model", "claude-sonnet-4-5"]),
        "model -> --model in argv: {argv:?}"
    );
    assert!(
        argv.windows(2)
            .any(|w| w == ["--permission-mode", "acceptEdits"]),
        "permission mode -> --permission-mode in argv: {argv:?}"
    );
    let session_arg = argv.iter().find_map(|a| a.strip_prefix("--session-id="));
    assert_eq!(
        session_arg,
        Some(session_id.as_str()),
        "the returned sessionId equals the --session-id=<uuid>: {argv:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// 8.T6 — `session/load` with an id this process never minted spawns
/// `--resume=<id>` and returns ok (terminal → Rich Chat).
#[tokio::test]
async fn inv_session_load_foreign_id_resumes() {
    let root = repo_root();
    let tmp = std::env::temp_dir().join(format!("acp-agent-t6-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let transcript = init_only_transcript(&tmp);
    let argv_out = tmp.join("argv.json");

    let foreign_id = "some-terminal-started-session-id";
    let opts = fake_opts(&root, &transcript, &argv_out);
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let root_for_client = root.clone();
    let foreign_owned = foreign_id.to_string();
    let result = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| {
                let root_for_client = root_for_client.clone();
                let foreign = foreign_owned.clone();
                async move {
                    let params = json!({
                        "sessionId": foreign,
                        "cwd": root_for_client.to_string_lossy(),
                    });
                    let resp = connection
                        .send_request(UntypedMessage::new("session/load", params)?)
                        .block_task()
                        .await?;
                    assert_eq!(
                        resp["sessionId"].as_str().unwrap_or(""),
                        foreign,
                        "session/load returns the requested id"
                    );
                    Ok::<(), agent_client_protocol::Error>(())
                }
            },
        )
        .await;
    assert!(result.is_ok(), "session/load resolves ok");
    drop(result);
    let _ = timeout(Duration::from_secs(5), serve_task).await;

    let argv = read_argv(&argv_out);
    assert!(
        argv.iter().any(|a| a == &format!("--resume={foreign_id}")),
        "session/load must spawn --resume=<id>: {argv:?}"
    );
    assert!(
        !argv.iter().any(|a| a.starts_with("--session-id=")),
        "a resumed session must NOT spawn --session-id: {argv:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

/// 12.1 end-to-end — `_session/steering` over the channel: idle +
/// `promptRequired` → `{outcome:"promptRequired", reason:"noRunningTurn"}`;
/// `idleBehavior:"x"` → `invalidParams`; and a turn in flight → `{outcome:
/// "injected"}`. Exercises the session actor's `Command::Steer` path (12.1).
#[tokio::test]
async fn inv_steering_end_to_end() {
    let root = repo_root();
    let tmp = std::env::temp_dir().join(format!("acp-agent-steer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");

    // A transcript that holds a turn in flight (echo + assistant, no result).
    let transcript = tmp.join("steer.transcript.jsonl");
    std::fs::write(
        &transcript,
        "{\"expect\":{\"type\":\"control_request\",\"request\":{\"subtype\":\"initialize\"}},\"emit\":[{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{\"models\":[],\"commands\":[]}}}]}\n{\"expect\":{\"type\":\"user\"},\"emit\":[{\"type\":\"user\",\"uuid\":\"$MATCH.uuid\",\"isReplay\":true,\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hold\"}]}},{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}}]}\n",
    )
    .expect("write transcript");

    let timings = claude_agent_acp_rs::process::Timings {
        force_cancel_grace: Duration::from_millis(1000),
        ..claude_agent_acp_rs::process::Timings::default()
    };
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            transcript.to_string_lossy().into_owned(),
        )],
        default_cwd: Some(root.clone()),
        timings,
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let result = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                // initialize
                connection
                    .send_request(UntypedMessage::new(
                        "initialize",
                        json!({ "protocolVersion": 1 }),
                    )?)
                    .block_task()
                    .await?;
                // session/new
                let created = connection
                    .send_request(UntypedMessage::new("session/new", json!({}))?)
                    .block_task()
                    .await?;
                let session_id = created["sessionId"].as_str().unwrap_or("").to_string();

                // Idle (no turn yet) + promptRequired opt-in -> promptRequired.
                let steer_params = json!({
                    "sessionId": session_id,
                    "prompt": [ { "type": "text", "text": "follow up" } ],
                    "_meta": { "steering": { "idleBehavior": "promptRequired" } }
                });
                let resp = connection
                    .send_request(UntypedMessage::new("_session/steering", steer_params)?)
                    .block_task()
                    .await?;
                assert_eq!(
                    resp,
                    json!({ "outcome": "promptRequired", "reason": "noRunningTurn" }),
                    "idle + promptRequired must yield promptRequired"
                );

                // Bad idleBehavior -> invalidParams.
                let steer_params = json!({
                    "sessionId": session_id,
                    "prompt": [ { "type": "text", "text": "x" } ],
                    "_meta": { "steering": { "idleBehavior": "x" } }
                });
                let resp = connection
                    .send_request(UntypedMessage::new("_session/steering", steer_params)?)
                    .block_task()
                    .await;
                let err = resp.expect_err("bad idleBehavior must error");
                assert_eq!(
                    serde_json::to_value(&err)
                        .ok()
                        .and_then(|v| v.get("code").cloned())
                        .and_then(|c| c.as_i64()),
                    Some(-32602),
                    "bad idleBehavior must be invalidParams"
                );

                // Start a turn that stays in flight (mid-turn transcript), then
                // steer while it is running -> injected.
                let prompt_task = {
                    let connection = connection.clone();
                    let session_id = session_id.clone();
                    tokio::spawn(async move {
                        if let Ok(msg) = UntypedMessage::new(
                            "session/prompt",
                            json!({
                                "sessionId": session_id,
                                "prompt": [ { "type": "text", "text": "hold" } ]
                            }),
                        ) {
                            let _ = connection.send_request(msg).block_task().await;
                        }
                    })
                };
                tokio::time::sleep(Duration::from_millis(300)).await;

                let steer_params = json!({
                    "sessionId": session_id,
                    "prompt": [ { "type": "text", "text": "interject" } ],
                    "_meta": { "steering": { "idleBehavior": "promptRequired" } }
                });
                let resp = connection
                    .send_request(UntypedMessage::new("_session/steering", steer_params)?)
                    .block_task()
                    .await?;
                assert_eq!(
                    resp,
                    json!({ "outcome": "injected" }),
                    "a turn in flight must inject (now priority)"
                );

                // Let the test wind down; drop the in-flight prompt task.
                drop(prompt_task);
                Ok::<(), agent_client_protocol::Error>(())
            },
        )
        .await;
    assert!(result.is_ok(), "steering drive resolves");
    drop(result);
    let _ = timeout(Duration::from_secs(5), serve_task).await;

    let _ = std::fs::remove_dir_all(&tmp);
}

/// 8.T4 — nothing but JSON-RPC frames ever reaches stdout: a stray `println!`
/// on the binary's stdout would surface as a non-JSON line. Spawn the real
/// binary over stdio, send an `initialize` request, close stdin, and assert
/// every non-empty stdout line parses as a JSON-RPC frame.
#[test]
fn nothing_but_jsonrpc_on_stdout() {
    let bin = rust_agent_binary();
    assert!(bin.exists(), "rust agent binary missing: {}", bin.display());

    let mut child = std::process::Command::new(&bin)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn the binary");

    let mut stdin = child.stdin.take().expect("take stdin");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":1}}}}"#
    )
    .expect("write initialize");
    drop(stdin);

    let output = child.wait_with_output().expect("wait for the binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(!lines.is_empty(), "the binary produced no stdout at all");
    for (i, line) in lines.drain(..).enumerate() {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("stdout line {i} is not JSON: {line:?}: {e}"));
        assert!(
            v.get("jsonrpc").is_some(),
            "stdout line {i} is not a JSON-RPC frame: {line}"
        );
    }
}
