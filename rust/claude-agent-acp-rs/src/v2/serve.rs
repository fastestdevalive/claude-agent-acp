//! Protocol router and ACP v2 connection serving (M-106, O-06, O-139, Q-36).
//!
//! Upstream references:
//! - `src/serve.ts:9-51`
//! - `src/index.ts:86-89`
//! - `@agentclientprotocol/sdk` `protocol-router.js`

use agent_client_protocol::Error;
use serde_json::{json, Value};

use super::initialize::{v1_initialize_request, v2_initialize_response};
use super::session::{
    v1_fork_session_request, v1_new_session_request, v1_restore_session_request,
    v1_set_session_config_option_request, v2_new_session_response, v2_resume_session_response,
};

/// The environment variable that enables experimental ACP v2 support (O-06, O-139).
pub const EXPERIMENTAL_V2_ENV: &str = "CLAUDE_AGENT_ACP_EXPERIMENTAL_V2";

/// The highest protocol version the wire accepts (`protocol-router.js:7`).
pub const MAX_PROTOCOL_VERSION: u64 = 0xffff;

/// Check whether experimental ACP v2 is opted into through the connection's
/// environment snapshot (O-06, O-139; `src/index.ts:86-89`).
///
/// The snapshot is the startup value the adapter serves with, so the opt-in is
/// fixed for the life of a connection and can be injected in tests.
pub fn is_experimental_v2_enabled(env: &crate::env_snapshot::EnvSnapshot) -> bool {
    env.get(EXPERIMENTAL_V2_ENV) == Some("1")
}

/// The protocol version a connection serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolMode {
    V1,
    V2,
}

/// Negotiated protocol version (kept for the router tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolVersion {
    V1,
    V2,
}

impl From<ProtocolMode> for ProtocolVersion {
    fn from(mode: ProtocolMode) -> Self {
        match mode {
            ProtocolMode::V1 => ProtocolVersion::V1,
            ProtocolMode::V2 => ProtocolVersion::V2,
        }
    }
}

/// The outcome of negotiating `initialize`: which protocol the connection
/// serves and the params the v1 agent reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Negotiated {
    pub mode: ProtocolMode,
    pub params: Value,
}

/// The upstream `initialize.protocolVersion` invalid-params error
/// (`protocol-router.js:362-364`): the detail is the error `data`.
fn invalid_protocol_version() -> Error {
    Error::new(-32602, "Invalid params").data(invalid_protocol_version_data())
}

/// The `data` of [`invalid_protocol_version`].
fn invalid_protocol_version_data() -> Value {
    json!("initialize.protocolVersion must be a valid ACP protocol version")
}

/// The upstream `Unsupported protocol version` error
/// (`protocol-router.js:1059-1064`).
fn unsupported_protocol_version(requested: u64) -> Error {
    Error::new(-32022, "Unsupported protocol version")
        .data(unsupported_protocol_version_data(requested))
}

/// The `data` of [`unsupported_protocol_version`].
fn unsupported_protocol_version_data(requested: u64) -> Value {
    json!({ "requested": requested, "supported": [1, 2] })
}

/// Read `params.protocolVersion` as a protocol version: any finite integral
/// number in `0..=0xffff` (`protocol-router.js:386-391`: `typeof value ===
/// "number" && Number.isInteger(value)`). `JSON.parse("2.0")` is the integer
/// `2`, so a serializer that writes the version as a float still routes; a
/// string, a negative number, a fraction or a missing field is not a version.
fn protocol_version(params: &Value) -> Option<u64> {
    let value = params.as_object()?.get("protocolVersion")?;
    let version = if let Some(integer) = value.as_u64() {
        integer
    } else {
        value
            .as_f64()
            .filter(|number| number.is_finite() && number.fract() == 0.0 && *number >= 0.0)
            .map(|number| number as u64)?
    };
    (version <= MAX_PROTOCOL_VERSION).then_some(version)
}

/// Negotiate the protocol version from an `initialize` request (B1/M-106).
///
/// With experimental v2 off, the connection stays v1 and the params are passed
/// through unchanged, exactly as before (O-139). With it on, the version is
/// validated as upstream's router does, and a v2 request becomes the v1 request
/// that the agent reads (`src/v2/initialize.ts:20-55`).
pub fn negotiate_initialize(params: &Value, experimental_v2: bool) -> Result<Negotiated, Error> {
    if !experimental_v2 {
        return Ok(Negotiated {
            mode: ProtocolMode::V1,
            params: params.clone(),
        });
    }
    let Some(requested) = protocol_version(params) else {
        return Err(invalid_protocol_version());
    };
    match requested {
        0 => Err(unsupported_protocol_version(requested)),
        1 => Ok(Negotiated {
            mode: ProtocolMode::V1,
            params: params.clone(),
        }),
        _ => Ok(Negotiated {
            mode: ProtocolMode::V2,
            params: v1_initialize_request(params),
        }),
    }
}

