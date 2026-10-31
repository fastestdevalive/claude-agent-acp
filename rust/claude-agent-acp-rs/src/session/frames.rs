//! Reports of the `system` / top-level frames that only describe a tool call
//! (F7a): `tool_progress` (N-20), `memory_recall` (T-05 / T-82) and
//! `permission_denied` (P-01 / T-81).
//!
//! None of them reaches the turn machine: they are tool reports, never a turn's
//! echo or result, so they cannot activate a queued head (the hazard of
//! `ensure_active_turn`). The functions here are pure over the mapper state.

use std::collections::HashMap;

use agent_client_protocol::schema::v1::{
    SessionUpdate, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use serde_json::{json, Map, Value};

use crate::map::MapState;
use crate::tools;

/// A string field of a frame; `None` for a missing or non-string value.
fn str_of<'a>(frame: &'a Value, key: &str) -> Option<&'a str> {
    frame.get(key).and_then(Value::as_str)
}

/// The report of a `memory_recall` frame (`AcpToolCallRenderer.memoryRecall`,
/// `renderer.ts:578-605`): a completed `read` tool call. A synthesis shows the
/// recalled text, a plain recall shows the memory files. Returned as the raw
/// `update` JSON: the renderer leaves `content` and `locations` out when empty.
pub fn memory_recall_update(frame: &Value) -> Value {
    let mode = str_of(frame, "mode").unwrap_or("");
    let memories: &[Value] = frame
        .get("memories")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    let is_synthesis = mode == "synthesize";
    let locations: Vec<Value> = if is_synthesis {
        Vec::new()
    } else {
        memories
            .iter()
            .map(|memory| json!({ "path": memory.get("path").cloned().unwrap_or(Value::Null) }))
            .collect()
    };
    let content: Vec<Value> = if is_synthesis {
        memories
            .iter()
            .filter_map(|memory| memory.get("content").and_then(Value::as_str))
            .map(|text| json!({ "type": "content", "content": { "type": "text", "text": text } }))
            .collect()
    } else {
        Vec::new()
    };
    let count = memories.len();
    let title = if is_synthesis {
        "Recalled synthesized memory".to_string()
    } else {
        format!(
            "Recalled {count} {}",
            if count == 1 { "memory" } else { "memories" }
        )
    };
    let mut update = Map::new();
    update.insert("sessionUpdate".into(), json!("tool_call"));
    update.insert(
        "toolCallId".into(),
        frame.get("uuid").cloned().unwrap_or(Value::Null),
    );
    update.insert("title".into(), json!(title));
    update.insert("kind".into(), json!("read"));
    update.insert("status".into(), json!("completed"));
    if !locations.is_empty() {
        update.insert("locations".into(), Value::Array(locations));
    }
    if !content.is_empty() {
        update.insert("content".into(), Value::Array(content));
    }
    update.insert(
        "_meta".into(),
        json!({ "claudeCode": { "toolName": "memory_recall", "toolResponse": { "mode": mode } } }),
    );
    Value::Object(update)
}

