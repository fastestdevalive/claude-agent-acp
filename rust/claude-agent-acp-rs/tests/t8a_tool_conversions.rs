//! T8A — the `tool conversions` cases of `src/tests/acp-agent.test.ts`:
//! `toolInfoFromToolUse`, the TodoWrite plan conversion and
//! `toolUpdateFromToolResult`. Each test mirrors the upstream inputs and the
//! expected values, so the Rust port proves the same observable shape.

use claude_agent_acp_rs::map::{
    map_consolidated_assistant, materialize_diff_old_text, materialize_tool_call_defaults, MapState,
};

use claude_agent_acp_rs::tools::{
    plan_entries, tool_info_from_tool_use, tool_result_content, ClaudePlanEntry, ToolShape,
};
use serde_json::{json, Value};

/// The four fields `toolInfoFromToolUse` returns, in the wire shape the ACP
/// tool-call frame carries them.
fn shape_json(shape: &ToolShape) -> Value {
    json!({
        "kind": serde_json::to_value(shape.kind).unwrap(),
        "title": shape.title,
        "content": serde_json::to_value(&shape.content).unwrap(),
        "locations": serde_json::to_value(&shape.locations).unwrap(),
    })
}

fn shape(name: &str, input: Value) -> Value {
    shape_json(&tool_info_from_tool_use(name, &input, None))
}

fn text_block(text: &str) -> Value {
    json!({"type": "content", "content": {"type": "text", "text": text}})
}

#[test]
fn t8a_bash_nicely() {
    let got = shape(
        "Bash",
        json!({"command": "rm README.md.rm", "description": "Delete README.md.rm file"}),
    );
    assert_eq!(
        got,
        json!({
            "kind": "execute",
            "title": "rm README.md.rm",
            "content": [text_block("Delete README.md.rm file")],
            "locations": [],
        })
    );
}

#[test]
fn t8a_bash_command_is_the_title_without_a_description() {
    let got = shape("Bash", json!({"command": "git diff"}));
    assert_eq!(got["kind"], "execute");
    assert_eq!(got["title"], "git diff");
}

#[test]
fn t8a_glob_nicely() {
    assert_eq!(
        shape("Glob", json!({"pattern": "*/**.ts"})),
        json!({
            "kind": "search",
            "title": "Find `*/**.ts`",
            "content": [],
            "locations": [],
        })
    );
}

#[test]
fn t8a_task_tool_call() {
    let prompt = "The user has asked me to \"Create a Task to do the work!\" but hasn't specified what specific work they want done. I need to:\n\n1. First understand what work needs to be done by examining the current state of the repository\n2. Look at the git status to see what files have been modified\n3. Check if there are any obvious tasks that need completion based on the current state\n4. If the work isn't clear from the context, ask the user to specify what work they want accomplished\n\nThe git status shows: \"M src/tests/acp-agent.test.ts\" - there's a modified test file that might need attention.\n\nPlease examine the repository state and determine what work needs to be done, then either complete it or ask the user for clarification on the specific task they want accomplished.";
    assert_eq!(
        shape(
            "Task",
            json!({
                "description": "Handle user's work request",
                "prompt": prompt,
                "subagent_type": "general-purpose",
            })
        ),
        json!({
            "kind": "think",
            "title": "Handle user's work request",
            "content": [text_block(prompt)],
            "locations": [],
        })
    );
}

#[test]
fn t8a_grep_tool_call() {
    assert_eq!(
        shape("Grep", json!({"pattern": ".*"})),
        json!({
            "kind": "search",
            "title": "grep \".*\"",
            "content": [],
            "locations": [],
        })
    );
}

