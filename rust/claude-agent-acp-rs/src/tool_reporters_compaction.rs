//! `tool-calls/reporters/compaction.ts`: the synthetic "Compact conversation"
//! tool call, for a client without the ACP compaction updates.
//!
//! AIR detects it by `_meta.jetbrains.air.contextCompaction`, which holds the
//! trigger, the token counts, the duration, and the error. The error also goes
//! to `content` once, because it is the result to show. No `rawOutput` repeats
//! the facts for AIR. Every other client gets the upstream fields: the tool
//! name `compact`, and the facts in `rawOutput`. It gets no AIR key.

use serde_json::{json, Map, Value};

use crate::context_compaction_meta::{create_context_compaction_meta, CompactionFacts};

/// The tool call title (`TITLE`).
pub const COMPACTION_TITLE: &str = "Compact conversation";

/// `compactionToolCall.started`: the opening `tool_call`.
#[must_use]
pub fn compaction_tool_call_started(compaction_id: &str, air_client: bool) -> Value {
    let mut update = Map::new();
    update.insert("sessionUpdate".into(), json!("tool_call"));
    // A missing id is `undefined` upstream and JSON drops the field; the empty
    // id is this port's spelling of it.
    if !compaction_id.is_empty() {
        update.insert("toolCallId".into(), json!(compaction_id));
    }
    update.insert("title".into(), json!(COMPACTION_TITLE));
    update.insert("kind".into(), json!("think"));
    update.insert("status".into(), json!("in_progress"));
    update.insert(
        "_meta".into(),
        compaction_meta(&CompactionFacts::default(), air_client),
    );
    Value::Object(update)
}

/// `compactionToolCall.inProgress`: the one re-asserted `in_progress` update.
#[must_use]
pub fn compaction_tool_call_in_progress(compaction_id: &str, air_client: bool) -> Value {
    let mut update = Map::new();
    update.insert("sessionUpdate".into(), json!("tool_call_update"));
    if !compaction_id.is_empty() {
        update.insert("toolCallId".into(), json!(compaction_id));
    }
    update.insert("status".into(), json!("in_progress"));
    update.insert(
        "_meta".into(),
        compaction_meta(&CompactionFacts::default(), air_client),
    );
    Value::Object(update)
}

/// `compactionToolCall.finished`: the terminal report.
///
/// A missed opening makes it the first report, a `tool_call`; otherwise it is
/// a `tool_call_update`. `status` is `None` for a boundary enrichment that
/// carries no status (`status: undefined`).
#[must_use]
pub fn compaction_tool_call_finished(
    compaction_id: &str,
    status: Option<&str>,
    facts: &CompactionFacts,
    first: bool,
    air_client: bool,
) -> Value {
    let error_content = if status == Some("failed") {
        facts
            .error
            .as_ref()
            .filter(|error| !error.is_empty())
            .map(|error| {
                json!([{
                    "type": "content",
                    "content": {"type": "text", "text": format!("Compaction failed: {error}")}
                }])
            })
    } else {
        None
    };
    let raw_output = (!air_client && !facts.is_empty()).then(|| facts.to_json());
    let mut update = Map::new();
    if first {
        update.insert("sessionUpdate".into(), json!("tool_call"));
        if !compaction_id.is_empty() {
            update.insert("toolCallId".into(), json!(compaction_id));
        }
        update.insert("title".into(), json!(COMPACTION_TITLE));
        update.insert("kind".into(), json!("think"));
        update.insert("status".into(), json!(status.unwrap_or("completed")));
    } else {
        update.insert("sessionUpdate".into(), json!("tool_call_update"));
        if !compaction_id.is_empty() {
            update.insert("toolCallId".into(), json!(compaction_id));
        }
        if let Some(status) = status {
            update.insert("status".into(), json!(status));
        }
    }
    if let Some(content) = error_content {
        update.insert("content".into(), content);
    }
    if let Some(raw_output) = raw_output {
        update.insert("rawOutput".into(), raw_output);
    }
    update.insert("_meta".into(), compaction_meta(facts, air_client));
    Value::Object(update)
}

/// The compaction facts for AIR, and the upstream tool name for every other
/// client (`meta`).
fn compaction_meta(facts: &CompactionFacts, air_client: bool) -> Value {
    if air_client {
        create_context_compaction_meta(facts)
    } else {
        json!({"claudeCode": {"toolName": "compact"}})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_and_in_progress_are_the_legacy_tool_call() {
        assert_eq!(
            compaction_tool_call_started("c", false),
            json!({
                "sessionUpdate": "tool_call", "toolCallId": "c",
                "title": "Compact conversation", "kind": "think",
                "status": "in_progress",
                "_meta": {"claudeCode": {"toolName": "compact"}}
            })
        );
        assert_eq!(
            compaction_tool_call_in_progress("c", true),
            json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "c",
                "status": "in_progress",
                "_meta": {"jetbrains": {"air": {"version": 1,
                    "contextCompaction": {"version": 1}}}}
            })
        );
    }

    #[test]
    fn finished_without_an_opening_is_a_terminal_tool_call() {
        let facts = CompactionFacts {
            trigger: Some("manual".into()),
            pre_tokens: Some(json!(3)),
            ..CompactionFacts::default()
        };
        assert_eq!(
            compaction_tool_call_finished("c", Some("completed"), &facts, true, false),
            json!({
                "sessionUpdate": "tool_call", "toolCallId": "c",
                "title": "Compact conversation", "kind": "think",
                "status": "completed",
                "rawOutput": {"trigger": "manual", "preTokens": 3},
                "_meta": {"claudeCode": {"toolName": "compact"}}
            })
        );
    }

    #[test]
    fn failed_carries_the_error_once_in_content_and_never_raw_output() {
        let facts = CompactionFacts {
            error: Some("summary rejected".into()),
            ..CompactionFacts::default()
        };
        assert_eq!(
            compaction_tool_call_finished("c", Some("failed"), &facts, true, true),
            json!({
                "sessionUpdate": "tool_call", "toolCallId": "c",
                "title": "Compact conversation", "kind": "think",
                "status": "failed",
                "content": [{
                    "type": "content",
                    "content": {"type": "text", "text": "Compaction failed: summary rejected"}
                }],
                "_meta": {"jetbrains": {"air": {"version": 1,
                    "contextCompaction": {"version": 1, "error": "summary rejected"}}}}
            })
        );
    }

    #[test]
    fn enrichment_after_an_opening_has_no_status() {
        let facts = CompactionFacts {
            trigger: Some("manual".into()),
            pre_tokens: Some(json!(3)),
            ..CompactionFacts::default()
        };
        assert_eq!(
            compaction_tool_call_finished("c", None, &facts, false, true),
            json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "c",
                "_meta": {"jetbrains": {"air": {"version": 1,
                    "contextCompaction": {"version": 1, "trigger": "manual", "preTokens": 3}}}}
            })
        );
        assert_eq!(
            compaction_tool_call_finished("c", None, &facts, false, false),
            json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "c",
                "rawOutput": {"trigger": "manual", "preTokens": 3},
                "_meta": {"claudeCode": {"toolName": "compact"}}
            })
        );
    }
}
