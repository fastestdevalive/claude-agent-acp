//! Unit tests for the v2 agent surface's request/response translation
//! (M-106, M-108..M-112, O-86; ported from `src/tests/acp-v2.test.ts`).

use serde_json::json;

use super::agent::v1_request_for;

/// M-108 — a v2 `session/new` becomes the v1 request: optional `mcpServers` and
/// `systemPrompt` are converted, an omitted list becomes an empty one
/// (`v2/session.ts:25-33`).
#[test]
fn m108_v2_new_session_request_becomes_v1() {
    let v2 = json!({
        "cwd": "/workspace/project",
        "mcpServers": [
            {"type": "stdio", "name": "files", "command": "/usr/local/bin/mcp-files"},
            {"type": "http", "name": "linear", "url": "https://mcp.linear.app/mcp"},
        ],
        "systemPrompt": {"content": [
            {"type": "text", "text": "be brief",
             "annotations": {"audience": ["user", "_acme"]}}
        ]},
    });
    let request = v1_request_for("session/new", &v2).expect("translated");
    assert_eq!(request.method, "session/new");
    let params = &request.params;
    assert_eq!(params["cwd"], "/workspace/project");
    assert_eq!(
        params["mcpServers"],
        json!([
            {"name": "files", "command": "/usr/local/bin/mcp-files", "args": [], "env": []},
            {"type": "http", "name": "linear", "url": "https://mcp.linear.app/mcp", "headers": []}
        ])
    );
    // v1 takes only the user/assistant audiences.
    assert_eq!(
        params["systemPrompt"]["content"][0]["annotations"]["audience"],
        json!(["user"])
    );

    // An omitted list is an empty one in v1.
    let bare = v1_request_for("session/new", &json!({"cwd": "/p"})).expect("translated");
    assert_eq!(bare.params["mcpServers"], json!([]));
    assert!(bare.params.get("systemPrompt").is_none());
}

/// "rejects an MCP transport that v1 cannot express" — `invalidParams`
/// (`v2/session.ts:178-181`).
#[test]
fn rejects_an_mcp_transport_v1_cannot_express() {
    let error = v1_request_for(
        "session/new",
        &json!({
            "cwd": "/p",
            "mcpServers": [{"type": "_custom", "name": "custom"}],
        }),
    )
    .expect_err("must reject");
    assert_eq!(i32::from(error.code), -32602);
    assert_eq!(
        error.message,
        "Invalid params: MCP servers of type _custom are not supported"
    );
    assert_eq!(error.data, Some(json!({"type": "_custom"})));
}

/// M-109 — `session/resume` with `replayFrom: {type:"start"}` becomes
/// `session/load`; another cursor is rejected (`v2/session.ts:44-67`).
#[test]
fn m109_v2_resume_replay_cursor_selects_the_v1_method() {
    let resume = v1_request_for("session/resume", &json!({"sessionId": "s", "cwd": "/p"}))
        .expect("translated");
    assert_eq!(resume.method, "session/resume");
    let load = v1_request_for(
        "session/resume",
        &json!({"sessionId": "s", "cwd": "/p", "replayFrom": {"type": "start"}}),
    )
    .expect("translated");
    assert_eq!(load.method, "session/load");
    assert_eq!(load.params["mcpServers"], json!([]));
    assert!(load.params.get("replayFrom").is_none());
    let error = v1_request_for(
        "session/resume",
        &json!({"sessionId": "s", "cwd": "/p",
                "replayFrom": {"type": "_message", "messageId": "m"}}),
    )
    .expect_err("must reject");
    assert_eq!(i32::from(error.code), -32602);
    assert_eq!(
        error.message,
        "Invalid params: Replaying from a _message cursor is not supported"
    );
}

/// M-110 — a v2 `session/fork` becomes the v1 fork request, with the MCP
/// servers and system prompt converted (`v2/session.ts:73-80`).
#[test]
fn m110_v2_fork_request_becomes_v1() {
    let request = v1_request_for(
        "session/fork",
        &json!({
            "sessionId": "s",
            "cwd": "/p",
            "mcpServers": [{"type": "http", "name": "linear", "url": "https://mcp.linear.app/mcp"}],
        }),
    )
    .expect("translated");
    assert_eq!(request.method, "session/fork");
    assert_eq!(request.params["sessionId"], "s");
    assert_eq!(request.params["mcpServers"][0]["name"], "linear");
    assert_eq!(request.params["mcpServers"][0]["headers"], json!([]));
}

/// M-112 — a v2 config value is untagged for v1 (`type: "id"`), a boolean keeps
/// its tag, and an unknown type is `invalidParams` (`v2/session.ts:105-124`).
#[test]
fn m112_v2_config_option_values_become_v1() {
    let id = v1_request_for(
        "session/set_config_option",
        &json!({"sessionId": "s", "configId": "mode", "type": "id", "value": "plan"}),
    )
    .expect("translated");
    assert_eq!(id.params["configId"], "mode");
    assert_eq!(id.params["value"], "plan");
    assert!(id.params.get("type").is_none());

    let boolean = v1_request_for(
        "session/set_config_option",
        &json!({"sessionId": "s", "configId": "fast", "type": "boolean", "value": true}),
    )
    .expect("translated");
    assert_eq!(boolean.params["type"], "boolean");
    assert_eq!(boolean.params["value"], true);

    let error = v1_request_for(
        "session/set_config_option",
        &json!({"sessionId": "s", "configId": "x", "type": "_custom", "value": 1}),
    )
    .expect_err("must reject");
    assert_eq!(i32::from(error.code), -32602);
}

/// M-111 — `session/list`, `session/close` and `session/delete` pass through to
/// the v1 method of the same name (`v2/agent.ts:84,114-120`).
#[test]
fn m111_list_close_delete_pass_through() {
    for method in ["session/list", "session/close", "session/delete"] {
        let request = v1_request_for(method, &json!({"sessionId": "s"})).expect("translated");
        assert_eq!(request.method, method);
        assert_eq!(request.params["sessionId"], "s");
    }
}