/// The report of a `tool_progress` beat (`acp-agent.ts:8125-8176`,
/// `AcpToolCallRenderer.progress`): an `in_progress` update of the tool call
/// the beat reports on. `None` when the beat names no call the client has seen
/// (or repeats the last beat).
///
/// Not every beat reports under the id of a tool call the client has seen:
/// heartbeats derive `<tool_use_id>-heartbeat-<n>`, and the `agent_api_retry`
/// beats report under `agent_<assistant_message_id>`. The SDK stamps
/// `parent_tool_use_id` with the executing tool's real id whenever the beat does
/// not carry one of its own, so the beat falls back to it.
pub fn tool_progress_update(
    frame: &Value,
    state: &mut MapState,
    live_background: &HashMap<String, String>,
    air: bool,
) -> Option<SessionUpdate> {
    let tool_use_id = str_of(frame, "tool_use_id").unwrap_or("");
    let parent = str_of(frame, "parent_tool_use_id");
    let call_id = if state.has_emitted(tool_use_id) {
        tool_use_id
    } else {
        parent?
    };
    // Ids leave the emitted set at `tool_result`, so this also stops a beat that
    // races past completion from reopening a finished call.
    if !state.has_emitted(call_id) {
        return None;
    }
    // The beat is in the subagent whose Agent call is `parent_tool_use_id`,
    // unless it reports on that Agent call itself.
    let subagent_parent = parent.filter(|parent| {
        !parent.is_empty()
            && *parent != call_id
            && live_background.values().any(|value| value == parent)
    });
    // `tool_name` names the tool that reports the beat. When the beat falls back
    // to the parent call, AIR gets the name of that call, or no name; every other
    // client gets `tool_name`, like upstream.
    let tool_name = if call_id != tool_use_id && air {
        state
            .tool_use_from_cache(call_id)
            .and_then(|tool_use| tool_use.get("name"))
            .and_then(Value::as_str)
    } else {
        str_of(frame, "tool_name")
    };
    let mut tool_response = Map::new();
    tool_response.insert(
        "elapsedTimeSeconds".into(),
        frame
            .get("elapsed_time_seconds")
            .cloned()
            .unwrap_or(Value::Null),
    );
    // For Agent/Task calls: the subagent type, and the SDK retry counters while
    // the subagent waits out an API rate limit.
    if let Some(subagent_type) = frame.get("subagent_type").filter(|v| !v.is_null()) {
        tool_response.insert("subagentType".into(), subagent_type.clone());
    }
    if let Some(retry) = frame.get("subagent_retry").filter(|v| !v.is_null()) {
        tool_response.insert("subagentRetry".into(), retry.clone());
    }
    let mut claude_code = Map::new();
    if let Some(name) = tool_name {
        claude_code.insert("toolName".into(), json!(name));
    }
    if let Some(parent) = subagent_parent {
        claude_code.insert("parentToolUseId".into(), json!(parent));
    }
    claude_code.insert("toolResponse".into(), Value::Object(tool_response));

    // The field tracker sends the `in_progress` status once, so a later beat
    // carries only the progress.
    let fields = ToolCallUpdateFields::new().status(ToolCallStatus::InProgress);
    let changed = state.track_tool_call_update(call_id, &fields);
    let meta_changed = state.progress_meta_changed(call_id, &claude_code);
    if changed.is_none() && !meta_changed {
        return None;
    }
    // A later update can be the first to name the subagent a call belongs to
    // (`recordToolCallParent` in `sendUpdate`).
    if let Some(parent) = subagent_parent {
        state.record_tool_call_parent(call_id, parent);
    }
    let mut meta = Map::new();
    meta.insert("claudeCode".into(), Value::Object(claude_code));
    Some(SessionUpdate::ToolCallUpdate(
        ToolCallUpdate::new(call_id.to_string(), changed.unwrap_or_default()).meta(meta),
    ))
}