/// Outcome of routing the first frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteDecision {
    /// Proceed with the chosen protocol version.
    Route(ProtocolVersion),
    /// Reject the connection with a JSON-RPC error.
    Reject {
        code: i32,
        message: String,
        data: Option<Value>,
        id: Option<Value>,
        batched: bool,
    },
}

/// Inspect the first frame and environment opt-in to determine the protocol version (M-106, O-139).
///
/// Port of `acpProtocolRouter` from `src/serve.ts:37-51` and `protocol-router.js:59-180`.
pub fn route_first_frame(first_frame: &Value, experimental_v2: bool) -> RouteDecision {
    // Without experimental v2, the adapter serves ACP v1 alone and never routes
    // by protocol version, exactly as before (O-139, `src/serve.ts:10-12`).
    if !experimental_v2 {
        return RouteDecision::Route(ProtocolVersion::V1);
    }

    let reject =
        |code: i32, message: &str, data: Option<Value>, id: Option<Value>, batched: bool| {
            RouteDecision::Reject {
                code,
                message: message.to_string(),
                data,
                id,
                batched,
            }
        };
    let invalid_request = |detail: &str, id: Option<Value>, batched: bool| -> RouteDecision {
        reject(-32600, "Invalid request", Some(json!(detail)), id, batched)
    };

    let mut batched = false;
    let message = if let Some(arr) = first_frame.as_array() {
        if arr.len() == 1
            && arr[0].get("method").and_then(Value::as_str) == Some("initialize")
            && arr[0].get("id").is_some()
        {
            batched = true;
            &arr[0]
        } else {
            return invalid_request(
                "first ACP message must be an initialize request",
                None,
                false,
            );
        }
    } else {
        first_frame
    };

    let Some(obj) = message.as_object() else {
        return invalid_request(
            "first ACP message must be an initialize request",
            None,
            false,
        );
    };

    let id = obj.get("id").cloned();

    // Must be a request message with method "initialize"
    let method = obj.get("method").and_then(Value::as_str);
    if method != Some("initialize") {
        return invalid_request("first ACP request must be initialize", id, batched);
    }

    let params = match obj.get("params") {
        Some(params) if params.is_object() => params,
        _ => {
            return reject(
                -32602,
                "Invalid params",
                Some(invalid_protocol_version_data()),
                id,
                batched,
            );
        }
    };

    let Some(requested) = protocol_version(params) else {
        return reject(
            -32602,
            "Invalid params",
            invalid_protocol_version().data,
            id,
            batched,
        );
    };

    match requested {
        0 => reject(
            -32022,
            "Unsupported protocol version",
            Some(unsupported_protocol_version_data(requested)),
            id,
            batched,
        ),
        1 => RouteDecision::Route(ProtocolVersion::V1),
        _ => RouteDecision::Route(ProtocolVersion::V2),
    }
}

/// Dispatch a v2 client request to the appropriate v1 handler and translate the response.
///
/// Handles `initialize`, `session/new`, `session/resume`, `session/fork`, and
/// `session/set_config_option`. For other methods, returns the upstream-shaped
/// method-not-found error rather than hanging.
pub fn handle_v2_request_sync<F>(
    method: &str,
    params: &Value,
    mut v1_invoke: F,
) -> Result<Value, Error>
where
    F: FnMut(&str, &Value) -> Result<Value, Error>,
{
    match method {
        "initialize" => {
            let v1_req = v1_initialize_request(params);
            let v1_res = v1_invoke("initialize", &v1_req)?;
            v2_initialize_response(&v1_res)
        }
        "session/new" => {
            let v1_req = v1_new_session_request(params)?;
            let v1_res = v1_invoke("session/new", &v1_req)?;
            Ok(v2_new_session_response(&v1_res))
        }
        "session/resume" => {
            let restore = v1_restore_session_request(params)?;
            let v1_res = v1_invoke(restore.method, &restore.request)?;
            Ok(v2_resume_session_response(&v1_res))
        }
        "session/fork" => {
            let v1_req = v1_fork_session_request(params)?;
            let v1_res = v1_invoke("session/fork", &v1_req)?;
            Ok(v2_new_session_response(&v1_res))
        }
        "session/set_config_option" | "session/setConfigOption" => {
            let v1_req = v1_set_session_config_option_request(params)?;
            v1_invoke("session/set_config_option", &v1_req)
        }
        _ => Err(
            Error::new(-32601, format!("\"Method not found\": {method}"))
                .data(json!({ "method": method })),
        ),
    }
}
