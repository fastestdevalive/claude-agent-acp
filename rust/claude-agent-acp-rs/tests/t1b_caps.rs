//! Turn 1 lane B: client-capability driven pure builders (F2a): the
//! `initialize` auth methods (WB-04) and the display-terminal reports of a
//! command (C-23, `renderer.ts:173-175,391-418`).

use claude_agent_acp_rs::client_caps::{auth_methods, ClientCapabilities, CommandOutcome};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use serde_json::{json, Value};

fn caps(value: Value) -> ClientCapabilities {
    ClientCapabilities::from_value(Some(&value))
}

fn ids(methods: &[Value]) -> Vec<&str> {
    methods.iter().filter_map(|m| m["id"].as_str()).collect()
}

fn argv() -> Vec<String> {
    vec!["/pkg/dist/index.js".into()]
}

// ---- WB-04: authMethods variants ----

#[test]
fn plain_client_gets_no_auth_methods() {
    let methods = auth_methods(
        &caps(json!({})),
        &EnvSnapshot::empty(),
        "/bin/node",
        &argv(),
        false,
    );
    assert!(methods.is_empty());
}

#[test]
fn terminal_capable_client_gets_both_logins_without_terminal_auth_meta() {
    let methods = auth_methods(
        &caps(json!({"auth": {"terminal": true}})),
        &EnvSnapshot::empty(),
        "/bin/node",
        &argv(),
        false,
    );
    assert_eq!(ids(&methods), ["claude-ai-login", "console-login"]);
    assert_eq!(methods[0]["type"], "terminal");
    assert_eq!(
        methods[0]["args"],
        json!(["--cli", "auth", "login", "--claudeai"])
    );
    assert_eq!(methods[0]["description"], "Use Claude subscription ");
    assert!(methods[0].get("_meta").is_none());
}

#[test]
fn terminal_auth_meta_client_gets_the_launch_command() {
    let methods = auth_methods(
        &caps(json!({"_meta": {"terminal-auth": true}})),
        &EnvSnapshot::empty(),
        "/bin/node",
        &argv(),
        false,
    );
    assert_eq!(
        methods[0]["_meta"]["terminal-auth"],
        json!({
            "command": "/bin/node",
            "args": ["/pkg/dist/index.js", "--cli", "auth", "login", "--claudeai"],
            "label": "Claude Login",
        })
    );
    assert_eq!(
        methods[1]["_meta"]["terminal-auth"]["label"],
        "Anthropic Console Login"
    );
}

#[test]
fn remote_environment_offers_the_tui_login_instead() {
    for var in [
        "NO_BROWSER",
        "SSH_CONNECTION",
        "SSH_CLIENT",
        "SSH_TTY",
        "CLAUDE_CODE_REMOTE",
    ] {
        let env = EnvSnapshot::from_pairs([(var, "1")]);
        let methods = auth_methods(
            &caps(json!({"auth": {"terminal": true}, "_meta": {"terminal-auth": true}})),
            &env,
            "/bin/node",
            &argv(),
            false,
        );
        assert_eq!(ids(&methods), ["claude-login"], "{var}");
        assert_eq!(methods[0]["args"], json!(["--cli"]));
        assert_eq!(
            methods[0]["_meta"]["terminal-auth"]["args"],
            json!(["/pkg/dist/index.js", "--cli"])
        );
    }
    // An empty value is falsy, like `process.env.NO_BROWSER`.
    let env = EnvSnapshot::from_pairs([("NO_BROWSER", "")]);
    let methods = auth_methods(
        &caps(json!({"auth": {"terminal": true}})),
        &env,
        "/bin/node",
        &argv(),
        false,
    );
    assert_eq!(ids(&methods), ["claude-ai-login", "console-login"]);
}

#[test]
fn hide_claude_auth_drops_the_claude_ai_login_only() {
    let c = caps(json!({"auth": {"terminal": true}}));
    let methods = auth_methods(&c, &EnvSnapshot::empty(), "/bin/node", &argv(), true);
    assert_eq!(ids(&methods), ["console-login"]);
    let remote = EnvSnapshot::from_pairs([("SSH_TTY", "/dev/pts/1")]);
    assert!(auth_methods(&c, &remote, "/bin/node", &argv(), true).is_empty());
}

#[test]
fn gateway_client_gets_the_two_gateway_methods_after_the_terminal_ones() {
    let methods = auth_methods(
        &caps(json!({"auth": {"terminal": true, "_meta": {"gateway": true}}})),
        &EnvSnapshot::empty(),
        "/bin/node",
        &argv(),
        false,
    );
    assert_eq!(
        ids(&methods),
        [
            "claude-ai-login",
            "console-login",
            "gateway",
            "gateway-bedrock"
        ]
    );
    assert_eq!(
        methods[2]["_meta"],
        json!({"gateway": {"protocol": "anthropic"}})
    );
    assert_eq!(
        methods[3]["_meta"],
        json!({"gateway": {"protocol": "bedrock"}})
    );
    assert_eq!(methods[3]["name"], "Custom model gateway");
}

#[test]
fn gateway_only_client_gets_only_gateway_methods() {
    let methods = auth_methods(
        &caps(json!({"auth": {"_meta": {"gateway": true}}})),
        &EnvSnapshot::empty(),
        "/bin/node",
        &argv(),
        false,
    );
    assert_eq!(ids(&methods), ["gateway", "gateway-bedrock"]);
}

// ---- C-23: display terminal for a command ----

fn outcome(output: &str, exit_code: Option<i64>, interrupted: bool) -> CommandOutcome {
    CommandOutcome {
        output: output.into(),
        exit_code,
        interrupted,
    }
}

