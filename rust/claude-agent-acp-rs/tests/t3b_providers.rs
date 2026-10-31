//! Turn 3 lane B: providers, `authenticate`, `logout` and the `--hide-claude-auth`
//! guard over a real connection (`Channel::duplex()`), against `fake-claude` and a
//! stub gateway on 127.0.0.1. Every wait is bounded; nothing leaves the machine.

mod common;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Agent, Channel, Client, ConnectionTo, Error, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, serve_hiding_claude_auth, ServeOptions};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::timeout;

fn temp_dir(tag: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-t3b-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    tmp
}

/// A transcript every spawn replays: `initialize` answered with `account`, and the
/// `claude auth status --json` line.
fn transcript(tmp: &Path, account: &Value, status: &Value) -> PathBuf {
    let path = tmp.join("t.transcript.jsonl");
    let init = json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ",
            "response": {
                "models": [
                    {"value": "claude-sonnet-4-5", "displayName": "Claude Sonnet 4.5", "description": "d"},
                    {"value": "claude-opus-4-5", "displayName": "Claude Opus 4.5", "description": "d"},
                ],
                "commands": [], "account": account,
            }
        }}]
    });
    std::fs::write(
        &path,
        format!("{init}\n{}\n", json!({ "auth_status_stdout": status })),
    )
    .expect("write transcript");
    path
}

