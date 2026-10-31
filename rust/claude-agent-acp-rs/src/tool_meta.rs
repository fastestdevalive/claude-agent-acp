//! Tool-call attribution helpers (rust-live-divergences D14 / D15).
//!
//! Node stamps two kinds of attribution onto a tool report:
//!
//! - `_meta.claudeCode.parentToolUseId` for every update made while processing
//!   a frame whose `parent_tool_use_id` is a string (`stampParentToolUseId`,
//!   `acp-agent.js:552`).
//! - `_meta.claudeCode.nonExecutionKind` / `userFeedback` from the message-level
//!   `tool_result_meta` sidecar (`tool-result-meta.ts:18-34`,
//!   `acp-agent.js:10124,10162`).
//!
//! Both are pure: this module validates the sidecar and builds the `_meta`
//! object in Node's key order (`toolName`, the non-execution keys, then the
//! parent last), so a tool update is byte-identical to the Node adapter's.

use std::collections::HashMap;

use serde_json::{Map, Value};

/// One validated `tool_result_meta` entry (`tool-result-meta.ts:1-5`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonExecution {
    /// Why the tool did not run: `user-rejected`, `permission-rule`,
    /// `interrupted`, `cancelled`, `automode-*`, … (`tool-result-meta.ts:8-14`).
    pub kind: String,
    /// The user's feedback when the CLI recorded one.
    pub feedback: Option<String>,
}

