//! The ACP v2 tool-call translation (T-101, Q-91; upstream `src/v2/tool-call.ts`).
//!
//! Translates v1 `tool_call` and `tool_call_update` session updates into v2
//! `tool_call_update`s, drops null fields, validates content blocks (passing v2
//! diffs and display terminals, rejecting v1 diffs), and produces permission
//! subjects.

use agent_client_protocol::Error;
use serde_json::{Map, Value};

/// Check whether a tool call content block is a v2 diff.
///
/// Upstream: `isV2Diff(item): item is ToolCallContent & v2.ToolCallContent`.
/// A diff that the agent built in v2 form carries a `changes` array.
pub fn is_v2_diff(item: &Value) -> bool {
    item.as_object()
        .is_some_and(|obj| obj.contains_key("changes"))
}

/// The v2 form of a tool call content block.
///
/// Upstream: `v2ToolCallContent(item: ToolCallContent): v2.ToolCallContent`.
/// Content and terminal blocks pass through; v2 diffs pass through;
/// v1 diffs (which lack changes / operation details) are rejected.
pub fn v2_tool_call_content(item: &Value) -> Result<Value, Error> {
    let item_type = item.get("type").and_then(Value::as_str);
    match item_type {
        Some("content") | Some("terminal") => Ok(item.clone()),
        _ => {
            if is_v2_diff(item) {
                Ok(item.clone())
            } else {
                Err(Error::new(-32603, "An ACP v2 client cannot take a v1 diff"))
            }
        }
    }
}

/// The v2 form of the fields of a v1 tool call report.
///
/// Upstream: `v2ToolCallPatch(toolCall: ToolCall | ToolCallUpdate): v2.ToolCallUpdate`.
/// Drops null fields, drops `sessionUpdate`, translates `content`, and keeps
/// all other fields. Content is placed last after non-null patch fields.
///
/// C-24: a v1 `failed` result that the CLI stamped as aborted
/// (`nonExecutionKind` `interrupted`/`cancelled`) is `cancelled` on v2
/// (`AcpToolCallRenderer.resultStatus`, `renderer.ts:196-204`). The v1 status
/// has no `cancelled`, so the translation rewrites it here; every other field
/// keeps its v1 order.
pub fn v2_tool_call_patch(tool_call: &Value) -> Result<Value, Error> {
    let Some(obj) = tool_call.as_object() else {
        return Ok(tool_call.clone());
    };
    let aborted = obj.get("status").and_then(Value::as_str) == Some("failed")
        && tool_call
            .pointer("/_meta/claudeCode/nonExecutionKind")
            .and_then(Value::as_str)
            .is_some_and(|kind| matches!(kind, "interrupted" | "cancelled"));
    let mut map = Map::new();
    for (key, value) in obj {
        if key == "sessionUpdate" || key == "content" || value.is_null() {
            continue;
        }
        if key == "status" && aborted {
            map.insert(
                key.clone(),
                Value::String(super::session::tool_call_status_for_aborted(true).to_string()),
            );
            continue;
        }
        map.insert(key.clone(), value.clone());
    }
    if let Some(content_val) = obj.get("content") {
        if !content_val.is_null() {
            let content = if let Some(arr) = content_val.as_array() {
                let mut items = Vec::with_capacity(arr.len());
                for item in arr {
                    items.push(v2_tool_call_content(item)?);
                }
                Value::Array(items)
            } else {
                content_val.clone()
            };
            map.insert("content".to_string(), content);
        }
    }
    Ok(Value::Object(map))
}

/// The v2 form of a v1 tool call report.
///
/// Upstream: `v2ToolCallUpdate(report: V1ToolCallReport): V2ToolCallUpdate`.
/// Upserts by `toolCallId` with `sessionUpdate: "tool_call_update"`.
/// Spreads the patch fields first and sets `sessionUpdate` last.
pub fn v2_tool_call_update(report: &Value) -> Result<Value, Error> {
    let patch = v2_tool_call_patch(report)?;
    let mut map = match patch {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("value".to_string(), other);
            m
        }
    };
    map.insert(
        "sessionUpdate".to_string(),
        Value::String("tool_call_update".to_string()),
    );
    Ok(Value::Object(map))
}