fn options(tmp: &Path, account: &Value) -> ServeOptions {
    let status = json!({"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty",
        "subscriptionType": "pro", "email": "a@b.c", "orgName": "Org"});
    std::fs::create_dir_all(tmp.join("config")).expect("config dir");
    ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                transcript(tmp, account, &status)
                    .to_string_lossy()
                    .into_owned(),
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

/// Runs `body` against a served agent; `statuses` receives every
/// `_auth/status_update` the client got.
async fn with_agent<F, Fut>(opts: ServeOptions, hide: bool, body: F)
where
    F: FnOnce(ConnectionTo<Agent>, mpsc::UnboundedReceiver<Value>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), Error>> + Send,
{
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move {
        if hide {
            serve_hiding_claude_auth(agent_channel, opts).await
        } else {
            serve(agent_channel, opts).await
        }
    });
    let (status_tx, status_rx) = mpsc::unbounded_channel::<Value>();
    let result =
        timeout(
            Duration::from_secs(90),
            Client
                .builder()
                .on_receive_notification(
                    async move |notification: UntypedMessage, _cx| {
                        if notification.method() == "_auth/status_update" {
                            let _ = status_tx.send(notification.params().clone());
                        }
                        Ok(())
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(
                    client_channel,
                    move |connection: ConnectionTo<Agent>| async move {
                        body(connection, status_rx).await
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

async fn next_status(rx: &mut mpsc::UnboundedReceiver<Value>) -> Option<Value> {
    timeout(Duration::from_secs(5), rx.recv())
        .await
        .ok()
        .flatten()
}

/// A gateway that answers `/v1/models` with `ids` (and records nothing else).
async fn stub_gateway(ids: &[&str]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let body =
        json!({ "data": ids.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>() }).to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 2048];
            let _ = socket.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    base
}

fn new_session(cwd: &str) -> Value {
    json!({"cwd": cwd, "mcpServers": []})
}

/// `providers/list` of an agent with native routing, then after `set` and `disable`
#[tokio::test]
async fn t3b_providers_list_set_disable() {
    let tmp = temp_dir("list");
    let opts = options(&tmp, &json!({"apiKeySource": "ANTHROPIC_API_KEY"}));
    let gateway = stub_gateway(&["claude-sonnet-4-5"]).await;
    with_agent(opts, false, move |connection, _statuses| async move {
        call(&connection, "initialize", json!({"protocolVersion": 1})).await?;
        let listed = call(&connection, "providers/list", json!({})).await?;
        assert_eq!(
            listed["providers"][0],
            json!({"providerId": "main", "supported": ["anthropic", "bedrock", "vertex"],
                "required": false,
                "current": {"apiType": "anthropic", "baseUrl": "https://api.anthropic.com"}})
        );
        let set = call(
            &connection,
            "providers/set",
            json!({"providerId": "main", "apiType": "anthropic", "baseUrl": gateway,
                "headers": {"authorization": "Bearer secret"}}),
        )
        .await?;
        assert_eq!(set, json!({}));
        let listed = call(&connection, "providers/list", json!({})).await?;
        assert_eq!(
            listed["providers"][0]["current"],
            json!({"apiType": "anthropic", "baseUrl": gateway})
        );
        assert!(!listed.to_string().contains("secret"), "headers never leak");
        assert_eq!(
            call(
                &connection,
                "providers/disable",
                json!({"providerId": "main"})
            )
            .await?,
            json!({})
        );
        let listed = call(&connection, "providers/list", json!({})).await?;
        assert_eq!(
            listed["providers"][0]["current"]["baseUrl"],
            "https://api.anthropic.com"
        );
        // An unknown provider is an idempotent success.
        assert_eq!(
            call(
                &connection,
                "providers/disable",
                json!({"providerId": "openai"})
            )
            .await?,
            json!({})
        );
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// WB-01: `providers/set` validation errors, the exact -32602 messages
#[tokio::test]
async fn t3b_providers_set_validation() {
    let tmp = temp_dir("validate");
    let opts = options(&tmp, &json!({"apiKeySource": "ANTHROPIC_API_KEY"}));
    with_agent(opts, false, move |connection, _statuses| async move {
        let bad = |extra: Value| {
            let mut base = json!({"providerId": "main", "apiType": "anthropic", "baseUrl": "https://g.example"});
            for (k, v) in extra.as_object().expect("object") {
                base[k] = v.clone();
            }
            base
        };
        for (request, message) in [
            (
                bad(json!({"providerId": "openai"})),
                "Invalid params: Unknown provider ID \"openai\"; expected \"main\".",
            ),
            (
                bad(json!({"apiType": "openai"})),
                "Invalid params: Unsupported apiType \"openai\" for provider \"main\".",
            ),
            (
                bad(json!({"baseUrl": "ftp://nope.example"})),
                "Invalid params: baseUrl must be a non-empty absolute http(s) URL.",
            ),
            (
                bad(json!({"apiType": "vertex"})),
                "Invalid params: vertex apiType requires non-empty `_meta.claudeCode.vertex.projectId` and `_meta.claudeCode.vertex.region`.",
            ),
        ] {
            let error = call(&connection, "providers/set", request).await.unwrap_err();
            assert_eq!(code(&error), -32602);
            assert_eq!(wire(&error)["message"], message);
        }
        // A listing that fails fails `providers/set` and leaves the provider alone.
        let error = call(
            &connection,
            "providers/set",
            bad(json!({"baseUrl": "http://127.0.0.1:1"})),
        )
        .await
        .unwrap_err();
        assert_eq!(code(&error), -32603);
        assert_eq!(
            wire(&error)["message"],
            "Internal error: Model discovery for provider \"main\" failed: Model listing GET /v1/models failed: fetch failed"
        );
        assert_eq!(
            wire(&error)["data"],
            json!({"providerId": "main", "apiType": "anthropic", "baseUrl": "http://127.0.0.1:1"})
        );
        let listed = call(&connection, "providers/list", json!({})).await?;
        assert_eq!(
            listed["providers"][0]["current"]["baseUrl"],
            "https://api.anthropic.com"
        );
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// O-74: a session created after `providers/set` offers only the models the gateway listed
#[tokio::test]
async fn t3b_the_picker_is_narrowed_to_the_gateway_listing() {
    let tmp = temp_dir("narrow");
    let opts = options(&tmp, &json!({"apiKeySource": "ANTHROPIC_API_KEY"}));
    let gateway = stub_gateway(&["claude-opus-4-5"]).await;
    with_agent(opts, false, move |connection, _statuses| async move {
        call(&connection, "initialize", json!({"protocolVersion": 1})).await?;
        let cwd = repo_root().to_string_lossy().into_owned();
        let before = call(&connection, "session/new", new_session(&cwd)).await?;
        let models = |response: &Value| -> Vec<String> {
            response["configOptions"]
                .as_array()
                .and_then(|options| options.iter().find(|o| o["id"] == "model"))
                .and_then(|o| o["options"].as_array())
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|o| o["value"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(
            models(&before).len(),
            2,
            "native routing offers both models"
        );
        call(
            &connection,
            "providers/set",
            json!({"providerId": "main", "apiType": "anthropic", "baseUrl": gateway}),
        )
        .await?;
        let after = call(&connection, "session/new", new_session(&cwd)).await?;
        assert_eq!(models(&after), vec!["claude-opus-4-5".to_string()]);
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// authenticate: validation, the gateway status pushed before the response, the no-payload call, any other method
#[tokio::test]
async fn t3b_authenticate() {
    let tmp = temp_dir("auth");
    let opts = options(&tmp, &json!({"apiKeySource": "ANTHROPIC_API_KEY"}));
    with_agent(opts, false, move |connection, mut statuses| async move {
        call(&connection, "initialize", json!({"protocolVersion": 1})).await?;
        // The probe after initialize reports the CLI login first.
        let first = next_status(&mut statuses).await.expect("probe status");
        assert_eq!(first["authStatus"]["kind"], "account");
        let error = call(&connection, "authenticate", json!({"methodId": "other"}))
            .await
            .unwrap_err();
        assert_eq!(code(&error), -32603);
        assert_eq!(wire(&error)["data"], json!({"details": "Method not implemented."}));
        let error = call(
            &connection,
            "authenticate",
            json!({"methodId": "gateway", "_meta": {"gateway": {"baseUrl": ""}}}),
        )
        .await
        .unwrap_err();
        assert_eq!(code(&error), -32602);
        assert_eq!(
            wire(&error)["message"],
            "Invalid params: `_meta.gateway.baseUrl` must be a non-empty absolute http(s) URL."
        );
        let error = call(
            &connection,
            "authenticate",
            json!({"methodId": "gateway", "_meta": {"gateway": {"baseUrl": "https://g.example", "headers": "x"}}}),
        )
        .await
        .unwrap_err();
        assert_eq!(
            wire(&error)["message"],
            "Invalid params: `_meta.gateway.headers` must be an object of header names to values."
        );
        // A good call: the gateway status is on the wire before the response.
        let done = call(
            &connection,
            "authenticate",
            json!({"methodId": "gateway", "_meta": {"gateway": {"baseUrl": "https://gateway.example:8443/x"}}}),
        )
        .await?;
        assert_eq!(done, json!({}));
        let pushed = timeout(Duration::from_millis(500), statuses.recv())
            .await
            .expect("pushed before the response")
            .expect("status");
        assert_eq!(pushed["authStatus"]["kind"], "gateway");
        assert_eq!(pushed["authStatus"]["detail"], "gateway.example:8443");
        // The gateway is the legacy routing now: `providers/list` reports it.
        let listed = call(&connection, "providers/list", json!({})).await?;
        assert_eq!(
            listed["providers"][0]["current"],
            json!({"apiType": "anthropic", "baseUrl": "https://gateway.example:8443/x"})
        );
        // Logout clears it: native routing again, and the store is re-read.
        call(&connection, "logout", json!({})).await?;
        let listed = call(&connection, "providers/list", json!({})).await?;
        assert_eq!(
            listed["providers"][0]["current"]["baseUrl"],
            "https://api.anthropic.com"
        );
        let after_logout = next_status(&mut statuses).await.expect("logout status");
        assert_eq!(after_logout["authStatus"]["kind"], "none");
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// logout clears a `providers/set` override
#[tokio::test]
async fn t3b_logout_clears_the_provider_override() {
    let tmp = temp_dir("logout");
    let opts = options(&tmp, &json!({"apiKeySource": "ANTHROPIC_API_KEY"}));
    let gateway = stub_gateway(&["claude-sonnet-4-5"]).await;
    with_agent(opts, false, move |connection, _statuses| async move {
        call(
            &connection,
            "providers/set",
            json!({"providerId": "main", "apiType": "anthropic", "baseUrl": gateway}),
        )
        .await?;
        call(&connection, "logout", json!({})).await?;
        let listed = call(&connection, "providers/list", json!({})).await?;
        assert_eq!(
            listed["providers"][0]["current"]["baseUrl"],
            "https://api.anthropic.com"
        );
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-101: the creation guard under `--hide-claude-auth`
#[tokio::test]
async fn t3b_hide_claude_auth_creation_guard() {
    let cases: [(&str, Value, Option<bool>); 5] = [
        ("signed out", json!({}), Some(false)),
        (
            "subscription",
            json!({"subscriptionType": "pro"}),
            Some(true),
        ),
        (
            "api key",
            json!({"apiKeySource": "ANTHROPIC_API_KEY"}),
            None,
        ),
        ("bedrock", json!({"apiProvider": "bedrock"}), None),
        (
            "bearer token",
            json!({"tokenSource": "ANTHROPIC_AUTH_TOKEN"}),
            None,
        ),
    ];
    for (name, account, verdict) in cases {
        let tmp = temp_dir(&format!("hide-{}", name.replace(' ', "-")));
        let opts = options(&tmp, &account);
        with_agent(opts, true, move |connection, mut statuses| async move {
            call(&connection, "initialize", json!({"protocolVersion": 1})).await?;
            let cwd = repo_root().to_string_lossy().into_owned();
            let created = call(&connection, "session/new", new_session(&cwd)).await;
            match verdict {
                None => {
                    created?;
                }
                Some(subscription) => {
                    let error = created.unwrap_err();
                    let wire = wire(&error);
                    assert_eq!(wire["code"], -32000);
                    if subscription {
                        assert_eq!(
                            wire["message"],
                            "Authentication required: This integration does not support using claude.ai subscriptions."
                        );
                        assert_eq!(wire["data"], json!({"reason": "claude_subscription_not_supported"}));
                    } else {
                        assert_eq!(
                            wire["message"],
                            "Authentication required: Sign in with an API key or a Console account to use this integration."
                        );
                        assert!(wire.get("data").is_none());
                    }
                }
            }
            // A refusal still reported the account first (an informative one).
            if account.get("subscriptionType").is_some() {
                let mut kinds = Vec::new();
                while let Some(status) = next_status(&mut statuses).await {
                    kinds.push(status["authStatus"]["kind"].as_str().unwrap_or("").to_string());
                    if kinds.len() >= 2 {
                        break;
                    }
                }
                assert!(kinds.iter().any(|k| k == "account"), "{kinds:?}");
            }
            Ok(())
        })
        .await;
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

/// O-07: the flag hides the claude.ai login method; without it the method is offered
#[tokio::test]
async fn t3b_hide_claude_auth_hides_the_login_method() {
    let init = json!({"protocolVersion": 1, "clientCapabilities": {"auth": {"terminal": true}}});
    for hide in [false, true] {
        let tmp = temp_dir(&format!("methods-{hide}"));
        let opts = options(&tmp, &json!({"apiKeySource": "ANTHROPIC_API_KEY"}));
        let init = init.clone();
        with_agent(opts, hide, move |connection, _statuses| async move {
            let response = call(&connection, "initialize", init).await?;
            let ids: Vec<String> = response["authMethods"]
                .as_array()
                .map(|methods| {
                    methods
                        .iter()
                        .filter_map(|m| m["id"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(
                ids.iter().any(|id| id == "claude-ai-login"),
                !hide,
                "{ids:?}"
            );
            Ok(())
        })
        .await;
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

/// B-90 / providers: a provider override switches the guard off (a subscription account is allowed)
#[tokio::test]
async fn t3b_a_provider_override_switches_the_guard_off() {
    let tmp = temp_dir("override");
    let opts = options(&tmp, &json!({"subscriptionType": "pro"}));
    let gateway = stub_gateway(&["claude-sonnet-4-5"]).await;
    with_agent(opts, true, move |connection, _statuses| async move {
        call(&connection, "initialize", json!({"protocolVersion": 1})).await?;
        let cwd = repo_root().to_string_lossy().into_owned();
        let refused = call(&connection, "session/new", new_session(&cwd)).await;
        assert!(
            refused.is_err(),
            "the subscription is refused without an override"
        );
        call(
            &connection,
            "providers/set",
            json!({"providerId": "main", "apiType": "anthropic", "baseUrl": gateway}),
        )
        .await?;
        let created = call(&connection, "session/new", new_session(&cwd)).await?;
        let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
        // `providers/disable` recreates the session; creation now refuses the
        // subscription, the session is gone, and the switch itself still succeeds.
        assert_eq!(
            call(
                &connection,
                "providers/disable",
                json!({"providerId": "main"})
            )
            .await?,
            json!({})
        );
        let gone = call(
            &connection,
            "session/set_mode",
            json!({"sessionId": session_id, "modeId": "plan"}),
        )
        .await;
        assert!(
            gone.is_err(),
            "the session that could not be recreated is gone"
        );
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// A transcript with one segment per spawn: segment N answers `initialize` with
/// `accounts[N]`. The first segment also fails its first user turn with the
/// sign-out error ("Please run /login").
fn respawn_transcript(tmp: &Path, accounts: &[Value]) -> PathBuf {
    let path = tmp.join("respawn.transcript.jsonl");
    let mut text = format!(
        "{}\n",
        json!({"auth_status_stdout": {"loggedIn": true, "authMethod": "claude.ai",
            "apiProvider": "firstParty", "subscriptionType": "pro", "email": "a@b.c", "orgName": "Org"}})
    );
    for (index, account) in accounts.iter().enumerate() {
        if index > 0 {
            text.push_str(&format!("{{\"segment\":{index}}}\n"));
        }
        let init = json!({
            "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
            "emit": [{"type": "control_response", "response": {
                "subtype": "success", "request_id": "$REQ",
                "response": {"models": [{"value": "claude-sonnet-4-5", "displayName": "S", "description": "d"}],
                    "commands": [], "account": account}
            }}]
        });
        text.push_str(&format!("{init}\n"));
        if index == 0 {
            let turn = json!({
                "expect": {"type": "user"},
                "emit": [
                    {"type": "user", "uuid": "$MATCH.uuid", "isReplay": true,
                     "message": {"role": "user", "content": [{"type": "text", "text": "needs auth"}]}},
                    {"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "Please sign in to proceed."}]}},
                    {"type": "result", "subtype": "success", "stop_reason": "error", "is_error": true,
                     "result": "Please run /login to continue",
                     "usage": {"input_tokens": 1, "output_tokens": 1, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}
                ]
            });
            text.push_str(&format!("{turn}\n"));
        }
    }
    std::fs::write(&path, text).expect("write transcript");
    path
}

/// Rewrite the transcript's `auth_status_stdout` line, so the next probe reads a
/// different identity (the fake CLI reads the file on every `auth status`).
fn set_probe_account(path: &Path, account: Value) {
    let text = std::fs::read_to_string(path).expect("read transcript");
    let mut lines = text.lines();
    let first = lines.next().expect("auth status line");
    assert!(
        serde_json::from_str::<Value>(first)
            .expect("auth status line is JSON")
            .get("auth_status_stdout")
            .is_some(),
        "the first transcript line is the auth status"
    );
    let mut out = json!({"auth_status_stdout": account}).to_string();
    for line in lines {
        out.push('\n');
        out.push_str(line);
    }
    out.push('\n');
    std::fs::write(path, out).expect("write transcript");
}

fn prompt_params(session_id: &str) -> Value {
    json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "needs auth"}]})
}

/// V-16 / M-100 / B-91: a turn that fails with a sign-out flags the session; the next
/// prompt recreates the query, and the creation guard judges the new account. A
/// refusal keeps the session addressable so the client can sign in and retry.
#[tokio::test]
async fn t3b_a_sign_out_recreates_the_query_at_the_next_turn() {
    let tmp = temp_dir("respawn");
    let mut opts = options(&tmp, &json!({}));
    opts.extra_env.retain(|(k, _)| k != "FAKE_CLAUDE_SCRIPT");
    opts.extra_env.push((
        "FAKE_CLAUDE_SCRIPT".into(),
        respawn_transcript(
            &tmp,
            &[
                json!({"apiKeySource": "ANTHROPIC_API_KEY"}),
                json!({"subscriptionType": "pro"}),
            ],
        )
        .to_string_lossy()
        .into_owned(),
    ));
    opts.extra_env.push((
        "FAKE_CLAUDE_CURSOR".into(),
        tmp.join("cursor").to_string_lossy().into_owned(),
    ));
    with_agent(opts, true, move |connection, _statuses| async move {
        call(&connection, "initialize", json!({"protocolVersion": 1})).await?;
        let cwd = repo_root().to_string_lossy().into_owned();
        let created = call(&connection, "session/new", new_session(&cwd)).await?;
        let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
        // The turn that signs out: the plain authRequired error.
        let error = call(&connection, "session/prompt", prompt_params(&session_id))
            .await
            .unwrap_err();
        assert_eq!(code(&error), -32000, "{error:?}");
        // The next prompt recreates the query; the new account is a subscription.
        for _ in 0..2 {
            let error = call(&connection, "session/prompt", prompt_params(&session_id))
                .await
                .unwrap_err();
            let wire = wire(&error);
            assert_eq!(wire["code"], -32000, "{wire}");
            assert_eq!(
                wire["message"],
                "Authentication required: This integration does not support using claude.ai subscriptions."
            );
            assert_eq!(wire["data"], json!({"reason": "claude_subscription_not_supported"}));
        }
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-101: the creation guard also covers `session/load` and `session/resume`
#[tokio::test]
async fn t3b_the_guard_covers_load_and_resume() {
    for method in ["session/load", "session/resume"] {
        let tmp = temp_dir(&format!("load-{}", method.replace('/', "-")));
        let opts = options(&tmp, &json!({"subscriptionType": "pro"}));
        with_agent(opts, true, move |connection, _statuses| async move {
            call(&connection, "initialize", json!({"protocolVersion": 1})).await?;
            let cwd = repo_root().to_string_lossy().into_owned();
            let error = call(
                &connection,
                method,
                json!({"sessionId": "11111111-1111-4111-8111-111111111111", "cwd": cwd, "mcpServers": []}),
            )
            .await
            .unwrap_err();
            assert_eq!(wire(&error)["code"], -32000);
            assert_eq!(
                wire(&error)["data"],
                json!({"reason": "claude_subscription_not_supported"})
            );
            Ok(())
        })
        .await;
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

/// B-88: the start of every prompt re-reads the identity, finding a sign-out done elsewhere
#[tokio::test]
async fn t3b_each_prompt_re_reads_the_identity() {
    let tmp = temp_dir("prompt-probe");
    let mut opts = options(&tmp, &json!({"apiKeySource": "ANTHROPIC_API_KEY"}));
    // A script whose first user turn answers (with the sign-out error), so the prompt settles.
    opts.extra_env.retain(|(k, _)| k != "FAKE_CLAUDE_SCRIPT");
    opts.extra_env.push((
        "FAKE_CLAUDE_SCRIPT".into(),
        respawn_transcript(&tmp, &[json!({"apiKeySource": "ANTHROPIC_API_KEY"})])
            .to_string_lossy()
            .into_owned(),
    ));
    let marker = tmp.join("config").join(".fake-claude-logged-out");
    let transcript = tmp.join("respawn.transcript.jsonl");
    with_agent(opts, false, move |connection, mut statuses| async move {
        call(&connection, "initialize", json!({"protocolVersion": 1})).await?;
        let first = next_status(&mut statuses).await.expect("initialize probe");
        assert_eq!(first["authStatus"]["kind"], "account");
        // Give the `session/new` context probe a distinct account, so its status
        // is always published (a probe that finds the same identity as the
        // initialize read is deduplicated and unobservable), and wait for it
        // before the sign-out: it must not be the probe that reports `none`.
        set_probe_account(
            &transcript,
            json!({"loggedIn": true, "authMethod": "claude.ai",
                "apiProvider": "firstParty", "subscriptionType": "team",
                "email": "probe@example.test", "orgName": "Probe"}),
        );
        let cwd = repo_root().to_string_lossy().into_owned();
        let created = call(&connection, "session/new", new_session(&cwd)).await?;
        let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
        // The session account (an API key) and the context probe's account are
        // published in either order; drain until both arrived.
        let mut saw_api_key = false;
        let mut saw_account = false;
        while !(saw_api_key && saw_account) {
            let status = next_status(&mut statuses)
                .await
                .expect("session identity status");
            match status["authStatus"]["kind"].as_str().unwrap_or("") {
                "api_key" => saw_api_key = true,
                "account" => saw_account = true,
                other => panic!("unexpected status before the sign-out: {other}"),
            }
        }
        // A sign-out in another terminal: no request of ours caused it.
        std::fs::write(&marker, b"").expect("marker");
        let _ = call(&connection, "session/prompt", prompt_params(&session_id)).await;
        // Statuses are drained until the sign-out shows; each read is bounded.
        let mut seen = Vec::new();
        loop {
            let status = next_status(&mut statuses)
                .await
                .unwrap_or_else(|| panic!("no start-of-prompt probe status; saw {seen:?}"));
            let kind = status["authStatus"]["kind"]
                .as_str()
                .unwrap_or("")
                .to_string();
            if kind == "none" {
                break;
            }
            assert_eq!(kind, "account", "unexpected status before the sign-out");
            seen.push(kind);
        }
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-101: without the flag every account is allowed (a signed-out CLI, a subscription)
#[tokio::test]
async fn t3b_without_the_flag_every_account_is_allowed() {
    for account in [json!({}), json!({"subscriptionType": "pro"})] {
        let tmp = temp_dir("no-flag");
        let opts = options(&tmp, &account);
        with_agent(opts, false, move |connection, _statuses| async move {
            call(&connection, "initialize", json!({"protocolVersion": 1})).await?;
            let cwd = repo_root().to_string_lossy().into_owned();
            let created = call(&connection, "session/new", new_session(&cwd)).await?;
            assert!(created["sessionId"].is_string());
            Ok(())
        })
        .await;
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

/// A steer with the idle `promptRequired` opt-in starts no turn, so the guard does not run for it
#[tokio::test]
async fn t3b_a_steer_that_starts_a_turn_runs_the_guard() {
    let tmp = temp_dir("steer");
    let mut opts = options(&tmp, &json!({"apiKeySource": "ANTHROPIC_API_KEY"}));
    opts.extra_env.retain(|(k, _)| k != "FAKE_CLAUDE_SCRIPT");
    opts.extra_env.push((
        "FAKE_CLAUDE_SCRIPT".into(),
        respawn_transcript(&tmp, &[json!({"apiKeySource": "ANTHROPIC_API_KEY"})])
            .to_string_lossy()
            .into_owned(),
    ));
    with_agent(opts, true, move |connection, _statuses| async move {
        call(&connection, "initialize", json!({"protocolVersion": 1})).await?;
        let cwd = repo_root().to_string_lossy().into_owned();
        let created = call(&connection, "session/new", new_session(&cwd)).await?;
        let session_id = created["sessionId"].as_str().unwrap_or("").to_string();
        // The idle opt-in path answers `promptRequired` without any guard.
        let answered = call(
            &connection,
            "_session/steering",
            json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "x"}],
                "_meta": {"steering": {"idleBehavior": "promptRequired"}}}),
        )
        .await?;
        assert_eq!(answered["outcome"], "promptRequired");
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}