/// The report of a `permission_denied` frame (`acp-agent.ts:6299-6353`,
/// `AcpToolCallRenderer.permissionDenied`, `renderer.ts:612-644`): the tool call
/// a rule, the classifier or a mode denied before it ran fails with the reason.
///
/// `None` when the client was never given the call (the assistant message that
/// carried the tool_use was dropped by the cancelled-turn guard, issue #851), or
/// when an agent-scoped denial has no known lineage for a client that shows
/// subagents.
pub fn permission_denied_update(
    frame: &Value,
    state: &mut MapState,
    live_background: &HashMap<String, String>,
    air: bool,
    supports_subagents: bool,
) -> Option<SessionUpdate> {
    let tool_use_id = str_of(frame, "tool_use_id")?;
    // The tool is denied: its PostToolUse hook will not run.
    state.pending_hooks.remove(tool_use_id);
    if !state.has_emitted(tool_use_id) {
        return None;
    }
    // A denial inside a subagent identifies the subagent by `agent_id` (as
    // canUseTool does), never by the Agent/Task call that spawned it.
    let agent_id = str_of(frame, "agent_id").filter(|id| !id.is_empty());
    let parent = agent_id.and_then(|id| live_background.get(id).map(String::as_str));
    if agent_id.is_some() && parent.is_none() && supports_subagents {
        // An agent-scoped denial with missing lineage cannot safely be presented
        // in the root transcript.
        return None;
    }
    let decision_reason = str_of(frame, "decision_reason");
    let message = str_of(frame, "message");
    let reason = decision_reason.or(message);
    // AIR and v2 get the SDK message only when it differs from the reason, which
    // the content shows. Every other client gets the upstream toolResponse.
    let mut response = Map::new();
    if let Some(kind) = frame.get("decision_reason_type").filter(|v| !v.is_null()) {
        response.insert("decisionReasonType".into(), kind.clone());
    }
    if !air {
        if let Some(reason) = decision_reason {
            response.insert("decisionReason".into(), json!(reason));
        }
        if let Some(message) = message {
            response.insert("message".into(), json!(message));
        }
    } else if let (Some(reason), Some(message)) = (decision_reason, message) {
        if message != reason {
            response.insert("message".into(), json!(message));
        }
    }
    let mut claude_code = Map::new();
    claude_code.insert(
        "toolName".into(),
        frame.get("tool_name").cloned().unwrap_or(Value::Null),
    );
    if let Some(parent) = parent.filter(|parent| !parent.is_empty()) {
        claude_code.insert("parentToolUseId".into(), json!(parent));
    }
    claude_code.insert("toolResponse".into(), Value::Object(response));
    let mut meta = Map::new();
    meta.insert("claudeCode".into(), Value::Object(claude_code));

    let text = format!("Permission denied: {}", reason.unwrap_or("undefined"));
    let fields = ToolCallUpdateFields::new()
        .status(ToolCallStatus::Failed)
        .content(vec![tools::text_content(text)]);
    // A denial is final, so it replaces a pinned approval patch, and the call
    // needs no parent record any more.
    state.forget_tool_call_parent(tool_use_id);
    let fields = state.track_final_tool_call_update(tool_use_id, &fields)?;
    Some(SessionUpdate::ToolCallUpdate(
        ToolCallUpdate::new(tool_use_id.to_string(), fields).meta(meta),
    ))
}

// ---------------------------------------------------------------- F7b

/// A JS template-literal rendering of a frame field (`${message.x}`): a missing
/// or null field prints `undefined` / `null`, a number prints without a fraction
/// when integral.
fn js_text(frame: &Value, key: &str) -> String {
    match frame.get(key) {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => match number.as_f64() {
            Some(float) if float.fract() == 0.0 && float.abs() < 1e15 => {
                format!("{}", float as i64)
            }
            _ => number.to_string(),
        },
        Some(other) => other.to_string(),
    }
}

/// Length in UTF-16 units, as JavaScript counts a string.
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// What an `informational` frame produces.
#[derive(Debug, Clone, PartialEq)]
pub struct Informational {
    /// The `update` to publish (a `notice`, or the transcript line).
    pub update: Value,
    /// A transcript line is assistant text the client was given.
    pub transcript: bool,
    /// The normalised text to remember on the turn (notice clients only).
    pub notice_text: Option<String>,
}