/// Validate the untyped `tool_result_meta` sidecar (`parseToolResultMeta`,
/// `tool-result-meta.ts:18-34`). Non-array input yields an empty map; a
/// non-object entry or one without a string `id` / `non_execution_kind` is
/// skipped. Unknown kinds are preserved so newer CLIs stay forward-compatible;
/// a later entry for the same id wins (Node's `Map.set`).
pub fn parse_tool_result_meta(raw: &Value) -> HashMap<String, NonExecution> {
    let mut by_id = HashMap::new();
    let Some(entries) = raw.as_array() else {
        return by_id;
    };
    for entry in entries {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let (Some(id), Some(kind)) = (
            entry.get("id").and_then(Value::as_str),
            entry.get("non_execution_kind").and_then(Value::as_str),
        ) else {
            continue;
        };
        by_id.insert(
            id.to_string(),
            NonExecution {
                kind: kind.to_string(),
                feedback: entry
                    .get("user_feedback")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
        );
    }
    by_id
}

/// The `_meta` of a tool report with attribution: `{claudeCode:{toolName,
/// nonExecutionKind?, userFeedback?, parentToolUseId?}}`.
///
/// Node spreads the result meta first and stamps the parent after it
/// (`renderer.ts:493-499` + `stampParentToolUseId`), so the parent is the last
/// key inside `claudeCode`.
pub fn meta_with_parent_and_nonexec(
    name: &str,
    non_exec: Option<&NonExecution>,
    parent: Option<&str>,
) -> Map<String, Value> {
    let mut claude_code = Map::new();
    claude_code.insert("toolName".into(), Value::String(name.to_string()));
    if let Some(non_exec) = non_exec {
        claude_code.insert(
            "nonExecutionKind".into(),
            Value::String(non_exec.kind.clone()),
        );
        if let Some(feedback) = &non_exec.feedback {
            claude_code.insert("userFeedback".into(), Value::String(feedback.clone()));
        }
    }
    if let Some(parent) = parent {
        claude_code.insert("parentToolUseId".into(), Value::String(parent.to_string()));
    }
    let mut meta = Map::new();
    meta.insert("claudeCode".into(), Value::Object(claude_code));
    meta
}

/// Stamp `parentToolUseId` into an existing `_meta` object, keeping every other
/// key (`stampParentToolUseId`, `acp-agent.js:552`). A second call with the same
/// parent leaves the value unchanged, so the shared stamper can never double
/// stamp. An absent parent is a no-op.
pub fn stamp_parent(meta: &mut Map<String, Value>, parent: Option<&str>) {
    let Some(parent) = parent else {
        return;
    };
    let claude_code = meta
        .entry("claudeCode".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !claude_code.is_object() {
        *claude_code = Value::Object(Map::new());
    }
    if let Some(claude_code) = claude_code.as_object_mut() {
        claude_code.insert("parentToolUseId".into(), Value::String(parent.to_string()));
    }
}

/// Whether the CLI stamped the result because the tool's run was aborted
/// (`isAbortedToolResult`, `tool-result-meta.ts:8-14`): `interrupted` or
/// `cancelled`. Only a v2 client maps that to its `cancelled` status; every
/// other client keeps `failed`, so this is used by the v2 gate.
pub fn is_aborted(non_exec: Option<&NonExecution>) -> bool {
    non_exec.is_some_and(|non_exec| matches!(non_exec.kind.as_str(), "interrupted" | "cancelled"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `parseToolResultMeta` skips malformed entries and keeps unknown kinds.
    #[test]
    fn inv_77_parse_tool_result_meta_validates_and_preserves_unknown() {
        let parsed = parse_tool_result_meta(&json!([
            {"id": "t1", "non_execution_kind": "user-rejected", "user_feedback": "no"},
            {"id": 2, "non_execution_kind": "interrupted"},
            {"id": "t3"},
            {"id": "t4", "non_execution_kind": "some-future-kind"},
            "not-an-object",
        ]));
        assert_eq!(parsed.len(), 2, "{parsed:?}");
        assert_eq!(
            parsed.get("t1"),
            Some(&NonExecution {
                kind: "user-rejected".into(),
                feedback: Some("no".into()),
            })
        );
        assert_eq!(
            parsed.get("t4"),
            Some(&NonExecution {
                kind: "some-future-kind".into(),
                feedback: None,
            })
        );
        assert!(parse_tool_result_meta(&json!({"id": "t1"})).is_empty());
        assert!(parse_tool_result_meta(&Value::Null).is_empty());
    }

    /// Node's key order: `toolName`, `nonExecutionKind`, `userFeedback`,
    /// `parentToolUseId`; an absent optional key is omitted.
    #[test]
    fn inv_77_meta_key_order_and_omission() {
        let non_exec = NonExecution {
            kind: "user-rejected".into(),
            feedback: Some("nope".into()),
        };
        let meta = meta_with_parent_and_nonexec("Edit", Some(&non_exec), Some("toolu_P"));
        let keys: Vec<&String> = meta["claudeCode"].as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "toolName",
                "nonExecutionKind",
                "userFeedback",
                "parentToolUseId"
            ]
        );
        assert_eq!(meta["claudeCode"]["toolName"], "Edit");
        assert_eq!(meta["claudeCode"]["nonExecutionKind"], "user-rejected");
        assert_eq!(meta["claudeCode"]["userFeedback"], "nope");
        assert_eq!(meta["claudeCode"]["parentToolUseId"], "toolu_P");

        let plain = meta_with_parent_and_nonexec("Bash", None, None);
        assert_eq!(plain["claudeCode"], json!({"toolName": "Bash"}));
    }

    /// A stamp never duplicates the key and keeps the other claudeCode keys.
    #[test]
    fn inv_76_no_double_stamp() {
        let mut meta = meta_with_parent_and_nonexec("Read", None, Some("toolu_P"));
        stamp_parent(&mut meta, Some("toolu_P"));
        assert_eq!(
            meta["claudeCode"],
            json!({"toolName": "Read", "parentToolUseId": "toolu_P"})
        );
        stamp_parent(&mut meta, None);
        assert_eq!(meta["claudeCode"]["parentToolUseId"], "toolu_P");
    }

    /// `isAbortedToolResult` covers exactly `interrupted` / `cancelled`.
    #[test]
    fn inv_77_is_aborted_kinds() {
        assert!(is_aborted(Some(&NonExecution {
            kind: "interrupted".into(),
            feedback: None
        })));
        assert!(is_aborted(Some(&NonExecution {
            kind: "cancelled".into(),
            feedback: None
        })));
        assert!(!is_aborted(Some(&NonExecution {
            kind: "user-rejected".into(),
            feedback: None
        })));
        assert!(!is_aborted(None));
    }
}
