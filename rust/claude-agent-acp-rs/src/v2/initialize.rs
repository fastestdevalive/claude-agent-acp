//! The ACP v2 `initialize` handshake, expressed through the v1 handshake of
//! `ClaudeAcpAgent` (C-44, C-45, C-46, C-47, Q-84; upstream `src/v2/initialize.ts`).
//!
//! The agent decides its capabilities and auth methods in v1 terms. The v2
//! request becomes the v1 request that the agent reads, and the v1 response of
//! the agent becomes the v2 response, so both protocol versions advertise from
//! one source.

use agent_client_protocol::Error;
use serde_json::{json, Map, Value};

/// The v1 protocol version constant.
pub const V1_PROTOCOL_VERSION: u64 = 1;

/// The v2 protocol version constant.
pub const V2_PROTOCOL_VERSION: u64 = 2;

/// Error message returned when client filesystem is accessed on v2 (C-47).
pub const V2_FS_ERROR_MESSAGE: &str = "ACP v2 has no client file system";

/// The top-level `_meta` keys of the agent's v1 `initialize` response that
/// advertise an extension the v2 surface does not serve yet: `steering`, for
/// `_session/steering`.
const UNSERVED_EXTENSION_KEYS: &[&str] = &["steering"];

/// The v1 `initialize` request that a v2 client means (C-44).
///
/// Port of `v1InitializeRequest(request: v2.InitializeRequest): InitializeRequest`
/// in `src/v2/initialize.ts:20-55`.
pub fn v1_initialize_request(request: &Value) -> Value {
    let capabilities = request.get("capabilities");

    // Auth capabilities mapping:
    // v1 marks terminal auth support with a boolean, v2 with an object.
    let mut auth_map = Map::new();
    let terminal_supported = capabilities
        .and_then(|c| c.get("auth"))
        .and_then(|a| a.get("terminal"))
        .is_some_and(|t| !t.is_null());
    auth_map.insert("terminal".to_string(), Value::Bool(terminal_supported));
    if let Some(auth_meta) = capabilities
        .and_then(|c| c.get("auth"))
        .and_then(|a| a.get("_meta"))
    {
        if !auth_meta.is_null() {
            auth_map.insert("_meta".to_string(), auth_meta.clone());
        }
    }

    // Client capabilities
    let mut client_caps = Map::new();
    // v2 has no client file system and no client terminals: the agent
    // reads, writes, and runs commands itself (C-47).
    client_caps.insert(
        "fs".to_string(),
        json!({
            "readTextFile": false,
            "writeTextFile": false
        }),
    );
    client_caps.insert("terminal".to_string(), Value::Bool(false));

    // Every v2 client handles boolean config options, notices, and
    // compaction updates; v1 asks for a marker.
    client_caps.insert(
        "session".to_string(),
        json!({
            "configOptions": { "boolean": {} },
            "notices": {},
            "compaction": {}
        }),
    );

    // Every v2 client takes `plan_update` and `plan_removed`.
    client_caps.insert("plan".to_string(), json!({}));

    // Subagent updates are part of v2's baseline session model.
    client_caps.insert("subagents".to_string(), json!({}));

    client_caps.insert("auth".to_string(), Value::Object(auth_map));

    if let Some(elicitation) = capabilities.and_then(|c| c.get("elicitation")) {
        if !elicitation.is_null() {
            client_caps.insert("elicitation".to_string(), elicitation.clone());
        }
    }

    if let Some(caps_meta) = capabilities.and_then(|c| c.get("_meta")) {
        if !caps_meta.is_null() {
            client_caps.insert("_meta".to_string(), caps_meta.clone());
        }
    }

    let mut out = Map::new();
    out.insert("protocolVersion".to_string(), json!(V1_PROTOCOL_VERSION));

    if let Some(info) = request.get("info") {
        if !info.is_null() {
            out.insert("clientInfo".to_string(), info.clone());
        }
    }

    out.insert("clientCapabilities".to_string(), Value::Object(client_caps));

    if let Some(meta) = request.get("_meta") {
        if !meta.is_null() {
            out.insert("_meta".to_string(), meta.clone());
        }
    }

    Value::Object(out)
}

