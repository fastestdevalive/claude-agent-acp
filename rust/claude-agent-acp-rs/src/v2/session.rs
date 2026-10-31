//! The ACP v2 session lifecycle, expressed through the v1 session methods of
//! `ClaudeAcpAgent` (O-86, M-118, Q-88; upstream `src/v2/session.ts`).
//!
//! v2 requests become the v1 requests that the agent serves, and its v1
//! responses become v2 responses.

use agent_client_protocol::Error;
use serde_json::{json, Map, Value};

/// Check whether an RPC method name is a valid ACP extension method (M-118).
///
/// Extension methods on v2 must start with "_" (`src/v2/agent.ts:404-406`).
pub fn is_extension_method(method: &str) -> bool {
    method.starts_with('_')
}

/// Validate that a method name is a valid extension method, returning an Error
/// if it is not (M-118; `src/v2/agent.ts:245-248`).
pub fn validate_extension_method(method: &str) -> Result<(), Error> {
    if !is_extension_method(method) {
        return Err(Error::new(
            -32603,
            format!("{method} is not an ACP extension method"),
        ));
    }
    Ok(())
}

/// Translate select options for v2: group id becomes groupId (O-86).
fn v2_select_options(options: &[Value]) -> Vec<Value> {
    let is_grouped = options.iter().any(|entry| {
        entry
            .as_object()
            .is_some_and(|o| o.contains_key("group") || o.contains_key("groupId"))
    });

    if !is_grouped {
        return options.to_vec();
    }

    options
        .iter()
        .map(|entry| {
            if let Some(obj) = entry.as_object() {
                let mut map = obj.clone();
                if let Some(group) = map.shift_remove("group") {
                    map.insert("groupId".to_string(), group);
                }
                Value::Object(map)
            } else {
                entry.clone()
            }
        })
        .collect()
}

/// Convert v1 SessionConfigOption array to v2 shape (O-86; `v2/session.ts:126-135`).
///
/// v2 names the id of an option `configId` and the id of a select group `groupId`.
pub fn v2_config_options(options: &[Value]) -> Vec<Value> {
    options
        .iter()
        .map(|opt| {
            let Some(map) = opt.as_object() else {
                return opt.clone();
            };
            let mut out = map.clone();
            if let Some(id) = out.shift_remove("id") {
                out.insert("configId".to_string(), id);
            }
            if out.get("type").and_then(Value::as_str) == Some("select") {
                if let Some(opts) = out.get("options").and_then(Value::as_array) {
                    out.insert("options".to_string(), Value::Array(v2_select_options(opts)));
                }
            }
            Value::Object(out)
        })
        .collect()
}

/// Convert v1 AvailableCommand array to v2 shape (O-86; `v2/session.ts:147-152`).
///
/// v2 tags the free-text input of a command with `type: "text"`.
pub fn v2_available_commands(commands: &[Value]) -> Vec<Value> {
    commands
        .iter()
        .map(|cmd| {
            let Some(map) = cmd.as_object() else {
                return cmd.clone();
            };
            let mut out = map.clone();
            if let Some(input) = out.shift_remove("input") {
                if let Some(input_map) = input.as_object() {
                    // Upstream `{ type: "text", ...input }`: `type` comes first,
                    // and an input `type` overwrites it in place.
                    let mut tagged = Map::new();
                    tagged.insert("type".to_string(), Value::String("text".to_string()));
                    for (key, value) in input_map {
                        tagged.insert(key.clone(), value.clone());
                    }
                    out.insert("input".to_string(), Value::Object(tagged));
                } else if !input.is_null() {
                    out.insert("input".to_string(), input);
                }
            }
            Value::Object(out)
        })
        .collect()
}

/// Convert v2 McpServer to v1 McpServer (`v2/session.ts:167-182`).
fn v1_mcp_server(server: &Value) -> Result<Value, Error> {
    let Some(map) = server.as_object() else {
        return Ok(server.clone());
    };
    let server_type = map.get("type").and_then(Value::as_str).unwrap_or("");
    if server_type.is_empty() || server_type == "stdio" {
        let mut out = map.clone();
        out.remove("type");
        if !out.contains_key("args") {
            out.insert("args".to_string(), Value::Array(vec![]));
        }
        if !out.contains_key("env") {
            out.insert("env".to_string(), Value::Array(vec![]));
        }
        Ok(Value::Object(out))
    } else if server_type == "http" {
        let mut out = map.clone();
        if !out.contains_key("headers") {
            out.insert("headers".to_string(), Value::Array(vec![]));
        }
        Ok(Value::Object(out))
    } else if server_type == "acp" {
        Ok(server.clone())
    } else {
        Err(Error::new(
            -32602,
            format!("Invalid params: MCP servers of type {server_type} are not supported"),
        )
        .data(json!({ "type": server_type })))
    }
}

