//! Tests for ACP v2 translation modules ported from `src/tests/acp-v2.test.ts`.

use serde_json::json;

use super::permission::{
    v1_permission_response, v2_permission_presentation, v2_permission_request,
};
use super::session_update::{v2_child_state_update, v2_session_update, SESSION_PLAN_ID};
use super::terminal::{base64_encode, V2Terminals};
use super::tool_call_meta::V2ToolCallMeta;
use crate::hooks::{build_pre_tool_use_preview_update, init_hooks_payload_with_v2};

fn text(t: &str) -> Vec<serde_json::Value> {
    vec![json!({
        "type": "text",
        "text": t,
    })]
}

fn base64_str(t: &str) -> String {
    base64_encode(t.as_bytes())
}

#[test]
fn test_v2_session_update_fails_on_untranslated_kinds() {
    let update = json!({
        "sessionUpdate": "subagent_state_update",
        "subagentSessionId": "agent_n",
        "state": "failed",
    });
    let err = v2_session_update(&update).unwrap_err();
    assert!(err
        .message
        .contains("does not translate subagent_state_update session updates yet"));
}

#[test]
fn test_v2_session_update_subagent_update_passthrough() {
    let announcement = json!({
        "sessionUpdate": "subagent_update",
        "sessionId": "agent-1",
        "title": "Explore",
        "description": "Look around",
        "state": { "state": "running" },
    });
    assert_eq!(
        v2_session_update(&announcement).unwrap(),
        Some(announcement)
    );

    let message = json!({
        "sessionUpdate": "session_message",
        "messageId": "toolu_agent",
        "senderSessionId": "root",
        "recipientSessionId": "agent-1",
        "content": text("Look around"),
    });
    assert_eq!(v2_session_update(&message).unwrap(), Some(message));

    let chunk = json!({
        "sessionUpdate": "session_message_chunk",
        "messageId": "m",
        "content": text("Look")[0],
    });
    assert_eq!(v2_session_update(&chunk).unwrap(), Some(chunk));
}

#[test]
fn test_v2_session_update_key_order_byte_strings() {
    let chunk = json!({
        "messageId": "m1",
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": "hi" }
    });
    let translated = v2_session_update(&chunk).unwrap().unwrap();
    assert_eq!(
        serde_json::to_string(&translated).unwrap(),
        r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"},"messageId":"m1"}"#
    );

    let subagent = json!({
        "sessionUpdate": "subagent_update",
        "state": { "state": "running" },
        "sessionId": "a",
    });
    let translated_sub = v2_session_update(&subagent).unwrap().unwrap();
    assert_eq!(
        serde_json::to_string(&translated_sub).unwrap(),
        r#"{"sessionUpdate":"subagent_update","sessionId":"a","state":{"state":"running"}}"#
    );
}

#[test]
fn test_v2_child_state_update_subagent_work_state() {
    let failed = json!({
        "state": "idle",
        "stopReason": "error",
        "error": { "code": -32603, "message": "Overloaded" },
    });
    let update = json!({
        "sessionUpdate": "subagent_update",
        "sessionId": "agent-1",
        "state": failed.clone(),
    });
    let child = v2_child_state_update(&update).unwrap();
    assert_eq!(
        child,
        Some(json!({
            "sessionId": "agent-1",
            "update": {
                "sessionUpdate": "state_update",
                "state": "idle",
                "stopReason": "error",
                "error": { "code": -32603, "message": "Overloaded" }
            }
        }))
    );

    let no_state = json!({
        "sessionUpdate": "subagent_update",
        "sessionId": "agent-1",
        "title": "X",
    });
    assert_eq!(v2_child_state_update(&no_state).unwrap(), None);
}

#[test]
fn test_v2_session_update_fails_on_undefined_subagent_state() {
    let paused = json!({
        "sessionUpdate": "subagent_update",
        "sessionId": "agent-1",
        "state": { "state": "_paused" },
    });
    let err1 = v2_session_update(&paused).unwrap_err();
    assert!(err1
        .message
        .contains("does not translate the subagent state _paused"));

    let timeout = json!({
        "sessionUpdate": "subagent_update",
        "sessionId": "agent-1",
        "state": { "state": "idle", "stopReason": "_timeout" },
    });
    let err2 = v2_session_update(&timeout).unwrap_err();
    assert!(err2
        .message
        .contains("the subagent state idle with the stop reason _timeout"));
}