/// The session capabilities of the v2 surface (C-46; `v2/initialize.ts:108-115`).
pub fn v2_session_capabilities() -> Value {
    json!({
        "prompt": { "image": {}, "embeddedContext": {} },
        "mcp": { "stdio": {}, "http": {} },
        "delete": {},
        "fork": {},
        "additionalDirectories": {},
        "systemPrompt": { "mode": { "override": {} } }
    })
}

/// Convert a single v1 AuthMethod into v2 AuthMethod shape (C-45; `v2/initialize.ts:118-129`).
///
/// v2 names the id of a method `methodId`, requires `type`, and lists `env` as name and value pairs.
pub fn v2_auth_method(method: &Value) -> Value {
    let mut out = match method.as_object() {
        Some(map) => map.clone(),
        None => return method.clone(),
    };

    let id = out.shift_remove("id").unwrap_or(Value::Null);
    if !id.is_null() {
        out.insert("methodId".to_string(), id);
    }

    let is_terminal = out.get("type").and_then(Value::as_str) == Some("terminal");
    if is_terminal {
        if let Some(env_val) = out.shift_remove("env") {
            if let Some(env_map) = env_val.as_object() {
                let env_array: Vec<Value> = env_map
                    .iter()
                    .map(|(k, v)| json!({ "name": k, "value": v }))
                    .collect();
                out.insert("env".to_string(), Value::Array(env_array));
            }
        }
    } else {
        out.insert("type".to_string(), Value::String("agent".to_string()));
    }

    Value::Object(out)
}

/// The v2 `initialize` response for the v1 response of the agent (C-45, C-46).
///
/// Port of `v2InitializeResponse(response: InitializeResponse): v2.InitializeResponse`
/// in `src/v2/initialize.ts:70-93`.
pub fn v2_initialize_response(response: &Value) -> Result<Value, Error> {
    let agent_info = response.get("agentInfo").ok_or_else(|| {
        Error::new(
            -32603,
            "ACP v2 requires agentInfo, and the agent reported none",
        )
    })?;
    if agent_info.is_null() {
        return Err(Error::new(
            -32603,
            "ACP v2 requires agentInfo, and the agent reported none",
        ));
    }

    let mut caps_map = Map::new();
    caps_map.insert("session".to_string(), v2_session_capabilities());
    caps_map.insert("providers".to_string(), json!({}));

    if let Some(agent_caps_meta) = response
        .get("agentCapabilities")
        .and_then(|c| c.get("_meta"))
    {
        if !agent_caps_meta.is_null() {
            caps_map.insert("_meta".to_string(), agent_caps_meta.clone());
        }
    }

    let mut out = Map::new();
    out.insert("protocolVersion".to_string(), json!(V2_PROTOCOL_VERSION));
    out.insert("info".to_string(), agent_info.clone());
    out.insert("capabilities".to_string(), Value::Object(caps_map));

    let auth_methods = response
        .get("authMethods")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(v2_auth_method).collect::<Vec<_>>())
        .unwrap_or_default();
    out.insert("authMethods".to_string(), Value::Array(auth_methods));

    // Strip UNSERVED_EXTENSION_KEYS (such as "steering") from top-level `_meta`
    if let Some(top_meta) = response.get("_meta").and_then(Value::as_object) {
        let filtered: Map<String, Value> = top_meta
            .iter()
            .filter(|(k, _)| !UNSERVED_EXTENSION_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !filtered.is_empty() {
            out.insert("_meta".to_string(), Value::Object(filtered));
        }
    }

    Ok(Value::Object(out))
}

/// Reject helper for v2 client filesystem calls (C-47).
pub fn reject_v2_client_fs() -> Error {
    Error::new(-32603, V2_FS_ERROR_MESSAGE)
}
