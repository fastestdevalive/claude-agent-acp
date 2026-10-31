//! T7C tests for the F9 field tracker and the AIR rendering choices (T-03,
//! T-04, T-46, T-59, T-60, T-85, T-86, U-25, U-30, U-32, U-36), ported from
//! `src/tests/field-tracker.test.ts`, `src/tests/tool-call-contract.test.ts`
//! and `src/tests/tools.test.ts`. The behaviours are driven through the real
//! mapping path (`map_consolidated_assistant` / `map_user_frame`) so that each
//! test exercises what the running agent emits.

use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    SessionUpdate, ToolCallContent, ToolCallStatus, ToolCallUpdateFields, ToolKind,
};
use serde_json::{json, Value};

use crate::client_caps::ClientCapabilities;
use crate::map::{
    map_consolidated_assistant, map_stream_event, map_user_frame, MapState, TrackedToolCall,
};
use crate::tools::ToolShape;

/// An AIR client with terminal snapshots and no AIR capabilities: it gets the
/// contract and a display copy of the input (`tool-call-contract.test.ts:21-23`).
fn terminal_air() -> ClientCapabilities {
    ClientCapabilities::from_value(Some(&json!({
        "_meta": {
            "terminal_output": true,
            "jetbrains": {"air": {"version": 1, "capabilities": []}}
        }
    })))
}

/// AIR: terminal deltas, and it renders `rawInput` itself.
fn air() -> ClientCapabilities {
    ClientCapabilities::from_value(Some(&json!({
        "_meta": {
            "terminal_output": true,
            "terminal_output_delta": true,
            "jetbrains": {"air": {
                "version": 1,
                "capabilities": ["diffPatch", "rawInputRendering", "planFile"]
            }}
        }
    })))
}

/// An AIR client with neither a terminal nor AIR capabilities.
fn plain_air() -> ClientCapabilities {
    ClientCapabilities::from_value(Some(&json!({
        "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": []}}}
    })))
}

/// A client without `_meta.jetbrains.air`.
fn plain() -> ClientCapabilities {
    ClientCapabilities::default()
}

fn state_with(capabilities: ClientCapabilities, cwd: &str) -> MapState {
    let mut state = MapState::default();
    state.set_client_caps(capabilities.clone());
    state.set_diff_patch(capabilities.diff_patch);
    if capabilities.air_client {
        state.configure_air(PathBuf::from(cwd));
    } else {
        state.set_cwd(PathBuf::from(cwd));
    }
    state
}

fn assistant_frame(id: &str, name: &str, input: Value) -> Value {
    json!({
        "type": "assistant",
        "parent_tool_use_id": null,
        "message": {"role": "assistant", "content": [
            {"type": "tool_use", "id": id, "name": name, "input": input}
        ]}
    })
}

/// The `tool_result` echo of a `user` frame; `structured` is the frame's
/// `tool_use_result` (`tool-call-contract.test.ts:59-69`).
fn user_frame(id: &str, content: Value, is_error: bool, structured: Option<Value>) -> Value {
    let mut frame = json!({
        "type": "user",
        "parent_tool_use_id": null,
        "message": {"role": "user", "content": [
            {
                "type": "tool_result",
                "tool_use_id": id,
                "content": content,
                "is_error": is_error
            }
        ]}
    });
    if let Some(structured) = structured {
        frame["tool_use_result"] = structured;
    }
    frame
}

/// The JSON a `SessionUpdate` reaches the client as, with the defaults the
/// session materialises on emit (`materialize_tool_call_defaults`).
fn wire(update: &SessionUpdate) -> Value {
    let mut value = serde_json::to_value(update).unwrap();
    crate::map::materialize_tool_call_defaults(&mut value);
    crate::map::materialize_diff_old_text(&mut value);
    value
}

struct Report {
    call: Value,
    updates: Vec<Value>,
}

/// `report()` of `tool-call-contract.test.ts:44-72`: the `tool_call` of a
/// tool_use, and the updates of its result.
fn report(
    capabilities: ClientCapabilities,
    name: &str,
    input: Value,
    result: Option<(Value, bool, Option<Value>)>,
) -> Report {
    let mut state = state_with(capabilities, "/work");
    let call_updates = map_consolidated_assistant(&assistant_frame("t", name, input), &mut state);
    let call = wire(&call_updates[0]);
    let updates = match result {
        Some((content, is_error, structured)) => {
            // `report()` of the upstream test passes no `toolCallFields`: the
            // renderer output is asserted before the changed-field filter.
            state.clear_tool_tracker();
            map_user_frame(&user_frame("t", content, is_error, structured), &mut state)
                .iter()
                .map(wire)
                .collect()
        }
        None => Vec::new(),
    };
    Report { call, updates }
}

// ---------------------------------------------------------------------------
// field-tracker.test.ts > ToolCallFieldTracker
// ---------------------------------------------------------------------------