/// The wire `tool_call` of one tool use: the typed mapper output with the
/// fields the wire materializes (`oldText:null` on a creation diff, the pending
/// status).
fn wire_shape(name: &str, input: Value) -> Value {
    let mut state = MapState::default();
    let frame = json!({
        "type": "assistant",
        "parent_tool_use_id": null,
        "message": {"role": "assistant", "content": [{
            "type": "tool_use",
            "id": "toolu_wire",
            "name": name,
            "input": input,
        }]},
    });
    let updates = map_consolidated_assistant(&frame, &mut state);
    let mut value = serde_json::to_value(&updates[0]).unwrap();
    materialize_tool_call_defaults(&mut value);
    materialize_diff_old_text(&mut value);
    value
}

#[test]
fn t8a_write_tool_calls() {
    let first = wire_shape(
        "Write",
        json!({
            "file_path": "/Users/test/project/example.txt",
            "content": "Hello, World!\nThis is test content.",
        }),
    );
    assert_eq!(first["kind"], "edit");
    assert_eq!(first["title"], "Write /Users/test/project/example.txt");
    assert_eq!(
        first["content"],
        json!([{
            "type": "diff",
            "path": "/Users/test/project/example.txt",
            "oldText": null,
            "newText": "Hello, World!\nThis is test content.",
        }])
    );
    assert_eq!(
        first["locations"],
        json!([{"path": "/Users/test/project/example.txt"}])
    );

    let second = wire_shape(
        "Write",
        json!({
            "file_path": "/Users/test/project/config.json",
            "content": "{\"version\": \"1.0.0\"}",
        }),
    );
    assert_eq!(second["kind"], "edit");
    assert_eq!(second["title"], "Write /Users/test/project/config.json");
    assert_eq!(
        second["content"],
        json!([{
            "type": "diff",
            "path": "/Users/test/project/config.json",
            "oldText": null,
            "newText": "{\"version\": \"1.0.0\"}",
        }])
    );
    assert_eq!(
        second["locations"],
        json!([{"path": "/Users/test/project/config.json"}])
    );
}

#[test]
fn t8a_pending_write_before_its_file_path() {
    assert_eq!(
        shape("Write", json!({})),
        json!({
            "kind": "edit",
            "title": "Preparing file…",
            "content": [],
            "locations": [],
        })
    );
}

#[test]
fn t8a_edit_tool_calls() {
    assert_eq!(
        shape(
            "Edit",
            json!({
                "file_path": "/Users/test/project/test.txt",
                "old_string": "old text",
                "new_string": "new text",
            })
        ),
        json!({
            "kind": "edit",
            "title": "Edit /Users/test/project/test.txt",
            "content": [{
                "type": "diff",
                "path": "/Users/test/project/test.txt",
                "oldText": "old text",
                "newText": "new text",
            }],
            "locations": [{"path": "/Users/test/project/test.txt"}],
        })
    );

    let old = "struct PromptState {\n    active_command: Option<ActiveCommand>,\n    active_web_search: Option<String>,\n}";
    let new = "struct PromptState {\n    active_commands: HashMap<String, ActiveCommand>,\n    active_web_search: Option<String>,\n}";
    assert_eq!(
        shape(
            "Edit",
            json!({
                "replace_all": false,
                "file_path": "/Users/benbrandt/github/codex-acp/src/thread.rs",
                "old_string": old,
                "new_string": new,
            })
        ),
        json!({
            "kind": "edit",
            "title": "Edit /Users/benbrandt/github/codex-acp/src/thread.rs",
            "content": [{
                "type": "diff",
                "path": "/Users/benbrandt/github/codex-acp/src/thread.rs",
                "oldText": old,
                "newText": new,
            }],
            "locations": [{"path": "/Users/benbrandt/github/codex-acp/src/thread.rs"}],
        })
    );

    assert_eq!(
        shape("Edit", json!({})),
        json!({
            "kind": "edit",
            "title": "Edit",
            "content": [],
            "locations": [],
        })
    );
}

