//! Integration tests for Turn 6 Lane C: ACP v2 translation modules (F40).
//!
//! Tests the pure translation functions and stateful stores across:
//! - v2 session updates (U-83, U-84, U-85, U-86, U-104, Q-87)
//! - v2 tool call reports (T-101, Q-91)
//! - v2 tool call meta tracker (T-103, Q-90)
//! - v2 display terminals (T-102, Q-89)
//! - v2 permission translation (P-51, P-61, P-62, Q-85)
//! - v2 PreToolUse exact-patch preview hook (T-105)

use serde_json::json;

use claude_agent_acp_rs::hooks::{build_pre_tool_use_preview_update, init_hooks_payload_with_v2};
use claude_agent_acp_rs::v2::{
    is_v2_diff, v1_permission_response, v2_permission_presentation, v2_permission_request,
    v2_session_update, v2_tool_call_content, v2_tool_call_update, TerminalSplit, V2Terminals,
    V2ToolCallMeta, SESSION_PLAN_ID,
};

#[test]
fn t6c_integration_session_update_pure_translation() {
    // Untranslated kinds error
    let unknown = json!({ "sessionUpdate": "custom_extension_event" });
    assert!(v2_session_update(&unknown).is_err());

    // Current mode update dropped on v2
    let mode = json!({ "sessionUpdate": "current_mode_update", "currentModeId": "plan" });
    assert_eq!(v2_session_update(&mode).unwrap(), None);

    // Plan to plan_update
    let plan = json!({
        "sessionUpdate": "plan",
        "entries": [{ "content": "Task 1", "priority": "high", "status": "pending" }],
    });
    let translated_plan = v2_session_update(&plan).unwrap().unwrap();
    assert_eq!(translated_plan["sessionUpdate"], "plan_update");
    assert_eq!(translated_plan["plan"]["planId"], SESSION_PLAN_ID);
    assert_eq!(translated_plan["plan"]["type"], "items");

    // Compaction pass-through
    let compaction = json!({
        "sessionUpdate": "compaction_update",
        "compactionId": "cmp_1",
        "status": "completed",
    });
    assert_eq!(v2_session_update(&compaction).unwrap(), Some(compaction));
}

#[test]
fn t6c_integration_tool_call_form_and_v2_diff() {
    let report = json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "t1",
        "title": "Edit main.rs",
        "name": "Edit",
        "status": "pending",
        "rawOutput": null,
        "content": [
            { "type": "terminal", "terminalId": "term_1" }
        ],
    });
    let v2_upd = v2_tool_call_update(&report).unwrap();
    assert_eq!(v2_upd["sessionUpdate"], "tool_call_update");
    assert_eq!(v2_upd["toolCallId"], "t1");
    assert!(v2_upd.get("rawOutput").is_none());

    // v2 diff passes
    let diff = json!({
        "type": "diff",
        "path": "/src/main.rs",
        "operation": "update",
        "changes": [{ "oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 2, "lines": ["-a", "+b", "+c"] }],
    });
    assert!(is_v2_diff(&diff));
    assert_eq!(v2_tool_call_content(&diff).unwrap(), diff);

    // v1 diff rejected
    let v1_diff = json!({ "type": "diff", "path": "/a.txt", "oldText": "old", "newText": "new" });
    assert!(!is_v2_diff(&v1_diff));
    assert!(v2_tool_call_content(&v1_diff).is_err());
}

#[test]
fn t6c_integration_tool_call_meta_store() {
    let mut store = V2ToolCallMeta::new();
    let rep1 = json!({
        "toolCallId": "tc_1",
        "_meta": {
            "claudeCode": { "toolName": "Bash", "parentToolUseId": "agent_0" },
            "flag": 1,
        }
    });
    let out1 = store.apply("sess_1", &rep1, true, false);
    assert_eq!(out1.len(), 1);
    assert_eq!(out1[0]["name"], "Bash");
    assert_eq!(out1[0]["_meta"]["flag"], 1);

    // Subject carries full meta
    let subj = store.subject("sess_1", &json!({ "toolCallId": "tc_1" }));
    assert_eq!(subj["_meta"]["claudeCode"]["parentToolUseId"], "agent_0");
    assert_eq!(subj["_meta"]["flag"], 1);

    // Session forget
    store.forget("sess_1");
    let out_after_forget = store.apply("sess_1", &json!({ "toolCallId": "tc_1" }), false, false);
    assert!(out_after_forget[0].get("_meta").is_none());
}

#[test]
fn t6c_integration_display_terminals_registry() {
    let mut terminals = V2Terminals::new();
    let r = json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "call_bash",
        "rawInput": { "command": "cargo check" },
        "_meta": {
            "terminal_info": { "terminal_id": "term_99" },
            "terminal_output": { "terminal_id": "term_99", "data": "Compiling..." },
            "terminal_exit": { "terminal_id": "term_99", "exit_code": 0, "signal": null },
        }
    });
    let split: TerminalSplit = terminals.split("s1", &r);
    assert_eq!(split.terminal.len(), 1);
    assert_eq!(split.terminal[0]["sessionUpdate"], "terminal_update");
    assert_eq!(split.terminal[0]["terminalId"], "term_99");
    assert_eq!(split.terminal[0]["command"], "cargo check");
    assert_eq!(split.terminal[0]["exitStatus"]["exitCode"], 0);

    // After exit, running command is cleared
    let post_exit_report = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "t_cmd",
        "rawInput": { "command": "cargo check" },
    });
    let post_exit_split = terminals.split("s1", &post_exit_report);
    assert!(
        post_exit_split.terminal.is_empty(),
        "After exit, running command must be cleared so subsequent reports produce no terminal updates"
    );

    terminals.forget("s1");
}

