//! Turn 3 fix round 3, lane FE: the registry actor never loses a request the
//! connection already read, and a connection that ends does not wait for it.
//! Hermetic duplex transport, `fake-claude` transcript, every wait bounded.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use claude_agent_acp_rs::agent::{serve, serve_with_shutdown, ServeOptions};
use claude_agent_acp_rs::codec::WireTransport;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::process::Timings;
use common::{fake_claude_binary, repo_root};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;
use tokio::time::timeout;

fn initialize_step(sleep_ms: Option<u64>) -> String {
    let mut emit = Vec::new();
    if let Some(ms) = sleep_ms {
        emit.push(json!({ "sleep_ms": ms }));
    }
    emit.push(json!({"type": "control_response", "response": {
        "subtype": "success", "request_id": "$REQ",
        "response": {
            "models": [{"value": "claude-sonnet-4-5", "displayName": "Claude Sonnet 4.5", "description": "d"}],
            "account": {"apiKeySource": "ANTHROPIC_API_KEY"},
            "commands": [], "modes": [], "agents": [], "modelInfos": [],
            "toolAliases": {}, "authMethods": [], "configOptions": {},
        }
    }}));
    json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": emit,
    })
    .to_string()
}

fn write_transcript(tmp: &Path, initialize_sleep_ms: Option<u64>) -> PathBuf {
    let path = tmp.join("t3fe.transcript.jsonl");
    let text = format!(
        "{}\n{{\"auth_status_stdout\":{{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"apiProvider\":\"firstParty\",\"apiKeySource\":\"ANTHROPIC_API_KEY\"}}}}\n",
        initialize_step(initialize_sleep_ms)
    );
    std::fs::write(&path, text).expect("write transcript");
    path
}

fn temp_dir(tag: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-t3fe-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    tmp
}

fn options(tmp: &Path, initialize_sleep_ms: Option<u64>) -> ServeOptions {
    std::fs::create_dir_all(tmp.join("config")).expect("config dir");
    ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                write_transcript(tmp, initialize_sleep_ms)
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "FAKE_CLAUDE_CURSOR".to_string(),
                tmp.join("cursor").to_string_lossy().into_owned(),
            ),
            (
                "FAKE_CLAUDE_ARGV_OUT".to_string(),
                tmp.join("argv.json").to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                tmp.join("config").to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(repo_root()),
        timings: Timings {
            force_cancel_grace: Duration::from_millis(200),
            ..Timings::default()
        },
        env: EnvSnapshot::empty(),
        logger: None,
    }
}

/// Step 1 regression: a client that sends `initialize` and closes stdin still
/// gets the reply. The handler waits for the actor to serve the request, so the
/// connection cannot end before it.
#[tokio::test]
async fn t3fe_initialize_then_eof_still_answers() {
    timeout(Duration::from_secs(30), async {
        let (mut client_in, agent_in) = tokio::io::duplex(1 << 16);
        let (agent_out, client_out) = tokio::io::duplex(1 << 16);
        let task = tokio::spawn(serve(
            WireTransport::new(agent_in, agent_out),
            ServeOptions {
                env: EnvSnapshot::empty(),
                default_cwd: Some(repo_root()),
                ..Default::default()
            },
        ));
        let line = serde_json::to_string(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": 1}
        }))
        .expect("serialize");
        client_in
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("write initialize");
        client_in.flush().await.expect("flush");
        drop(client_in);

        let mut reader = BufReader::new(client_out);
        let mut reply = String::new();
        let read = timeout(Duration::from_secs(10), reader.read_line(&mut reply))
            .await
            .expect("the initialize reply must not be lost to EOF")
            .expect("read reply");
        assert!(read > 0, "the agent closed stdout without answering");
        let frame: Value = serde_json::from_str(&reply).expect("JSON-RPC frame");
        assert_eq!(frame["id"], 1);
        assert!(frame.get("result").is_some(), "{frame}");

        let ended = timeout(Duration::from_secs(10), task)
            .await
            .expect("serve ends after the client went away");
        assert!(ended.is_ok(), "serve returned an error: {ended:?}");
    })
    .await
    .expect("the exchange finished in time");
}

/// Step 4 (finding F): a shutdown while a request is being served (a slow
/// `session/new` starting a CLI) returns at once, instead of letting the actor
/// finish it for nobody up to the 6 s dispose bound.
#[tokio::test]
async fn t3fe_shutdown_with_a_request_in_flight_returns_fast() {
    timeout(Duration::from_secs(30), async {
        let _guard = claude_agent_acp_rs::process::ManagedSourceGuard::arm();
        let tmp = temp_dir("shutdown");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (client_in, agent_in) = tokio::io::duplex(1 << 20);
        let (agent_out, client_out) = tokio::io::duplex(1 << 20);
        let task = tokio::spawn(serve_with_shutdown(
            WireTransport::new(agent_in, agent_out),
            // The session's CLI handshake takes longer than the bound asserted
            // below, so a shutdown that waits for it is visible.
            options(&tmp, Some(3000)),
            shutdown_rx,
        ));
        let mut input = client_in;
        let mut reader = BufReader::new(client_out);

        let send = |value: Value| {
            let mut line = serde_json::to_string(&value).expect("serialize");
            line.push('\n');
            line
        };
        input
            .write_all(
                send(json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                    "params":{"protocolVersion":1}}))
                .as_bytes(),
            )
            .await
            .expect("initialize");
        input.flush().await.expect("flush");
        let mut line = String::new();
        let n = timeout(Duration::from_secs(20), reader.read_line(&mut line))
            .await
            .expect("initialize reply timed out")
            .expect("read agent line");
        assert!(n > 0, "agent closed stdout");

        // `session/new` is served inline: the actor is inside its start when the
        // shutdown arrives.
        input
            .write_all(
                send(json!({"jsonrpc":"2.0","id":2,"method":"session/new",
                    "params":{"cwd":repo_root().to_string_lossy(),"mcpServers":[]}}))
                .as_bytes(),
            )
            .await
            .expect("session/new");
        input.flush().await.expect("flush");
        tokio::time::sleep(Duration::from_millis(100)).await;

        let started = Instant::now();
        shutdown_tx.send(true).expect("signal shutdown");
        let ended = timeout(Duration::from_secs(5), task)
            .await
            .expect("a shutdown with a request in flight must not hang");
        let elapsed = started.elapsed();
        assert!(ended.is_ok(), "serve returned an error: {ended:?}");
        assert!(
            elapsed < Duration::from_secs(2),
            "shutdown took {elapsed:?}, the actor was still serving the request"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    })
    .await
    .expect("the exchange finished in time");
}