#[test]
fn test_v2_session_update_compaction_passthrough() {
    let update = json!({
        "sessionUpdate": "compaction_update",
        "compactionId": "c",
        "status": "completed",
        "summary": text("The summary."),
    });
    assert_eq!(v2_session_update(&update).unwrap(), Some(update));

    let chunk = json!({
        "sessionUpdate": "compaction_summary_chunk",
        "compactionId": "c",
        "content": { "type": "text", "text": "The" },
    });
    assert_eq!(v2_session_update(&chunk).unwrap(), Some(chunk));
}

#[test]
fn test_v2_session_update_plan_to_plan_update() {
    let entries = json!([{
        "content": "Test",
        "priority": "high",
        "status": "pending",
    }]);
    let meta = json!({ "claudeCode": { "parentToolUseId": "toolu_agent" } });
    let input = json!({
        "sessionUpdate": "plan",
        "entries": entries,
        "_meta": meta,
    });
    assert_eq!(
        v2_session_update(&input).unwrap(),
        Some(json!({
            "sessionUpdate": "plan_update",
            "plan": {
                "type": "items",
                "planId": SESSION_PLAN_ID,
                "entries": [
                    { "content": "Test", "priority": "high", "status": "pending" }
                ]
            },
            "_meta": { "claudeCode": { "parentToolUseId": "toolu_agent" } }
        }))
    );
}

#[test]
fn test_v2_session_update_plan_operations_passthrough() {
    let update = json!({
        "sessionUpdate": "plan_update",
        "plan": { "type": "markdown", "planId": "plan-mode", "content": "# Plan" },
    });
    assert_eq!(v2_session_update(&update).unwrap(), Some(update));

    let removed = json!({
        "sessionUpdate": "plan_removed",
        "planId": "plan-mode",
    });
    assert_eq!(v2_session_update(&removed).unwrap(), Some(removed));
}

#[test]
fn test_v2_tool_call_update_upsert() {
    let input = json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "t",
        "title": "Read",
        "name": "Read",
        "kind": "read",
        "status": "pending",
        "content": [{ "type": "content", "content": text("a.ts")[0] }],
        "locations": [{ "path": "/p/a.ts" }],
    });
    assert_eq!(
        v2_session_update(&input).unwrap(),
        Some(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t",
            "title": "Read",
            "name": "Read",
            "kind": "read",
            "status": "pending",
            "content": [{ "type": "content", "content": text("a.ts")[0] }],
            "locations": [{ "path": "/p/a.ts" }],
        }))
    );
}

#[test]
fn test_v2_tool_call_update_drops_null_fields() {
    let input = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "t",
        "status": "completed",
        "title": null,
        "name": null,
        "content": null,
        "locations": null,
        "rawOutput": null,
        "_meta": null,
    });
    assert_eq!(
        v2_session_update(&input).unwrap(),
        Some(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t",
            "status": "completed",
        }))
    );

    let empty_content = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "t",
        "content": [],
    });
    assert_eq!(
        v2_session_update(&empty_content).unwrap(),
        Some(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t",
            "content": [],
        }))
    );
}

#[test]
fn test_v2_tool_call_update_key_order_byte_string() {
    let input = json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "t",
        "content": [],
        "title": "x",
    });
    let translated = v2_session_update(&input).unwrap().unwrap();
    let serialized = serde_json::to_string(&translated).unwrap();
    assert_eq!(
        serialized,
        r#"{"toolCallId":"t","title":"x","content":[],"sessionUpdate":"tool_call_update"}"#
    );
}

#[test]
fn test_v2_tool_call_update_v2_diff_passthrough() {
    let diff = json!({
        "type": "diff",
        "path": "/p/a.ts",
        "operation": "update",
        "changes": [
            { "oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1, "lines": ["-a", "+b"] }
        ]
    });
    let input = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "t",
        "content": [diff.clone()],
    });
    assert_eq!(
        v2_session_update(&input).unwrap(),
        Some(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t",
            "content": [diff],
        }))
    );
}