/// The report of an `informational` frame (`acp-agent.ts:6354-6406`): a
/// free-form notice from the SDK (why a hook blocked continuation, say).
/// Clients on the notice contract get a `notice` at the SDK's level, except
/// `info` (transcript-mode noise) and a repeat for the same tool call; the
/// rest get a transcript line with the level folded into the text.
pub fn informational_update(
    frame: &Value,
    supports_notices: bool,
    noticed_tool_uses: &mut std::collections::HashSet<String>,
) -> Option<Informational> {
    let level = str_of(frame, "level").unwrap_or("info");
    let content = str_of(frame, "content").unwrap_or("");
    if supports_notices {
        if level == "info" {
            return None;
        }
        if let Some(tool_use_id) = str_of(frame, "tool_use_id").filter(|id| !id.is_empty()) {
            if !noticed_tool_uses.insert(tool_use_id.to_string()) {
                return None;
            }
        }
    }
    let severity = if level == "warning" {
        "warning"
    } else {
        "info"
    };
    let transcript_text = if level == "info" {
        content.to_string()
    } else {
        format!("**{}:** {content}", crate::notices::sentence_case(level))
    };
    let (title, description) = crate::notices::split_notice_text(
        content,
        if severity == "warning" {
            "Claude reported a warning"
        } else {
            "Claude reported a notice"
        },
    );
    let notice = crate::notices::SessionNotice::new(severity, title, description);
    let meta = json!({ "claudeCode": { "kind": "informational", "level": level } });
    let update = crate::notices::notice_or_transcript_update(
        &notice,
        supports_notices,
        Some(&transcript_text),
        Some(&meta),
    );
    Some(Informational {
        update,
        transcript: !supports_notices,
        notice_text: supports_notices.then(|| crate::notices::normalize_notice_text(content)),
    })
}

/// The `mirror_error` log line (`acp-agent.ts:6289-6298`): the SDK failed to
/// persist session history, a potential data loss worth a log line.
pub fn mirror_error_log(frame: &Value) -> String {
    format!(
        "Session {}: failed to persist history: {}",
        js_text(frame, "session_id"),
        js_text(frame, "error")
    )
}

/// The title of an `api_retry` warning (`acp-agent.ts:6606-6631`) and the
/// failure kind it publishes under; `None` for a 401 retry, which is the CLI's
/// credential re-check: it gives up on the first attempt and reports the
/// sign-out, which carries the `login` action (issue #1072).
pub fn api_retry_title(
    frame: &Value,
) -> Option<(crate::session_failure::ClaudeFailureKind, String)> {
    use crate::session_failure::{provider_failure_category, ClaudeFailureKind};
    let no_status = frame.get("error_status") == Some(&Value::Null);
    let kind = if no_status {
        ClaudeFailureKind::TransportLost
    } else {
        provider_failure_category(str_of(frame, "error"), false)
    };
    if kind == ClaudeFailureKind::AuthRequired {
        return None;
    }
    // `no_response` (SDK 0.3.261+): the API sent no response headers within the
    // first-byte window, so this retry waits longer for them.
    let seconds = |ms: f64| format!("{}s", (ms / 1000.0).round().max(1.0) as i64);
    let no_response = frame
        .get("no_response")
        .filter(|value| value.is_object())
        .map(|value| {
            let ms = |key: &str| value.get(key).and_then(Value::as_f64).unwrap_or(0.0);
            format!(
                " No response after {}; waiting up to {}.",
                seconds(ms("waited_ms")),
                seconds(ms("retry_wait_ms"))
            )
        })
        .unwrap_or_default();
    let attempt = format!(
        "attempt {} of {}.{no_response}",
        js_text(frame, "attempt"),
        js_text(frame, "max_retries")
    );
    let title = if no_status {
        format!("Reconnecting to Claude, {attempt}")
    } else {
        format!("Retrying Claude, {attempt}")
    };
    Some((kind, title))
}

/// How a `model_refusal_fallback` reaches the user.
#[derive(Debug, Clone, PartialEq)]
pub enum FallbackDelivery {
    /// A `notice`, or the bold-label transcript line.
    Update { update: Value, transcript: bool },
    /// An AIR advisory failure record.
    Advisory {
        title: String,
        details: Option<String>,
    },
}