/// `field-tracker.test.ts` > "ToolCallFieldTracker" > "removes unchanged fields
/// and drops an update with nothing new".
#[test]
fn t7c_removes_unchanged_fields_and_drops_an_update_with_nothing_new() {
    let mut state = MapState::default();
    state.seed_tool_tracker(
        "t".to_string(),
        TrackedToolCall::initial(
            ToolKind::Read,
            "Read a.ts".to_string(),
            Vec::new(),
            Vec::new(),
            Some(json!({"file_path": "/a.ts"})),
        ),
    );
    let mut meta = serde_json::Map::new();
    meta.insert("claudeCode".into(), json!({"toolName": "Read"}));
    state.seed_tool_meta("t", &meta);

    let repeated = serde_json::from_value(json!({
        "title": "Read a.ts",
        "kind": "read",
        "rawInput": {"file_path": "/a.ts"},
    }))
    .unwrap();
    let kept = state.track_tool_call_update_with_meta("t", &repeated, Some(&meta));
    assert!(kept.is_none(), "an update with nothing new is dropped");

    let changed: ToolCallUpdateFields = serde_json::from_value(json!({
        "title": "Read a.ts (1 - 10)",
        "kind": "read",
        "rawInput": {"file_path": "/a.ts", "limit": 10},
    }))
    .unwrap();
    let kept = state
        .track_tool_call_update_with_meta("t", &changed, None)
        .expect("the changed update");
    assert_eq!(kept.title.as_deref(), Some("Read a.ts (1 - 10)"));
    assert_eq!(
        kept.raw_input,
        Some(json!({"file_path": "/a.ts", "limit": 10}))
    );
    assert!(kept.kind.is_none(), "the repeated kind is dropped");
}

/// `field-tracker.test.ts` > "ToolCallFieldTracker" > "keeps an update whose
/// _meta carries more than claudeCode".
#[test]
fn t7c_keeps_an_update_whose_meta_carries_more_than_claudecode() {
    let mut state = MapState::default();
    state.seed_tool_tracker(
        "t".to_string(),
        TrackedToolCall::initial(
            ToolKind::Execute,
            "ls".to_string(),
            Vec::new(),
            Vec::new(),
            None,
        ),
    );
    let delta = serde_json::Map::from_iter([(
        "terminal_output_delta".to_string(),
        json!({"terminal_id": "t", "data": "."}),
    )]);
    assert!(
        state
            .track_tool_call_update_with_meta("t", &ToolCallUpdateFields::new(), Some(&delta))
            .is_some(),
        "a key outside the merged namespaces always counts"
    );
    assert!(
        state
            .track_tool_call_update_with_meta("t", &ToolCallUpdateFields::new(), Some(&delta))
            .is_some(),
        "appended terminal output is never compared"
    );
}

/// `field-tracker.test.ts` > "ToolCallFieldTracker" > "merges _meta.jetbrains.air
/// keys like claudeCode keys".
#[test]
fn t7c_merges_meta_jetbrains_air_keys_like_claudecode_keys() {
    let mut state = MapState::default();
    state.seed_tool_tracker(
        "t".to_string(),
        TrackedToolCall::initial(
            ToolKind::Execute,
            "ls".to_string(),
            Vec::new(),
            Vec::new(),
            None,
        ),
    );
    let meta = serde_json::Map::from_iter([
        ("claudeCode".to_string(), json!({"toolName": "Bash"})),
        (
            "jetbrains".to_string(),
            json!({"air": {"version": 1, "commandTitle": "List files"}}),
        ),
    ]);
    state.seed_tool_meta("t", &meta);

    let empty = ToolCallUpdateFields::new();
    assert!(state
        .track_tool_call_update_with_meta("t", &empty, Some(&meta))
        .is_none());

    let changed = serde_json::Map::from_iter([
        ("claudeCode".to_string(), json!({"toolName": "Bash"})),
        (
            "jetbrains".to_string(),
            json!({"air": {"version": 1, "commandTitle": "List all files"}}),
        ),
    ]);
    assert!(state
        .track_tool_call_update_with_meta("t", &empty, Some(&changed))
        .is_some());
}

/// `field-tracker.test.ts` > "ToolCallFieldTracker" > "keeps pinned content
/// until the final result replaces it".
#[test]
fn t7c_keeps_pinned_content_until_the_final_result_replaces_it() {
    let mut state = MapState::default();
    state.seed_tool_tracker(
        "t".to_string(),
        TrackedToolCall::initial(
            ToolKind::Edit,
            "Edit a.ts".to_string(),
            Vec::new(),
            Vec::new(),
            None,
        ),
    );
    let patch = vec![json!({"type": "diff", "path": "/a.ts", "oldText": null, "newText": ""})];
    state.pin_approval_preview("t", None, &patch);

    let snippet: ToolCallUpdateFields = serde_json::from_value(json!({
        "content": [{"type": "diff", "path": "/a.ts", "oldText": "a", "newText": "b"}]
    }))
    .unwrap();
    assert!(state.track_tool_call_update("t", &snippet).is_none());
    let fin = state
        .track_final_tool_call_update("t", &snippet)
        .expect("the final result replaces the pinned patch");
    assert_eq!(fin.content, snippet.content);
}