#[test]
fn test_v2_terminal_extension_split() {
    let mut terminals = V2Terminals::new();
    let tool_call_id = "toolu_bash";

    // The first report of a streamed tool use names the terminal, without a command yet.
    let r1 = terminals.split(
        "s",
        &json!({
            "sessionUpdate": "tool_call",
            "toolCallId": tool_call_id,
            "title": "Terminal",
            "rawInput": {},
            "content": [{ "type": "terminal", "terminalId": "term" }],
            "_meta": { "claudeCode": { "toolName": "Bash" }, "terminal_info": { "terminal_id": "term" } },
        }),
    );
    assert_eq!(
        r1.terminal,
        vec![json!({ "sessionUpdate": "terminal_update", "terminalId": "term" })]
    );
    assert_eq!(
        r1.report,
        Some(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": tool_call_id,
            "title": "Terminal",
            "rawInput": {},
            "content": [{ "type": "terminal", "terminalId": "term" }],
            "_meta": { "claudeCode": { "toolName": "Bash" } },
        }))
    );

    // The command, once the input carries it, and only once.
    let refinement = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": tool_call_id,
        "rawInput": { "command": "make" },
    });
    let r2 = terminals.split("s", &refinement);
    assert_eq!(
        r2.terminal,
        vec![
            json!({ "sessionUpdate": "terminal_update", "terminalId": "term", "command": "make" })
        ]
    );
    let r3 = terminals.split("s", &refinement);
    assert_eq!(r3.terminal, Vec::<serde_json::Value>::new());

    // A chunk of output, then the output report that carries nothing else.
    let r4 = terminals.split(
        "s",
        &json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": tool_call_id,
            "_meta": {
                "claudeCode": { "parentToolUseId": "toolu_agent" },
                "terminal_output_delta": { "terminal_id": "term", "data": "bu" },
            },
        }),
    );
    assert_eq!(
        r4.terminal,
        vec![json!({
            "sessionUpdate": "terminal_output_chunk",
            "terminalId": "term",
            "data": base64_str("bu"),
        })]
    );
    assert_eq!(r4.report, None);

    // The exit, then the status.
    let r5 = terminals.split(
        "s",
        &json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": tool_call_id,
            "status": "failed",
            "_meta": {
                "claudeCode": { "toolName": "Bash" },
                "terminal_output": { "terminal_id": "term", "data": "build" },
                "terminal_exit": { "terminal_id": "term", "exit_code": 2, "signal": "SIGTERM" },
            },
        }),
    );
    assert_eq!(
        r5.terminal,
        vec![json!({
            "sessionUpdate": "terminal_update",
            "terminalId": "term",
            "output": { "data": base64_str("build") },
            "exitStatus": { "exitCode": 2, "signal": "SIGTERM" },
        })]
    );
    assert_eq!(
        r5.report,
        Some(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": tool_call_id,
            "status": "failed",
            "_meta": { "claudeCode": { "toolName": "Bash" } },
        }))
    );

    // The command exited: a later report updates no terminal.
    let hook = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": tool_call_id,
        "rawInput": { "command": "make all" },
        "_meta": { "claudeCode": { "toolResponse": { "stdout": "build" } } },
    });
    let r6 = terminals.split("s", &hook);
    assert_eq!(r6.terminal, Vec::<serde_json::Value>::new());
    assert_eq!(r6.report, Some(hook));
}

#[test]
fn test_v2_terminal_null_keys_and_repeated_command_with_output() {
    let mut terminals = V2Terminals::new();
    let tool_call_id = "term_tool";

    // 1. Initial tool call with terminal_info and initial command
    let init = json!({
        "sessionUpdate": "tool_call",
        "toolCallId": tool_call_id,
        "rawInput": { "command": "cargo test" },
        "_meta": {
            "terminal_info": { "terminal_id": "t1" },
        },
    });
    let r1 = terminals.split("s", &init);
    assert_eq!(
        r1.terminal,
        vec![json!({
            "sessionUpdate": "terminal_update",
            "terminalId": "t1",
            "command": "cargo test",
        })]
    );

    // 2. Report with null terminal keys (null output, null delta, null exit)
    let null_update = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": tool_call_id,
        "status": "pending",
        "_meta": {
            "terminal_output": null,
            "terminal_output_delta": null,
            "terminal_exit": null,
        },
    });
    let r2 = terminals.split("s", &null_update);
    // Must emit no terminal updates and command must stay running!
    assert_eq!(r2.terminal, Vec::<serde_json::Value>::new());

    // 3. Repeated command with output (kills mutant M3: command must NOT be emitted again)
    let output_update = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": tool_call_id,
        "rawInput": { "command": "cargo test" },
        "_meta": {
            "terminal_output": { "terminal_id": "t1", "data": "running 1 test" },
        },
    });
    let r3 = terminals.split("s", &output_update);
    assert_eq!(
        r3.terminal,
        vec![json!({
            "sessionUpdate": "terminal_update",
            "terminalId": "t1",
            "output": { "data": base64_str("running 1 test") },
        })]
    );
    // Explicitly assert "command" is NOT in the terminal_update
    assert!(r3.terminal[0].get("command").is_none());
}