#[test]
fn t8a_read_tool_calls() {
    for path in [
        "/Users/test/project/readme.md",
        "/Users/test/project/data.json",
    ] {
        assert_eq!(
            shape("Read", json!({"file_path": path})),
            json!({
                "kind": "read",
                "title": format!("Read {path}"),
                "content": [],
                "locations": [{"path": path, "line": 1}],
            })
        );
    }

    assert_eq!(
        shape(
            "Read",
            json!({"file_path": "/Users/test/project/large.txt", "limit": 100})
        ),
        json!({
            "kind": "read",
            "title": "Read /Users/test/project/large.txt (1 - 100)",
            "content": [],
            "locations": [{"path": "/Users/test/project/large.txt", "line": 1}],
        })
    );

    assert_eq!(
        shape(
            "Read",
            json!({"file_path": "/Users/test/project/large.txt", "offset": 50, "limit": 100})
        ),
        json!({
            "kind": "read",
            "title": "Read /Users/test/project/large.txt (50 - 149)",
            "content": [],
            "locations": [{"path": "/Users/test/project/large.txt", "line": 50}],
        })
    );

    assert_eq!(
        shape(
            "Read",
            json!({"file_path": "/Users/test/project/large.txt", "offset": 200})
        ),
        json!({
            "kind": "read",
            "title": "Read /Users/test/project/large.txt (from line 200)",
            "content": [],
            "locations": [{"path": "/Users/test/project/large.txt", "line": 200}],
        })
    );
}

#[test]
fn t8a_todo_write_plan_entries() {
    let todos = json!([
        {"content": "Analyze existing test coverage and identify gaps", "status": "in_progress", "activeForm": "Analyzing existing test coverage"},
        {"content": "Add comprehensive edge case tests", "status": "pending", "activeForm": "Adding comprehensive edge case tests"},
        {"content": "Add performance and timing tests", "status": "pending", "activeForm": "Adding performance and timing tests"},
        {"content": "Add error handling and panic behavior tests", "status": "pending", "activeForm": "Adding error handling tests"},
        {"content": "Add concurrent access and race condition tests", "status": "pending", "activeForm": "Adding concurrent access tests"},
        {"content": "Add tests for Each function with various data types", "status": "pending", "activeForm": "Adding Each function tests"},
        {"content": "Add benchmark tests for performance measurement", "status": "pending", "activeForm": "Adding benchmark tests"},
        {"content": "Improve test organization and helper functions", "status": "pending", "activeForm": "Improving test organization"},
    ]);
    let mut state = MapState::default();
    let frame = json!({
        "type": "assistant",
        "parent_tool_use_id": null,
        "message": {"role": "assistant", "content": [{
            "type": "tool_use",
            "id": "toolu_01HaXZ4LfdchSeSR8ygt4zyq",
            "name": "TodoWrite",
            "input": {"todos": todos},
        }]},
    });
    let updates = map_consolidated_assistant(&frame, &mut state);
    assert_eq!(updates.len(), 1, "{updates:?}");
    let value = serde_json::to_value(&updates[0]).unwrap();
    assert_eq!(
        serde_json::to_string(&value).unwrap(),
        r#"{"sessionUpdate":"plan","entries":[{"content":"Analyzing existing test coverage","priority":"medium","status":"in_progress"},{"content":"Add comprehensive edge case tests","priority":"medium","status":"pending"},{"content":"Add performance and timing tests","priority":"medium","status":"pending"},{"content":"Add error handling and panic behavior tests","priority":"medium","status":"pending"},{"content":"Add concurrent access and race condition tests","priority":"medium","status":"pending"},{"content":"Add tests for Each function with various data types","priority":"medium","status":"pending"},{"content":"Add benchmark tests for performance measurement","priority":"medium","status":"pending"},{"content":"Improve test organization and helper functions","priority":"medium","status":"pending"}]}"#
    );

    // The same conversion through the exported `plan_entries` helper.
    let entries: Vec<ClaudePlanEntry> = serde_json::from_value(todos).unwrap();
    assert_eq!(plan_entries(Some(&entries)).len(), 8);
}