/// `field-tracker.test.ts` > "ToolCallFieldTracker" > "ends an entry after both
/// the tool_result and the hook".
#[test]
fn t7c_ends_an_entry_after_both_the_tool_result_and_the_hook() {
    fn tracks(state: &mut MapState, id: &str) -> bool {
        let probe: ToolCallUpdateFields =
            serde_json::from_value(json!({"title": "probe"})).unwrap();
        state.track_tool_call_update(id, &probe);
        !state.track_tool_call_update(id, &probe).is_some()
    }

    let mut state = MapState::default();
    for id in ["a", "b", "c"] {
        state.seed_tool_tracker(
            id.to_string(),
            TrackedToolCall::initial(
                ToolKind::Other,
                id.to_uppercase(),
                Vec::new(),
                Vec::new(),
                None,
            ),
        );
    }
    state.finish_result_tool_tracker("a", false);
    state.finish_result_tool_tracker("b", true);
    state.finish_hook_tool_tracker("c");
    assert!(!tracks(&mut state, "a"));
    assert!(tracks(&mut state, "b"));
    assert!(tracks(&mut state, "c"));

    state.finish_hook_tool_tracker("b");
    state.finish_result_tool_tracker("c", false);
    assert!(!tracks(&mut state, "b"));
    assert!(!tracks(&mut state, "c"));
}

// ---------------------------------------------------------------------------
// tool-call-contract.test.ts > the ACP tool call contract
// ---------------------------------------------------------------------------

/// `tool-call-contract.test.ts` > "the ACP tool call contract" > "keeps the
/// Write file text only in the diff".
#[test]
fn t7c_keeps_the_write_file_text_only_in_the_diff() {
    for capabilities in [terminal_air(), plain_air()] {
        let report = report(
            capabilities,
            "Write",
            json!({"file_path": "/work/a.ts", "content": "text"}),
            None,
        );
        assert_eq!(report.call["rawInput"], json!({"file_path": "/work/a.ts"}));
        assert_eq!(
            report.call["content"],
            json!([{"type": "diff", "path": "/work/a.ts", "oldText": null, "newText": "text"}])
        );
    }
}

/// `tool-call-contract.test.ts` > "the ACP tool call contract" > "keeps the
/// Write file text of an alias key only in the diff".
#[test]
fn t7c_keeps_the_write_file_text_of_an_alias_key_only_in_the_diff() {
    for input in [
        json!({"path": "/work/a.ts", "file_text": "text"}),
        json!({"path": "/work/a.ts", "file_content": "text"}),
    ] {
        let report = report(plain_air(), "Write", input, None);
        assert_eq!(report.call["title"], "Write a.ts");
        assert_eq!(report.call["locations"], json!([{"path": "/work/a.ts"}]));
        assert_eq!(report.call["rawInput"], json!({"path": "/work/a.ts"}));
        assert_eq!(
            report.call["content"],
            json!([{"type": "diff", "path": "/work/a.ts", "oldText": null, "newText": "text"}])
        );
    }
}

/// `tool-call-contract.test.ts` > "the ACP tool call contract" > "keeps the Edit
/// text only in the diff".
#[test]
fn t7c_keeps_the_edit_text_only_in_the_diff() {
    let report = report(
        terminal_air(),
        "Edit",
        json!({
            "file_path": "/work/a.ts",
            "old_string": "a",
            "new_string": "b",
            "replace_all": true
        }),
        Some((json!("The file was updated"), false, None)),
    );
    assert_eq!(
        report.call["rawInput"],
        json!({"file_path": "/work/a.ts", "replace_all": true})
    );
    assert_eq!(
        report.call["content"],
        json!([{"type": "diff", "path": "/work/a.ts", "oldText": "a", "newText": "b"}])
    );
    // The confirmation has no display form.
    assert_eq!(report.updates[0]["rawOutput"], "The file was updated");
}

/// `tool-call-contract.test.ts` > "the ACP tool call contract" > "keeps the
/// NotebookEdit source in rawInput with one display copy for Zed".
#[test]
fn t7c_keeps_the_notebookedit_source_in_rawinput_with_one_display_copy_for_zed() {
    let input = json!({"notebook_path": "/work/a.ipynb", "cell_id": "c", "new_source": "x = 1"});
    let zed = report(
        terminal_air(),
        "NotebookEdit",
        input.clone(),
        Some((json!("Updated c with x = 1"), false, None)),
    );
    assert_eq!(zed.call["rawInput"], input);
    assert_eq!(
        zed.call["content"],
        json!([{"type": "content", "content": {"type": "text", "text": "```\nx = 1\n```"}}])
    );
    assert!(zed.updates[0].get("content").is_none());
    assert!(zed.updates[0].get("rawOutput").is_none());

    let air_report = report(air(), "NotebookEdit", input.clone(), None);
    assert_eq!(air_report.call["content"], json!([]));
    assert_eq!(air_report.call["rawInput"], input);
}

