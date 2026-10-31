//! T6G: the per-connection v2 outbound translation (C-24, B-112, U-82..U-86,
//! P-61/P-62, Q-89/Q-90; upstream `v2/agent.ts:174-402`).
//!
//! [`V2Outbound`] is the connection's `V2ClientConnection` without the socket:
//! every test feeds it the v1 updates the agent emits and asserts the v2
//! notifications it produces.

use serde_json::{json, Value};

use super::agent::V2Outbound;

fn kinds(notifications: &[Value]) -> Vec<String> {
    notifications
        .iter()
        .map(|notification| {
            notification["update"]["sessionUpdate"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

/// acp-v2.test.ts: "reports a tool call as a v2 tool_call_update" — a v1
/// `tool_call` becomes a v2 upsert, never a `tool_call`.
#[test]
fn t6g_a_tool_call_becomes_a_tool_call_update() {
    let mut out = V2Outbound::new();
    let notifications = out.translate(
        "s",
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Read a.txt",
            "kind": "read",
            "status": "pending",
            "content": [],
            "rawInput": {"file_path": "a.txt"},
            "_meta": {"claudeCode": {"toolName": "Read"}},
        }),
    );
    assert_eq!(kinds(&notifications), vec!["tool_call_update"]);
    let update = &notifications[0]["update"];
    assert_eq!(update["toolCallId"], "t1");
    assert_eq!(update["name"], "Read", "toolName becomes name once");
    assert!(update.get("_meta").is_none(), "the name key is left out");
    // A later report of the same call is an upsert too, and carries only the
    // `_meta` keys that changed.
    let notifications = out.translate(
        "s",
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "status": "completed",
            "_meta": {"claudeCode": {"toolName": "Read", "x": 1}},
        }),
    );
    assert_eq!(kinds(&notifications), vec!["tool_call_update"]);
    assert_eq!(notifications[0]["update"]["status"], "completed");
    assert_eq!(
        notifications[0]["update"]["_meta"],
        json!({"claudeCode": {"x": 1}})
    );
}

/// acp-v2.test.ts: "reports a plan as a plan_update with the session's plan id".
#[test]
fn t6g_a_plan_becomes_a_plan_update() {
    let mut out = V2Outbound::new();
    let notifications = out.translate(
        "s",
        json!({
            "sessionUpdate": "plan",
            "entries": [{"content": "ship", "priority": "medium", "status": "pending"}],
        }),
    );
    assert_eq!(kinds(&notifications), vec!["plan_update"]);
    let plan = &notifications[0]["update"]["plan"];
    assert_eq!(plan["type"], "items");
    assert_eq!(plan["planId"], "plan");
    assert_eq!(plan["entries"][0]["content"], "ship");
}

/// acp-v2.test.ts: "drops the mode update of a v2 client" — the mode is the
/// mode config option there (`v2SessionUpdate`, `v2/session-update.ts`).
#[test]
fn t6g_no_current_mode_update_reaches_a_v2_client() {
    let mut out = V2Outbound::new();
    let notifications = out.translate(
        "s",
        json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}),
    );
    assert!(notifications.is_empty(), "{notifications:?}");
}

/// acp-v2.test.ts: "replays the history clearing each message before its first
/// chunk" (B-112).
#[test]
fn t6g_a_replay_clears_each_message_before_its_first_chunk() {
    let mut out = V2Outbound::new();
    out.begin_replay("s");
    let chunk = json!({
        "sessionUpdate": "user_message_chunk",
        "messageId": "u1",
        "content": {"type": "text", "text": "hi"},
    });
    let notifications = out.translate("s", chunk.clone());
    assert_eq!(
        kinds(&notifications),
        vec!["user_message", "user_message_chunk"]
    );
    assert_eq!(notifications[0]["update"]["content"], json!([]));
    assert_eq!(
        notifications[0]["update"]["messageId"],
        notifications[1]["update"]["messageId"]
    );
    // A second chunk of the same message is not cleared again.
    let notifications = out.translate("s", chunk);
    assert_eq!(kinds(&notifications), vec!["user_message_chunk"]);
    // Outside a replay nothing is cleared.
    out.end_replay("s");
    let notifications = out.translate(
        "s",
        json!({
            "sessionUpdate": "agent_message_chunk",
            "messageId": "a1",
            "content": {"type": "text", "text": "x"},
        }),
    );
    assert_eq!(kinds(&notifications), vec!["agent_message_chunk"]);
}