#[test]
fn t6c_integration_permission_translation() {
    // P-61: title required
    let req_invalid = json!({ "sessionId": "s", "toolCall": { "toolCallId": "t" } });
    assert!(v2_permission_request(&req_invalid).is_err());

    let req_valid = json!({
        "sessionId": "s",
        "title": "Execute Bash?",
        "description": "Run shell command",
        "toolCall": { "toolCallId": "t", "title": "echo 1" },
        "options": [{ "optionId": "allow", "name": "Allow", "kind": "allow_once" }],
    });
    let v2_req = v2_permission_request(&req_valid).unwrap();
    assert_eq!(v2_req["title"], "Execute Bash?");
    assert_eq!(v2_req["description"], "Run shell command");
    assert_eq!(v2_req["subject"]["type"], "tool_call");

    // P-62: non-selected outcome cancelled
    let v2_resp_later = json!({ "outcome": { "outcome": "postpone" } });
    assert_eq!(
        v1_permission_response(&v2_resp_later)["outcome"]["outcome"],
        "cancelled"
    );

    let v2_resp_sel = json!({ "outcome": { "outcome": "selected", "optionId": "allow" } });
    assert_eq!(
        v1_permission_response(&v2_resp_sel)["outcome"]["outcome"],
        "selected"
    );

    // P-51: presentation helper
    let (title, desc) =
        v2_permission_presentation("Bash", &json!({}), None, "ls -la", Some("check directory"));
    assert_eq!(title, "ls -la");
    assert_eq!(desc, Some("Reason: check directory".to_string()));
}

#[test]
fn t6c_integration_hooks_v2_preview() {
    let hooks_payload = init_hooks_payload_with_v2(true);
    let pre_tool = hooks_payload.get("PreToolUse").expect("PreToolUse present");
    assert_eq!(pre_tool[0]["matcher"], "Edit|Write");
    assert_eq!(pre_tool[0]["hookCallbackIds"][0], "hook_0");
    assert_eq!(
        hooks_payload["PostToolUse"][0]["hookCallbackIds"][0],
        "hook_1"
    );

    let non_file_update =
        build_pre_tool_use_preview_update("t_bash", "Bash", &json!({}), None, None);
    assert!(non_file_update.is_none());

    let temp_dir = std::env::temp_dir().join(format!("t6c-edit-preview-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&temp_dir);
    let file_path = temp_dir.join("file.rs");
    std::fs::write(&file_path, "fn main() {}\n").unwrap();

    let edit_input = json!({
        "file_path": file_path.to_str().unwrap(),
        "old_string": "fn main() {}\n",
        "new_string": "fn main() { println!(\"hi\"); }\n",
    });

    let preview = build_pre_tool_use_preview_update(
        "toolu_edit_2",
        "Edit",
        &edit_input,
        Some(&temp_dir),
        Some("toolu_parent_0"),
    )
    .expect("Edit preview generated");

    assert_eq!(preview["sessionUpdate"], "tool_call_update");
    assert_eq!(preview["toolCallId"], "toolu_edit_2");
    assert!(preview["content"][0]["changes"].is_array());
    assert_eq!(
        preview["_meta"]["claudeCode"]["parentToolUseId"],
        "toolu_parent_0"
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}
