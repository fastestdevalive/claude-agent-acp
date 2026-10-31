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

/// 8.T5 / rust-parity-fixes 1.3 — `session/new` spawns Node's argv: the mode
/// comes from the user settings tier (`acceptEdits`), `_meta.claudeCode.options.
/// permissionMode` is IGNORED (X3), `model`/`betas`/`extraArgs`/`env` map to
/// tokens and allow-listed env keys, and the returned `sessionId` equals the
/// `--session-id=<uuid>`.
#[tokio::test]
async fn inv_session_new_model_permission_and_uuid() {
    let root = repo_root();
    let tmp = std::env::temp_dir().join(format!("acp-agent-t5-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    // Hermetic settings tier (.claude/settings.local.json under cwd), so the
    // host's real ~/.claude/settings.json cannot decide this test's argv,
    // avoiding process-wide std::env::set_var("CLAUDE_CONFIG_DIR") that affects
    // parallel tests.
    let cwd = tmp.join("cwd");
    let claude_dir = cwd.join(".claude");
    std::fs::create_dir_all(&claude_dir).expect("create cwd/.claude");
    std::fs::write(
        claude_dir.join("settings.local.json"),
        r#"{"permissions":{"defaultMode":"acceptEdits"}}"#,
    )
    .expect("write settings");
    let transcript = init_only_transcript(&tmp);
    let argv_out = tmp.join("argv.json");

    let opts = fake_opts(&root, &transcript, &argv_out);
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let cwd_for_client = cwd.clone();
    let result = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| {
                let cwd_for_client = cwd_for_client.clone();
                async move {
                    let params = json!({
                        "cwd": cwd_for_client.to_string_lossy(),
                        "_meta": {
                            "claudeCode": {
                                "options": {
                                    "model": "claude-sonnet-4-5",
                                    // Ignored by Node and Rust (X3).
                                    "permissionMode": "plan",
                                    "betas": ["context-1m-2025-08-07"],
                                    "extraArgs": {"chrome": null},
                                    "env": {"CLAUDE_CODE_ARTIFACT": "1"},
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
        argv.iter().any(|a| a == "--model=claude-sonnet-4-5"),
        "model -> --model=claude-sonnet-4-5 in argv: {argv:?}"
    );
    assert!(
        argv.iter().any(|a| a == "--permission-mode=acceptEdits"),
        "settings defaultMode -> --permission-mode=acceptEdits: {argv:?}"
    );
    assert!(
        !argv.iter().any(|a| a == "--permission-mode=plan"),
        "_meta permissionMode is ignored (X3): {argv:?}"
    );
    assert!(
        argv.iter().any(|a| a == "--betas=context-1m-2025-08-07"),
        "betas -> --betas=a,b: {argv:?}"
    );
    assert!(
        argv.iter().any(|a| a == "--chrome"),
        "extraArgs null -> a bare --chrome: {argv:?}"
    );
    assert!(
        argv.iter()
            .any(|a| a == "--disallowedTools=AskUserQuestion"),
        "AskUserQuestion is always disallowed (Decision 4): {argv:?}"
    );
    let session_arg = argv.iter().find_map(|a| a.strip_prefix("--session-id="));
    assert_eq!(
        session_arg,
        Some(session_id.as_str()),
        "the returned sessionId equals the --session-id=<uuid>: {argv:?}"
    );
    // Env is asserted by KEY, never by value.
    let record = read_json(&argv_out);
    assert!(
        record["env"].get("CLAUDE_CODE_ARTIFACT").is_some(),
        "options.env key reaches claude: {:?}",
        record["env"]
            .as_object()
            .map(|m| m.keys().collect::<Vec<_>>())
    );

    std::env::remove_var("CLAUDE_CONFIG_DIR");
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
        ..Default::default()
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

/// Phase 14 — `session/new` with a relative `cwd` and with a nonexistent `cwd`
/// returns `invalidParams` (-32602) on the wire with the adapter's message and
/// `data:{cwd}`, before any spawn happens (issue #749).
#[tokio::test]
async fn inv_cwd_validation_on_session_new_wire() {
    let opts = ServeOptions::default();
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let missing = std::env::temp_dir().join(format!("cwd-missing-{}", std::process::id()));
    let missing_str = missing.to_string_lossy().into_owned();
    let result = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                // Relative cwd -> invalidParams with the absolute-path message.
                let resp = connection
                    .send_request(UntypedMessage::new(
                        "session/new",
                        json!({ "cwd": "relative/dir" }),
                    )?)
                    .block_task()
                    .await;
                let err = resp.expect_err("relative cwd must be rejected");
                let v = serde_json::to_value(&err).unwrap();
                assert_eq!(v["code"], -32602);
                // F2c / M-01: `RequestError.invalidParams` prefixes the text.
                assert_eq!(
                    v["message"],
                    "Invalid params: `cwd` must be an absolute path, but received: relative/dir"
                );
                assert_eq!(v["data"]["cwd"], "relative/dir");

                // Nonexistent absolute cwd -> invalidParams with the not-exist message.
                let resp = connection
                    .send_request(UntypedMessage::new(
                        "session/new",
                        json!({ "cwd": missing_str }),
                    )?)
                    .block_task()
                    .await;
                let err = resp.expect_err("missing cwd must be rejected");
                let v = serde_json::to_value(&err).unwrap();
                assert_eq!(v["code"], -32602);
                assert!(v["message"].as_str().unwrap().contains("does not exist"));
                assert_eq!(v["data"]["cwd"], missing_str);
                Ok::<(), agent_client_protocol::Error>(())
            },
        )
        .await;
    assert!(result.is_ok(), "cwd-validation drive resolves");
    drop(result);
    let _ = timeout(Duration::from_secs(5), serve_task).await;
}

/// 8.T4 — nothing but JSON-RPC frames ever reaches stdout: a stray `println!`
/// on the binary's stdout would surface as a non-JSON line. Spawn the real
/// binary over stdio, send an `initialize` request, close stdin, and assert
/// every non-empty stdout line parses as a JSON-RPC frame.
#[test]
fn nothing_but_jsonrpc_on_stdout() {
    let bin = rust_agent_binary();
    assert!(bin.exists(), "rust agent binary missing: {}", bin.display());

    let mut cmd = std::process::Command::new(&bin);
    cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    if let Some(tmpdir) = std::env::var_os("TMPDIR") {
        cmd.env("TMPDIR", tmpdir);
    }
    if let Some(lang) = std::env::var_os("LANG") {
        cmd.env("LANG", lang);
    }
    for (k, v) in std::env::vars() {
        if k.starts_with("LC_") {
            cmd.env(k, v);
        }
    }
    #[cfg(windows)]
    for key in ["SystemRoot", "SYSTEMROOT", "COMSPEC", "PATHEXT"] {
        if let Some(val) = std::env::var_os(key) {
            cmd.env(key, val);
        }
    }

    let mut child = cmd
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

/// rust-parity-fixes Decision 10 (P6) & Item 2.5/2.6:
/// - session/new returns {sessionId, modes, configOptions} without models
/// - session/set_mode and session/set_config_option succeed and reject invalid values
/// - session/set_model returns method not found (-32601)
/// - session/resume on live session reuses existing session without leak and reports live mode
#[tokio::test]
async fn inv_63_set_mode_and_config_option() {
    let tmp = std::env::temp_dir().join(format!("acp-agent-inv63-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let cwd = tmp.join("cwd");
    let claude_dir = cwd.join(".claude");
    let config_dir = tmp.join("config");
    std::fs::create_dir_all(&claude_dir).expect("create cwd/.claude");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::write(
        claude_dir.join("settings.json"),
        r#"{"model":"claude-sonnet-4-5","effortLevel":"default"}"#,
    )
    .expect("write settings");
    let transcript = tmp.join("config.transcript.jsonl");
    std::fs::write(
        &transcript,
        "{\"expect\":{\"type\":\"control_request\",\"request\":{\"subtype\":\"initialize\"}},\"emit\":[{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{\"models\":[{\"value\":\"claude-sonnet-4-5\"}],\"commands\":[],\"account\":{}}}}]}\n\
{\"expect\":{\"type\":\"control_request\",\"request\":{\"subtype\":\"set_permission_mode\",\"mode\":\"acceptEdits\"}},\"emit\":[{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{}}}]}\n\
{\"expect\":{\"type\":\"control_request\",\"request\":{\"subtype\":\"set_permission_mode\",\"mode\":\"plan\"}},\"emit\":[{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$REQ\",\"response\":{}}}]}\n",
    )
    .expect("write transcript");
    let argv_out = tmp.join("argv.json");

    let opts = fake_opts(&cwd, &transcript, &argv_out).with_env(
        claude_agent_acp_rs::env_snapshot::EnvSnapshot::from_iter([(
            "CLAUDE_CONFIG_DIR",
            config_dir.to_string_lossy().to_string(),
        )]),
    );
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let cwd_for_client = cwd.clone();
    let result = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| {
                let cwd_for_client = cwd_for_client.clone();
                async move {
                    // initialize
                    let _ = connection
                        .send_request(UntypedMessage::new(
                            "initialize",
                            json!({ "protocolVersion": 1 }),
                        )?)
                        .block_task()
                        .await?;

                    // session/new
                    let created = connection
                        .send_request(UntypedMessage::new(
                            "session/new",
                            json!({ "cwd": cwd_for_client.to_string_lossy() }),
                        )?)
                        .block_task()
                        .await?;

                    let session_id = created["sessionId"].as_str().unwrap().to_string();
                    assert!(
                        created.get("models").is_none(),
                        "session/new response has no models"
                    );
                    assert!(
                        created.get("modes").is_some(),
                        "session/new response has modes"
                    );
                    assert!(
                        created.get("configOptions").is_some(),
                        "session/new response has configOptions"
                    );
                    assert_eq!(created["modes"]["currentModeId"], "default");

                    // session/set_mode with valid mode
                    let set_mode_res = connection
                        .send_request(UntypedMessage::new(
                            "session/set_mode",
                            json!({ "sessionId": session_id, "modeId": "acceptEdits" }),
                        )?)
                        .block_task()
                        .await?;
                    assert_eq!(set_mode_res, json!({}));

                    // session/set_mode with invalid mode -> -32603 "Invalid Mode"
                    let err = connection
                        .send_request(UntypedMessage::new(
                            "session/set_mode",
                            json!({ "sessionId": session_id, "modeId": "unknown_mode" }),
                        )?)
                        .block_task()
                        .await
                        .expect_err("invalid mode must fail");
                    assert_eq!(i32::from(err.code), -32603);
                    assert_eq!(err.data.as_ref().unwrap()["details"], "Invalid Mode");

                    // session/set_config_option for mode
                    let set_cfg_mode = connection
                        .send_request(UntypedMessage::new(
                            "session/set_config_option",
                            json!({ "sessionId": session_id, "configId": "mode", "value": "plan" }),
                        )?)
                        .block_task()
                        .await?;
                    let opts = set_cfg_mode["configOptions"].as_array().unwrap();
                    let mode_opt = opts.iter().find(|o| o["id"] == "mode").unwrap();
                    assert_eq!(mode_opt["currentValue"], "plan");

                    // session/set_config_option missing value -> the SDK's schema
                    // check: -32602 Invalid params (Node-captured, config-set-option-bad-value)
                    let err = connection
                        .send_request(UntypedMessage::new(
                            "session/set_config_option",
                            json!({ "sessionId": session_id, "configId": "mode" }),
                        )?)
                        .block_task()
                        .await
                        .expect_err("missing value must fail");
                    assert_eq!(i32::from(err.code), -32602);
                    assert_eq!(
                        err.data.as_ref().unwrap()["value"]["_errors"][1],
                        "Invalid input: expected string, received undefined"
                    );

                    // session/set_config_option with unknown config option
                    let err = connection
                        .send_request(UntypedMessage::new(
                            "session/set_config_option",
                            json!({ "sessionId": session_id, "configId": "bogus", "value": "x" }),
                        )?)
                        .block_task()
                        .await
                        .expect_err("unknown config option must fail");
                    assert_eq!(i32::from(err.code), -32603);
                    assert_eq!(
                        err.data.as_ref().unwrap()["details"],
                        "Unknown config option: bogus"
                    );

                    // session/set_model returns method not found (-32601)
                    let err = connection
                        .send_request(UntypedMessage::new(
                            "session/set_model",
                            json!({ "sessionId": session_id, "model": "haiku" }),
                        )?)
                        .block_task()
                        .await
                        .expect_err("session/set_model must be -32601");
                    assert_eq!(i32::from(err.code), -32601);

                    // session/resume on live session reuses it and returns live state (MAJOR 5)
                    let resumed = connection
                        .send_request(UntypedMessage::new(
                            "session/resume",
                            json!({ "sessionId": session_id }),
                        )?)
                        .block_task()
                        .await?;
                    assert_eq!(resumed["sessionId"], session_id);
                    assert_eq!(resumed["modes"]["currentModeId"], "plan");

                    Ok::<(), agent_client_protocol::Error>(())
                }
            },
        )
        .await;

    assert!(result.is_ok(), "result: {result:?}");
    let _ = timeout(Duration::from_secs(5), serve_task).await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// rust-air-gaps G8 (M-93, WB-14) — `_session/goal` is served (no longer
/// -32601): every `parseGoalRequest` rejection is `-32602` with Node's exact
/// message and NO `data`, and an unknown session is `-32603` with
/// `data:{details:"Session not found"}` (Node's thrown `Error` mapping).
#[tokio::test]
async fn inv_53_goal_params_wire() {
    let opts = ServeOptions::default();
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let result = Client
        .builder()
        .connect_with(
            client_channel,
            move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
                let cases = [
                    (
                        json!({ "sessionId": "s", "action": "set" }),
                        "Invalid params: goal action \"set\" requires a non-empty objective",
                    ),
                    (
                        json!({ "sessionId": "s", "action": "pause" }),
                        "Invalid params: goal action must be \"set\" or \"clear\"",
                    ),
                    (
                        json!({ "action": "clear" }),
                        "Invalid params: goal params require a non-empty sessionId",
                    ),
                ];
                for (params, message) in cases {
                    let err = connection
                        .send_request(UntypedMessage::new("_session/goal", params.clone())?)
                        .block_task()
                        .await
                        .expect_err("bad goal params must error");
                    assert_eq!(i32::from(err.code), -32602, "{params}");
                    assert_eq!(err.message, message, "{params}");
                    assert!(err.data.is_none(), "no data: {params}");
                }
                // A non-object params value (null) is the object-check message.
                let err = connection
                    .send_request(UntypedMessage::new("_session/goal", json!(null))?)
                    .block_task()
                    .await
                    .expect_err("null goal params must error");
                assert_eq!(i32::from(err.code), -32602);
                assert_eq!(err.message, "Invalid params: goal params must be an object");
                assert!(err.data.is_none());

                // A well-formed request for an unknown session: the method is
                // routed (not -32601), and the error is Node's -32603/details.
                let err = connection
                    .send_request(UntypedMessage::new(
                        "_session/goal",
                        json!({ "sessionId": "missing", "action": "clear" }),
                    )?)
                    .block_task()
                    .await
                    .expect_err("unknown session must error");
                assert_eq!(i32::from(err.code), -32603);
                assert_eq!(
                    err.data.as_ref().and_then(|d| d.get("details")),
                    Some(&json!("Session not found"))
                );
                Ok::<(), agent_client_protocol::Error>(())
            },
        )
        .await;
    assert!(result.is_ok(), "goal wire drive resolves: {result:?}");
    drop(result);
    let _ = timeout(Duration::from_secs(5), serve_task).await;
}