/// The report of a `model_refusal_fallback` frame (`acp-agent.ts:6632-6709`):
/// the SDK retried a refused turn on the fallback model. Returns the delivery
/// and the model the session now runs, when the swap is persistent.
///
/// Current CLIs only emit direction `retry` (persistent swap); `revert` marks a
/// turn-only fallback. `scope: "local"` (CLI 2.1.232+) marks a subagent,
/// side-question or background fork response that fell back while the session
/// model is unchanged. Absent scope means an older CLI, where every retry was a
/// session-level swap.
pub fn model_refusal_fallback(
    frame: &Value,
    supports_notices: bool,
    air_session_failures: bool,
) -> (FallbackDelivery, Option<String>) {
    let local = str_of(frame, "scope") == Some("local");
    let persistent = str_of(frame, "direction") != Some("revert") && !local;
    let category = str_of(frame, "api_refusal_category")
        .filter(|category| !category.is_empty())
        .map(|category| format!(" ({category})"))
        .unwrap_or_default();
    let fallback_model = js_text(frame, "fallback_model");
    let original_model = js_text(frame, "original_model");
    let outcome = if persistent {
        format!("The session will continue on {fallback_model}.")
    } else if local {
        format!(
            "Only that response came from {fallback_model}; the session stays on {original_model}."
        )
    } else {
        format!("The session stays on {original_model}.")
    };
    let summary = format!(
        "{original_model} declined this request{category}; retried with {fallback_model}. {outcome}"
    );
    let explanation = str_of(frame, "api_refusal_explanation").filter(|text| !text.is_empty());
    let notice_text = match explanation {
        Some(explanation) => format!("{summary}\n\n{explanation}"),
        None => summary.clone(),
    };
    let max = crate::notices::MAX_NOTICE_TITLE_LENGTH;
    let delivery = if !supports_notices && air_session_failures {
        let use_details = explanation.is_some_and(|text| js_len(&summary) + 2 + js_len(text) > max);
        FallbackDelivery::Advisory {
            title: if use_details { summary } else { notice_text },
            details: use_details.then(|| explanation.unwrap_or_default().to_string()),
        }
    } else {
        // A title must stand alone: the one-line summary when it is short
        // enough, else a generic title with everything in the description.
        let notice = if js_len(&summary) > max {
            crate::notices::SessionNotice::new(
                "warning",
                "Model fallback",
                Some(notice_text.clone()),
            )
        } else {
            crate::notices::SessionNotice::new("warning", summary, explanation.map(str::to_string))
        };
        let transcript = format!("**Model fallback:** {notice_text}");
        FallbackDelivery::Update {
            update: crate::notices::notice_or_transcript_update(
                &notice,
                supports_notices,
                Some(&transcript),
                None,
            ),
            transcript: !supports_notices,
        }
    };
    let synced = (persistent
        && frame
            .get("fallback_model")
            .and_then(Value::as_str)
            .is_some())
    .then_some(fallback_model);
    (delivery, synced)
}

