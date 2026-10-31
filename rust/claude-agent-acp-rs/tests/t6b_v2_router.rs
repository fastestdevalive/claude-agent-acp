//! T6B STEP 1-3 — the v2 agent surface served over the real in-process channel
//! (M-106, M-108..M-112, M-117, M-118; upstream `src/tests/acp-v2.test.ts`).
//!
//! The connection negotiates ACP v2 in `initialize` (the opt-in comes from the
//! startup env snapshot), and every later request is served by the v2 router:
//! the v1 handler answers, and its answer is translated to the v2 shape. The
//! CLI is the local `fake-claude`; nothing leaves the machine.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Agent, Channel, Client, ConnectionTo, Error, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::session_store::{project_slug, projects_root_with_env};
use claude_agent_acp_rs::v2::EXPERIMENTAL_V2_ENV;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

fn temp_dir(tag: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-t6b-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    tmp
}

/// A transcript whose `initialize` handshake lists one slash command, so the
/// setup response has `availableCommands`.
fn transcript(tmp: &Path) -> PathBuf {
    let path = tmp.join("init.transcript.jsonl");
    let init = json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ",
            "response": {
                "models": [{"value": "claude-sonnet-4-5", "displayName": "Claude Sonnet 4.5", "description": "d"}],
                "commands": [
                    {"name": "review", "description": "Review a change", "argumentHint": "<pull request>"}
                ],
                "account": {},
            }
        }}]
    });
    let mode = json!({
        "expect": {"type": "control_request", "request": {"subtype": "set_permission_mode", "mode": "plan"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ", "response": {}
        }}]
    });
    std::fs::write(&path, format!("{init}\n{mode}\n")).expect("write transcript");
    path
}

fn options(tmp: &Path) -> ServeOptions {
    let config_dir = tmp.join("claude");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                transcript(tmp).to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                config_dir.to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(repo_root()),
        env: EnvSnapshot::from_pairs([
            (EXPERIMENTAL_V2_ENV.to_string(), "1".to_string()),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                config_dir.to_string_lossy().into_owned(),
            ),
        ]),
        ..Default::default()
    }
}

/// What the client saw: every `session/update` notification, in order.
struct Seen {
    updates: Vec<Value>,
    notifications: Vec<String>,
}

/// Runs `body` on a fresh v2 connection; returns the client's collected
/// notifications.
async fn with_v2<F, Fut>(tmp: &Path, body: F) -> Seen
where
    F: FnOnce(ConnectionTo<Agent>, mpsc::UnboundedSender<(String, Value)>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), Error>> + Send,
{
    let (agent_channel, client_channel) = Channel::duplex();
    let opts = options(tmp);
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, Value)>();
    let body_tx = tx.clone();
    let collector = tokio::spawn(async move {
        let mut seen = Seen {
            updates: Vec::new(),
            notifications: Vec::new(),
        };
        while let Some((method, params)) = rx.recv().await {
            seen.notifications.push(method.clone());
            if method == "session/update" {
                seen.updates.push(params);
            }
        }
        seen
    });
    let result =
        timeout(
            Duration::from_secs(90),
            Client
                .builder()
                .on_receive_notification(
                    async move |notification: UntypedMessage, _cx| {
                        let _ = tx.send((
                            notification.method().to_string(),
                            notification.params().clone(),
                        ));
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(
                    client_channel,
                    move |connection: ConnectionTo<Agent>| async move {
                        body(connection, body_tx).await
                    },
                ),
        )
        .await;
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    result
        .expect("the exchange finished in time")
        .expect("exchange");
    collector.await.expect("collector")
}

async fn call(
    connection: &ConnectionTo<Agent>,
    method: &str,
    params: Value,
) -> Result<Value, Error> {
    connection
        .send_request(UntypedMessage::new(method, params)?)
        .block_task()
        .await
}

fn code(error: &Error) -> i64 {
    serde_json::to_value(error).expect("error")["code"]
        .as_i64()
        .unwrap_or_default()
}

async fn initialize(connection: &ConnectionTo<Agent>) -> Value {
    call(
        connection,
        "initialize",
        json!({
            "protocolVersion": 2,
            "info": {"name": "v2-test-client", "version": "1.0.0"},
            "capabilities": {"auth": {"terminal": {}}},
        }),
    )
    .await
    .expect("v2 initialize")
}

fn cwd() -> String {
    repo_root().to_string_lossy().into_owned()
}

/// A seeded source transcript for fork/replay, in the projects directory of
/// `config_dir`.
fn seed_session(config_dir: &Path, cwd: &Path, session_id: &str) {
    let env = EnvSnapshot::from_pairs([(
        "CLAUDE_CONFIG_DIR".to_string(),
        config_dir.to_string_lossy().into_owned(),
    )]);
    let dir = projects_root_with_env(&env).join(project_slug(&cwd.to_string_lossy()));
    std::fs::create_dir_all(&dir).expect("project dir");
    let body = [
        json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":session_id,
            "timestamp":"2026-01-01T00:00:00.000Z",
            "message":{"role":"user","content":"one"}})
        .to_string(),
        json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":session_id,
            "timestamp":"2026-01-01T00:00:01.000Z",
            "message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"first"}]}})
        .to_string(),
    ]
    .join("\n")
        + "\n";
    std::fs::write(dir.join(format!("{session_id}.jsonl")), body).expect("seed transcript");
}

