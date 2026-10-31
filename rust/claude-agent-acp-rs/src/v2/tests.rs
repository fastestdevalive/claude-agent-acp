//! Unit tests for ACP v2 surface (ported from `src/tests/acp-v2.test.ts`).

use serde_json::{json, Value};

use super::initialize::{
    reject_v2_client_fs, v1_initialize_request, v2_auth_method, v2_initialize_response,
    v2_session_capabilities, V1_PROTOCOL_VERSION, V2_FS_ERROR_MESSAGE, V2_PROTOCOL_VERSION,
};
use super::serve::{
    negotiate_initialize, route_first_frame, ProtocolMode, ProtocolVersion, RouteDecision,
};
use super::session::{
    is_extension_method, v1_new_session_request, v1_restore_session_request,
    v1_set_session_config_option_request, v2_available_commands, v2_config_options,
    validate_extension_method,
};

#[test]
fn test_c44_v1_initialize_request_mapping() {
    let v2_req = json!({
        "protocolVersion": 2,
        "info": {
            "name": "v2-test-client",
            "version": "1.0.0"
        },
        "capabilities": {
            "auth": {
                "terminal": {},
                "_meta": { "gateway": true }
            },
            "elicitation": { "form": true },
            "_meta": { "clientKey": "clientVal" }
        },
        "_meta": { "reqKey": "reqVal" }
    });

    let v1_req = v1_initialize_request(&v2_req);

    assert_eq!(v1_req["protocolVersion"], V1_PROTOCOL_VERSION);
    assert_eq!(v1_req["clientInfo"]["name"], "v2-test-client");
    assert_eq!(v1_req["_meta"]["reqKey"], "reqVal");

    let caps = &v1_req["clientCapabilities"];
    assert_eq!(caps["fs"]["readTextFile"], false);
    assert_eq!(caps["fs"]["writeTextFile"], false);
    assert_eq!(caps["terminal"], false);
    assert_eq!(caps["session"]["configOptions"]["boolean"], json!({}));
    assert_eq!(caps["session"]["notices"], json!({}));
    assert_eq!(caps["session"]["compaction"], json!({}));
    assert_eq!(caps["plan"], json!({}));
    assert_eq!(caps["subagents"], json!({}));
    assert_eq!(caps["auth"]["terminal"], true);
    assert_eq!(caps["auth"]["_meta"]["gateway"], true);
    assert_eq!(caps["elicitation"]["form"], true);
    assert_eq!(caps["_meta"]["clientKey"], "clientVal");
}

#[test]
fn test_c47_v2_client_fs_rejected() {
    let err = reject_v2_client_fs();
    assert_eq!(err.message, V2_FS_ERROR_MESSAGE);
    assert_eq!(err.code, agent_client_protocol::ErrorCode::InternalError);
}

