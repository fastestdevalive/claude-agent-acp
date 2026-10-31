//! T8A — `toolUpdateFromDiffToolResponse` (the `hooks` port of
//! `dist/diff.js:toolUpdateFromDiffToolResponse`). The tests mirror the
//! upstream inputs and assert the exact ACP diff/location shape.

use claude_agent_acp_rs::hooks::tool_update_from_diff_tool_response;
use serde_json::{json, Value};

fn update(tool_response: Value) -> Value {
    let (content, locations) = tool_update_from_diff_tool_response(&tool_response);
    let mut result = serde_json::Map::new();
    if !content.is_empty() {
        result.insert(
            "content".into(),
            serde_json::to_value(&content).unwrap_or(Value::Null),
        );
    }
    if !locations.is_empty() {
        result.insert(
            "locations".into(),
            serde_json::to_value(&locations).unwrap_or(Value::Null),
        );
    }
    Value::Object(result)
}

#[test]
fn t8a_empty_for_non_object_input() {
    for input in [Value::Null, json!("string"), json!(42)] {
        assert_eq!(update(input), json!({}));
    }
}

#[test]
fn t8a_empty_when_file_path_or_structured_patch_is_missing() {
    assert_eq!(update(json!({})), json!({}));
    assert_eq!(update(json!({"filePath": "/foo.ts"})), json!({}));
    assert_eq!(update(json!({"structuredPatch": []})), json!({}));
}

#[test]
fn t8a_single_hunk_structured_patch() {
    assert_eq!(
        update(json!({
            "filePath": "/Users/test/project/test.txt",
            "structuredPatch": [{
                "oldStart": 1,
                "oldLines": 3,
                "newStart": 1,
                "newLines": 3,
                "lines": [" context before", "-old line", "+new line", " context after"],
            }],
        })),
        json!({
            "content": [{
                "type": "diff",
                "path": "/Users/test/project/test.txt",
                "oldText": "context before\nold line\ncontext after",
                "newText": "context before\nnew line\ncontext after",
            }],
            "locations": [{"path": "/Users/test/project/test.txt", "line": 1}],
        })
    );
}

#[test]
fn t8a_multiple_hunks_become_multiple_diffs() {
    assert_eq!(
        update(json!({
            "filePath": "/Users/test/project/file.ts",
            "structuredPatch": [
                {"oldStart": 5, "oldLines": 1, "newStart": 5, "newLines": 1,
                 "lines": ["-oldValue", "+newValue"]},
                {"oldStart": 20, "oldLines": 1, "newStart": 20, "newLines": 1,
                 "lines": ["-oldValue", "+newValue"]},
            ],
        })),
        json!({
            "content": [
                {"type": "diff", "path": "/Users/test/project/file.ts",
                 "oldText": "oldValue", "newText": "newValue"},
                {"type": "diff", "path": "/Users/test/project/file.ts",
                 "oldText": "oldValue", "newText": "newValue"},
            ],
            "locations": [
                {"path": "/Users/test/project/file.ts", "line": 5},
                {"path": "/Users/test/project/file.ts", "line": 20},
            ],
        })
    );
}

#[test]
fn t8a_deletion_has_an_empty_new_text() {
    assert_eq!(
        update(json!({
            "filePath": "/Users/test/project/file.ts",
            "structuredPatch": [{
                "oldStart": 10,
                "oldLines": 2,
                "newStart": 10,
                "newLines": 1,
                "lines": [" context", "-removed line"],
            }],
        })),
        json!({
            "content": [{
                "type": "diff",
                "path": "/Users/test/project/file.ts",
                "oldText": "context\nremoved line",
                "newText": "context",
            }],
            "locations": [{"path": "/Users/test/project/file.ts", "line": 10}],
        })
    );
}

#[test]
fn t8a_empty_structured_patch_array_is_empty() {
    assert_eq!(
        update(json!({
            "filePath": "/Users/test/project/file.ts",
            "structuredPatch": [],
        })),
        json!({})
    );
}

#[test]
fn t8a_write_create_and_edit_shaped_responses_stay_empty() {
    // A create's optimistic content already shows creation semantics.
    assert_eq!(
        update(json!({
            "filePath": "/Users/test/project/new.ts",
            "structuredPatch": [],
            "type": "create",
            "content": "new content",
            "originalFile": null,
        })),
        json!({})
    );
    // FileEditOutput carries no `type`; its optimistic old/new diff is truthful.
    assert_eq!(
        update(json!({
            "filePath": "/Users/test/project/file.ts",
            "structuredPatch": [],
            "originalFile": "old content",
        })),
        json!({})
    );
}