/// `tool-call-contract.test.ts` > "the ACP tool call contract" > "shows one
/// display copy of the description without a terminal".
#[test]
fn t7c_shows_one_display_copy_of_the_description_without_a_terminal() {
    let report = report(
        plain_air(),
        "Bash",
        json!({"command": "ls", "description": "List files"}),
        Some((json!("a"), false, None)),
    );
    assert_eq!(
        report.call["content"],
        json!([{"type": "content", "content": {"type": "text", "text": "List files"}}])
    );
    assert_eq!(
        report.updates[0]["content"],
        json!([{"type": "content", "content": {"type": "text", "text": "```console\na\n```"}}])
    );
    assert!(report.updates[0].get("rawOutput").is_none());
}

/// `tool-call-contract.test.ts` > "the ACP tool call contract" > "shows the
/// WebFetch prompt once to Zed and never to AIR".
#[test]
fn t7c_shows_the_webfetch_prompt_once_to_zed_and_never_to_air() {
    let input = json!({"url": "https://e.com", "prompt": "Summarize"});
    let zed = report(terminal_air(), "WebFetch", input.clone(), None);
    assert_eq!(
        zed.call["content"],
        json!([{"type": "content", "content": {"type": "text", "text": "Summarize"}}])
    );
    let air_report = report(air(), "WebFetch", input, None);
    assert_eq!(air_report.call["content"], json!([]));
}

/// `tool-call-contract.test.ts` > "the ACP tool call contract" > "marks an Agent
/// as a subagent and shows its prompt only to Zed".
#[test]
fn t7c_marks_an_agent_as_a_subagent_and_shows_its_prompt_only_to_zed() {
    let input = json!({"description": "Explore", "prompt": "Inspect the project"});
    let zed = report(terminal_air(), "Agent", input.clone(), None);
    assert_eq!(zed.call["title"], "Explore");
    assert_eq!(
        zed.call["content"],
        json!([{"type": "content", "content": {"type": "text", "text": "Inspect the project"}}])
    );
    assert_eq!(
        zed.call["_meta"]["jetbrains"]["air"]["subagent"],
        json!(true)
    );
    assert!(zed.call["_meta"]["claudeCode"].get("subagent").is_none());
    let air_report = report(air(), "Task", input, None);
    assert_eq!(air_report.call["content"], json!([]));
}

/// `tool-call-contract.test.ts` > "the ACP tool call contract" > "keeps the
/// AskUserQuestion question out of the title".
#[test]
fn t7c_keeps_the_askuserquestion_question_out_of_the_title() {
    let input =
        json!({"questions": [{"question": "Which mode?", "header": "Mode", "options": []}]});
    let zed = report(terminal_air(), "AskUserQuestion", input.clone(), None);
    assert_eq!(zed.call["title"], "Asking for your input");
    assert_eq!(
        zed.call["content"],
        json!([{"type": "content", "content": {"type": "text", "text": "Which mode?"}}])
    );
    let air_report = report(air(), "AskUserQuestion", input, None);
    assert_eq!(air_report.call["content"], json!([]));
}

/// `tool-call-contract.test.ts` > "the ACP tool call contract" > "sends the
/// in_progress status of progress beats once".
#[test]
fn t7c_sends_the_in_progress_status_of_progress_beats_once() {
    let mut state = state_with(terminal_air(), "/work");
    let call = map_consolidated_assistant(&assistant_frame("t", "Agent", json!({})), &mut state);
    assert_eq!(call.len(), 1);

    let beat = |seconds: u64| {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "claudeCode".into(),
            json!({
                "toolName": "Agent",
                "toolResponse": {"elapsedTimeSeconds": seconds, "subagentType": "Explore"}
            }),
        );
        meta
    };
    let fields: ToolCallUpdateFields =
        serde_json::from_value(json!({"status": "in_progress"})).unwrap();
    let first = state
        .track_tool_call_update_with_meta("t", &fields, Some(&beat(1)))
        .expect("the first beat");
    assert_eq!(first.status, Some(ToolCallStatus::InProgress));
    let second = state
        .track_tool_call_update_with_meta("t", &fields, Some(&beat(2)))
        .expect("the second beat carries only the progress");
    assert!(second.status.is_none(), "the repeated status is dropped");
}

// ---------------------------------------------------------------------------
// tool-call-contract.test.ts > ExitPlanMode
// ---------------------------------------------------------------------------