#[test]
fn test_c45_c46_v2_initialize_response() {
    let v1_resp = json!({
        "protocolVersion": 1,
        "agentInfo": {
            "name": "@agentclientprotocol/claude-agent-acp",
            "title": "Claude Agent",
            "version": "0.94.0"
        },
        "agentCapabilities": {
            "_meta": { "agentMeta": 123 }
        },
        "authMethods": [
            {
                "id": "claude-login",
                "name": "Log in with Claude",
                "type": "terminal",
                "args": ["--cli"],
                "env": { "FOO": "bar" }
            },
            {
                "id": "agent-auth",
                "name": "Custom agent auth"
            }
        ],
        "_meta": {
            "steering": { "supported": true },
            "other": "kept"
        }
    });

    let v2_resp =
        v2_initialize_response(&v1_resp).expect("v2 initialize response mapping should succeed");

    assert_eq!(v2_resp["protocolVersion"], V2_PROTOCOL_VERSION);
    assert_eq!(
        v2_resp["info"]["name"],
        "@agentclientprotocol/claude-agent-acp"
    );
    assert_eq!(v2_resp["capabilities"]["providers"], json!({}));
    assert_eq!(v2_resp["capabilities"]["_meta"]["agentMeta"], 123);

    // C-46 session capabilities
    assert_eq!(
        v2_resp["capabilities"]["session"],
        v2_session_capabilities()
    );
    assert_eq!(
        v2_resp["capabilities"]["session"]["prompt"]["image"],
        json!({})
    );
    assert_eq!(
        v2_resp["capabilities"]["session"]["prompt"]["embeddedContext"],
        json!({})
    );
    assert_eq!(
        v2_resp["capabilities"]["session"]["mcp"]["stdio"],
        json!({})
    );
    assert_eq!(v2_resp["capabilities"]["session"]["mcp"]["http"], json!({}));
    assert_eq!(v2_resp["capabilities"]["session"]["delete"], json!({}));
    assert_eq!(v2_resp["capabilities"]["session"]["fork"], json!({}));
    assert_eq!(
        v2_resp["capabilities"]["session"]["additionalDirectories"],
        json!({})
    );
    assert_eq!(
        v2_resp["capabilities"]["session"]["systemPrompt"]["mode"]["override"],
        json!({})
    );

    // C-45 steering stripped from top-level _meta, other keys retained
    assert_eq!(v2_resp["_meta"]["steering"], Value::Null);
    assert_eq!(v2_resp["_meta"]["other"], "kept");

    // auth methods mapped
    let methods = v2_resp["authMethods"]
        .as_array()
        .expect("array of auth methods");
    assert_eq!(methods.len(), 2);
    assert_eq!(methods[0]["methodId"], "claude-login");
    assert_eq!(methods[0]["type"], "terminal");
    assert_eq!(methods[0]["args"][0], "--cli");
    assert_eq!(methods[0]["env"][0]["name"], "FOO");
    assert_eq!(methods[0]["env"][0]["value"], "bar");

    assert_eq!(methods[1]["methodId"], "agent-auth");
    assert_eq!(methods[1]["type"], "agent");
}

#[test]
fn test_c45_v2_initialize_response_requires_agent_info() {
    let v1_resp = json!({
        "protocolVersion": 1
    });
    let err = v2_initialize_response(&v1_resp).expect_err("should require agentInfo");
    assert!(err.message.contains("ACP v2 requires agentInfo"));
}

#[test]
fn test_o86_v2_config_options() {
    let options = vec![json!({
        "id": "model",
        "name": "Model",
        "type": "select",
        "currentValue": "sonnet",
        "options": [{
            "group": "claude",
            "name": "Claude",
            "options": [{ "value": "sonnet", "name": "S" }]
        }]
    })];

    let v2_opts = v2_config_options(&options);
    assert_eq!(v2_opts[0]["configId"], "model");
    assert_eq!(v2_opts[0]["id"], Value::Null);
    assert_eq!(v2_opts[0]["options"][0]["groupId"], "claude");
    assert_eq!(v2_opts[0]["options"][0]["group"], Value::Null);
}

#[test]
fn test_o86_v2_available_commands() {
    let commands = vec![
        json!({
            "name": "review",
            "description": "Review a change",
            "input": { "hint": "<pull request>" }
        }),
        json!({
            "name": "clear",
            "description": "Clear context"
        }),
    ];

    let v2_cmds = v2_available_commands(&commands);
    assert_eq!(v2_cmds[0]["name"], "review");
    assert_eq!(v2_cmds[0]["input"]["type"], "text");
    assert_eq!(v2_cmds[0]["input"]["hint"], "<pull request>");
    assert_eq!(v2_cmds[1]["name"], "clear");
    assert_eq!(v2_cmds[1]["input"], Value::Null);
}