/// Convert content block audience for v1: v1 takes only "user" or "assistant".
fn v1_content_block(block: &Value) -> Value {
    let Some(map) = block.as_object() else {
        return block.clone();
    };
    let mut out = map.clone();
    if let Some(annotations) = out.get("annotations").and_then(Value::as_object) {
        let mut ann = annotations.clone();
        if let Some(audience) = ann.get("audience").and_then(Value::as_array) {
            let filtered: Vec<Value> = audience
                .iter()
                .filter(|a| matches!(a.as_str(), Some("user" | "assistant")))
                .cloned()
                .collect();
            if filtered.is_empty() {
                ann.remove("audience");
            } else {
                ann.insert("audience".to_string(), Value::Array(filtered));
            }
        }
        out.insert("annotations".to_string(), Value::Object(ann));
    }
    Value::Object(out)
}

/// Convert v2 systemPrompt field to v1 systemPrompt field (`v2/session.ts:86-91`).
fn v1_system_prompt_field(system_prompt: Option<&Value>) -> Option<Value> {
    let sp = system_prompt?;
    if sp.is_null() {
        return None;
    }
    let Some(map) = sp.as_object() else {
        return Some(sp.clone());
    };
    let mut out = map.clone();
    if let Some(content) = out.get("content").and_then(Value::as_array) {
        let converted: Vec<Value> = content.iter().map(v1_content_block).collect();
        out.insert("content".to_string(), Value::Array(converted));
    }
    Some(Value::Object(out))
}

/// Map v2 NewSessionRequest to v1 NewSessionRequest (`v2/session.ts:25-33`).
pub fn v1_new_session_request(params: &Value) -> Result<Value, Error> {
    let mut out = match params.as_object() {
        Some(map) => map.clone(),
        None => return Ok(params.clone()),
    };

    let mcp_servers = out.remove("mcpServers");
    let mut v1_servers = Vec::new();
    if let Some(servers) = mcp_servers.as_ref().and_then(Value::as_array) {
        for s in servers {
            v1_servers.push(v1_mcp_server(s)?);
        }
    }
    out.insert("mcpServers".to_string(), Value::Array(v1_servers));

    let sp = out.remove("systemPrompt");
    if let Some(v1_sp) = v1_system_prompt_field(sp.as_ref()) {
        out.insert("systemPrompt".to_string(), v1_sp);
    }

    Ok(Value::Object(out))
}

/// The result of `v1_restore_session_request`: method ("resume" or "load") + params.
pub struct V1RestoreResult {
    pub method: &'static str,
    pub request: Value,
}

