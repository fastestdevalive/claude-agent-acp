//! T7B STEP 3 — providers and auth over the v2 agent surface (M-115, M-116;
//! upstream `src/v2/agent.ts:68-76`). The methods pass through to the v1
//! handlers unchanged: v1 and v2 share the request/response types.
//!
//! Hermetic duplex connection, local `fake-claude`, every wait bounded.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::{Agent, Channel, Client, ConnectionTo, Error, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::v2::EXPERIMENTAL_V2_ENV;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::timeout;

/// A local gateway that answers `/v1/models` with `ids`; nothing leaves the host.
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

fn temp_dir(tag: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-t7b-prov-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    tmp
}

/// The initialize handshake plus the `set_permission_mode` the adapter sends
/// when a session starts.
fn transcript(tmp: &Path) -> PathBuf {
    let path = tmp.join("init.transcript.jsonl");
    let init = json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ",
            "response": {
                "models": [{"value": "claude-sonnet-4-5", "displayName": "Claude Sonnet 4.5", "description": "d"}],
                "commands": [], "account": {},
            }
        }}]
    });
    std::fs::write(&path, format!("{init}\n")).expect("write transcript");
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

/// Runs `body` on a fresh v2 connection.
async fn with_v2<F, Fut>(tmp: &Path, body: F)
where
    F: FnOnce(ConnectionTo<Agent>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), Error>> + Send,
{
    let (agent_channel, client_channel) = Channel::duplex();
    let opts = options(tmp);
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let collector = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let result = timeout(
        Duration::from_secs(90),
        Client
            .builder()
            .on_receive_notification(
                async move |_notification: UntypedMessage, _cx| {
                    let _ = tx.send(());
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                client_channel,
                move |connection: ConnectionTo<Agent>| async move { body(connection).await },
            ),
    )
    .await;
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    result
        .expect("the exchange finished in time")
        .expect("exchange");
    collector.await.expect("collector");
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

/// M-115 — the v2 initialize advertises `providers: {}` and the surface serves
/// `providers/list` with the v1 provider shape.
#[tokio::test]
async fn m115_v2_providers_list() {
    let tmp = temp_dir("list");
    with_v2(&tmp, move |connection| async move {
        let init = initialize(&connection).await;
        assert_eq!(init["protocolVersion"], 2);
        assert_eq!(init["capabilities"]["providers"], json!({}));

        let listed = call(&connection, "providers/list", json!({})).await?;
        let providers = listed["providers"].as_array().expect("a providers array");
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["providerId"], "main");
        assert_eq!(providers[0]["required"], false);
        assert_eq!(
            providers[0]["supported"],
            json!(["anthropic", "bedrock", "vertex"])
        );
        assert_eq!(providers[0]["current"]["apiType"], "anthropic");
        assert_eq!(
            providers[0]["current"]["baseUrl"],
            "https://api.anthropic.com"
        );
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-115 — `providers/set` validates the provider id and answers `{}` when the
/// update finished; an unknown provider is `invalidParams`.
#[tokio::test]
async fn m115_v2_providers_set() {
    let tmp = temp_dir("set");
    let gateway = stub_gateway(&["claude-sonnet-4-5"]).await;
    let set_gateway = gateway.clone();
    with_v2(&tmp, move |connection| async move {
        initialize(&connection).await;
        let error = call(
            &connection,
            "providers/set",
            json!({
                "providerId": "other",
                "apiType": "anthropic",
                "baseUrl": set_gateway,
            }),
        )
        .await
        .expect_err("an unknown provider must be rejected");
        assert_eq!(code(&error), -32602);

        let response = call(
            &connection,
            "providers/set",
            json!({
                "providerId": "main",
                "apiType": "anthropic",
                "baseUrl": set_gateway,
            }),
        )
        .await?;
        assert_eq!(response, json!({}));

        // The routing took effect: `providers/list` reports the override.
        let listed = call(&connection, "providers/list", json!({})).await?;
        assert_eq!(listed["providers"][0]["current"]["baseUrl"], set_gateway);
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-115 — `providers/disable` is an idempotent success for an unknown id and
/// clears the override for `main`.
#[tokio::test]
async fn m115_v2_providers_disable() {
    let tmp = temp_dir("disable");
    let gateway = stub_gateway(&["claude-sonnet-4-5"]).await;
    with_v2(&tmp, move |connection| async move {
        initialize(&connection).await;
        let unknown = call(
            &connection,
            "providers/disable",
            json!({"providerId": "other"}),
        )
        .await?;
        assert_eq!(unknown, json!({}));

        let _ = call(
            &connection,
            "providers/set",
            json!({
                "providerId": "main",
                "apiType": "anthropic",
                "baseUrl": gateway,
            }),
        )
        .await?;
        let disabled = call(
            &connection,
            "providers/disable",
            json!({"providerId": "main"}),
        )
        .await?;
        assert_eq!(disabled, json!({}));
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

/// M-116 — `auth/login` maps to the v1 `authenticate`; an unsupported method is
/// the internal "Method not implemented." error, a gateway login answers `{}`.
#[tokio::test]
async fn m116_v2_auth_login() {
    let tmp = temp_dir("login");
    with_v2(&tmp, move |connection| async move {
        initialize(&connection).await;
        let error = call(&connection, "auth/login", json!({"methodId": "console"}))
            .await
            .expect_err("an unsupported auth method must be rejected");
        assert_eq!(code(&error), -32603);
        assert_eq!(
            error.data.as_ref().and_then(|d| d.get("details")),
            Some(&json!("Method not implemented."))
        );

        let response = call(
            &connection,
            "auth/login",
            json!({
                "methodId": "gateway",
                "_meta": {"gateway": {"baseUrl": "https://gw.example.com"}},
            }),
        )
        .await?;
        assert_eq!(response, json!({}));
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// M-116 — `auth/logout` maps to the v1 `logout` and answers `{}` (the local
/// fake CLI clears its marker).
#[tokio::test]
async fn m116_v2_auth_logout() {
    let tmp = temp_dir("logout");
    with_v2(&tmp, move |connection| async move {
        initialize(&connection).await;
        let response = call(&connection, "auth/logout", json!({})).await?;
        assert_eq!(response, json!({}));
        Ok(())
    })
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
}