#[test]
fn test_o86_v1_set_session_config_option_request() {
    let req_id = json!({
        "sessionId": "s",
        "configId": "model",
        "type": "id",
        "value": "sonnet"
    });
    let v1_req_id = v1_set_session_config_option_request(&req_id).unwrap();
    assert_eq!(v1_req_id["sessionId"], "s");
    assert_eq!(v1_req_id["configId"], "model");
    assert_eq!(v1_req_id["value"], "sonnet");
    assert_eq!(v1_req_id["type"], Value::Null);

    let req_bool = json!({
        "sessionId": "s",
        "configId": "auto_approve",
        "type": "boolean",
        "value": true
    });
    let v1_req_bool = v1_set_session_config_option_request(&req_bool).unwrap();
    assert_eq!(v1_req_bool["type"], "boolean");
    assert_eq!(v1_req_bool["value"], true);

    let req_invalid = json!({
        "sessionId": "s",
        "configId": "c",
        "type": "_range",
        "value": 3
    });
    let err = v1_set_session_config_option_request(&req_invalid).expect_err("unsupported type");
    assert!(err
        .message
        .contains("Config option values of type _range are not supported"));
}

/// FA1-03 — `SetSessionConfigOptionRequest.isId`/`isBoolean`
/// (`guards.gen.js:929-933`) require the exact tag AND a valid payload; a
/// missing/null/non-string `type` (or an ill-typed `value`) falls through to
/// `invalidParams({type}, "Config option values of type <String(type)> are not
/// supported")` (`v2/session.ts:114-122`). `JSON.stringify` drops an undefined
/// `type` key, so the missing case carries `data:{}`.
#[test]
fn fa1_03_v2_set_config_option_requires_a_valid_tag_and_payload() {
    // Missing type: Node's `${params.type}` is "undefined" and data is `{}`.
    let err = v1_set_session_config_option_request(&json!({
        "sessionId": "s",
        "configId": "mode",
        "value": "plan"
    }))
    .expect_err("a missing type must be rejected");
    assert_eq!(i32::from(err.code), -32602);
    assert_eq!(
        err.message,
        "Invalid params: Config option values of type undefined are not supported"
    );
    assert_eq!(err.data, Some(json!({})));

    // Explicit null: `${null}` is "null", data keeps the key.
    let err = v1_set_session_config_option_request(&json!({
        "sessionId": "s",
        "configId": "mode",
        "type": null,
        "value": "plan"
    }))
    .expect_err("a null type must be rejected");
    assert_eq!(i32::from(err.code), -32602);
    assert_eq!(
        err.message,
        "Invalid params: Config option values of type null are not supported"
    );
    assert_eq!(err.data, Some(json!({"type": null})));

    // A number type is stringified into the message.
    let err = v1_set_session_config_option_request(&json!({
        "sessionId": "s",
        "configId": "mode",
        "type": 5,
        "value": "plan"
    }))
    .expect_err("a number type must be rejected");
    assert_eq!(i32::from(err.code), -32602);
    assert_eq!(
        err.message,
        "Invalid params: Config option values of type 5 are not supported"
    );
    assert_eq!(err.data, Some(json!({"type": 5})));

    // A known tag with an ill-typed value matches no guard: Node reports the
    // tag ("id"), not the value type.
    let err = v1_set_session_config_option_request(&json!({
        "sessionId": "s",
        "configId": "mode",
        "type": "id",
        "value": 7
    }))
    .expect_err("an id tag with a non-string value must be rejected");
    assert_eq!(i32::from(err.code), -32602);
    assert_eq!(
        err.message,
        "Invalid params: Config option values of type id are not supported"
    );
    assert_eq!(err.data, Some(json!({"type": "id"})));

    let err = v1_set_session_config_option_request(&json!({
        "sessionId": "s",
        "configId": "auto_approve",
        "type": "boolean",
        "value": "true"
    }))
    .expect_err("a boolean tag with a non-boolean value must be rejected");
    assert_eq!(i32::from(err.code), -32602);
    assert_eq!(
        err.message,
        "Invalid params: Config option values of type boolean are not supported"
    );
    assert_eq!(err.data, Some(json!({"type": "boolean"})));
}