#[test]
fn t8a_edit_result_success_and_failure() {
    let input = json!({
        "file_path": "/Users/test/project/test.txt",
        "old_string": "old",
        "new_string": "new",
    });
    // A successful Edit is only a confirmation: no update.
    assert!(tool_result_content(
        "Edit",
        &input,
        None,
        &json!([{"type": "text", "text": "not valid json"}]),
        false,
    )
    .is_none());

    let content = tool_result_content(
        "Edit",
        &input,
        None,
        &json!([{"type": "text", "text": "Failed to find `old_string`"}]),
        true,
    )
    .expect("the failure text is the update");
    assert_eq!(
        serde_json::to_value(&content).unwrap(),
        json!([text_block("```\nFailed to find `old_string`\n```")])
    );
}

#[test]
fn t8a_document_blocks_are_compact_placeholders() {
    let input = json!({"file_path": "/Users/test/report.pdf"});
    let base64 = tool_result_content(
        "Read",
        &input,
        None,
        &json!([{
            "type": "document",
            "title": "Quarterly report",
            "source": {
                "type": "base64",
                "media_type": "application/pdf",
                "data": "A".repeat(4096),
            },
        }]),
        false,
    )
    .expect("a document result");
    assert_eq!(
        serde_json::to_value(&base64).unwrap(),
        json!([text_block(
            "[document \"Quarterly report\": application/pdf, 3.0 KB]"
        )])
    );

    let url = tool_result_content(
        "Read",
        &Value::Null,
        None,
        &json!([{
            "type": "document",
            "source": {"type": "url", "url": "https://example.com/spec.pdf"},
        }]),
        false,
    )
    .expect("a document result");
    assert_eq!(
        serde_json::to_value(&url).unwrap(),
        json!([text_block("[document: https://example.com/spec.pdf]")])
    );
}

#[test]
fn t8a_tool_result_content_transforms() {
    let case = |name: &str, input: Value, content: Value, is_error: bool| -> Value {
        let result = tool_result_content(name, &input, None, &content, is_error)
            .expect("the block maps to ACP content");
        serde_json::to_value(&result).unwrap()
    };

    assert_eq!(
        case(
            "ToolSearch",
            json!({"query": "test"}),
            json!([{"type": "tool_reference", "tool_name": "some_discovered_tool"}]),
            false,
        ),
        json!([text_block("Tool: some_discovered_tool")])
    );

    assert_eq!(
        case(
            "WebSearch",
            json!({"query": "test"}),
            json!([{
                "type": "web_search_result",
                "title": "Test Result",
                "url": "https://example.com",
                "encrypted_content": "...",
                "page_age": null,
            }]),
            false,
        ),
        json!([text_block("Test Result (https://example.com)")])
    );

    assert_eq!(
        case(
            "WebSearch",
            json!({"query": "test"}),
            json!({"type": "web_search_tool_result_error", "error_code": "unavailable"}),
            false,
        ),
        json!([text_block("Error: unavailable")])
    );

    assert_eq!(
        case(
            "CodeExecution",
            json!({}),
            json!({
                "type": "code_execution_result",
                "stdout": "Hello World",
                "stderr": "",
                "return_code": 0,
                "content": [],
            }),
            false,
        ),
        json!([text_block("Output: Hello World")])
    );

    assert_eq!(
        case(
            "WebFetch",
            json!({"url": "https://example.com"}),
            json!({
                "type": "web_fetch_result",
                "url": "https://example.com",
                "content": {
                    "type": "document",
                    "citations": null,
                    "title": null,
                    "source": {"type": "text", "media_type": "text/plain", "data": "Page content here"},
                },
            }),
            false,
        ),
        json!([text_block("Fetched: https://example.com")])
    );

    assert_eq!(
        case(
            "ToolSearch",
            json!({"query": "test"}),
            json!({
                "type": "tool_search_tool_search_result",
                "tool_references": [
                    {"type": "tool_reference", "tool_name": "tool_a"},
                    {"type": "tool_reference", "tool_name": "tool_b"},
                ],
            }),
            false,
        ),
        json!([text_block("Tools found: tool_a, tool_b")])
    );
}