/// acp-v2.test.ts: "reports a subagent's work state on the child's own stream
/// after the replay ends" (U-86).
#[test]
fn t6g_a_replayed_subagent_state_goes_out_after_the_replay() {
    let mut out = V2Outbound::new();
    out.begin_replay("parent");
    let notifications = out.translate(
        "parent",
        json!({
            "sessionUpdate": "subagent_update",
            "sessionId": "agent-1",
            "state": {"state": "idle", "stopReason": "end_turn"},
        }),
    );
    assert_eq!(kinds(&notifications), vec!["subagent_update"]);
    let deferred = out.end_replay("parent");
    assert_eq!(deferred.len(), 1);
    assert_eq!(deferred[0]["sessionId"], "agent-1");
    assert_eq!(deferred[0]["update"]["state"], "idle");
    assert_eq!(deferred[0]["update"]["stopReason"], "end_turn");
    // Live, the child's copy follows the parent's.
    let notifications = out.translate(
        "parent",
        json!({
            "sessionUpdate": "subagent_update",
            "sessionId": "agent-2",
            "state": {"state": "running"},
        }),
    );
    assert_eq!(
        kinds(&notifications),
        vec!["subagent_update", "state_update"]
    );
    assert_eq!(notifications[1]["sessionId"], "agent-2");
    assert_eq!(notifications[1]["update"]["state"], "running");
}

/// acp-v2.test.ts: "asks about a tool call with a title and the stored `_meta`"
/// (P-61) — the subject is the v2 tool-call patch of the stored call.
#[test]
fn t6g_a_permission_subject_carries_the_stored_meta() {
    let mut out = V2Outbound::new();
    out.translate(
        "s",
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Run ls",
            "status": "pending",
            "_meta": {"claudeCode": {"toolName": "Bash", "hook": "pre"}},
        }),
    );
    let request = out
        .permission_request(
            "s",
            &json!({
                "sessionId": "s",
                "title": "Run ls?",
                "description": "Reason: it lists files",
                "toolCall": {"toolCallId": "t1", "title": "Run ls", "status": "pending"},
                "options": [{"kind": "allow_once", "name": "Yes", "optionId": "allow-once"}],
            }),
        )
        .expect("a title is required");
    assert_eq!(request["title"], "Run ls?");
    assert_eq!(request["description"], "Reason: it lists files");
    assert_eq!(request["subject"]["type"], "tool_call");
    assert_eq!(request["subject"]["toolCall"]["toolCallId"], "t1");
    assert_eq!(
        request["subject"]["toolCall"]["_meta"],
        json!({"claudeCode": {"hook": "pre"}})
    );
    // A request without a title is rejected (`v2PermissionRequest`).
    assert!(out
        .permission_request("s", &json!({"toolCall": {"toolCallId": "t1"}}))
        .is_err());
}

/// A closed session's stores are forgotten (`v2/agent.ts:116-117`).
#[test]
fn t6g_forget_drops_a_sessions_stores() {
    let mut out = V2Outbound::new();
    out.translate(
        "s",
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Run ls",
            "status": "pending",
            "_meta": {"claudeCode": {"toolName": "Bash"}},
        }),
    );
    out.forget("s");
    // The call is unknown again, so a report of it names it afresh.
    let notifications = out.translate(
        "s",
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "title": "Run ls",
            "status": "completed",
            "_meta": {"claudeCode": {"toolName": "Bash"}},
        }),
    );
    assert_eq!(notifications[0]["update"]["name"], "Bash");
}
