//! The ACP v2 permission translation (P-51, P-61, P-62, Q-85; upstream `src/v2/permission.ts`).
//!
//! Translates between v1 and v2 permission requests and responses:
//! - v2 permission request requires `title`, takes optional `description`,
//!   and has `subject: { type: "tool_call", toolCall: <v2_tool_call_patch> }`.
//! - v2 permission response outcomes other than "selected" become "cancelled" in v1.
//! - P-51 helper computes title and description for v2 permission requests.

use agent_client_protocol::Error;
use serde_json::{json, Map, Value};

use super::tool_call::v2_tool_call_patch;

/// Compute the permission prompt title and description for a v2 permission request (P-51).
///
/// Port of `buildClaudePermissionPresentation` lines 86-94 in `src/permissions/presentation.ts`.
pub fn v2_permission_presentation(
    tool_name: &str,
    input: &Value,
    display_name: Option<&str>,
    tool_call_title: &str,
    decision_reason: Option<&str>,
) -> (String, Option<String>) {
    crate::tools::permission_title_and_description(
        tool_name,
        input,
        display_name,
        tool_call_title,
        decision_reason,
    )
}

/// The v2 form of a permission request of the agent (P-61).
///
/// Upstream: `v2PermissionRequest(request: AcpPermissionRequest): v2.RequestPermissionRequest`.
/// Fails if `title` is missing.
pub fn v2_permission_request(request: &Value) -> Result<Value, Error> {
    let title = match request.get("title") {
        Some(Value::String(s)) => s,
        _ => {
            return Err(Error::new(
                -32603,
                "An ACP v2 permission request needs a title",
            ))
        }
    };

    let mut map = Map::new();
    if let Some(session_id) = request.get("sessionId") {
        map.insert("sessionId".to_string(), session_id.clone());
    }
    map.insert("title".to_string(), Value::String(title.clone()));
    if let Some(desc) = request.get("description").and_then(Value::as_str) {
        if !desc.is_empty() {
            map.insert("description".to_string(), Value::String(desc.to_string()));
        }
    }

    let tool_call = request.get("toolCall").unwrap_or(&Value::Null);
    let patched_tool_call = v2_tool_call_patch(tool_call)?;
    let mut subject = Map::new();
    subject.insert("type".to_string(), Value::String("tool_call".to_string()));
    subject.insert("toolCall".to_string(), patched_tool_call);
    map.insert("subject".to_string(), Value::Object(subject));

    if let Some(options) = request.get("options") {
        map.insert("options".to_string(), options.clone());
    }
    if let Some(meta) = request.get("_meta") {
        if !meta.is_null() {
            map.insert("_meta".to_string(), meta.clone());
        }
    }

    Ok(Value::Object(map))
}

/// The v1 form of the answer to a permission request (P-62).
///
/// Upstream: `v1PermissionResponse(response: v2.RequestPermissionResponse): RequestPermissionResponse`.
/// Any outcome other than "selected" becomes "cancelled".
pub fn v1_permission_response(response: &Value) -> Value {
    let outcome = response.get("outcome");
    let outcome_kind = outcome
        .and_then(|o| o.get("outcome"))
        .and_then(Value::as_str);

    let v1_outcome = if outcome_kind == Some("selected") {
        outcome
            .cloned()
            .unwrap_or_else(|| json!({"outcome": "selected"}))
    } else {
        json!({"outcome": "cancelled"})
    };

    let mut map = Map::new();
    map.insert("outcome".to_string(), v1_outcome);
    if let Some(meta) = response.get("_meta") {
        if !meta.is_null() {
            map.insert("_meta".to_string(), meta.clone());
        }
    }

    Value::Object(map)
}