#[test]
fn test_m118_extension_methods() {
    assert!(is_extension_method("_auth/status_update"));
    assert!(is_extension_method("_session/custom"));
    assert!(!is_extension_method("session/new"));
    assert!(!is_extension_method("initialize"));

    assert!(validate_extension_method("_custom").is_ok());
    let err = validate_extension_method("custom").expect_err("should reject non-underscore");
    assert_eq!(err.message, "custom is not an ACP extension method");
}

#[test]
fn test_m106_o139_route_first_frame() {
    let v1_init = json!({
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1
        }
    });

    let v2_init = json!({
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 2,
            "info": { "name": "test", "version": "1.0.0" }
        }
    });

    // Without experimental v2, always routes to v1
    assert_eq!(
        route_first_frame(&v2_init, false),
        RouteDecision::Route(ProtocolVersion::V1)
    );
    assert_eq!(
        route_first_frame(&v1_init, false),
        RouteDecision::Route(ProtocolVersion::V1)
    );

    // With experimental v2 enabled
    assert_eq!(
        route_first_frame(&v2_init, true),
        RouteDecision::Route(ProtocolVersion::V2)
    );
    assert_eq!(
        route_first_frame(&v1_init, true),
        RouteDecision::Route(ProtocolVersion::V1)
    );

    // Higher protocol version negotiates v2
    let v3_init = json!({
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 3
        }
    });
    assert_eq!(
        route_first_frame(&v3_init, true),
        RouteDecision::Route(ProtocolVersion::V2)
    );

    // Batched single initialize
    let batched_v2 = json!([v2_init]);
    assert_eq!(
        route_first_frame(&batched_v2, true),
        RouteDecision::Route(ProtocolVersion::V2)
    );

    // Invalid first method
    let not_init = json!({
        "id": 1,
        "method": "session/new",
        "params": {}
    });
    match route_first_frame(&not_init, true) {
        RouteDecision::Reject { message, data, .. } => {
            assert_eq!(message, "Invalid request");
            assert_eq!(data, Some(json!("first ACP request must be initialize")));
        }
        _ => panic!("expected rejection"),
    }
}

#[test]
fn test_v2_session_lifecycle_helpers() {
    let new_req = json!({
        "cwd": "/work",
        "systemPrompt": {
            "mode": "override",
            "content": [{ "type": "text", "text": "Be brief.", "annotations": { "audience": ["_bot"] } }]
        }
    });
    let v1_new = v1_new_session_request(&new_req).unwrap();
    assert_eq!(v1_new["cwd"], "/work");
    // Audience was stripped because _bot is unknown to v1
    assert_eq!(
        v1_new["systemPrompt"]["content"][0]["annotations"]
            .as_object()
            .unwrap()
            .len(),
        0
    );

    let resume_req = json!({
        "sessionId": "s",
        "replayFrom": { "type": "start" }
    });
    let res = v1_restore_session_request(&resume_req).unwrap();
    assert_eq!(res.method, "session/load");

    let bad_replay = json!({
        "sessionId": "s",
        "replayFrom": { "type": "end" }
    });
    assert!(v1_restore_session_request(&bad_replay).is_err());
}

#[test]
fn test_c24_v2_cancelled_status_and_diff() {
    use super::session::{is_final_tool_call_status, tool_call_status_for_aborted};
    use crate::diff::{v2_diff_content, FileChange};

    // C-24 tool call status
    assert!(is_final_tool_call_status("completed", false));
    assert!(is_final_tool_call_status("failed", false));
    assert!(!is_final_tool_call_status("cancelled", false));

    assert!(is_final_tool_call_status("completed", true));
    assert!(is_final_tool_call_status("failed", true));
    assert!(is_final_tool_call_status("cancelled", true));

    assert_eq!(tool_call_status_for_aborted(false), "failed");
    assert_eq!(tool_call_status_for_aborted(true), "cancelled");

    // C-24 v2 diff format
    let diff = v2_diff_content("/work/file.txt", FileChange::Create, None);
    assert_eq!(diff["type"], "diff");
    assert_eq!(diff["changes"][0]["operation"], "add");
    assert_eq!(diff["changes"][0]["path"], "/work/file.txt");
    assert_eq!(diff["changes"][0]["fileType"], "text");
}