/// `tool-call-contract.test.ts` > "...ExitPlanMode" > "shows the plan once and
/// sends no approval text".
#[test]
fn t7c_shows_the_plan_once_and_sends_no_approval_text() {
    let report = report(
        terminal_air(),
        "ExitPlanMode",
        json!({"plan": "1. Do it"}),
        Some((
            json!("User has approved your plan.\n\n## Approved Plan:\n1. Do it"),
            false,
            None,
        )),
    );
    assert_eq!(
        report.call["content"],
        json!([{"type": "content", "content": {"type": "text", "text": "1. Do it"}}])
    );
    assert_eq!(report.updates[0]["status"], "completed");
    assert_eq!(report.updates[0]["title"], "Exited Plan Mode");
    assert!(report.updates[0].get("rawOutput").is_none());
    assert!(report.updates[0].get("content").is_none());
    assert_eq!(
        self::report(air(), "ExitPlanMode", json!({"plan": "1. Do it"}), None).call["content"],
        json!([])
    );
}

/// `tool-call-contract.test.ts` > "...ExitPlanMode" > "sends the rejection
/// reason once, unfenced, in rawOutput".
#[test]
fn t7c_sends_the_rejection_reason_once_unfenced_in_rawoutput() {
    let report = report(
        terminal_air(),
        "ExitPlanMode",
        json!({"plan": "1. Do it"}),
        Some((json!("```\nKeep the tests\n```"), true, None)),
    );
    assert_eq!(report.updates[0]["status"], "failed");
    assert_eq!(report.updates[0]["rawOutput"], "Keep the tests");
    assert!(report.updates[0].get("content").is_none());
}

