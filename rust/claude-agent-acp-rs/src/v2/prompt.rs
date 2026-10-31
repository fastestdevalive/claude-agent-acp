//! ACP v2 `session/prompt`, served through the turn events of the session
//! (M-113, M-114, U-81, U-82, U-86, Q-86; upstream `src/v2/prompt.ts`).
//!
//! v2 answers the prompt once Claude Code takes it in, with the id of the user
//! message it became, and reports the rest of the turn as `state_update`s. The
//! session emits the prompt's `user_message` and every state update itself, on
//! its ordered outbound channel, so the wire order matches upstream's
//! synchronous `send`.

use agent_client_protocol::Error;
use serde_json::{json, Value};

use crate::turn_events::{
    ForegroundEvent, ForegroundState, JsonRpcError, TurnOutcome, V2IntakeError,
};

/// The `-32800` error of a prompt that a cancel ended in the queue
/// (`RequestError.requestCancelled()`).
pub fn request_cancelled() -> Error {
    Error::new(-32800, "Request cancelled")
}

/// The error of a turn that ended before Claude Code took its prompt in
/// (`RequestError.internalError({ stopReason }, ...)`, `v2/prompt.ts:43-52`).
pub fn turn_ended_before_intake(outcome: &TurnOutcome) -> Error {
    // `RequestError.internalError(d, m)` writes `Internal error: m`
    // (`sdk/dist/jsonrpc.js:1014-1022`).
    Error::new(
        -32603,
        "Internal error: The turn ended before Claude Code took the prompt in",
    )
    .data(json!({ "stopReason": outcome.stop_reason.as_str() }))
}

/// The JSON-RPC error of a failed turn (`jsonRpcError`, `v2/prompt.ts:114-126`).
pub fn json_rpc_error(error: &JsonRpcError) -> Error {
    let mut err = Error::new(error.code, error.message.clone());
    if let Some(data) = &error.data {
        err = err.data(data.clone());
    }
    err
}

/// Map a [`V2IntakeError`] to the JSON-RPC error that answers the prompt.
pub fn intake_error(error: &V2IntakeError) -> Error {
    match error {
        V2IntakeError::Cancelled => request_cancelled(),
        V2IntakeError::Ended(stop_reason) => turn_ended_before_intake(&TurnOutcome {
            stop_reason: *stop_reason,
            usage: None,
            meta: None,
        }),
        V2IntakeError::Failed(error) => json_rpc_error(error),
    }
}

/// The v1 prompt request of a v2 one: the same request with its content blocks
/// mapped to v1 (`v1PromptRequest`, `v2/prompt.ts:148-150`).
pub fn v1_prompt_request(params: &Value) -> Result<Value, Error> {
    let mut request = params
        .as_object()
        .cloned()
        .ok_or_else(|| Error::new(-32602, "Invalid params"))?;
    let prompt = request
        .get("prompt")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mapped = prompt
        .iter()
        .map(v1_content_block)
        .collect::<Result<Vec<_>, _>>()?;
    request.insert("prompt".to_string(), Value::Array(mapped));
    Ok(Value::Object(request))
}

/// A v2 content block as the v1 block the agent serves, or the invalid-params
/// error of a kind v1 cannot express (`v1ContentBlock`, `v2/prompt.ts:152-167`).
pub fn v1_content_block(block: &Value) -> Result<Value, Error> {
    let kind = block.get("type").and_then(Value::as_str);
    if !matches!(
        kind,
        Some("text" | "image" | "audio" | "resource_link" | "resource")
    ) {
        let spelled = kind.unwrap_or("undefined");
        let data = match kind {
            Some(kind) => json!({ "type": kind }),
            None => json!({}),
        };
        // `RequestError.invalidParams(d, m)` writes `Invalid params: m`.
        return Err(Error::new(
            -32602,
            format!("Invalid params: Content blocks of type {spelled} are not supported"),
        )
        .data(data));
    }
    let Some(object) = block.as_object() else {
        return Ok(block.clone());
    };
    let mut out = object.clone();
    if let Some(annotations) = out.shift_remove("annotations") {
        if !annotations.is_null() {
            out.insert("annotations".to_string(), v1_annotations(&annotations));
        }
    }
    Ok(Value::Object(out))
}