#[test]
fn test_b1_negotiate_initialize_v2_translates_before_parsing() {
    // The golden request: a v2 client sends `capabilities`, not
    // `clientCapabilities`. The negotiation turns it into the v1 request the
    // agent reads, so the auth flag and the baseline caps are visible.
    let v2_req = json!({
        "protocolVersion": 2,
        "info": { "name": "v2-client", "version": "1.0.0" },
        "capabilities": { "auth": { "terminal": {} } }
    });
    let negotiated = negotiate_initialize(&v2_req, true).expect("v2 negotiates");
    assert_eq!(negotiated.mode, ProtocolMode::V2);
    let v1 = &negotiated.params;
    assert_eq!(v1["protocolVersion"], V1_PROTOCOL_VERSION);
    assert_eq!(v1["clientInfo"]["name"], "v2-client");
    assert_eq!(v1["clientCapabilities"]["auth"]["terminal"], true);
    assert_eq!(v1["clientCapabilities"]["terminal"], false);
    assert_eq!(
        v1["clientCapabilities"]["session"]["configOptions"]["boolean"],
        json!({})
    );
    assert_eq!(v1["clientCapabilities"]["plan"], json!({}));
    assert_eq!(v1["clientCapabilities"]["subagents"], json!({}));
    // A v1 client's params stay untouched.
    let v1_req = json!({ "protocolVersion": 1, "clientCapabilities": {} });
    let negotiated = negotiate_initialize(&v1_req, true).expect("v1 negotiates");
    assert_eq!(negotiated.mode, ProtocolMode::V1);
    assert_eq!(negotiated.params, v1_req);
}

#[test]
fn test_o139_negotiate_without_opt_in_never_translates() {
    // Without the opt-in the v1 path is byte-identical: no version check, no
    // rewrite, even for a version 2 request.
    let v2_req = json!({ "protocolVersion": 2, "capabilities": {} });
    let negotiated = negotiate_initialize(&v2_req, false).expect("v1 stays v1");
    assert_eq!(negotiated.mode, ProtocolMode::V1);
    assert_eq!(negotiated.params, v2_req);
}

#[test]
fn test_l4_negotiate_initialize_errors_match_upstream() {
    // protocolVersion 0: unsupported, with requested/supported data.
    let zero = json!({ "protocolVersion": 0 });
    let err = negotiate_initialize(&zero, true).expect_err("version 0 is rejected");
    assert_eq!(i32::from(err.code), -32022);
    assert_eq!(err.message, "Unsupported protocol version");
    assert_eq!(
        err.data,
        Some(json!({ "requested": 0, "supported": [1, 2] }))
    );

    // Missing, string, fractional, negative and out-of-range versions are
    // invalid params with the detail as `data` (`protocol-router.js:386-391`).
    for params in [
        json!({}),
        json!({ "protocolVersion": "2" }),
        json!({ "protocolVersion": 2.5 }),
        json!({ "protocolVersion": -1 }),
        json!({ "protocolVersion": 70000 }),
        json!(null),
    ] {
        let err = negotiate_initialize(&params, true).expect_err("invalid version");
        assert_eq!(i32::from(err.code), -32602);
        assert_eq!(err.message, "Invalid params");
        assert_eq!(
            err.data,
            Some(json!(
                "initialize.protocolVersion must be a valid ACP protocol version"
            ))
        );
    }

    // FA3b-02: `JSON.parse("2.0")` is the integer 2, so a serializer that
    // writes the version as a float still routes like Node.
    for (params, mode) in [
        (json!({ "protocolVersion": 2.0 }), ProtocolMode::V2),
        (json!({ "protocolVersion": 1.0 }), ProtocolMode::V1),
        (json!({ "protocolVersion": 3.0 }), ProtocolMode::V2),
    ] {
        let negotiated = negotiate_initialize(&params, true).expect("integral float routes");
        assert_eq!(negotiated.mode, mode, "{params}");
    }
}