#[test]
fn only_a_command_on_a_terminal_client_gets_a_terminal() {
    let terminal = caps(json!({"_meta": {"terminal_output": true}}));
    assert!(terminal.has_terminal(true));
    assert!(!terminal.has_terminal(false));
    assert!(!caps(json!({})).has_terminal(true));
    assert_eq!(
        terminal.terminal_info_meta(true, "toolu_1"),
        Some(json!({"terminal_info": {"terminal_id": "toolu_1"}}))
    );
    assert_eq!(terminal.terminal_info_meta(false, "toolu_1"), None);
    assert_eq!(caps(json!({})).terminal_info_meta(true, "toolu_1"), None);
}

#[test]
fn terminal_result_for_a_plain_terminal_client() {
    let c = caps(json!({"_meta": {"terminal_output": true}}));
    let r = c
        .terminal_result(
            Some("toolu_1"),
            Some("toolu_1"),
            &outcome("hi\n", Some(0), false),
            false,
        )
        .expect("terminal result");
    assert_eq!(
        r.content,
        Some(json!({"type": "terminal", "terminalId": "toolu_1"}))
    );
    assert_eq!(
        r.output_meta,
        json!({"terminal_output": {"terminal_id": "toolu_1", "data": "hi\n"}})
    );
    assert_eq!(
        r.exit_meta,
        json!({"terminal_exit": {"terminal_id": "toolu_1", "exit_code": 0, "signal": null}})
    );
    // An unknown exit code stays null for a non-AIR client.
    let unknown = c
        .terminal_result(Some("toolu_1"), None, &outcome("", None, false), true)
        .expect("terminal result");
    assert_eq!(unknown.exit_meta["terminal_exit"]["exit_code"], Value::Null);
}

#[test]
fn terminal_result_uses_the_delta_channel_when_negotiated() {
    let c = caps(json!({"_meta": {"terminal_output_delta": true}}));
    let r = c
        .terminal_result(Some("t"), None, &outcome("x", Some(2), false), false)
        .expect("terminal result");
    assert_eq!(
        r.output_meta,
        json!({"terminal_output_delta": {"terminal_id": "t", "data": "x"}})
    );
    assert_eq!(r.exit_meta["terminal_exit"]["exit_code"], 2);
}

#[test]
fn air_terminal_result_has_no_content_and_numeric_exit_codes() {
    let c = caps(json!({
        "_meta": {"terminal_output": true, "jetbrains": {"air": {"version": 1, "capabilities": []}}}
    }));
    let ok = c
        .terminal_result(Some("t"), None, &outcome("", None, false), false)
        .expect("terminal result");
    assert_eq!(ok.content, None);
    assert_eq!(ok.exit_meta["terminal_exit"]["exit_code"], 0);
    let failed = c
        .terminal_result(Some("t"), None, &outcome("", Some(0), false), true)
        .expect("terminal result");
    assert_eq!(failed.exit_meta["terminal_exit"]["exit_code"], 1);
    let interrupted = c
        .terminal_result(Some("t"), None, &outcome("", Some(0), true), false)
        .expect("terminal result");
    assert_eq!(interrupted.exit_meta["terminal_exit"]["exit_code"], 1);
    let exited = c
        .terminal_result(Some("t"), None, &outcome("", Some(7), false), false)
        .expect("terminal result");
    assert_eq!(exited.exit_meta["terminal_exit"]["exit_code"], 7);
}

#[test]
fn terminal_result_falls_back_to_the_result_id_and_needs_some_id() {
    let c = caps(json!({"_meta": {"terminal_output": true}}));
    let r = c
        .terminal_result(
            Some(""),
            Some("toolu_r"),
            &outcome("o", Some(0), false),
            false,
        )
        .expect("terminal result");
    assert_eq!(
        r.content,
        Some(json!({"type": "terminal", "terminalId": "toolu_r"}))
    );
    // No usable id: the output becomes a code block instead.
    assert_eq!(
        c.terminal_result(None, Some(""), &outcome("o", Some(0), false), false),
        None
    );
    // A client without terminal support never gets a terminal result.
    assert_eq!(
        caps(json!({})).terminal_result(Some("t"), None, &outcome("o", Some(0), false), false),
        None
    );
}

#[test]
fn terminal_exit_for_a_command_that_got_no_result() {
    let c = caps(json!({"_meta": {"terminal_output": true}}));
    assert_eq!(
        c.terminal_exit_meta(true, "t"),
        Some(json!({"terminal_exit": {"terminal_id": "t", "exit_code": null, "signal": null}}))
    );
    let air = caps(json!({
        "_meta": {"terminal_output": true, "jetbrains": {"air": {"version": 1}}}
    }));
    assert_eq!(
        air.terminal_exit_meta(true, "t").expect("exit")["terminal_exit"]["exit_code"],
        1
    );
    assert_eq!(c.terminal_exit_meta(false, "t"), None);
    assert_eq!(caps(json!({})).terminal_exit_meta(true, "t"), None);
}

/// `tools.test.ts` > "keys the terminal metas off the tool_use id, which is
/// what was announced": a result that names another id cannot retarget them.
#[test]
fn t8c_terminal_metas_follow_the_announced_id() {
    let c = caps(json!({"_meta": {"terminal_output": true}}));
    let r = c
        .terminal_result(
            Some("toolu_bash"),
            Some("toolu_something_else"),
            &outcome("out", Some(0), false),
            false,
        )
        .expect("terminal result");
    assert_eq!(
        r.content,
        Some(json!({"type": "terminal", "terminalId": "toolu_bash"}))
    );
    assert_eq!(
        r.output_meta,
        json!({"terminal_output": {"terminal_id": "toolu_bash", "data": "out"}})
    );
    assert_eq!(
        r.exit_meta,
        json!({"terminal_exit": {"terminal_id": "toolu_bash", "exit_code": 0, "signal": null}})
    );
}
