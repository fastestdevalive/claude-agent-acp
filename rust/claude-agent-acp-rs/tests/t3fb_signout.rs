//! Turn 3 fix round, lane B (V-16 / B-91): every entry point that starts a turn
//! recreates a signed-out query first, and the per-turn `--hide-claude-auth`
//! guard runs for a goal turn too. Hermetic duplex connection, `fake-claude`
//! segments, every wait bounded.

mod common;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Agent, Channel, Client, ConnectionTo, Error, UntypedMessage};
use claude_agent_acp_rs::agent::{serve_hiding_claude_auth, ServeOptions};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

fn temp_dir(tag: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-t3fb-signout-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    tmp
}

fn init_step(account: &Value) -> String {
    json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ",
            "response": {
                "models": [{"value": "claude-sonnet-4-5", "displayName": "S", "description": "d"}],
                "commands": [], "account": account,
            }
        }}]
    })
    .to_string()
}

/// The first turn signs out: the CLI answers with the login error.
fn auth_fail_turn() -> String {
    json!({
        "expect": {"type": "user"},
        "emit": [
            {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
             "message": {"role": "user", "content": [{"type": "text", "text": "needs auth"}]}},
            {"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "Please sign in to proceed."}]}},
            {"type": "result", "subtype": "success", "stop_reason": "error", "is_error": true,
             "result": "Please run /login to continue", "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}
        ]
    })
    .to_string()
}

/// A turn that answers: the recreated query runs the steered or goal turn.
fn answering_turn(text: &str) -> String {
    json!({
        "expect": {"type": "user"},
        "emit": [
            {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
             "message": {"role": "user", "content": [{"type": "text", "text": text}]}},
            {"type": "assistant", "message": {"role": "assistant",
                "content": [{"type": "text", "text": "on it"}]}},
            {"type": "result", "subtype": "success", "stop_reason": "end_turn",
             "is_error": false, "result": "on it",
             "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}
        ]
    })
    .to_string()
}

/// Two segments: the signing-out turn, then the query the next turn recreates.
fn transcript(tmp: &Path, second_account: &Value, second_turn: &str) -> PathBuf {
    let path = tmp.join("signout.transcript.jsonl");
    // The CLI's own status matches the session account, so no probe flags the
    // session: only the sign-out turn does.
    let text = format!(
        "{}\n{}\n{{\"segment\":1}}\n{}\n{}\n{}\n",
        init_step(&json!({"apiKeySource": "ANTHROPIC_API_KEY"})),
        auth_fail_turn(),
        init_step(second_account),
        second_turn,
        json!({"auth_status_stdout": {"loggedIn": true, "apiProvider": "firstParty",
            "apiKeySource": "ANTHROPIC_API_KEY"}})
    );
    std::fs::write(&path, text).expect("write transcript");
    path
}

fn options(tmp: &Path, script: PathBuf) -> ServeOptions {
    std::fs::create_dir_all(tmp.join("config")).expect("config dir");
    ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                script.to_string_lossy().into_owned(),
            ),
            (
                "FAKE_CLAUDE_CURSOR".to_string(),
                tmp.join("cursor").to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                tmp.join("config").to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(repo_root()),
        env: EnvSnapshot::empty(),
        ..Default::default()
    }
}

/// Runs `body` against a served agent that hides the claude.ai login;
/// `updates` receives every `session/update`.
async fn with_agent<F, Fut>(opts: ServeOptions, body: F)
where
    F: FnOnce(ConnectionTo<Agent>, mpsc::UnboundedReceiver<Value>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), Error>> + Send,
{
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task =
        tokio::spawn(async move { serve_hiding_claude_auth(agent_channel, opts).await });
    let (update_tx, update_rx) = mpsc::unbounded_channel::<Value>();
    let result =
        timeout(
            Duration::from_secs(90),
            Client
                .builder()
                .on_receive_notification(
                    async move |notification: UntypedMessage, _cx| {
                        if notification.method() == "session/update" {
                            let _ = update_tx.send(notification.params().clone());
                        }
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(
                    client_channel,
                    move |connection: ConnectionTo<Agent>| async move {
                        body(connection, update_rx).await
                    },
                ),
        )
        .await;
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    result
        .expect("the exchange finished in time")
        .expect("the exchange succeeded");
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

fn wire(error: &Error) -> Value {
    serde_json::to_value(error).expect("error")
}

fn new_session(cwd: &str) -> Value {
    json!({"cwd": cwd, "mcpServers": []})
}

fn prompt_params(session_id: &str) -> Value {
    json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "needs auth"}]})
}

/// Signs the session out: the first turn fails with `authRequired`, which marks
/// the query for a recreate and closes it.
async fn sign_out(connection: &ConnectionTo<Agent>, session_id: &str) -> Result<(), Error> {
    let error = call(connection, "session/prompt", prompt_params(session_id))
        .await
        .unwrap_err();
    assert_eq!(code(&error), -32000, "{error:?}");
    // The turn's own sign-out, not a creation guard refusal.
    assert_eq!(
        wire(&error)["message"],
        "Authentication required",
        "{error:?}"
    );
    Ok(())
}

/// A steer after a sign-out recreates the query first; the recreation's creation
/// guard refuses the subscription account exactly like a prompt does.
#[tokio::test]
async fn t3fb_a_steer_after_a_sign_out_recreates_the_query() {
    let tmp = temp_dir("steer");
    let script = transcript(
        &tmp,
        &json!({"subscriptionType": "pro"}),
        &answering_turn("x"),
    );
    with_agent(options(&tmp, script), move |connection, _updates| async move {
        call(&connection, "initialize", json!({"protocolVersion": 1})).await?;
        let cwd = repo_root().to_string_lossy().into_owned();
        let created = call(&connection, "session/new", new_session(&cwd)).await?;
        let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
        sign_out(&connection, &session_id).await?;
        // The steer must not run on the stale CLI: it recreates, and the new
        // account is a subscription.
        let error = call(
            &connection,
            "_session/steering",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "x"}]}),
        )
        .await
        .unwrap_err();
        let wire = wire(&error);
        assert_eq!(wire["code"], -32000, "{wire}");
        assert_eq!(
            wire["message"],
            "Authentication required: This integration does not support using claude.ai subscriptions."
        );
        assert_eq!(wire["data"], json!({"reason": "claude_subscription_not_supported"}));
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// A goal after a sign-out recreates the query first and runs the per-turn
/// guard; the refusal reaches the client instead of a detached turn.
#[tokio::test]
async fn t3fb_a_goal_after_a_sign_out_recreates_the_query() {
    let tmp = temp_dir("goal");
    let script = transcript(
        &tmp,
        &json!({"subscriptionType": "pro"}),
        &answering_turn("x"),
    );
    with_agent(
        options(&tmp, script),
        move |connection, _updates| async move {
            call(&connection, "initialize", json!({"protocolVersion": 1})).await?;
            let cwd = repo_root().to_string_lossy().into_owned();
            let created = call(&connection, "session/new", new_session(&cwd)).await?;
            let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
            sign_out(&connection, &session_id).await?;
            let error = call(
                &connection,
                "_session/goal",
                json!({"sessionId": session_id, "action": "set", "objective": "ship it"}),
            )
            .await
            .unwrap_err();
            let wire = wire(&error);
            assert_eq!(wire["code"], -32000, "{wire}");
            assert_eq!(
                wire["data"],
                json!({"reason": "claude_subscription_not_supported"})
            );
            Ok(())
        },
    )
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// The fail-open side: a goal whose recreate is accepted runs its turn on the
/// fresh query (not refused, not on the stale CLI) and answers `{}` once it
/// settled.
#[tokio::test]
async fn t3fb_a_goal_after_a_sign_out_runs_on_the_new_query() {
    let tmp = temp_dir("accepted");
    let script = transcript(
        &tmp,
        &json!({"apiKeySource": "ANTHROPIC_API_KEY"}),
        &answering_turn("/goal ship it"),
    );
    with_agent(
        options(&tmp, script),
        move |connection, _updates| async move {
            call(&connection, "initialize", json!({"protocolVersion": 1})).await?;
            let cwd = repo_root().to_string_lossy().into_owned();
            let created = call(&connection, "session/new", new_session(&cwd)).await?;
            let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
            sign_out(&connection, &session_id).await?;
            let answered = call(
                &connection,
                "_session/goal",
                json!({"sessionId": session_id, "action": "set", "objective": "ship it"}),
            )
            .await?;
            assert_eq!(answered, json!({}), "{answered}");
            Ok(())
        },
    )
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// G4 / F34 — `acp-agent-settings.test.ts` > "keeps the live mode when a
/// sign-out respawn starts a fresh conversation": the recreated child's argv
/// carries the session's live mode, not the default.
#[tokio::test]
async fn g4_f34_a_sign_out_respawn_keeps_the_live_mode() {
    let tmp = temp_dir("live-mode");
    let argv_out = tmp.join("argv.json");
    let set_mode_step = json!({
        "expect": {"type": "control_request", "request": {"subtype": "set_permission_mode"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ", "response": {}
        }}]
    })
    .to_string();
    let script = tmp.join("signout-mode.transcript.jsonl");
    std::fs::write(
        &script,
        format!(
            "{}\n{}\n{}\n{{\"segment\":1}}\n{}\n{}\n{}\n",
            init_step(&json!({"apiKeySource": "ANTHROPIC_API_KEY"})),
            set_mode_step,
            auth_fail_turn(),
            init_step(&json!({"subscriptionType": "pro"})),
            answering_turn("x"),
            json!({"auth_status_stdout": {"loggedIn": true, "apiProvider": "firstParty",
                "apiKeySource": "ANTHROPIC_API_KEY"}})
        ),
    )
    .expect("write transcript");
    let mut opts = options(&tmp, script);
    opts.extra_env.push((
        "FAKE_CLAUDE_ARGV_OUT".to_string(),
        argv_out.to_string_lossy().into_owned(),
    ));
    with_agent(opts, move |connection, _updates| async move {
        call(&connection, "initialize", json!({"protocolVersion": 1})).await?;
        let cwd = repo_root().to_string_lossy().into_owned();
        let created = call(&connection, "session/new", new_session(&cwd)).await?;
        let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
        call(
            &connection,
            "session/set_mode",
            json!({"sessionId": session_id, "modeId": "plan"}),
        )
        .await?;
        sign_out(&connection, &session_id).await?;
        // The steer recreates the query (the live mode must follow) and then the
        // creation guard refuses the subscription account.
        let _ = call(
            &connection,
            "_session/steering",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "x"}]}),
        )
        .await;
        Ok::<(), Error>(())
    })
    .await;
    let recorded: Value =
        serde_json::from_str(&std::fs::read_to_string(&argv_out).expect("the argv record"))
            .expect("valid argv json");
    let argv = recorded["argv"].as_array().expect("an argv array");
    assert!(
        argv.iter()
            .any(|arg| arg.as_str() == Some("--permission-mode=plan")),
        "the recreated child keeps the live mode: {argv:?}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