#[test]
fn test_l4_route_first_frame_error_shapes() {
    let version_zero = json!({
        "id": 7,
        "method": "initialize",
        "params": { "protocolVersion": 0 }
    });
    match route_first_frame(&version_zero, true) {
        RouteDecision::Reject {
            code,
            message,
            data,
            id,
            ..
        } => {
            assert_eq!(code, -32022);
            assert_eq!(message, "Unsupported protocol version");
            assert_eq!(data, Some(json!({ "requested": 0, "supported": [1, 2] })));
            assert_eq!(id, Some(json!(7)));
        }
        other => panic!("expected rejection, got {other:?}"),
    }

    let float_version = json!({
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": 2.0 }
    });
    // FA3b-02 (`protocol-router.js:386-391`): `JSON.parse("2.0")` is the
    // integer 2, so the float spelling routes to v2, as Node does.
    assert_eq!(
        route_first_frame(&float_version, true),
        RouteDecision::Route(ProtocolVersion::V2)
    );

    let fractional_version = json!({
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": 2.5 }
    });
    match route_first_frame(&fractional_version, true) {
        RouteDecision::Reject {
            code,
            message,
            data,
            ..
        } => {
            assert_eq!(code, -32602);
            assert_eq!(message, "Invalid params");
            assert_eq!(
                data,
                Some(json!(
                    "initialize.protocolVersion must be a valid ACP protocol version"
                ))
            );
        }
        other => panic!("expected rejection, got {other:?}"),
    }

    let not_initialize = json!({ "id": 2, "method": "session/new", "params": {} });
    match route_first_frame(&not_initialize, true) {
        RouteDecision::Reject {
            code,
            message,
            data,
            ..
        } => {
            assert_eq!(code, -32600);
            assert_eq!(message, "Invalid request");
            assert_eq!(data, Some(json!("first ACP request must be initialize")));
        }
        other => panic!("expected rejection, got {other:?}"),
    }
}

#[test]
fn test_l1_v2_auth_method_keeps_upstream_key_order() {
    // Upstream `{...terminal, methodId: id}`: the v1 order is
    // description,name,id,type,args, so v2 is description,name,type,args,methodId.
    let v1_method = json!({
        "description": "Use Claude subscription ",
        "name": "Claude Subscription",
        "id": "claude-ai-login",
        "type": "terminal",
        "args": ["--cli", "auth", "login", "--claudeai"]
    });
    let v2_method = v2_auth_method(&v1_method);
    let keys: Vec<&str> = v2_method
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["description", "name", "type", "args", "methodId"],
        "key order equals Node's JSON.stringify order"
    );

    // A non-terminal method keeps its fields and appends type/methodId.
    let agent_method = json!({ "id": "a", "name": "A" });
    let mapped = v2_auth_method(&agent_method);
    assert_eq!(mapped["type"], "agent");
    let keys: Vec<&str> = mapped
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["name", "methodId", "type"]);
}

#[test]
fn test_l1_v2_available_commands_tag_type_first() {
    let commands = vec![json!({
        "name": "review",
        "description": "Review a change",
        "input": { "hint": "<pull request>" }
    })];
    let mapped = v2_available_commands(&commands);
    let keys: Vec<&str> = mapped[0]["input"]
        .as_object()
        .expect("input object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["type", "hint"], "type comes first, as upstream");
}