#[test]
fn test_v2_terminal_exit_omits_unknown_exit_code() {
    let mut terminals = V2Terminals::new();
    terminals.split(
        "s",
        &json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "toolu_bash",
            "title": "Terminal",
            "_meta": { "terminal_info": { "terminal_id": "term" } },
        }),
    );
    let split = terminals.split(
        "s",
        &json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "toolu_bash",
            "status": "failed",
            "_meta": { "terminal_exit": { "terminal_id": "term", "exit_code": null, "signal": null } },
        }),
    );
    assert_eq!(
        split.terminal,
        vec![json!({
            "sessionUpdate": "terminal_update",
            "terminalId": "term",
            "exitStatus": {},
        })]
    );
}

#[test]
fn test_v2_terminal_forgets_closed_session() {
    let mut terminals = V2Terminals::new();
    terminals.split(
        "closed",
        &json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "toolu_bash",
            "title": "Terminal",
            "_meta": { "terminal_info": { "terminal_id": "term" } },
        }),
    );
    terminals.split(
        "open",
        &json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "toolu_bash",
            "title": "Terminal",
            "_meta": { "terminal_info": { "terminal_id": "term" } },
        }),
    );

    terminals.forget("closed");

    let cmd_closed = terminals
        .split(
            "closed",
            &json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "toolu_bash",
                "rawInput": { "command": "ls" },
            }),
        )
        .terminal;
    let cmd_open = terminals
        .split(
            "open",
            &json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "toolu_bash",
                "rawInput": { "command": "ls" },
            }),
        )
        .terminal;

    assert_eq!(cmd_closed, Vec::<serde_json::Value>::new());
    assert_eq!(cmd_open.len(), 1);
}