/// v1 knows two audience roles; an audience of only unknown roles is dropped
/// rather than narrowed to nobody (`v1Annotations`, `v2/prompt.ts:173-183`).
pub fn v1_annotations(annotations: &Value) -> Value {
    let Some(object) = annotations.as_object() else {
        return annotations.clone();
    };
    let Some(audience) = object.get("audience") else {
        return annotations.clone();
    };
    if audience.is_null() {
        let mut out = object.clone();
        out.shift_remove("audience");
        return Value::Object(out);
    }
    let roles: Vec<&Value> = audience
        .as_array()
        .map(|roles| {
            roles
                .iter()
                .filter(|role| matches!(role.as_str(), Some("assistant" | "user")))
                .collect()
        })
        .unwrap_or_default();
    let original_len = audience.as_array().map_or(0, Vec::len);
    if roles.is_empty() && original_len > 0 {
        let mut out = object.clone();
        out.shift_remove("audience");
        return Value::Object(out);
    }
    let mut out = object.clone();
    out.insert(
        "audience".to_string(),
        Value::Array(roles.into_iter().cloned().collect()),
    );
    Value::Object(out)
}

/// The `state_update` of a foreground event (`v2Foreground`, `v2/prompt.ts:91-111`).
pub fn state_update(event: &ForegroundEvent) -> Value {
    match event {
        ForegroundEvent::Running => json!({
            "sessionUpdate": "state_update",
            "state": "running",
        }),
        ForegroundEvent::AwaitingUser => json!({
            "sessionUpdate": "state_update",
            "state": "requires_action",
        }),
        ForegroundEvent::Ended(outcome) => idle_update(outcome),
        ForegroundEvent::Failed(error) => failed_update(error),
    }
}

/// The state update for a bare foreground state (`foregroundState`).
pub fn foreground_state_update(state: ForegroundState) -> Value {
    match state {
        ForegroundState::Idle => json!({
            "sessionUpdate": "state_update",
            "state": "idle",
        }),
        ForegroundState::Running => json!({
            "sessionUpdate": "state_update",
            "state": "running",
        }),
        ForegroundState::RequiresAction => json!({
            "sessionUpdate": "state_update",
            "state": "requires_action",
        }),
    }
}

/// The `idle` of work that ended, with its stop reason, usage and `_meta`
/// (`idle`, `v2/prompt.ts:134-141`).
pub fn idle_update(outcome: &TurnOutcome) -> Value {
    let mut update = serde_json::Map::new();
    update.insert("sessionUpdate".into(), json!("state_update"));
    update.insert("state".into(), json!("idle"));
    update.insert(
        "stopReason".into(),
        Value::String(outcome.stop_reason.as_str().to_string()),
    );
    if let Some(usage) = &outcome.usage {
        update.insert("usage".into(), usage.to_json());
    }
    if let Some(meta) = &outcome.meta {
        update.insert("_meta".into(), meta.clone());
    }
    Value::Object(update)
}

/// The `idle` of work that failed, with the JSON-RPC error v1 answers the
/// prompt with (`v2Foreground.failed`, `v2/prompt.ts:102-109`).
pub fn failed_update(error: &JsonRpcError) -> Value {
    json!({
        "sessionUpdate": "state_update",
        "state": "idle",
        "stopReason": "error",
        "error": error.to_json(),
    })
}

/// The RFD's subagent work-state validation and the child's own `state_update`
/// are lane C's `v2/session_update.rs`; the prompt path uses the same ones.
pub use super::session_update::{v2_child_state_update, v2_subagent_state};