/// Map v2 ResumeSessionRequest to v1 request (`v2/session.ts:44-67`).
pub fn v1_restore_session_request(params: &Value) -> Result<V1RestoreResult, Error> {
    let mut out = match params.as_object() {
        Some(map) => map.clone(),
        None => {
            return Ok(V1RestoreResult {
                method: "session/resume",
                request: params.clone(),
            })
        }
    };

    let replay_from = out.remove("replayFrom");
    let mcp_servers = out.remove("mcpServers");
    let sp = out.remove("systemPrompt");

    if let Some(v1_sp) = v1_system_prompt_field(sp.as_ref()) {
        out.insert("systemPrompt".to_string(), v1_sp);
    }

    if replay_from.is_none() || replay_from.as_ref().is_some_and(Value::is_null) {
        if let Some(servers) = mcp_servers.as_ref().and_then(Value::as_array) {
            let mut v1_servers = Vec::new();
            for s in servers {
                v1_servers.push(v1_mcp_server(s)?);
            }
            out.insert("mcpServers".to_string(), Value::Array(v1_servers));
        }
        return Ok(V1RestoreResult {
            method: "session/resume",
            request: Value::Object(out),
        });
    }

    let cursor_type = replay_from
        .as_ref()
        .and_then(|r| r.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if cursor_type != "start" {
        return Err(Error::new(
            -32602,
            format!(
                "Invalid params: Replaying from a {} cursor is not supported",
                if cursor_type.is_empty() {
                    "unknown"
                } else {
                    cursor_type
                }
            ),
        )
        .data(json!({ "replayFrom": replay_from })));
    }

    let mut v1_servers = Vec::new();
    if let Some(servers) = mcp_servers.as_ref().and_then(Value::as_array) {
        for s in servers {
            v1_servers.push(v1_mcp_server(s)?);
        }
    }
    out.insert("mcpServers".to_string(), Value::Array(v1_servers));

    Ok(V1RestoreResult {
        method: "session/load",
        request: Value::Object(out),
    })
}

/// Map v2 ForkSessionRequest to v1 ForkSessionRequest (`v2/session.ts:73-80`).
pub fn v1_fork_session_request(params: &Value) -> Result<Value, Error> {
    let mut out = match params.as_object() {
        Some(map) => map.clone(),
        None => return Ok(params.clone()),
    };

    let mcp_servers = out.remove("mcpServers");
    if let Some(servers) = mcp_servers.as_ref().and_then(Value::as_array) {
        let mut v1_servers = Vec::new();
        for s in servers {
            v1_servers.push(v1_mcp_server(s)?);
        }
        out.insert("mcpServers".to_string(), Value::Array(v1_servers));
    }

    let sp = out.remove("systemPrompt");
    if let Some(v1_sp) = v1_system_prompt_field(sp.as_ref()) {
        out.insert("systemPrompt".to_string(), v1_sp);
    }

    Ok(Value::Object(out))
}

/// Map v2 SetSessionConfigOptionRequest to v1 SetSessionConfigOptionRequest (`v2/session.ts:106-124`).
pub fn v1_set_session_config_option_request(params: &Value) -> Result<Value, Error> {
    let Some(map) = params.as_object() else {
        return Ok(params.clone());
    };

    let session_id = map
        .get("sessionId")
        .cloned()
        .unwrap_or(Value::String("".to_string()));
    let config_id = map
        .get("configId")
        .cloned()
        .unwrap_or(Value::String("".to_string()));

    let mut out = Map::new();
    out.insert("sessionId".to_string(), session_id);
    out.insert("configId".to_string(), config_id);

    if let Some(meta) = map.get("_meta") {
        if !meta.is_null() {
            out.insert("_meta".to_string(), meta.clone());
        }
    }

    let value = map.get("value").cloned().unwrap_or(Value::Null);
    let raw_type = map.get("type");

    // `v2.SetSessionConfigOptionRequest.isId` / `isBoolean`
    // (`guards.gen.js:929-933`): the tag must be exact AND the payload valid
    // (`value` string for id, boolean for boolean, plus the request ids).
    let valid_ids = map.get("sessionId").is_some_and(Value::is_string)
        && map.get("configId").is_some_and(Value::is_string);
    let is_id = raw_type.and_then(Value::as_str) == Some("id") && value.is_string() && valid_ids;
    let is_boolean =
        raw_type.and_then(Value::as_str) == Some("boolean") && value.is_boolean() && valid_ids;

    // v2 tags a select value with `type: "id"`; v1 leaves it untagged.
    // v2 boolean has `type: "boolean"`.
    if is_id {
        out.insert("value".to_string(), value);
        return Ok(Value::Object(out));
    }
    if is_boolean {
        out.insert("type".to_string(), Value::String("boolean".to_string()));
        out.insert("value".to_string(), value);
        return Ok(Value::Object(out));
    }

    // Anything else throws `invalidParams({type}, "Config option values of
    // type ${params.type} are not supported")`. `JSON.stringify` drops an
    // undefined `type` key, so the missing case has `data:{}`.
    let type_text = match raw_type {
        Some(value) => js_type_text(value),
        None => "undefined".to_string(),
    };
    let data = match raw_type {
        Some(value) => json!({ "type": value }),
        None => json!({}),
    };
    Err(Error::new(
        -32602,
        format!("Invalid params: Config option values of type {type_text} are not supported"),
    )
    .data(data))
}

/// JavaScript `String(value)` for the `type` in the unsupported-value message.
fn js_type_text(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(boolean) => boolean.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_type_text(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// Final tool call statuses on v2 include "cancelled" (C-24; `src/tool-calls/renderer.ts:40-42`).
pub fn is_final_tool_call_status(status: &str, is_v2: bool) -> bool {
    status == "completed" || status == "failed" || (is_v2 && status == "cancelled")
}

/// Determine tool call status for an interrupted/cancelled tool call (C-24; `src/tool-calls/renderer.ts:201, 661`).
///
/// On v2, aborted tool calls get status "cancelled"; on v1, "failed".
pub fn tool_call_status_for_aborted(is_v2: bool) -> &'static str {
    if is_v2 {
        "cancelled"
    } else {
        "failed"
    }
}

/// Convert v1 ResumeSessionResponse or NewSessionResponse to v2 shape (`v2/session.ts:98-103`).
pub fn v2_resume_session_response(response: &Value) -> Value {
    let mut out = Map::new();

    if let Some(config_options) = response.get("configOptions").and_then(Value::as_array) {
        out.insert(
            "configOptions".to_string(),
            Value::Array(v2_config_options(config_options)),
        );
    }

    if let Some(meta) = response.get("_meta") {
        if !meta.is_null() {
            out.insert("_meta".to_string(), meta.clone());
        }
    }

    Value::Object(out)
}

/// Convert v1 NewSessionResponse to v2 shape (`v2/session.ts:94-96`).
pub fn v2_new_session_response(response: &Value) -> Value {
    let mut res = match v2_resume_session_response(response) {
        Value::Object(map) => map,
        _ => Map::new(),
    };

    if let Some(session_id) = response.get("sessionId") {
        res.insert("sessionId".to_string(), session_id.clone());
    }

    Value::Object(res)
}