/// M-106/M-108 — after a v2 `initialize` the connection is served by the v2
/// agent: `session/new` answers the v2 shape (no `modes`, `configId`, the
/// commands in the response) and no `available_commands_update` follows.
#[tokio::test]
async fn m106_m108_v2_session_new_answers_v2_shape() {
    let tmp = temp_dir("new");
    let seen = with_v2(&tmp, move |connection, _tx| async move {
        let init = initialize(&connection).await;
        assert_eq!(init["protocolVersion"], 2);
        let response = call(
            &connection,
            "session/new",
            json!({
                "cwd": cwd(),
                "mcpServers": [
                    {"type": "stdio", "name": "files", "command": "/usr/local/bin/mcp-files"},
                    {"type": "http", "name": "linear", "url": "https://mcp.linear.app/mcp"},
                ],
            }),
        )
        .await?;
        let session_id = response["sessionId"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(!session_id.is_empty(), "a session id: {response}");
        assert!(
            response.get("modes").is_none(),
            "v2 drops modes: {response}"
        );
        let options = response["configOptions"]
            .as_array()
            .expect("config options");
        let ids: Vec<&str> = options
            .iter()
            .filter_map(|option| option["configId"].as_str())
            .collect();
        assert!(ids.contains(&"mode"), "the mode option: {ids:?}");
        for option in options {
            assert!(option.get("id").is_none(), "v2 has no id: {option}");
        }
        assert_eq!(options[0]["category"], "mode");
        assert_eq!(options[0]["type"], "select");
        let commands = response["availableCommands"]
            .as_array()
            .expect("availableCommands");
        assert!(
            commands.iter().any(|command| command["name"] == "mcp"),
            "the adapter's own /mcp is listed: {commands:?}"
        );
        for command in commands {
            assert!(command["input"].is_null() || command["input"]["type"] == "text");
        }
        Ok(())
    })
    .await;

    assert!(
        !seen.notifications.iter().any(|method| {
            method == "session/update"
                && seen
                    .updates
                    .iter()
                    .any(|update| update["update"]["sessionUpdate"] == "available_commands_update")
        }),
        "a v2 setup response carries the commands, so no update repeats them: {:?}",
        seen.updates
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-111 — `session/list`, `session/close` and `session/delete` answer the v1
/// shapes, which v2 keeps (`v2/agent.ts:84,114-120`).
#[tokio::test]
async fn m111_v2_list_close_delete() {
    let tmp = temp_dir("list");
    let seen = with_v2(&tmp, move |connection, _tx| async move {
        initialize(&connection).await;
        let list = call(&connection, "session/list", json!({})).await?;
        assert!(list["sessions"].is_array(), "a sessions list: {list}");

        let created = call(&connection, "session/new", json!({"cwd": cwd()})).await?;
        let session_id = created["sessionId"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        let closed = call(
            &connection,
            "session/close",
            json!({"sessionId": session_id}),
        )
        .await?;
        assert_eq!(closed, json!({}));

        let deleted = call(
            &connection,
            "session/delete",
            json!({"sessionId": session_id}),
        )
        .await?;
        assert_eq!(deleted, json!({}));
        Ok(())
    })
    .await;
    let _ = seen;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-109 — a v2 `session/resume` without `replayFrom` resumes, answers the v2
/// config options, and sends the foreground-state snapshot before the response.
#[tokio::test]
async fn m109_v2_resume_sends_state_snapshot() {
    let tmp = temp_dir("resume");
    let seen = with_v2(&tmp, move |connection, _tx| async move {
        initialize(&connection).await;
        let created = call(&connection, "session/new", json!({"cwd": cwd()})).await?;
        let session_id = created["sessionId"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let resumed = call(
            &connection,
            "session/resume",
            json!({"sessionId": session_id, "cwd": cwd()}),
        )
        .await?;
        assert!(resumed.get("modes").is_none(), "v2 drops modes: {resumed}");
        assert_eq!(resumed["configOptions"][0]["configId"], "mode");
        assert!(resumed["availableCommands"].is_array());
        Ok(())
    })
    .await;
    // The snapshot is the last update the client got, and it came before the
    // response (the response is awaited after it).
    let state = seen
        .updates
        .iter()
        .rev()
        .find(|update| update["update"]["sessionUpdate"] == "state_update");
    let state = state.expect("a state_update snapshot");
    assert_eq!(state["update"]["state"], "idle");
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-109 — v1 replays only from the start: another cursor is `invalidParams`.
#[tokio::test]
async fn m109_v2_resume_replay_from_other_cursor_is_invalid_params() {
    let tmp = temp_dir("cursor");
    with_v2(&tmp, move |connection, _tx| async move {
        initialize(&connection).await;
        let created = call(&connection, "session/new", json!({"cwd": cwd()})).await?;
        let session_id = created["sessionId"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let error = call(
            &connection,
            "session/resume",
            json!({
                "sessionId": session_id,
                "cwd": cwd(),
                "replayFrom": {"type": "_message", "messageId": "m"},
            }),
        )
        .await
        .expect_err("must reject");
        assert_eq!(code(&error), -32602);
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-109 — `replayFrom: {type: "start"}` replays the history through
/// `session/load` before the response.
#[tokio::test]
async fn m109_v2_resume_replay_start_replays_history() {
    let tmp = temp_dir("replay");
    let config_dir = tmp.join("claude");
    let source = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
    seed_session(&config_dir, &repo_root(), source);
    let seen = with_v2(&tmp, move |connection, _tx| async move {
        initialize(&connection).await;
        let replayed = call(
            &connection,
            "session/resume",
            json!({
                "sessionId": source,
                "cwd": cwd(),
                "replayFrom": {"type": "start"},
            }),
        )
        .await?;
        assert!(
            replayed.get("modes").is_none(),
            "v2 drops modes: {replayed}"
        );
        assert_eq!(replayed["configOptions"][0]["configId"], "mode");
        assert!(replayed["availableCommands"].is_array());
        Ok(())
    })
    .await;
    // The replayed history arrived, and the state snapshot is after it.
    assert!(
        seen.updates
            .iter()
            .any(|update| update["update"]["sessionUpdate"] == "user_message_chunk"),
        "the history replayed: {:?}",
        seen.updates
    );
    assert_eq!(
        seen.updates.last().expect("an update")["update"]["sessionUpdate"],
        "state_update"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-110 — a v2 `session/fork` answers a promptable session with its config
/// options and commands (`v2/agent.ts:103-113`).
#[tokio::test]
async fn m110_v2_fork_returns_promptable_session() {
    let tmp = temp_dir("fork");
    let config_dir = tmp.join("claude");
    let source = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
    seed_session(&config_dir, &repo_root(), source);
    with_v2(&tmp, move |connection, _tx| async move {
        initialize(&connection).await;
        let fork = call(
            &connection,
            "session/fork",
            json!({
                "sessionId": source,
                "cwd": cwd(),
                "mcpServers": [{"type": "http", "name": "linear", "url": "https://mcp.linear.app/mcp"}],
            }),
        )
        .await?;
        let fork_id = fork["sessionId"].as_str().unwrap_or_default();
        assert!(!fork_id.is_empty(), "a fork id: {fork}");
        assert_ne!(fork_id, source);
        assert!(fork.get("modes").is_none(), "v2 drops modes: {fork}");
        let ids: Vec<&str> = fork["configOptions"]
            .as_array()
            .expect("config options")
            .iter()
            .filter_map(|option| option["configId"].as_str())
            .collect();
        assert!(ids.contains(&"mode"), "the mode option: {ids:?}");
        assert!(fork["availableCommands"].is_array());
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-112 — `session/set_config_option` takes a v2 value and answers v2 config
/// options; an unsupported value type is `invalidParams` (`v2/session.ts:105-124`).
#[tokio::test]
async fn m112_v2_set_config_option() {
    let tmp = temp_dir("config");
    with_v2(&tmp, move |connection, _tx| async move {
        initialize(&connection).await;
        let created = call(&connection, "session/new", json!({"cwd": cwd()})).await?;
        let session_id = created["sessionId"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let response = call(
            &connection,
            "session/set_config_option",
            json!({
                "sessionId": session_id,
                "configId": "mode",
                "type": "id",
                "value": "plan",
            }),
        )
        .await?;
        let options = response["configOptions"]
            .as_array()
            .expect("config options");
        assert_eq!(options[0]["configId"], "mode");
        assert_eq!(options[0]["currentValue"], "plan");
        assert!(options[0].get("id").is_none(), "v2 has no id: {options:?}");

        let error = call(
            &connection,
            "session/set_config_option",
            json!({
                "sessionId": session_id,
                "configId": "mode",
                "type": "_custom",
                "value": 1,
            }),
        )
        .await
        .expect_err("must reject");
        assert_eq!(code(&error), -32602);
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-117 — `session/cancel` is a notification on the v2 connection; an idle
/// session answers nothing (`v2/agent.ts:132`).
#[tokio::test]
async fn m117_v2_cancel_is_a_notification() {
    let tmp = temp_dir("cancel");
    with_v2(&tmp, move |connection, _tx| async move {
        initialize(&connection).await;
        let created = call(&connection, "session/new", json!({"cwd": cwd()})).await?;
        let session_id = created["sessionId"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        connection.send_notification(UntypedMessage::new(
            "session/cancel",
            json!({"sessionId": session_id}),
        )?)?;
        // The connection is still serving: a later request answers.
        let list = call(&connection, "session/list", json!({})).await?;
        assert!(list["sessions"].is_array());
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-118/M-106 — the router serves the v2 methods only: a v1-only method and an
/// unknown one are method-not-found, and a non-`_` extension method is rejected
/// (`v2/agent.ts:245-250`).
#[tokio::test]
async fn m118_v2_router_rejects_methods_it_does_not_serve() {
    let tmp = temp_dir("methods");
    with_v2(&tmp, move |connection, _tx| async move {
        initialize(&connection).await;
        for method in ["session/set_mode", "_unknown", "no/such_method"] {
            let error = call(&connection, method, json!({}))
                .await
                .expect_err("must reject");
            assert_eq!(code(&error), -32601, "{method}: {error:?}");
        }
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-106/O-139 — without the opt-in the same client gets the v1 surface: a v2
/// `session/new` is not translated and the v1 answer carries `modes`.
#[tokio::test]
async fn m106_without_opt_in_the_connection_stays_v1() {
    let tmp = temp_dir("v1");
    let config_dir = tmp.join("claude");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                transcript(&tmp).to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                config_dir.to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(repo_root()),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let response = timeout(
        Duration::from_secs(60),
        Client.builder().connect_with(
            client_channel,
            move |connection: ConnectionTo<Agent>| async move {
                let init = call(
                    &connection,
                    "initialize",
                    json!({
                        "protocolVersion": 1,
                        "clientCapabilities": {"auth": {"terminal": true}},
                    }),
                )
                .await?;
                assert_eq!(init["protocolVersion"], 1);
                call(
                    &connection,
                    "session/new",
                    json!({"cwd": cwd(), "mcpServers": []}),
                )
                .await
            },
        ),
    )
    .await
    .expect("in time")
    .expect("exchange");
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    assert!(
        response.get("modes").is_some(),
        "a v1 client still gets modes: {response}"
    );
    assert!(response.get("availableCommands").is_none());
    let _ = std::fs::remove_dir_all(&tmp);
}