fn rep(meta: Option<serde_json::Value>, fields: serde_json::Value) -> serde_json::Value {
    let mut map = match fields {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    if !map.contains_key("toolCallId") {
        map.insert("toolCallId".to_string(), json!("t"));
    }
    if let Some(m) = meta {
        map.insert("_meta".to_string(), m);
    }
    serde_json::Value::Object(map)
}

#[test]
fn test_v2_tool_call_meta_names_tool_once() {
    let mut meta = V2ToolCallMeta::new();
    let r1 = meta.apply(
        "s",
        &rep(
            Some(json!({ "claudeCode": { "toolName": "memory_recall" } })),
            json!({}),
        ),
        true,
        false,
    );
    assert_eq!(
        r1,
        vec![json!({ "toolCallId": "t", "name": "memory_recall" })]
    );

    let r2 = meta.apply(
        "s",
        &rep(
            Some(json!({ "claudeCode": { "toolName": "Bash" } })),
            json!({}),
        ),
        false,
        false,
    );
    assert_eq!(r2, vec![json!({ "toolCallId": "t" })]);

    // A report's own name wins.
    let r3 = meta.apply(
        "s",
        &rep(
            Some(json!({ "claudeCode": { "toolName": "Bash" } })),
            json!({ "name": "Read" }),
        ),
        true,
        false,
    );
    assert_eq!(r3, vec![json!({ "toolCallId": "t", "name": "Read" })]);
}

#[test]
fn test_v2_tool_call_meta_replays_claudecode_keys() {
    let mut meta = V2ToolCallMeta::new();
    let denial = json!({ "toolResponse": { "decisionReasonType": "rule" } });
    let r1 = meta.apply(
        "s",
        &rep(Some(json!({ "claudeCode": denial })), json!({})),
        true,
        false,
    );
    assert_eq!(
        r1,
        vec![rep(Some(json!({ "claudeCode": denial })), json!({}))]
    );

    // The client replaces `claudeCode` whole, so all of it goes again.
    let r2 = meta.apply(
        "s",
        &rep(
            Some(json!({ "claudeCode": { "nonExecutionKind": "permission-rule" } })),
            json!({}),
        ),
        false,
        false,
    );
    assert_eq!(
        r2,
        vec![rep(
            Some(json!({
                "claudeCode": {
                    "toolResponse": { "decisionReasonType": "rule" },
                    "nonExecutionKind": "permission-rule",
                }
            })),
            json!({})
        )]
    );
}

#[test]
fn test_v2_tool_call_meta_sends_only_changed_toplevel_keys() {
    let mut meta = V2ToolCallMeta::new();
    meta.apply(
        "s",
        &rep(
            Some(json!({ "claudeCode": { "toolResponse": { "elapsedTimeSeconds": 1 } }, "flag": { "a": 1 } })),
            json!({}),
        ),
        true,
        false,
    );
    let r1 = meta.apply(
        "s",
        &rep(
            Some(json!({ "claudeCode": { "toolResponse": { "status": "ok" } } })),
            json!({}),
        ),
        false,
        false,
    );
    assert_eq!(
        r1,
        vec![rep(
            Some(json!({ "claudeCode": { "toolResponse": { "status": "ok" } } })),
            json!({})
        )]
    );

    let r2 = meta.apply(
        "s",
        &rep(Some(json!({ "flag": { "b": 2 } })), json!({})),
        false,
        false,
    );
    assert_eq!(
        r2,
        vec![rep(Some(json!({ "flag": { "b": 2 } })), json!({}))]
    );
}

#[test]
fn test_v2_tool_call_meta_removes_null_keys() {
    let mut meta = V2ToolCallMeta::new();
    meta.apply(
        "s",
        &rep(Some(json!({ "flag": 1, "other": 2 })), json!({})),
        true,
        false,
    );
    let r1 = meta.apply(
        "s",
        &rep(Some(json!({ "flag": null })), json!({})),
        false,
        false,
    );
    assert_eq!(r1, vec![rep(Some(json!({ "flag": null })), json!({}))]);

    // A key the client does not hold needs no removal.
    let r2 = meta.apply(
        "s",
        &rep(Some(json!({ "flag": null })), json!({})),
        false,
        false,
    );
    assert_eq!(r2, vec![json!({ "toolCallId": "t" })]);
}

#[test]
fn test_v2_tool_call_meta_sends_no_meta_when_unchanged() {
    let mut meta = V2ToolCallMeta::new();
    meta.apply(
        "s",
        &rep(
            Some(json!({ "claudeCode": { "toolName": "Bash", "parentToolUseId": "p" } })),
            json!({}),
        ),
        true,
        false,
    );
    let r = meta.apply(
        "s",
        &rep(
            Some(json!({ "claudeCode": { "toolName": "Bash", "parentToolUseId": "p" } })),
            json!({ "status": "completed" }),
        ),
        false,
        false,
    );
    assert_eq!(r, vec![json!({ "toolCallId": "t", "status": "completed" })]);
}

#[test]
fn test_v2_tool_call_meta_stringify_equality() {
    let mut meta = V2ToolCallMeta::new();

    // 1. Key order change in object triggers patch:
    // Held {"o":{"a":1,"b":2}}, then report has {"o":{"b":2,"a":1}}
    let r1 = meta.apply(
        "s",
        &json!({
            "toolCallId": "u",
            "_meta": {
                "o": { "a": 1, "b": 2 },
            },
        }),
        true,
        false,
    );
    assert!(r1[0].get("_meta").is_some());

    let r2 = meta.apply(
        "s",
        &json!({
            "toolCallId": "u",
            "_meta": {
                "o": { "b": 2, "a": 1 },
            },
        }),
        false,
        false,
    );
    assert_eq!(
        r2,
        vec![json!({
            "toolCallId": "u",
            "_meta": {
                "o": { "b": 2, "a": 1 },
            },
        })]
    );

    // 2. Integral float equality (1.0 vs 1) normalises and sends no patch:
    let mut meta_num = V2ToolCallMeta::new();
    meta_num.apply(
        "s",
        &json!({
            "toolCallId": "t",
            "_meta": { "k": 1 },
        }),
        true,
        false,
    );
    let report_float: serde_json::Value =
        serde_json::from_str(r#"{"toolCallId":"t","_meta":{"k":1.0}}"#).unwrap();
    let r_num = meta_num.apply("s", &report_float, false, false);
    assert_eq!(r_num, vec![json!({ "toolCallId": "t" })]);
}

#[test]
fn test_v2_tool_call_meta_subject_carries_full_meta() {
    let mut meta = V2ToolCallMeta::new();
    meta.apply(
        "s",
        &rep(
            Some(json!({ "claudeCode": { "toolName": "Bash", "parentToolUseId": "p" } })),
            json!({}),
        ),
        true,
        false,
    );
    let s1 = meta.subject(
        "s",
        &rep(
            Some(json!({ "claudeCode": { "toolName": "Bash" } })),
            json!({}),
        ),
    );
    assert_eq!(
        s1,
        rep(
            Some(json!({ "claudeCode": { "parentToolUseId": "p" } })),
            json!({})
        )
    );

    // Its own keys are stored, since the subject updates the tool call.
    meta.subject(
        "s",
        &rep(
            Some(json!({ "claudeCode": { "mcpServer": { "name": "docs" } } })),
            json!({}),
        ),
    );
    let r = meta.apply(
        "s",
        &rep(
            Some(json!({ "claudeCode": { "nonExecutionKind": "x" } })),
            json!({}),
        ),
        false,
        false,
    );
    assert_eq!(
        r,
        vec![rep(
            Some(json!({
                "claudeCode": {
                    "parentToolUseId": "p",
                    "mcpServer": { "name": "docs" },
                    "nonExecutionKind": "x",
                }
            })),
            json!({})
        )]
    );
}

#[test]
fn test_v2_tool_call_meta_session_isolation_and_forget() {
    let mut meta = V2ToolCallMeta::new();
    let started = json!({ "claudeCode": { "parentToolUseId": "p" } });
    meta.apply(
        "closed",
        &rep(Some(started.clone()), json!({})),
        true,
        false,
    );
    meta.apply("open", &rep(Some(started), json!({})), true, false);
    meta.forget("closed");

    let failed = rep(
        Some(json!({ "claudeCode": { "nonExecutionKind": "x" } })),
        json!({}),
    );
    let r_closed = meta.apply("closed", &failed, false, false);
    let r_open = meta.apply("open", &failed, false, false);

    assert_eq!(r_closed[0]["_meta"], failed["_meta"]);
    assert_eq!(
        r_open[0]["_meta"],
        json!({ "claudeCode": { "parentToolUseId": "p", "nonExecutionKind": "x" } })
    );
}

#[test]
fn test_v2_tool_call_meta_resets_on_start() {
    let mut meta = V2ToolCallMeta::new();
    meta.apply(
        "s",
        &rep(
            Some(json!({ "claudeCode": { "toolName": "Bash", "parentToolUseId": "p" } })),
            json!({}),
        ),
        true,
        false,
    );
    let r = meta.apply(
        "s",
        &rep(
            Some(json!({ "claudeCode": { "toolName": "Read" } })),
            json!({}),
        ),
        true,
        false,
    );
    assert_eq!(
        r,
        vec![json!({ "toolCallId": "t", "name": "Read", "_meta": { "claudeCode": null } })]
    );
}

#[test]
fn test_v2_tool_call_meta_clears_on_replaying_start() {
    let mut meta = V2ToolCallMeta::new();
    let started = rep(
        Some(json!({ "claudeCode": { "parentToolUseId": "p" } })),
        json!({ "title": "ls" }),
    );
    let r1 = meta.apply("s", &started, true, true);
    assert_eq!(
        r1,
        vec![
            json!({ "toolCallId": "t", "title": "ls", "_meta": null }),
            json!({ "toolCallId": "t", "_meta": { "claudeCode": { "parentToolUseId": "p" } } }),
        ]
    );

    // A tool call without `_meta` needs only the clear.
    let r2 = meta.apply("s", &rep(None, json!({ "toolCallId": "u" })), true, true);
    assert_eq!(r2, vec![json!({ "toolCallId": "u", "_meta": null })]);

    // A tool call this connection reported holds what it sent.
    let r3 = meta.apply("s", &rep(None, json!({ "title": "ls" })), true, true);
    assert_eq!(
        r3,
        vec![json!({ "toolCallId": "t", "title": "ls", "_meta": { "claudeCode": null } })]
    );
}

#[test]
fn test_v2_permission_request_requires_title() {
    let request = json!({
        "sessionId": "s",
        "toolCall": { "toolCallId": "t", "title": "ls" },
        "options": [{ "optionId": "allow-once", "name": "Yes", "kind": "allow_once" }],
    });
    let err = v2_permission_request(&request).unwrap_err();
    assert!(err
        .message
        .contains("An ACP v2 permission request needs a title"));

    let with_title = json!({
        "sessionId": "s",
        "title": "Run ls?",
        "toolCall": { "toolCallId": "t", "title": "ls" },
        "options": [{ "optionId": "allow-once", "name": "Yes", "kind": "allow_once" }],
    });
    assert_eq!(
        v2_permission_request(&with_title).unwrap(),
        json!({
            "sessionId": "s",
            "title": "Run ls?",
            "subject": { "type": "tool_call", "toolCall": { "toolCallId": "t", "title": "ls" } },
            "options": [{ "optionId": "allow-once", "name": "Yes", "kind": "allow_once" }],
        })
    );
}

#[test]
fn test_v2_permission_response_non_selected_becomes_cancelled() {
    assert_eq!(
        v1_permission_response(&json!({ "outcome": { "outcome": "_later" } })),
        json!({ "outcome": { "outcome": "cancelled" } })
    );
    assert_eq!(
        v1_permission_response(&json!({ "outcome": { "outcome": "selected", "optionId": "o" } })),
        json!({ "outcome": { "outcome": "selected", "optionId": "o" } })
    );
}

#[test]
fn test_p61_p62_permission_request_and_response() {
    let req = json!({
        "sessionId": "s-1",
        "title": "Edit src/lib.rs",
        "description": "Apply patch to lib.rs",
        "toolCall": {
            "sessionUpdate": "tool_call",
            "toolCallId": "call_edit",
            "title": "Edit lib.rs",
            "content": []
        },
        "options": [
            { "optionId": "opt-allow", "name": "Allow", "kind": "allow_once" },
            { "optionId": "opt-deny", "name": "Deny", "kind": "deny" }
        ],
        "_meta": { "custom": "data" }
    });
    let v2_req = v2_permission_request(&req).unwrap();
    assert_eq!(v2_req["sessionId"], "s-1");
    assert_eq!(v2_req["title"], "Edit src/lib.rs");
    assert_eq!(v2_req["description"], "Apply patch to lib.rs");
    assert_eq!(v2_req["_meta"]["custom"], "data");
    assert_eq!(v2_req["subject"]["type"], "tool_call");
    assert_eq!(v2_req["subject"]["toolCall"]["toolCallId"], "call_edit");
    assert_eq!(v2_req["subject"]["toolCall"]["title"], "Edit lib.rs");

    // P-62: responses
    let resp_cancelled = json!({ "outcome": { "outcome": "custom_cancelled" } });
    assert_eq!(
        v1_permission_response(&resp_cancelled),
        json!({ "outcome": { "outcome": "cancelled" } })
    );

    let resp_selected = json!({
        "outcome": { "outcome": "selected", "optionId": "opt-allow" },
        "_meta": { "timestamp": 123 }
    });
    assert_eq!(
        v1_permission_response(&resp_selected),
        json!({
            "outcome": { "outcome": "selected", "optionId": "opt-allow" },
            "_meta": { "timestamp": 123 }
        })
    );
}

#[test]
fn test_v2_tool_call_rejects_v1_diff_and_passes_terminal() {
    let v1_diff = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "t",
        "content": [{ "type": "diff", "path": "/p/a.ts", "oldText": "a", "newText": "b" }],
    });
    let err = v2_session_update(&v1_diff).unwrap_err();
    assert!(err
        .message
        .contains("An ACP v2 client cannot take a v1 diff"));

    let terminal = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "t",
        "content": [{ "type": "terminal", "terminalId": "term" }],
    });
    let res = v2_session_update(&terminal).unwrap().unwrap();
    assert_eq!(
        res["content"],
        json!([{ "type": "terminal", "terminalId": "term" }])
    );
}