/// The agent id that a successful SendMessage result resumed
/// (`resumedNativeSubagentId`, `native-subagents.ts:1349-1353`): the SDK puts it
/// in `tool_use_result.resumedAgentId` when a finished agent runs again.
pub fn resumed_native_subagent_id(tool_use_result: Option<&Value>) -> Option<String> {
    let result = tool_use_result.filter(|value| value.is_object())?;
    if result.get("success") != Some(&Value::Bool(true)) {
        return None;
    }
    let id = result.get("resumedAgentId").and_then(Value::as_str)?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// `isSyntheticLoginMessage` (`acp-agent.ts:2314-2329`) on the API message of an
/// assistant frame: the CLI's own "Please run /login" reply.
pub fn is_synthetic_login_message(api_message: &Value) -> bool {
    if api_message.get("model").and_then(Value::as_str) != Some("<synthetic>") {
        return false;
    }
    let Some([block]) = api_message
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    else {
        return false;
    };
    block.get("type").and_then(Value::as_str) == Some("text")
        && block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.contains("Please run /login"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call the client holds: surfaced, cached and tracked.
    fn emitted(state: &mut MapState, id: &str, name: &str) {
        state.mark_emitted(
            id.to_string(),
            json!({"type": "tool_use", "id": id, "name": name, "input": {}}),
        );
        state.seed_tool_tracker(
            id.to_string(),
            crate::map::TrackedToolCall::initial(
                agent_client_protocol::schema::v1::ToolKind::Execute,
                name.to_string(),
                Vec::new(),
                Vec::new(),
                None,
            ),
        );
    }

    fn wire(update: &SessionUpdate) -> Value {
        serde_json::to_value(update).unwrap()
    }

    /// T-82 — a plain recall lists the memory files and counts them.
    #[test]
    fn t2a_memory_recall_plain_lists_the_files() {
        let one = memory_recall_update(&json!({
            "uuid": "m1", "mode": "select", "memories": [{"path": "/m/a.md"}]
        }));
        assert_eq!(one["title"], "Recalled 1 memory");
        assert_eq!(one["locations"], json!([{"path": "/m/a.md"}]));
        assert!(one.get("content").is_none(), "no content when empty");
        assert_eq!(one["toolCallId"], "m1");
        assert_eq!(one["kind"], "read");
        assert_eq!(one["status"], "completed");
        assert_eq!(
            one["_meta"],
            json!({"claudeCode": {"toolName": "memory_recall", "toolResponse": {"mode": "select"}}})
        );
        let two = memory_recall_update(&json!({
            "uuid": "m2", "mode": "select",
            "memories": [{"path": "/m/a.md"}, {"path": "/m/b.md", "content": "ignored"}]
        }));
        assert_eq!(two["title"], "Recalled 2 memories");
        assert!(two.get("content").is_none());
    }

    /// T-82 — a synthesis shows the recalled text and no files.
    #[test]
    fn t2a_memory_recall_synthesis_shows_the_text() {
        let update = memory_recall_update(&json!({
            "uuid": "m3", "mode": "synthesize",
            "memories": [{"path": "/m/a.md", "content": "remember this"}, {"path": "/m/b.md"}]
        }));
        assert_eq!(update["title"], "Recalled synthesized memory");
        assert!(update.get("locations").is_none());
        assert_eq!(
            update["content"],
            json!([{"type": "content", "content": {"type": "text", "text": "remember this"}}])
        );
    }

    /// N-20 — a beat reports under its own id, else the parent call; the status
    /// goes out once and an identical beat is dropped.
    #[test]
    fn t2a_tool_progress_resolves_the_id_and_dedupes() {
        let mut state = MapState::default();
        emitted(&mut state, "toolu_A", "Bash");
        let live = HashMap::new();
        let beat = json!({
            "type": "tool_progress", "tool_use_id": "toolu_A", "tool_name": "Bash",
            "parent_tool_use_id": null, "elapsed_time_seconds": 3
        });
        let first = tool_progress_update(&beat, &mut state, &live, false).expect("first beat");
        let first = wire(&first);
        assert_eq!(first["toolCallId"], "toolu_A");
        assert_eq!(first["status"], "in_progress");
        assert_eq!(
            first["_meta"]["claudeCode"],
            json!({"toolName": "Bash", "toolResponse": {"elapsedTimeSeconds": 3}})
        );
        // The same beat again carries nothing new.
        assert!(tool_progress_update(&beat, &mut state, &live, false).is_none());
        // A later beat carries only the progress, not the status.
        let later = json!({
            "type": "tool_progress", "tool_use_id": "toolu_A", "tool_name": "Bash",
            "parent_tool_use_id": null, "elapsed_time_seconds": 8
        });
        let later = wire(&tool_progress_update(&later, &mut state, &live, false).expect("later"));
        assert!(later.get("status").is_none(), "{later}");
        assert_eq!(
            later["_meta"]["claudeCode"]["toolResponse"]["elapsedTimeSeconds"],
            8
        );
    }

    /// N-20 — a synthetic heartbeat id falls back to the parent call; AIR names
    /// the parent's tool, an unknown id with no usable parent is dropped.
    #[test]
    fn t2a_tool_progress_falls_back_to_the_parent_call() {
        let mut state = MapState::default();
        emitted(&mut state, "toolu_AGENT", "Agent");
        let live = HashMap::new();
        let heartbeat = json!({
            "type": "tool_progress", "tool_use_id": "toolu_AGENT-heartbeat-1",
            "tool_name": "heartbeat", "parent_tool_use_id": "toolu_AGENT",
            "elapsed_time_seconds": 1, "subagent_type": "general-purpose",
            "subagent_retry": {"attempt": 2}
        });
        let mut plain = MapState::default();
        emitted(&mut plain, "toolu_AGENT", "Agent");
        let update = wire(&tool_progress_update(&heartbeat, &mut plain, &live, false).unwrap());
        assert_eq!(update["toolCallId"], "toolu_AGENT");
        assert_eq!(update["_meta"]["claudeCode"]["toolName"], "heartbeat");
        assert_eq!(
            update["_meta"]["claudeCode"]["toolResponse"],
            json!({"elapsedTimeSeconds": 1, "subagentType": "general-purpose", "subagentRetry": {"attempt": 2}})
        );
        // The call is its own parent: no parentToolUseId.
        assert!(update["_meta"]["claudeCode"]
            .get("parentToolUseId")
            .is_none());

        let air = wire(&tool_progress_update(&heartbeat, &mut state, &live, true).unwrap());
        assert_eq!(air["_meta"]["claudeCode"]["toolName"], "Agent");

        let lost = json!({
            "type": "tool_progress", "tool_use_id": "unknown", "parent_tool_use_id": null,
            "elapsed_time_seconds": 1
        });
        assert!(tool_progress_update(&lost, &mut state, &live, false).is_none());
        let finished = json!({
            "type": "tool_progress", "tool_use_id": "gone", "parent_tool_use_id": "gone-parent",
            "elapsed_time_seconds": 1
        });
        assert!(tool_progress_update(&finished, &mut state, &live, false).is_none());
    }

    /// N-20 — a beat inside a live subagent names the Agent call as its parent.
    #[test]
    fn t2a_tool_progress_inside_a_subagent_stamps_the_parent() {
        let mut state = MapState::default();
        emitted(&mut state, "toolu_INNER", "Bash");
        let mut live = HashMap::new();
        live.insert("task-1".to_string(), "toolu_AGENT".to_string());
        let beat = json!({
            "type": "tool_progress", "tool_use_id": "toolu_INNER", "tool_name": "Bash",
            "parent_tool_use_id": "toolu_AGENT", "elapsed_time_seconds": 2
        });
        let update = wire(&tool_progress_update(&beat, &mut state, &live, false).unwrap());
        assert_eq!(
            update["_meta"]["claudeCode"]["parentToolUseId"],
            "toolu_AGENT"
        );
        // Without a live subagent for that parent the stamp is left out.
        let mut other = MapState::default();
        emitted(&mut other, "toolu_INNER", "Bash");
        let update =
            wire(&tool_progress_update(&beat, &mut other, &HashMap::new(), false).unwrap());
        assert!(update["_meta"]["claudeCode"]
            .get("parentToolUseId")
            .is_none());
    }

    fn denial() -> Value {
        json!({
            "type": "system", "subtype": "permission_denied", "tool_use_id": "toolu_D",
            "tool_name": "Bash", "decision_reason_type": "rule",
            "decision_reason": "Denied by rule Bash(rm:*)", "message": "The user doesn't want to proceed",
        })
    }

    /// P-01 / T-81 — the surfaced call fails with the reason; a non-AIR client
    /// gets the whole toolResponse, AIR only a message that differs.
    #[test]
    fn t2a_permission_denied_fails_the_call_with_the_reason() {
        let mut state = MapState::default();
        emitted(&mut state, "toolu_D", "Bash");
        let update = wire(
            &permission_denied_update(&denial(), &mut state, &HashMap::new(), false, false)
                .expect("denial"),
        );
        assert_eq!(update["toolCallId"], "toolu_D");
        assert_eq!(update["status"], "failed");
        assert_eq!(
            update["content"],
            json!([{"type": "content", "content": {"type": "text", "text": "Permission denied: Denied by rule Bash(rm:*)"}}])
        );
        assert_eq!(
            update["_meta"]["claudeCode"],
            json!({"toolName": "Bash", "toolResponse": {
                "decisionReasonType": "rule",
                "decisionReason": "Denied by rule Bash(rm:*)",
                "message": "The user doesn't want to proceed",
            }})
        );

        let mut air = MapState::default();
        emitted(&mut air, "toolu_D", "Bash");
        let update = wire(
            &permission_denied_update(&denial(), &mut air, &HashMap::new(), true, false).unwrap(),
        );
        assert_eq!(
            update["_meta"]["claudeCode"]["toolResponse"],
            json!({"decisionReasonType": "rule", "message": "The user doesn't want to proceed"})
        );
        let mut same = MapState::default();
        emitted(&mut same, "toolu_D", "Bash");
        let mut equal = denial();
        equal["message"] = equal["decision_reason"].clone();
        let update = wire(
            &permission_denied_update(&equal, &mut same, &HashMap::new(), true, false).unwrap(),
        );
        assert_eq!(
            update["_meta"]["claudeCode"]["toolResponse"],
            json!({"decisionReasonType": "rule"})
        );
    }

    /// P-01 — the message is the reason when no decision reason came.
    #[test]
    fn t2a_permission_denied_without_a_decision_reason_uses_the_message() {
        let mut state = MapState::default();
        emitted(&mut state, "toolu_D", "Bash");
        let mut frame = denial();
        frame.as_object_mut().unwrap().remove("decision_reason");
        let update = wire(
            &permission_denied_update(&frame, &mut state, &HashMap::new(), false, false).unwrap(),
        );
        assert_eq!(
            update["content"][0]["content"]["text"],
            "Permission denied: The user doesn't want to proceed"
        );
    }

    /// P-01 — a denial for a call the client was never given is dropped (#851),
    /// and drops the pending hook either way.
    #[test]
    fn t2a_permission_denied_for_an_unknown_call_is_dropped() {
        let mut state = MapState::default();
        state.pending_hooks.insert("toolu_D".to_string());
        assert!(
            permission_denied_update(&denial(), &mut state, &HashMap::new(), false, false)
                .is_none()
        );
        assert!(!state.pending_hooks.contains("toolu_D"));
    }

    /// P-01 — an agent-scoped denial resolves its lineage through the live
    /// subagent; with no lineage a subagent-aware client gets nothing.
    #[test]
    fn t2a_permission_denied_agent_scoped_lineage() {
        let mut frame = denial();
        frame["agent_id"] = json!("task-9");
        let mut live = HashMap::new();
        live.insert("task-9".to_string(), "toolu_AGENT".to_string());

        let mut state = MapState::default();
        emitted(&mut state, "toolu_D", "Bash");
        let update =
            wire(&permission_denied_update(&frame, &mut state, &live, false, true).unwrap());
        assert_eq!(
            update["_meta"]["claudeCode"]["parentToolUseId"],
            "toolu_AGENT"
        );

        let mut lost = MapState::default();
        emitted(&mut lost, "toolu_D", "Bash");
        assert!(
            permission_denied_update(&frame, &mut lost, &HashMap::new(), false, true).is_none()
        );
        // A client without subagent support still gets the report in the root.
        let root = wire(
            &permission_denied_update(&frame, &mut lost, &HashMap::new(), false, false).unwrap(),
        );
        assert!(root["_meta"]["claudeCode"].get("parentToolUseId").is_none());
    }
}