fn plan_file_dir(tag: &str) -> (PathBuf, String) {
    let dir = std::env::temp_dir().join(format!(
        "acp-t7c-plan-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let plan = dir.join("plan.md");
    std::fs::write(&plan, "1. Do it").unwrap();
    let plan = plan.to_string_lossy().to_string();
    (dir, plan)
}

fn plan_file_air() -> ClientCapabilities {
    ClientCapabilities::from_value(Some(&json!({
        "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["planFile"]}}}
    })))
}

/// `tool-call-contract.test.ts` > "...ExitPlanMode with a plan file" > "sends
/// the plan file path and no plan text to a planFile client".
#[test]
fn t7c_sends_the_plan_file_path_and_no_plan_text_to_a_planfile_client() {
    let (dir, plan) = plan_file_dir("path");
    let report = report(
        plan_file_air(),
        "ExitPlanMode",
        json!({"plan": "1. Do it", "planFilePath": plan}),
        Some((
            json!("User has approved your plan.\n\n## Approved Plan:\n1. Do it"),
            false,
            None,
        )),
    );
    assert_eq!(report.call["rawInput"], json!({"planFilePath": plan}));
    assert_eq!(report.call["content"], json!([]));
    assert_eq!(report.updates[0]["status"], "completed");
    assert_eq!(report.updates[0]["title"], "Exited Plan Mode");
    assert_eq!(report.updates[0]["rawInput"], json!({"planFilePath": plan}));
    let all = json!([report.call, report.updates]);
    assert!(!all.to_string().contains("Do it"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `tool-call-contract.test.ts` > "...with a plan file" > "clears the plan file
/// for AIR in a call of its own when the plan is approved".
#[test]
fn t7c_clears_the_plan_file_for_air_in_a_call_of_its_own_when_the_plan_is_approved() {
    let (dir, plan) = plan_file_dir("clear");
    let report = report(
        plan_file_air(),
        "ExitPlanMode",
        json!({"plan": "1. Do it", "planFilePath": plan}),
        Some((json!("User has approved your plan."), false, None)),
    );
    assert_eq!(report.updates.len(), 2, "{:?}", report.updates);
    assert_eq!(
        report.updates[1],
        json!({
            "toolCallId": "t:plan-file-clear",
            "sessionUpdate": "tool_call",
            "title": "Exited Plan Mode",
            "kind": "switch_mode",
            "status": "completed",
            "rawInput": {"planFilePath": ""},
            "content": []
        })
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `tool-call-contract.test.ts` > "...with a plan file" > "keeps the plan file
/// when the plan is rejected".
#[test]
fn t7c_keeps_the_plan_file_when_the_plan_is_rejected() {
    let (dir, plan) = plan_file_dir("reject");
    let report = report(
        plan_file_air(),
        "ExitPlanMode",
        json!({"plan": "1. Do it", "planFilePath": plan}),
        Some((json!("```\nKeep the tests\n```"), true, None)),
    );
    assert_eq!(report.updates.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `tool-call-contract.test.ts` > "...with a plan file" > "sends the plan file
/// path with the rejection reason".
#[test]
fn t7c_sends_the_plan_file_path_with_the_rejection_reason() {
    let (dir, plan) = plan_file_dir("reject-path");
    let report = report(
        plan_file_air(),
        "ExitPlanMode",
        json!({"plan": "1. Do it", "planFilePath": plan}),
        Some((json!("```\nKeep the tests\n```"), true, None)),
    );
    assert_eq!(report.updates[0]["status"], "failed");
    assert_eq!(report.updates[0]["rawOutput"], "Keep the tests");
    assert_eq!(report.updates[0]["rawInput"], json!({"planFilePath": plan}));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `tool-call-contract.test.ts` > "...with a plan file" > "sends the plan file
/// of the structured result when the input names none".
#[test]
fn t7c_sends_the_plan_file_of_the_structured_result_when_the_input_names_none() {
    let (dir, plan) = plan_file_dir("structured");
    let input = json!({"plan": "1. Do it"});
    let report = report(
        plan_file_air(),
        "ExitPlanMode",
        input.clone(),
        Some((
            json!("User has approved your plan."),
            false,
            Some(json!({"plan": "1. Do it", "isAgent": false, "filePath": plan})),
        )),
    );
    assert_eq!(report.call["rawInput"], input);
    assert_eq!(report.updates[0]["rawInput"], json!({"planFilePath": plan}));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `tool-call-contract.test.ts` > "...with a plan file" > "sends the plan text
/// when the plan file does not exist".
#[test]
fn t7c_sends_the_plan_text_when_the_plan_file_does_not_exist() {
    let (dir, _) = plan_file_dir("missing");
    let missing =
        json!({"plan": "1. Do it", "planFilePath": dir.join("missing.md").to_string_lossy()});
    let report = report(
        plan_file_air(),
        "ExitPlanMode",
        missing.clone(),
        Some((json!("User has approved your plan."), false, None)),
    );
    assert_eq!(report.call["rawInput"], missing);
    assert_eq!(
        report.call["content"],
        json!([{"type": "content", "content": {"type": "text", "text": "1. Do it"}}])
    );
    assert!(report.updates[0].get("rawInput").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// `tool-call-contract.test.ts` > "...with a plan file" > "sends the plan text
/// to an AIR client without planFile".
#[test]
fn t7c_sends_the_plan_text_to_an_air_client_without_planfile() {
    let (dir, plan) = plan_file_dir("no-cap");
    let with_file = json!({"plan": "1. Do it", "planFilePath": plan});
    let report = report(terminal_air(), "ExitPlanMode", with_file.clone(), None);
    assert_eq!(report.call["rawInput"], with_file);
    assert_eq!(
        report.call["content"],
        json!([{"type": "content", "content": {"type": "text", "text": "1. Do it"}}])
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `tool-call-contract.test.ts` > "...with a plan file" > "sends the whole input
/// to a client that is not AIR".
#[test]
fn t7c_sends_the_whole_input_to_a_client_that_is_not_air() {
    let (dir, plan) = plan_file_dir("plain");
    let with_file = json!({"plan": "1. Do it", "planFilePath": plan});
    let report = report(
        plain(),
        "ExitPlanMode",
        with_file.clone(),
        Some((
            json!("User has approved your plan."),
            false,
            Some(json!({"plan": "1. Do it", "isAgent": false, "filePath": plan})),
        )),
    );
    assert_eq!(report.call["rawInput"], with_file);
    assert!(report
        .updates
        .iter()
        .all(|update| update.get("rawInput").is_none()));
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// tools.test.ts
// ---------------------------------------------------------------------------

fn shape_json(shape: &ToolShape) -> Value {
    let mut value = json!({
        "title": shape.title,
        "kind": serde_json::to_value(shape.kind).unwrap(),
        "content": shape.content,
    });
    if !shape.locations.is_empty() {
        value["locations"] = serde_json::to_value(&shape.locations).unwrap();
    }
    value
}

/// `tools.test.ts` > "NotebookEdit" > "shows the notebook, the cell, and the
/// new source".
#[test]
fn t7c_shows_the_notebook_the_cell_and_the_new_source() {
    let shape = crate::tools::tool_shape_with(
        "NotebookEdit",
        &json!({
            "notebook_path": "/work/analysis.ipynb",
            "cell_id": "cell-2",
            "new_source": "print(1)"
        }),
        Some(std::path::Path::new("/work")),
        true,
    );
    assert_eq!(
        shape_json(&shape),
        json!({
            "title": "Edit cell cell-2 in analysis.ipynb",
            "kind": "edit",
            "content": [{"type": "content", "content": {"type": "text", "text": "```\nprint(1)\n```"}}],
            "locations": [{"path": "/work/analysis.ipynb"}]
        })
    );
    let deleted = crate::tools::tool_shape_with(
        "NotebookEdit",
        &json!({
            "notebook_path": "/work/analysis.ipynb",
            "cell_id": "cell-2",
            "new_source": "",
            "edit_mode": "delete"
        }),
        None,
        true,
    );
    assert_eq!(deleted.title, "Delete cell cell-2 in /work/analysis.ipynb");
    assert!(deleted.content.is_empty());
}

/// `tools.test.ts` > "NotebookEdit" > "does not send the result text that
/// repeats the cell source".
#[test]
fn t7c_does_not_send_the_result_text_that_repeats_the_cell_source() {
    let report = report(
        terminal_air(),
        "NotebookEdit",
        json!({"notebook_path": "/work/a.ipynb", "cell_id": "c1", "new_source": "x = 1"}),
        Some((json!("Updated cell c1 with x = 1"), false, None)),
    );
    assert_eq!(report.updates[0]["status"], "completed");
    assert!(report.updates[0].get("content").is_none());
    assert!(report.updates[0].get("rawOutput").is_none());
}

/// `tools.test.ts` > "Bash terminal output" > "post-tool-use hook sends diff
/// content for Edit tool" > "keeps only the async launch markers of a
/// tool_response".
#[test]
fn t7c_keeps_only_the_async_launch_markers_of_a_tool_response() {
    let launched = json!({
        "status": "async_launched",
        "isAsync": true,
        "agentId": "agent-1",
        "description": "Research",
        "prompt": "Look around",
        "outputFile": "/tmp/agent-1.output"
    });
    let update = crate::hooks::post_tool_use_update(
        "toolu_async_agent",
        "Agent",
        None,
        &launched,
        false,
        true,
    )
    .expect("the markers of the launch");
    assert_eq!(
        update,
        json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "toolu_async_agent",
            "_meta": {
                "claudeCode": {
                    "toolName": "Agent",
                    "toolResponse": {"status": "async_launched", "isAsync": true}
                }
            }
        })
    );

    // A tool_response without a marker, and no change, sends nothing.
    let read = json!({"type": "text", "file": {"filePath": "/a.ts", "content": "whole file", "numLines": 1}});
    assert!(crate::hooks::post_tool_use_update(
        "toolu_read_hook",
        "Read",
        None,
        &read,
        false,
        true
    )
    .is_none());
}

/// `field-tracker.test.ts` > "tool call refinements" > "lets the error of a
/// rejected Edit replace a pinned approval patch".
#[test]
fn t7c_lets_the_error_of_a_rejected_edit_replace_a_pinned_approval_patch() {
    let mut state = state_with(plain_air(), "/work");
    let call = map_consolidated_assistant(
        &assistant_frame(
            "toolu_rejected",
            "Edit",
            json!({"file_path": "/a.ts", "old_string": "a", "new_string": "b"}),
        ),
        &mut state,
    );
    assert_eq!(call.len(), 1);
    let preview = vec![json!({"type": "diff", "path": "/a.ts", "oldText": null, "newText": ""})];
    state.pin_approval_preview("toolu_rejected", None, &preview);

    let updates = map_user_frame(
        &user_frame(
            "toolu_rejected",
            json!("The user doesn't want to proceed with this tool use."),
            true,
            None,
        ),
        &mut state,
    );
    let update = wire(&updates[0]);
    assert_eq!(update["status"], "failed");
    assert_eq!(update["content"][0]["type"], "content");
    assert_eq!(update["content"][0]["content"]["type"], "text");
}

/// `field-tracker.test.ts` > "tool call refinements" > "keeps an approval patch
/// against the snippet refinement until the hook".
#[test]
fn t7c_keeps_an_approval_patch_against_the_snippet_refinement_until_the_hook() {
    let mut state = state_with(plain_air(), "/work");
    let input = json!({"file_path": "/a.ts", "old_string": "a", "new_string": "b"});
    let preview = vec![json!({
        "type": "diff",
        "path": "/a.ts",
        "oldText": null,
        "newText": "",
        "_meta": {"jetbrains": {"air": {"version": 1, "diffPatch": {"text": "exact"}}}}
    })];
    // The permission flow emitted the tool call with the exact patch.
    state.seed_tool_tracker(
        "toolu_edit".to_string(),
        TrackedToolCall::initial(
            ToolKind::Edit,
            "Edit /a.ts".to_string(),
            serde_json::from_value::<Vec<ToolCallContent>>(Value::Array(preview.clone())).unwrap(),
            Vec::new(),
            Some(input.clone()),
        ),
    );
    state.pin_approval_preview("toolu_edit", None, &preview);
    state.mark_emitted(
        "toolu_edit".to_string(),
        json!({
            "type": "tool_use", "id": "toolu_edit", "name": "Edit", "input": input
        }),
    );

    // The consolidated tool_use refines the call with the snippet diff.
    let refine = map_consolidated_assistant(
        &assistant_frame("toolu_edit", "Edit", input.clone()),
        &mut state,
    );
    for update in &refine {
        if let SessionUpdate::ToolCallUpdate(update) = update {
            assert!(update.fields.content.is_none(), "the pinned patch stays");
        }
    }

    // The PostToolUse hook replaces it with the final change.
    let response = json!({
        "filePath": "/a.ts",
        "oldString": "a",
        "newString": "b",
        "originalFile": "a\n",
        "structuredPatch": [{"oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1, "lines": ["-a", "+b"]}]
    });
    let hook =
        crate::hooks::post_tool_use_update("toolu_edit", "Edit", None, &response, false, true)
            .expect("the final change");
    let mut hook = hook;
    state.filter_hook_tool_call_update("toolu_edit", &mut hook);
    assert_eq!(
        hook["content"],
        json!([{"type": "diff", "path": "/a.ts", "oldText": "a", "newText": "b"}])
    );
}

/// AIR with `diffPatch` and no `rawInputRendering` (`field-tracker.test.ts:17-19`).
fn patch_air() -> ClientCapabilities {
    ClientCapabilities::from_value(Some(&json!({
        "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["diffPatch"]}}}
    })))
}

fn stream_frame(event: Value) -> Value {
    json!({
        "type": "stream_event",
        "parent_tool_use_id": null,
        "uuid": "u",
        "session_id": "test-session",
        "event": event
    })
}

/// `field-tracker.test.ts` > "tool call refinements" > "sends only the fields
/// that each streamed refinement changes".
#[test]
fn t7c_sends_only_the_fields_that_each_streamed_refinement_changes() {
    let mut state = state_with(patch_air(), "/work");
    map_stream_event(
        &stream_frame(json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_grep", "name": "Grep", "input": {}}
        })),
        &mut state,
    );
    let first = map_stream_event(
        &stream_frame(json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{\"pattern\":\"todo\","}
        })),
        &mut state,
    );
    let second = map_stream_event(
        &stream_frame(json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "\"glob\":\"*.ts\","}
        })),
        &mut state,
    );

    // No rawInput travels until the input is complete.
    let first = wire(&first[0]);
    assert_eq!(first["title"], "grep \"todo\"");
    assert!(first.get("rawInput").is_none());
    assert!(first.get("kind").is_none(), "the repeated kind is dropped");
    let second = wire(&second[0]);
    assert_eq!(second["title"], "grep --include=\"*.ts\" \"todo\"");
    assert!(second.get("rawInput").is_none());
    assert!(second.get("kind").is_none(), "the repeated kind is dropped");
}

/// `field-tracker.test.ts` > "tool call refinements" > "sends the file text of
/// a Write once, in the hook patch".
#[test]
fn t7c_sends_the_file_text_of_a_write_once_in_the_hook_patch() {
    let dir = std::env::temp_dir().join(format!(
        "acp-t7c-write-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("new.ts");
    let file_path = file.to_string_lossy().to_string();
    let content = "export const big = 1;\n".repeat(100);
    let input = json!({"file_path": file_path, "content": content});
    let mut state = state_with(patch_air(), "/work");

    let call = map_consolidated_assistant(
        &assistant_frame("toolu_write", "Write", input.clone()),
        &mut state,
    );
    let call = wire(&call[0]);
    let refine =
        map_consolidated_assistant(&assistant_frame("toolu_write", "Write", input), &mut state);

    std::fs::write(&file, &content).unwrap();
    let result = map_user_frame(
        &user_frame(
            "toolu_write",
            json!(format!("File created successfully at: {file_path}")),
            false,
            None,
        ),
        &mut state,
    );
    let result = wire(&result[0]);

    // The tool call holds no file text: the input does not tell whether the
    // file exists. The hook sends the creation patch, which holds the text once.
    assert!(!call.to_string().contains("export const big"));
    assert_eq!(call["rawInput"], json!({"file_path": file_path}));
    assert!(refine.is_empty(), "the repeated tool_use refines nothing");
    assert!(result.get("content").is_none());

    let response = json!({
        "type": "create",
        "filePath": file_path,
        "content": content,
        "structuredPatch": [],
        "originalFile": null
    });
    let mut hook =
        crate::hooks::post_tool_use_update("toolu_write", "Write", None, &response, true, true)
            .expect("the creation patch");
    state.filter_hook_tool_call_update("toolu_write", &mut hook);
    let hook = state.filter_hook_meta(hook).expect("the hook update");
    state.finish_hook_tool_tracker("toolu_write");

    let all = json!([call, result, hook]);
    let text = all.to_string();
    assert_eq!(text.split("export const big").count() - 1, 100);
    assert!(text.contains("new file mode 100644"), "{text}");
    assert_eq!(
        state.tracked_tool_calls(),
        0,
        "the entry ended with the hook"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The AIR partial refinement strips the plan text from `rawInput` (T-85): an
/// ExitPlanMode that names an existing plan file sends the path.
#[test]
fn t7c_air_raw_input_carries_the_plan_file_path_instead_of_the_plan_text() {
    let (dir, plan) = plan_file_dir("raw-input");
    let state = state_with(plan_file_air(), "/work");
    let input = json!({"plan": "1. Do it", "planFilePath": plan});
    assert_eq!(
        state.raw_input("ExitPlanMode", &input),
        json!({"planFilePath": plan})
    );
    // A planFile client without an existing file keeps the plan text.
    let missing =
        json!({"plan": "1. Do it", "planFilePath": dir.join("missing.md").to_string_lossy()});
    assert_eq!(state.raw_input("ExitPlanMode", &missing), missing);
    let _ = std::fs::remove_dir_all(&dir);
}