#[test]
fn test_v2_session_update_chunk_requires_message_id() {
    let update = json!({
        "sessionUpdate": "agent_message_chunk",
        "content": text("hi")[0],
    });
    let err = v2_session_update(&update).unwrap_err();
    assert!(err
        .message
        .contains("An ACP v2 agent_message_chunk needs a messageId"));
}

#[test]
fn test_u85_current_mode_update_dropped() {
    let update = json!({
        "sessionUpdate": "current_mode_update",
        "currentModeId": "code",
    });
    assert_eq!(v2_session_update(&update).unwrap(), None);
}

#[test]
fn test_p51_permission_presentation_fields() {
    let input = json!({ "command": "echo test" });
    let (title, description) = v2_permission_presentation(
        "Bash",
        &input,
        None,
        "echo test",
        Some("user command approval"),
    );
    assert_eq!(title, "echo test");
    assert_eq!(
        description,
        Some("Reason: user command approval".to_string())
    );

    let (title_plan, desc_plan) =
        v2_permission_presentation("ExitPlanMode", &json!({}), None, "Exit Plan", None);
    assert_eq!(title_plan, "Ready to code?");
    assert_eq!(desc_plan, None);
}

#[test]
fn test_t105_pre_tool_use_preview_hook() {
    let hooks_v1 = init_hooks_payload_with_v2(false);
    assert!(hooks_v1.get("PreToolUse").is_none());
    assert_eq!(hooks_v1["PostToolUse"][0]["hookCallbackIds"][0], "hook_0");
    assert_eq!(
        hooks_v1["PostModelSwitch"][0]["hookCallbackIds"][0],
        "hook_1"
    );
    assert_eq!(hooks_v1["PostCompact"][0]["hookCallbackIds"][0], "hook_2");
    assert_eq!(hooks_v1["TaskCreated"][0]["hookCallbackIds"][0], "hook_3");
    assert_eq!(hooks_v1["TaskCompleted"][0]["hookCallbackIds"][0], "hook_4");

    let hooks_v2 = init_hooks_payload_with_v2(true);
    let pre_tool = hooks_v2
        .get("PreToolUse")
        .expect("PreToolUse registered on v2");
    assert_eq!(pre_tool[0]["matcher"], "Edit|Write");
    assert_eq!(pre_tool[0]["hookCallbackIds"][0], "hook_0");
    assert_eq!(hooks_v2["PostToolUse"][0]["hookCallbackIds"][0], "hook_1");
    assert_eq!(
        hooks_v2["PostModelSwitch"][0]["hookCallbackIds"][0],
        "hook_2"
    );
    assert_eq!(hooks_v2["PostCompact"][0]["hookCallbackIds"][0], "hook_3");
    assert_eq!(hooks_v2["TaskCreated"][0]["hookCallbackIds"][0], "hook_4");
    assert_eq!(hooks_v2["TaskCompleted"][0]["hookCallbackIds"][0], "hook_5");

    // Non-edit/write tool produces no preview
    assert!(
        build_pre_tool_use_preview_update("toolu_bash", "Bash", &json!({}), None, None).is_none()
    );

    // Edit tool with temp file produces v2 diff preview with changes and parentToolUseId
    let temp_dir = std::env::temp_dir().join(format!("t105-edit-preview-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_dir);
    let file_path = temp_dir.join("code.ts");
    std::fs::write(&file_path, "let x = 1;\nlet y = 2;\n").unwrap();

    let edit_input = json!({
        "file_path": file_path.to_str().unwrap(),
        "old_string": "let x = 1;\n",
        "new_string": "let x = 100;\n",
    });

    let update = build_pre_tool_use_preview_update(
        "toolu_edit_1",
        "Edit",
        &edit_input,
        Some(&temp_dir),
        Some("toolu_parent_99"),
    )
    .expect("preview update generated for Edit");

    assert_eq!(update["sessionUpdate"], "tool_call_update");
    assert_eq!(update["toolCallId"], "toolu_edit_1");
    let content = update["content"].as_array().expect("content is array");
    assert!(!content.is_empty());
    assert_eq!(content[0]["type"], "diff");
    assert!(
        content[0]["changes"].is_array(),
        "content must be a v2 diff with changes"
    );
    assert_eq!(
        update["_meta"]["claudeCode"]["parentToolUseId"],
        "toolu_parent_99"
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}

/// T8C (`tools.test.ts` > "reports an aborted run as cancelled to a v2 client,
/// and as failed to every other client"): an error result whose
/// `_meta.claudeCode.nonExecutionKind` says the CLI aborted the run becomes v2
/// `cancelled`; a refused run stays `failed`, and the kind stays in `_meta`.
#[test]
fn t8c_aborted_result_is_cancelled_on_v2() {
    let report = |kind: &str, status: &str| {
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "toolu_bash",
            "status": status,
            "content": [{"type": "content", "content": {"type": "text", "text": "x"}}],
            "_meta": {"claudeCode": {"toolName": "Bash", "nonExecutionKind": kind}},
        })
    };
    for kind in ["interrupted", "cancelled"] {
        let translated = v2_session_update(&report(kind, "failed")).unwrap().unwrap();
        assert_eq!(translated["status"], "cancelled", "{kind}");
        assert_eq!(
            translated["_meta"]["claudeCode"]["nonExecutionKind"], kind,
            "the kind stays on v2"
        );
    }
    // A refused tool did not run because of a decision: it failed.
    for kind in ["user-rejected", "permission-rule", "automode-blocked"] {
        let translated = v2_session_update(&report(kind, "failed")).unwrap().unwrap();
        assert_eq!(translated["status"], "failed", "{kind}");
    }
    // A successful result is never rewritten, whatever the sidecar says.
    let translated = v2_session_update(&report("interrupted", "completed"))
        .unwrap()
        .unwrap();
    assert_eq!(translated["status"], "completed");
}
