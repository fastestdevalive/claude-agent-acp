//! T6A2 tests for the permission request presentation (P-16, P-52, P-53, P-55,
//! P-56, P-57), ported from `permissions/presentation.ts`,
//! `tool-calls/renderer.ts` and `acp-agent.ts` and the upstream cases in
//! `src/tests/permission-presentation.test.ts` and `src/tests/acp-agent.test.ts`.

use serde_json::{json, Value};

use crate::permission::{build_permission_tool_call, PermissionToolCall};

fn call(tool_name: &str, input: Value) -> PermissionToolCall {
    PermissionToolCall {
        tool_name: tool_name.to_string(),
        tool_input: input,
        tool_use_id: "tool-1".to_string(),
        ..Default::default()
    }
}

/// `permission-presentation.test.ts` > "Claude permission presentation for a
/// client that is not AIR" > "repeats the whole tool call and sends no AIR
/// key": the whole call again, in Node's key order, and no `_meta`.
#[test]
fn t6a2_repeats_the_whole_tool_call_and_sends_no_air_key() {
    let input = json!({"command": "npm test", "description": "Run the tests"});
    let mut request = call("Bash", input.clone());
    // The upstream case is a client with `terminalOutput` (`ClientCapabilities(true)`).
    request.terminal_output = true;
    let tool_call = build_permission_tool_call(&request);
    assert_eq!(
        tool_call,
        json!({
            "toolCallId": "tool-1",
            "name": "Bash",
            "status": "pending",
            "rawInput": input,
            "title": "npm test",
            "kind": "execute",
            "content": [{"type": "terminal", "terminalId": "tool-1"}],
        })
    );
    assert!(tool_call.get("_meta").is_none());
    let keys: Vec<&str> = tool_call
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "toolCallId",
            "name",
            "status",
            "rawInput",
            "title",
            "kind",
            "content"
        ],
        "Node's key order (renderer.ts:315-330)"
    );
}

/// `permission-presentation.test.ts` > "...for a client that is not AIR" >
/// "keeps the Write file text in rawInput and adds a blocked path".
#[test]
fn t6a2_keeps_the_write_file_text_in_rawinput_and_adds_a_blocked_path() {
    let input = json!({"file_path": "/work/a.ts", "content": "x"});
    let mut request = call("Write", input.clone());
    request.tool_use_id = "tool-2".to_string();
    request.cwd = Some("/work".into());
    request.blocked_path = Some("/outside/b.ts".to_string());
    let tool_call = build_permission_tool_call(&request);
    assert_eq!(
        tool_call,
        json!({
            "toolCallId": "tool-2",
            "name": "Write",
            "status": "pending",
            "rawInput": input,
            "title": "Write a.ts",
            "kind": "edit",
            "content": [{"type": "diff", "path": "/work/a.ts", "oldText": null, "newText": "x"}],
            "locations": [{"path": "/work/a.ts"}, {"path": "/outside/b.ts"}],
        })
    );
    assert!(tool_call.get("_meta").is_none());
}

/// `permission-presentation.test.ts` > "...for a client that is not AIR" >
/// "shows the input of a network request that has no content" (P-55).
#[test]
fn t6a2_shows_the_input_of_a_network_request_that_has_no_content() {
    let mut request = call("SandboxNetworkAccess", json!({"host": "example.com"}));
    request.tool_use_id = "tool-3".to_string();
    request.title = Some("example.com".to_string());
    request.fallback_content = Some(json!([{
        "type": "content",
        "content": {"type": "text", "text": "```json\n{\n  \"host\": \"example.com\"\n}\n```"},
    }]));
    let tool_call = build_permission_tool_call(&request);
    assert_eq!(tool_call["title"], json!("example.com"));
    assert_eq!(
        tool_call["content"],
        json!([{
            "type": "content",
            "content": {"type": "text", "text": "```json\n{\n  \"host\": \"example.com\"\n}\n```"},
        }])
    );
}

/// `permission-presentation.test.ts` > "Claude permission ACP v1 presentation" >
/// "adds a non-duplicated blocked path to standard locations" (P-53): the
/// blocked path rides after the tool call's own locations, and is not repeated.
#[test]
fn t6a2_adds_a_non_duplicated_blocked_path_to_standard_locations() {
    let mut request = call("Read", json!({"file_path": "/work/a.ts"}));
    request.tool_use_id = "tool-4".to_string();
    request.air = true;
    request.blocked_path = Some("/outside/b.ts".to_string());
    let tool_call = build_permission_tool_call(&request);
    assert_eq!(
        tool_call["locations"],
        json!([{"path": "/work/a.ts", "line": 1}, {"path": "/outside/b.ts"}])
    );

    // A blocked path the tool call already carries adds no `locations` to AIR's
    // minimal request (`renderer.ts:338-340` adds them only for extra paths).
    let mut duplicate = call("Read", json!({"file_path": "/work/a.ts"}));
    duplicate.air = true;
    duplicate.blocked_path = Some("/work/a.ts".to_string());
    let tool_call = build_permission_tool_call(&duplicate);
    assert!(tool_call.get("locations").is_none());
}

/// `permission-presentation.test.ts` > "Claude permission ACP v1 presentation" >
/// "reuses the standard title for an unknown tool": AIR's minimal shape is
/// only `toolCallId`, `title` and `rawInput` (P-16).
#[test]
fn t6a2_reuses_the_standard_title_for_an_unknown_tool() {
    let mut request = call("mcp__demo__deploy", json!({"target": "staging"}));
    request.tool_use_id = "tool-5".to_string();
    request.air = true;
    let tool_call = build_permission_tool_call(&request);
    assert_eq!(
        tool_call,
        json!({
            "toolCallId": "tool-5",
            "title": "mcp__demo__deploy",
            "rawInput": {"target": "staging"},
        })
    );
}

/// `permission-presentation.test.ts` > "Claude permission ACP v1 presentation" >
/// "uses the provider-built patch for an edit confirmation": AIR's request
/// carries the exact patch as its content (P-16).
#[test]
fn t6a2_uses_the_provider_built_patch_for_an_edit_confirmation() {
    let preview = json!([{
        "type": "diff",
        "path": "/work/file.ts",
        "oldText": null,
        "newText": "",
        "_meta": {"jetbrains": {"air": {"version": 1, "diffPatch": {
            "version": 1, "format": "git_patch", "text": "patch"
        }}}},
    }]);
    let mut request = call(
        "Edit",
        json!({"file_path": "/work/file.ts", "old_string": "old", "new_string": "new"}),
    );
    request.air = true;
    request.preview_content = Some(preview.clone());
    let tool_call = build_permission_tool_call(&request);
    assert_eq!(tool_call["content"], preview);
    assert_eq!(tool_call["rawInput"], json!({"file_path": "/work/file.ts"}));
}

/// `permission-presentation.test.ts` > "Claude permission ACP v1 presentation" >
/// "uses Approve Plan as the tool title and keeps the question in permission
/// metadata": the tool call keeps the reporter's heading.
#[test]
fn t6a2_uses_approve_plan_as_the_tool_title() {
    let mut request = call("ExitPlanMode", json!({"plan": "Implement the change"}));
    request.air = true;
    let tool_call = build_permission_tool_call(&request);
    assert_eq!(tool_call["title"], json!("Approve Plan"));
}

/// `acp-agent.test.ts` > "forwards the MCP server provenance on the permission
/// request" (P-57): AIR's `_meta.claudeCode` carries only the MCP server.
#[test]
fn t6a2_forwards_the_mcp_server_provenance_on_the_permission_request() {
    let mut request = call("mcp__github__create_issue", json!({"title": "x"}));
    request.tool_use_id = "toolu_mcp".to_string();
    request.air = true;
    request.mcp_server = Some(json!({"name": "github", "source": "project"}));
    let tool_call = build_permission_tool_call(&request);
    assert_eq!(
        tool_call["_meta"],
        json!({"claudeCode": {"mcpServer": {"name": "github", "source": "project"}}})
    );
}

/// `acp-agent.test.ts` > "omits permission _meta for a root tool without MCP
/// provenance".
#[test]
fn t6a2_omits_permission_meta_for_a_root_tool_without_mcp_provenance() {
    let mut request = call("Bash", json!({"command": "ls"}));
    request.tool_use_id = "toolu_plain".to_string();
    let tool_call = build_permission_tool_call(&request);
    assert!(tool_call.get("_meta").is_none());
}

/// `acp-agent.test.ts` > "keeps the legacy child tool call before its root
/// permission request": the surfaced `tool_call` carries the parent; the
/// request does not repeat it for AIR, and names `toolName` + the parent for
/// every other client (P-57).
#[test]
fn t6a2_keeps_the_legacy_child_tool_call_before_its_root_permission_request() {
    let mut air = call("Bash", json!({"command": "ls"}));
    air.tool_use_id = "toolu_sub".to_string();
    air.air = true;
    air.parent_tool_use_id = Some("toolu_parent".to_string());
    assert!(build_permission_tool_call(&air).get("_meta").is_none());

    let mut plain = call("Bash", json!({"command": "ls"}));
    plain.parent_tool_use_id = Some("toolu_parent".to_string());
    assert_eq!(
        build_permission_tool_call(&plain)["_meta"],
        json!({"claudeCode": {"toolName": "Bash", "parentToolUseId": "toolu_parent"}})
    );
}

/// `permission-presentation.test.ts` > "Claude permission ACP v1 presentation" >
/// "reuses tool-call titles and temporarily exposes decisionReason" (P-52):
/// the reason is the prompt description, capped at 4000 characters.
#[test]
fn t6a2_decision_reason_is_the_permission_description() {
    let (title, description) = crate::tools::permission_title_and_description(
        "Read",
        &json!({"file_path": "/work/a.ts"}),
        None,
        "Read /work/a.ts",
        Some("Needed to inspect the dependency."),
    );
    assert_eq!(title, "Read /work/a.ts");
    assert_eq!(
        description.as_deref(),
        Some("Reason: Needed to inspect the dependency.")
    );
    let long = "x".repeat(4_001);
    let (_, description) = crate::tools::permission_title_and_description(
        "Read",
        &json!({}),
        None,
        "Read File",
        Some(&long),
    );
    assert_eq!(
        description, None,
        "a reason longer than 4000 characters is not shown"
    );
}

/// `permission-presentation.test.ts` > "Claude permission ACP v1 presentation" >
/// "shows the exact %s command instead of its model-authored description"
/// and "...uses the Terminal fallback for %s when no command is available yet"
/// (P-56): the subject is the host/displayName, the fallback is "Use tool?",
/// and shell titles are never compacted.
#[test]
fn t6a2_permission_title_and_description_subjects() {
    // SandboxNetworkAccess is named by its host.
    assert_eq!(
        crate::tools::permission_subject(
            "SandboxNetworkAccess",
            &json!({"host": "example.com"}),
            None
        )
        .as_deref(),
        Some("example.com")
    );
    // A computer-use call is named by the CLI's display name.
    assert_eq!(
        crate::tools::permission_subject(
            "mcp__computer-use__screenshot",
            &json!({}),
            Some("Take a screenshot")
        )
        .as_deref(),
        Some("Take a screenshot")
    );
    // The shell title keeps its exact bytes (quotes, tabs, newlines).
    let command = "echo \"a  b\tc\"\necho second";
    let shape = crate::tools::tool_shape("Bash", &json!({"command": command}), None);
    assert_eq!(shape.title, command);
    let (title, _) = crate::tools::permission_title_and_description(
        "Bash",
        &json!({"command": command}),
        None,
        &shape.title,
        None,
    );
    assert_eq!(title, command);
    // A long non-shell title falls back to "Use tool?".
    let long = "x".repeat(4_001);
    let (title, _) =
        crate::tools::permission_title_and_description("Read", &json!({}), None, &long, None);
    assert_eq!(title, "Use tool?");
    // ExitPlanMode's prompt heading is "Ready to code?".
    let (title, _) = crate::tools::permission_title_and_description(
        "ExitPlanMode",
        &json!({"plan": "x"}),
        None,
        "Approve Plan",
        None,
    );
    assert_eq!(title, "Ready to code?");
}

/// `permission-presentation.test.ts` > "Claude permission ACP v1 presentation" >
/// "uses Approve Plan as the tool title and keeps the question in permission
/// metadata" (P-59): AIR's request carries `_meta.jetbrains.air.permission`
/// with the prompt heading, while the tool call keeps the reporter's title.
#[test]
fn t6a2_uses_approve_plan_as_the_tool_title_and_keeps_the_question_in_permission_metadata() {
    let meta = crate::tools::air_permission_meta(
        "ExitPlanMode",
        &json!({"plan": "Implement the change"}),
        None,
        "Approve Plan",
        None,
        false,
    );
    assert_eq!(
        meta,
        json!({"jetbrains": {"air": {"version": 1, "permission": {
            "version": 1, "title": "Ready to code?"
        }}}})
    );
}

/// `permission-presentation.test.ts` > "Claude permission ACP v1 presentation" >
/// "forwards the CLI's defaultToNo hint in the permission record".
#[test]
fn t6a2_forwards_the_cli_default_to_no_hint() {
    let with_hint = crate::tools::air_permission_meta(
        "Bash",
        &json!({"command": "rm -rf build"}),
        None,
        "rm -rf build",
        None,
        true,
    );
    assert_eq!(
        with_hint,
        json!({"jetbrains": {"air": {"version": 1, "permission": {
            "version": 1, "title": "rm -rf build", "defaultToNo": true
        }}}})
    );
    let without = crate::tools::air_permission_meta(
        "Bash",
        &json!({"command": "rm -rf build"}),
        None,
        "rm -rf build",
        None,
        false,
    );
    assert_eq!(
        without,
        json!({"jetbrains": {"air": {"version": 1, "permission": {
            "version": 1, "title": "rm -rf build"
        }}}})
    );
}

/// `permission-presentation.test.ts` > "Claude permission ACP v1 presentation" >
/// "reuses tool-call titles and temporarily exposes decisionReason": the
/// permission heading is the tool-call heading, and the reason rides beside it.
#[test]
fn t6a2_reuses_tool_call_titles_and_temporarily_exposes_decision_reason() {
    let meta = crate::tools::air_permission_meta(
        "Read",
        &json!({"file_path": "/work/a.ts"}),
        None,
        "Read /work/a.ts",
        Some("Needed to inspect the dependency."),
        false,
    );
    assert_eq!(
        meta,
        json!({"jetbrains": {"air": {"version": 1, "permission": {
            "version": 1,
            "title": "Read /work/a.ts",
            "description": "Reason: Needed to inspect the dependency."
        }}}})
    );
}

/// `session-mode.test.ts` > "session mode" > "initializes the stable mode
/// catalog and falls unsupported Auto back": the clamp diagnostics are Node's
/// exact `logger.error` text (`session-mode.ts:159-176`), and a kept mode logs
/// nothing (Q-54).
#[test]
fn t6a2_initializes_the_stable_mode_catalog_and_falls_unsupported_auto_back() {
    assert_eq!(
        crate::agent::auto_mode_clamp_log("auto", true, "haiku", true).as_deref(),
        Some(
            "permissions.defaultMode \"auto\" is not available for model \"haiku\"; \
             falling back to \"acceptEdits\"."
        )
    );
    assert_eq!(
        crate::agent::auto_mode_clamp_log("auto", false, "opus", true),
        None
    );
    assert_eq!(
        crate::agent::auto_mode_clamp_log("warp", false, "opus", false).as_deref(),
        Some(
            "permissions.defaultMode \"warp\" is not available in this session; \
             falling back to \"default\"."
        )
    );
}

// --- G4 / F10: permission effects, normalization and presentation rows ---
//
// The product code is the Turn 6 A2 port (`permission_effects.rs`,
// `permission_options.rs`, `tools.rs`, `permission.rs`, `v2/permission.rs`);
// these tests prove the remaining `permission-*.test.ts` cases.

use crate::permission_effects::{apply_selection, SelectionOutcome};
use crate::permission_options::{
    build_options, normalize_change_set, OptionsContext, OPTION_ALLOW_WITH_UPDATES,
};

fn permission_ctx(tool_name: &str, input: Value) -> OptionsContext {
    OptionsContext {
        tool_name: tool_name.to_string(),
        display_name: None,
        input,
        cwd: std::path::PathBuf::from("/workspace"),
        change_set: None,
        no_persistent_rule: false,
        default_to_no: false,
        available_modes: Vec::new(),
        auto_unavailable: false,
        pre_plan_mode: None,
        context_used_percent: None,
        home_dir: None,
    }
}

fn with_change_set(mut ctx: OptionsContext, updates: Value) -> OptionsContext {
    ctx.change_set = normalize_change_set(&updates, false);
    assert!(
        ctx.change_set.is_some(),
        "a valid change set for {}",
        ctx.tool_name
    );
    ctx
}

/// `permission-effects.test.ts` > "Claude permission response effects" >
/// "applies the exact provider filesystem effect for %s" (`it.each` over
/// Read/Glob/Grep/Edit/Write/NotebookEdit): the durable allow carries the
/// snapshotted provider change set verbatim.
#[test]
fn g4_f10_applies_the_exact_provider_filesystem_effect() {
    for (tool_name, input, rule_tool) in [
        ("Read", json!({"file_path": "/outside/a.ts"}), "Read"),
        ("Glob", json!({"path": "/outside"}), "Read"),
        ("Grep", json!({"path": "/outside"}), "Read"),
        ("Edit", json!({"file_path": "/outside/a.ts"}), "Edit"),
        ("Write", json!({"file_path": "/outside/a.ts"}), "Edit"),
        (
            "NotebookEdit",
            json!({"notebook_path": "/outside/a.ipynb"}),
            "Edit",
        ),
    ] {
        let updates = json!([{
            "type": "addRules",
            "rules": [{"toolName": rule_tool, "ruleContent": "/outside/**"}],
            "behavior": "allow",
            "destination": "session",
        }]);
        let ctx = with_change_set(permission_ctx(tool_name, input), updates.clone());
        let change_set = ctx.change_set.clone().expect("a change set");
        assert_eq!(
            apply_selection(OPTION_ALLOW_WITH_UPDATES, &ctx),
            Ok(SelectionOutcome::Allow {
                updated_permissions: Some(change_set.updates.clone()),
                permanent: true,
            }),
            "{tool_name}"
        );
        assert_eq!(change_set.updates, updates.as_array().unwrap().clone());
    }
}

/// `permission-effects.test.ts` > "suppresses generated durable effects when an
/// ask rule forced the prompt": a forced ask offers Yes/No only, for a generated
/// WebFetch rule and for an MCP tool alike.
#[test]
fn g4_f10_suppresses_generated_durable_effects_for_a_forced_ask() {
    for (tool_name, input) in [
        ("WebFetch", json!({"url": "https://example.com"})),
        ("mcp__demo__deploy", json!({})),
    ] {
        let mut ctx = permission_ctx(tool_name, input);
        ctx.no_persistent_rule = true;
        let options = build_options(&ctx);
        assert_eq!(options.len(), 2, "{tool_name}");
        assert_eq!(options[0]["optionId"], json!("allow-once"));
        assert_eq!(options[1]["optionId"], json!("reject"));
    }
}

/// `permission-normalization.test.ts` > "durable permission normalization
/// safety" > "rejects strings whose untrimmed value exceeds the wire limit":
/// the tool name is measured before trimming (512 UTF-16 units).
#[test]
fn g4_f10_rejects_strings_whose_untrimmed_value_exceeds_the_wire_limit() {
    let updates = json!([{
        "type": "addRules",
        "destination": "session",
        "behavior": "allow",
        "rules": [{"toolName": format!("{}Bash", " ".repeat(512))}],
    }]);
    assert!(normalize_change_set(&updates, false).is_none());

    // The trimmed form fits, so the same name without the padding is accepted.
    let trimmed = json!([{
        "type": "addRules",
        "destination": "session",
        "behavior": "allow",
        "rules": [{"toolName": "Bash"}],
    }]);
    assert!(normalize_change_set(&trimmed, false).is_some());
}

/// `permission-presentation.test.ts` > "Claude permission suggestion
/// normalization" > "omits a durable choice for %j" (`undefined`, `[]`, `null`,
/// `"bad"`): anything that is not a non-empty array yields no durable choice.
#[test]
fn g4_f10_normalization_omits_a_durable_choice() {
    for suggestions in [Value::Null, json!([]), json!("bad"), json!({})] {
        assert!(
            normalize_change_set(&suggestions, false).is_none(),
            "{suggestions}"
        );
    }
}

/// `permission-presentation.test.ts` > "supports and snapshots %s"
/// (`addRules`/`replaceRules`/`removeRules`): the normalized updates equal the
/// provider suggestions and are an independent snapshot.
#[test]
fn g4_f10_normalization_supports_and_snapshots_rule_updates() {
    for kind in ["addRules", "replaceRules", "removeRules"] {
        let suggestions = json!([{
            "type": kind,
            "rules": [{"toolName": "Bash", "ruleContent": "npm test:*"}],
            "behavior": "allow",
            "destination": "session",
        }]);
        let change_set = normalize_change_set(&suggestions, false).expect("a valid change set");
        assert_eq!(
            change_set.updates,
            suggestions.as_array().unwrap().clone(),
            "{kind}"
        );
        assert!(
            !std::ptr::eq(&change_set.updates, suggestions.as_array().unwrap()),
            "{kind}: the snapshot is a copy"
        );
    }
}

/// `permission-presentation.test.ts` > "keeps the approved effect stable if the
/// provider mutates its suggestions later": the snapshot does not follow the
/// provider's later mutation.
#[test]
fn g4_f10_normalization_keeps_the_approved_effect_stable() {
    let mut suggestions = json!([{
        "type": "addRules",
        "rules": [{"toolName": "Bash", "ruleContent": "npm test:*"}],
        "behavior": "allow",
        "destination": "session",
    }]);
    let change_set = normalize_change_set(&suggestions, false).expect("a valid change set");
    suggestions[0] = json!({
        "type": "addRules",
        "rules": [{"toolName": "Bash", "ruleContent": "rm:*"}],
        "behavior": "allow",
        "destination": "userSettings",
    });
    assert_eq!(
        change_set.updates,
        json!([{
            "type": "addRules",
            "rules": [{"toolName": "Bash", "ruleContent": "npm test:*"}],
            "behavior": "allow",
            "destination": "session",
        }])
        .as_array()
        .unwrap()
        .clone()
    );
}

/// `permission-presentation.test.ts` > "supports setMode %s" (`default`,
/// `acceptEdits`, `bypassPermissions`, `plan`, `dontAsk`, `auto`).
#[test]
fn g4_f10_normalization_supports_set_mode() {
    for mode in [
        "default",
        "acceptEdits",
        "bypassPermissions",
        "plan",
        "dontAsk",
        "auto",
    ] {
        let suggestions = json!([{"type": "setMode", "mode": mode, "destination": "session"}]);
        let change_set = normalize_change_set(&suggestions, false).expect("a valid setMode");
        assert_eq!(
            change_set.updates,
            suggestions.as_array().unwrap().clone(),
            "{mode}"
        );
    }
}

/// `permission-presentation.test.ts` > "supports %s" (`addDirectories`,
/// `removeDirectories`).
#[test]
fn g4_f10_normalization_supports_directory_updates() {
    for kind in ["addDirectories", "removeDirectories"] {
        let suggestions = json!([{
            "type": kind,
            "directories": ["/one", "/two"],
            "destination": "localSettings",
        }]);
        let change_set =
            normalize_change_set(&suggestions, false).expect("a valid directory update");
        assert_eq!(
            change_set.updates,
            suggestions.as_array().unwrap().clone(),
            "{kind}"
        );
    }
}

/// `permission-presentation.test.ts` > "fails closed for an unknown or invalid
/// change set": five malformed shapes yield no durable choice.
#[test]
fn g4_f10_normalization_fails_closed_for_an_unknown_or_invalid_change_set() {
    for suggestions in [
        json!([{"type": "future", "destination": "session"}]),
        json!([{"type": "setMode", "mode": "future", "destination": "session"}]),
        json!([{
            "type": "addRules",
            "rules": [{"toolName": "Bash", "ruleContent": "npm test:*"}],
            "behavior": "future",
            "destination": "session",
        }]),
        json!([{
            "type": "addDirectories",
            "directories": ["/work"],
            "destination": "future",
        }]),
        json!([{
            "type": "addDirectories",
            "directories": [],
            "destination": "session",
        }]),
    ] {
        assert!(
            normalize_change_set(&suggestions, false).is_none(),
            "{suggestions}"
        );
    }
}

/// `permission-presentation.test.ts` > "suppresses every durable option for a
/// forced ask": a `matchedAskRule` change set is dropped even when valid.
#[test]
fn g4_f10_normalization_suppresses_every_durable_option_for_a_forced_ask() {
    let suggestions = json!([{
        "type": "addDirectories",
        "directories": ["/work"],
        "destination": "projectSettings",
    }]);
    assert!(normalize_change_set(&suggestions, false).is_some());
    assert!(normalize_change_set(&suggestions, true).is_none());
}

/// `permission-presentation.test.ts` > "Claude permission ACP v1 presentation" > "uses the Terminal fallback for %s when no command is available yet"
/// (`Bash`, `PowerShell`): the AIR heading and the tool call both say
/// "Terminal" and keep the (still streaming) input.
#[test]
fn g4_f10_uses_the_terminal_fallback_when_no_command_is_available() {
    for tool in ["Bash", "PowerShell"] {
        let input = json!({});
        let shape = crate::tools::tool_shape(tool, &input, None);
        assert_eq!(shape.title, "Terminal", "{tool}");
        assert_eq!(
            crate::tools::air_permission_meta(tool, &input, None, &shape.title, None, false),
            json!({"jetbrains": {"air": {"version": 1, "permission": {
                "version": 1, "title": "Terminal"
            }}}}),
            "{tool}"
        );
        let mut request = call(tool, input.clone());
        request.air = true;
        request.tool_use_id = format!("tool-{tool}");
        let tool_call = build_permission_tool_call(&request);
        assert_eq!(tool_call["title"], json!("Terminal"), "{tool}");
        assert_eq!(tool_call["rawInput"], input, "{tool}");
    }
}

/// `permission-presentation.test.ts` > "Claude permission ACP v1 presentation" > "%s command fidelity" > "preserves $label in the approval title": quoted
/// spaces and tabs, newlines after comments, surrounding whitespace and a
/// command longer than 4,000 characters all keep their exact bytes.
#[test]
fn g4_f10_command_fidelity_preserves_the_command_in_the_approval_title() {
    let long = format!("echo \"{}\"\necho last", "x".repeat(4_001));
    let commands = [
        "echo \"a  b\tc\"",
        "echo first # first command\necho second",
        " \techo first\n",
        long.as_str(),
    ];
    for tool in ["Bash", "PowerShell"] {
        for command in commands {
            let input = json!({"command": command, "description": "Run the requested command"});
            let shape = crate::tools::tool_shape(tool, &input, None);
            assert_eq!(shape.title, command, "{tool}");
            let meta =
                crate::tools::air_permission_meta(tool, &input, None, &shape.title, None, false);
            assert_eq!(
                meta,
                json!({"jetbrains": {"air": {"version": 1, "permission": {
                    "version": 1, "title": command
                }}}}),
                "{tool}"
            );
            let mut request = call(tool, input.clone());
            request.air = true;
            let tool_call = build_permission_tool_call(&request);
            assert_eq!(tool_call["title"], json!(command), "{tool}");
            assert_eq!(tool_call["rawInput"], input, "{tool}");
        }
    }
}

/// `permission-presentation.test.ts` > "keeps the WebFetch URL in structured
/// tool input": the heading names the URL and `rawInput` keeps it.
#[test]
fn g4_f10_keeps_the_webfetch_url_in_structured_tool_input() {
    let input = json!({
        "url": "https://example.com/docs",
        "prompt": "Read the API reference",
    });
    let shape = crate::tools::tool_shape("WebFetch", &input, None);
    assert_eq!(shape.title, "Fetch https://example.com/docs");
    assert_eq!(
        crate::tools::air_permission_meta("WebFetch", &input, None, &shape.title, None, false),
        json!({"jetbrains": {"air": {"version": 1, "permission": {
            "version": 1, "title": "Fetch https://example.com/docs"
        }}}})
    );
    let mut request = call("WebFetch", input.clone());
    request.air = true;
    request.tool_use_id = "tool-web-fetch".to_string();
    let tool_call = build_permission_tool_call(&request);
    assert_eq!(tool_call["title"], json!("Fetch https://example.com/docs"));
    assert_eq!(tool_call["rawInput"], input);
}

/// `permission-presentation.test.ts` > "keeps the WebSearch query out of the
/// permission description": the query names the heading, and the truncated
/// operation subtitle is never used as an explanation.
#[test]
fn g4_f10_keeps_the_websearch_query_out_of_the_permission_description() {
    let query = "Agent Client Protocol ACP specification subagents v2";
    let input = json!({"query": query});
    let shape = crate::tools::tool_shape("WebSearch", &input, None);
    let title = format!("Search \"{query}\"");
    assert_eq!(shape.title, title);
    let meta = crate::tools::air_permission_meta(
        "WebSearch",
        &input,
        Some("WebSearch"),
        &shape.title,
        None,
        false,
    );
    assert_eq!(
        meta,
        json!({"jetbrains": {"air": {"version": 1, "permission": {
            "version": 1, "title": title
        }}}})
    );
    assert!(
        meta["jetbrains"]["air"]["permission"]
            .get("description")
            .is_none(),
        "the operation subtitle is not a permission explanation"
    );
}

/// `permission-presentation.test.ts` > "reuses the %s tool-call title" (`Agent`,
/// `Task`, `ReviewArtifact`, `Workflow`, `Monitor`): the permission heading is
/// the tool call's own heading.
#[test]
fn g4_f10_reuses_the_tool_call_title() {
    for (tool, input, title) in [
        (
            "Agent",
            json!({"description": "Find the implementation"}),
            "Find the implementation",
        ),
        (
            "Task",
            json!({"description": "Review the tests"}),
            "Review the tests",
        ),
        ("ReviewArtifact", json!({}), "ReviewArtifact"),
        ("Workflow", json!({}), "Workflow"),
        ("Monitor", json!({}), "Monitor"),
    ] {
        let shape = crate::tools::tool_shape(tool, &input, None);
        assert_eq!(shape.title, title, "{tool}");
        let meta =
            crate::tools::air_permission_meta(tool, &input, Some(tool), &shape.title, None, false);
        assert_eq!(
            meta["jetbrains"]["air"]["permission"]["title"],
            json!(title),
            "{tool}"
        );
    }
}

/// `permission-presentation.test.ts` > "does not use the %s operation subtitle
/// as a permission explanation" (`it.each` over 12 tools): a tool's display
/// copy never becomes the permission `description`; only a `decisionReason`
/// does.
#[test]
fn g4_f10_does_not_use_the_operation_subtitle_as_a_permission_explanation() {
    for (tool, input, subtitle) in [
        (
            "Read",
            json!({"file_path": "/work/AGENTS.md"}),
            "Read AGENTS.md",
        ),
        ("Edit", json!({"file_path": "/work/a.ts"}), "Edit a.ts"),
        ("Write", json!({"file_path": "/work/a.ts"}), "Write a.ts"),
        (
            "NotebookEdit",
            json!({"notebook_path": "/work/a.ipynb"}),
            "Edit a.ipynb",
        ),
        ("Glob", json!({"pattern": "**/*.ts"}), "Find **/*.ts"),
        (
            "Grep",
            json!({"pattern": "permission"}),
            "Search for permission",
        ),
        ("Bash", json!({"command": "npm test"}), "Run npm test"),
        (
            "PowerShell",
            json!({"command": "Get-ChildItem"}),
            "List files",
        ),
        (
            "WebFetch",
            json!({"url": "https://example.com"}),
            "https://example.com",
        ),
        (
            "WebSearch",
            json!({"query": "ACP permissions"}),
            "ACP permissions",
        ),
        ("Skill", json!({"skill": "testing"}), "Use testing skill"),
        (
            "mcp__demo__deploy",
            json!({"target": "staging"}),
            "Deploy to staging",
        ),
    ] {
        let shape = crate::tools::tool_shape(tool, &input, None);
        let meta = crate::tools::air_permission_meta(tool, &input, None, &shape.title, None, false);
        assert!(
            meta["jetbrains"]["air"]["permission"]
                .get("description")
                .is_none(),
            "{tool} must not show {subtitle:?} as the permission description"
        );
    }
}

/// `permission-presentation.test.ts` > "Claude permission presentation for a v2
/// client" > "gives the prompt a title and description of its own, apart from
/// the tool call": the v2 title is "Ready to code?" with the reason, while the
/// tool call keeps "Approve Plan" and no AIR `_meta`.
#[test]
fn g4_f10_v2_gives_the_prompt_a_title_and_description_of_its_own() {
    let (title, description) = crate::v2::permission::v2_permission_presentation(
        "ExitPlanMode",
        &json!({"plan": "Do it."}),
        None,
        "Approve Plan",
        Some("Plan mode asks before coding."),
    );
    assert_eq!(title, "Ready to code?");
    assert_eq!(
        description.as_deref(),
        Some("Reason: Plan mode asks before coding.")
    );
    let request = call("ExitPlanMode", json!({"plan": "Do it."}));
    let tool_call = build_permission_tool_call(&request);
    assert_eq!(tool_call["title"], json!("Approve Plan"));
    assert!(
        tool_call.get("_meta").is_none(),
        "a v2 permission carries no AIR _meta"
    );
}

/// `permission-presentation.test.ts` > "Claude permission presentation for a v2
/// client" > "shows the exact preview patch, which the tool call does not
/// show": the negotiated v2 patch is the request's content.
#[test]
fn g4_f10_v2_shows_the_exact_preview_patch() {
    let preview = json!([crate::diff::v2_diff_content(
        "/work/a.ts",
        crate::diff::FileChange::Update,
        None,
    )]);
    let mut request = call(
        "Edit",
        json!({"file_path": "/work/a.ts", "old_string": "a", "new_string": "b"}),
    );
    request.preview_content = Some(preview.clone());
    let tool_call = build_permission_tool_call(&request);
    assert_eq!(tool_call["content"], preview);
    assert!(
        tool_call["content"][0].get("patch").is_none(),
        "the input diff carries no patch until the preview provides one"
    );
}

// --- AG-3 native file-change report (file-change-audit.test.ts) ---

use crate::file_change_audit::{
    agent_file_change_report_request_id, normalize_reported_paths, record_subagent_file_edits,
    reported_report, run_report, ReportJob, Reporter, Workspace, MAX_REPORT_BYTES,
};
use std::sync::Arc;

fn report_request(request_id: &str) -> Value {
    json!({"jetbrains": {"air": {
        "agentFileChangeReportRequest": {"version": 1, "requestId": request_id}
    }}})
}

/// A publisher that records every published `_meta` in order.
fn recording_publish() -> (
    crate::file_change_audit::Publish,
    std::sync::mpsc::Receiver<Value>,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    let publish = Arc::new(move |meta: Value| {
        let _ = tx.send(meta);
    });
    (publish, rx)
}

fn recorded(rx: &std::sync::mpsc::Receiver<Value>) -> Vec<Value> {
    rx.try_iter().collect()
}

fn no_log() -> Arc<dyn Fn(&str) + Send + Sync> {
    Arc::new(|_: &str| {})
}

fn report_job(
    reporter: &Reporter,
    handle: crate::file_change_audit::ReportHandle,
    request_id: &str,
    edited_paths: Vec<String>,
    publish: crate::file_change_audit::Publish,
    timeout_ms: u64,
) -> ReportJob {
    ReportJob {
        workspace: reporter.workspace().clone(),
        handle,
        request_id: request_id.to_string(),
        edited_paths,
        publish,
        log_error: no_log(),
        timeout: std::time::Duration::from_millis(timeout_ms),
    }
}

fn temp_workspace(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let base = std::env::temp_dir().join(format!("t6a2-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    (base, std::env::temp_dir())
}

/// `file-change-audit.test.ts` > "native agent file-change report" > "accepts
/// only the versioned prompt request shape" (M-94).
#[test]
fn t6a2_accepts_only_the_versioned_prompt_request_shape() {
    assert_eq!(
        agent_file_change_report_request_id(&report_request("turn:42_a-b.c")).as_deref(),
        Some("turn:42_a-b.c")
    );
    for request in [
        json!({"version": 2, "requestId": "turn-42"}),
        json!({"version": 1, "requestId": "contains spaces"}),
        json!({"version": 1, "requestId": "x".repeat(129)}),
        json!({"version": 1}),
        json!({"version": 1, "requestId": "turn-42", "extra": true}),
        json!(null),
        json!([{"version": 1, "requestId": "turn-42"}]),
        json!({"version": "1", "requestId": "turn-42"}),
    ] {
        assert_eq!(
            agent_file_change_report_request_id(&json!({
                "jetbrains": {"air": {"agentFileChangeReportRequest": request}}
            })),
            None
        );
    }
    assert_eq!(agent_file_change_report_request_id(&json!({})), None);
}

/// `file-change-audit.test.ts` > "owns request-id deduplication for the session".
#[test]
fn t6a2_owns_request_id_deduplication_for_the_session() {
    let mut reporter = Reporter::new(std::path::Path::new("/tmp"), &[]);
    let (request_id, handle) = reporter
        .request(&report_request("request-deduplicated"))
        .unwrap();
    assert_eq!(request_id, "request-deduplicated");
    assert!(!handle.is_finished());
    assert!(reporter
        .request(&report_request("request-deduplicated"))
        .is_none());
}

/// `file-change-audit.test.ts` > "uses Claude checkpoint dry-run and normalizes
/// its paths" (U-48): the dry run's paths are resolved against the canonical
/// workspace roots, deduped, and published as a `reported` report.
#[tokio::test]
async fn t6a2_uses_claude_checkpoint_dry_run_and_normalizes_its_paths() {
    let (cwd, temp_root) = temp_workspace("paths");
    let canonical_temp = std::fs::canonicalize(&temp_root).unwrap();
    let additional = temp_root.join(format!("t6a2-shared-{}", std::process::id()));
    let mut reporter = Reporter::new(&cwd, std::slice::from_ref(&additional));
    let (request_id, handle) = reporter.request(&report_request("request-1")).unwrap();
    let (publish, seen) = recording_publish();
    let preview = json!({
        "canRewind": true,
        "filesChanged": ["src/a.ts", "src/a.ts", additional.join("generated.ts").to_string_lossy()],
        "insertions": 12,
        "deletions": 3,
    });
    run_report(
        report_job(&reporter, handle, &request_id, Vec::new(), publish, 50),
        std::future::ready(Ok(preview)),
    )
    .await;
    let seen = recorded(&seen);
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0]["jetbrains"]["air"]["agentFileChangeReport"],
        json!({
            "version": 1,
            "requestId": "request-1",
            "status": "reported",
            "paths": [
                canonical_temp
                    .join(format!("t6a2-paths-{}", std::process::id()))
                    .join("src/a.ts")
                    .to_string_lossy(),
                canonical_temp
                    .join(format!("t6a2-shared-{}", std::process::id()))
                    .join("generated.ts")
                    .to_string_lossy(),
            ],
            "declaredComplete": false,
            "truncated": false,
        })
    );
    assert_eq!(seen[0]["jetbrains"]["air"]["version"], json!(1));
    let _ = std::fs::remove_dir_all(&cwd);
}

/// `file-change-audit.test.ts` > "publishes unavailable when Claude cannot
/// preview the checkpoint" (B-80 `invalidOutput`).
#[tokio::test]
async fn t6a2_publishes_unavailable_when_claude_cannot_preview_the_checkpoint() {
    let mut reporter = Reporter::new(std::path::Path::new("/tmp"), &[]);
    let (request_id, handle) = reporter
        .request(&report_request("request-invalid"))
        .unwrap();
    let (publish, seen) = recording_publish();
    run_report(
        report_job(&reporter, handle, &request_id, Vec::new(), publish, 50),
        std::future::ready(Ok(
            json!({"canRewind": false, "error": "checkpoint missing"}),
        )),
    )
    .await;
    let seen = recorded(&seen);
    assert_eq!(
        seen[0]["jetbrains"]["air"]["agentFileChangeReport"],
        json!({
            "version": 1,
            "requestId": "request-invalid",
            "status": "unavailable",
            "reason": "invalidOutput",
        })
    );
}

/// `file-change-audit.test.ts` > "times out fail-open and publishes only one
/// terminal" (B-80 `timeout`): a finish after the timeout publishes nothing.
#[tokio::test]
async fn t6a2_times_out_fail_open_and_publishes_only_one_terminal() {
    let mut reporter = Reporter::new(std::path::Path::new("/tmp"), &[]);
    let (request_id, handle) = reporter
        .request(&report_request("request-timeout"))
        .unwrap();
    let (publish, seen) = recording_publish();
    run_report(
        report_job(
            &reporter,
            handle.clone(),
            &request_id,
            Vec::new(),
            publish.clone(),
            1,
        ),
        std::future::pending::<Result<Value, String>>(),
    )
    .await;
    reporter.finish(&handle, &request_id, "cancelled", &publish);
    let seen = recorded(&seen);
    assert_eq!(
        seen[0]["jetbrains"]["air"]["agentFileChangeReport"],
        json!({
            "version": 1,
            "requestId": "request-timeout",
            "status": "unavailable",
            "reason": "timeout",
        })
    );
    assert_eq!(seen.len(), 1, "only one terminal outcome is published");
}

/// `file-change-audit.test.ts` > "lets cancellation win a race with a late
/// checkpoint response" (B-80 `cancelled`).
#[tokio::test]
async fn t6a2_lets_cancellation_win_a_race_with_a_late_checkpoint_response() {
    let mut reporter = Reporter::new(std::path::Path::new("/tmp"), &[]);
    let (request_id, handle) = reporter.request(&report_request("request-race")).unwrap();
    let (publish, seen) = recording_publish();
    let (resolve_tx, resolve_rx) = tokio::sync::oneshot::channel();
    let preview = async move {
        let value = resolve_rx.await.unwrap_or(Value::Null);
        Ok(value)
    };
    let reporting = run_report(
        report_job(
            &reporter,
            handle.clone(),
            &request_id,
            Vec::new(),
            publish.clone(),
            50,
        ),
        preview,
    );
    reporter.finish(&handle, &request_id, "cancelled", &publish);
    resolve_tx
        .send(json!({"canRewind": true, "filesChanged": ["src/late.ts"]}))
        .unwrap();
    reporting.await;
    let seen = recorded(&seen);
    assert_eq!(
        seen[0]["jetbrains"]["air"]["agentFileChangeReport"],
        json!({
            "version": 1,
            "requestId": "request-race",
            "status": "unavailable",
            "reason": "cancelled",
        })
    );
    assert_eq!(seen.len(), 1);
}

/// `file-change-audit.test.ts` > "caps path count and serialized report bytes"
/// (U-48).
#[tokio::test]
async fn t6a2_caps_path_count_and_serialized_report_bytes() {
    let (cwd, _) = temp_workspace("caps");
    let mut reporter = Reporter::new(&cwd, &[]);
    let (request_id, handle) = reporter.request(&report_request("request-caps")).unwrap();
    let (publish, seen) = recording_publish();
    let files: Vec<String> = (0..1030)
        .map(|index| format!("generated/{index}-{}.txt", "x".repeat(280)))
        .collect();
    run_report(
        report_job(&reporter, handle, &request_id, Vec::new(), publish, 50),
        std::future::ready(Ok(json!({"canRewind": true, "filesChanged": files}))),
    )
    .await;
    let seen = recorded(&seen);
    let report = &seen[0]["jetbrains"]["air"]["agentFileChangeReport"];
    assert_eq!(report["status"], json!("reported"));
    assert_eq!(report["declaredComplete"], json!(false));
    assert_eq!(report["truncated"], json!(true));
    assert!(
        serde_json::to_string(report).unwrap().len() <= MAX_REPORT_BYTES,
        "the serialized report fits the wire limit"
    );
    let _ = std::fs::remove_dir_all(&cwd);
}

/// `file-change-audit.test.ts` > "adds subagent edited paths to the checkpoint
/// paths" (T-89): the union is deduped and ordered checkpoint-first.
#[tokio::test]
async fn t6a2_adds_subagent_edited_paths_to_the_checkpoint_paths() {
    let cwd = std::env::current_dir().unwrap();
    let mut reporter = Reporter::new(&cwd, &[]);
    let (request_id, handle) = reporter.request(&report_request("request-union")).unwrap();
    let (publish, seen) = recording_publish();
    let edited = vec![
        cwd.join("GEMINI.md").to_string_lossy().to_string(),
        "src/a.ts".to_string(),
    ];
    run_report(
        report_job(&reporter, handle, &request_id, edited, publish, 50),
        std::future::ready(Ok(json!({
            "canRewind": true,
            "filesChanged": ["src/a.ts", "README.md"],
        }))),
    )
    .await;
    let seen = recorded(&seen);
    assert_eq!(
        seen[0]["jetbrains"]["air"]["agentFileChangeReport"]["paths"],
        json!([
            cwd.join("src/a.ts").to_string_lossy(),
            cwd.join("README.md").to_string_lossy(),
            cwd.join("GEMINI.md").to_string_lossy(),
        ])
    );
}

/// `file-change-audit.test.ts` > "reports subagent edited paths when the
/// checkpoint cannot be previewed", "...times out", "...throws" and "stays
/// unavailable when there are no subagent edited paths" (B-80).
#[tokio::test]
async fn t6a2_reports_subagent_edited_paths_when_the_checkpoint_cannot_be_previewed() {
    let cwd = std::env::current_dir().unwrap();
    let edited = vec!["GEMINI.md".to_string()];
    for preview in [
        Ok(json!({"canRewind": false, "error": "checkpoint missing"})),
        Err("boom".to_string()),
    ] {
        let mut reporter = Reporter::new(&cwd, &[]);
        let (request_id, handle) = reporter
            .request(&report_request("request-fallback"))
            .unwrap();
        let (publish, seen) = recording_publish();
        run_report(
            report_job(&reporter, handle, &request_id, edited.clone(), publish, 50),
            std::future::ready(preview),
        )
        .await;
        let seen = recorded(&seen);
        assert_eq!(
            seen[0]["jetbrains"]["air"]["agentFileChangeReport"],
            json!({
                "version": 1,
                "requestId": "request-fallback",
                "status": "reported",
                "paths": [cwd.join("GEMINI.md").to_string_lossy()],
                "declaredComplete": false,
                "truncated": false,
            })
        );
    }

    // A timeout falls back to the subagent edits too.
    let mut reporter = Reporter::new(&cwd, &[]);
    let (request_id, handle) = reporter
        .request(&report_request("request-timeout-fallback"))
        .unwrap();
    let (publish, seen) = recording_publish();
    run_report(
        report_job(&reporter, handle, &request_id, edited.clone(), publish, 1),
        std::future::pending::<Result<Value, String>>(),
    )
    .await;
    assert_eq!(
        recorded(&seen)[0]["jetbrains"]["air"]["agentFileChangeReport"]["status"],
        json!("reported")
    );

    // Without subagent edits an unusable checkpoint stays unavailable.
    let mut reporter = Reporter::new(&cwd, &[]);
    let (request_id, handle) = reporter.request(&report_request("request-empty")).unwrap();
    let (publish, seen) = recording_publish();
    run_report(
        report_job(&reporter, handle, &request_id, Vec::new(), publish, 50),
        std::future::ready(Ok(
            json!({"canRewind": false, "error": "checkpoint missing"}),
        )),
    )
    .await;
    assert_eq!(
        recorded(&seen)[0]["jetbrains"]["air"]["agentFileChangeReport"],
        json!({
            "version": 1,
            "requestId": "request-empty",
            "status": "unavailable",
            "reason": "invalidOutput",
        })
    );
}

/// `file-change-audit.test.ts` > "recordSubagentFileEdits" > "collects the
/// paths of subagent file-tool edits" (T-89).
#[test]
fn t6a2_collects_the_paths_of_subagent_file_tool_edits() {
    let mut tool_uses = std::collections::HashMap::new();
    tool_uses.insert(
        "edit".to_string(),
        ("Edit".to_string(), json!({"file_path": "/work/a.ts"})),
    );
    tool_uses.insert(
        "write".to_string(),
        ("Write".to_string(), json!({"file_path": "/work/b.ts"})),
    );
    tool_uses.insert(
        "notebook".to_string(),
        (
            "NotebookEdit".to_string(),
            json!({"notebook_path": "/work/n.ipynb"}),
        ),
    );
    tool_uses.insert(
        "read".to_string(),
        ("Read".to_string(), json!({"file_path": "/work/c.ts"})),
    );
    let content = json!([
        {"type": "tool_result", "tool_use_id": "edit", "content": "ok"},
        {"type": "tool_result", "tool_use_id": "write", "content": "ok"},
        {"type": "tool_result", "tool_use_id": "notebook", "content": "ok"},
        {"type": "tool_result", "tool_use_id": "read", "content": "ok"},
    ]);
    let mut edited = std::collections::BTreeSet::new();
    record_subagent_file_edits(Some("toolu_agent"), &content, &tool_uses, &mut edited);
    assert_eq!(
        edited.into_iter().collect::<Vec<_>>(),
        vec!["/work/a.ts", "/work/b.ts", "/work/n.ipynb"]
    );
}

/// `file-change-audit.test.ts` > "recordSubagentFileEdits" > "skips error
/// results" and "skips root-agent messages".
#[test]
fn t6a2_skips_error_results_and_root_agent_messages() {
    let mut tool_uses = std::collections::HashMap::new();
    tool_uses.insert(
        "edit".to_string(),
        ("Edit".to_string(), json!({"file_path": "/work/a.ts"})),
    );
    let mut edited = std::collections::BTreeSet::new();
    record_subagent_file_edits(
        Some("toolu_agent"),
        &json!([{"type": "tool_result", "tool_use_id": "edit", "content": "failed", "is_error": true}]),
        &tool_uses,
        &mut edited,
    );
    assert!(edited.is_empty());
    record_subagent_file_edits(
        None,
        &json!([{"type": "tool_result", "tool_use_id": "edit", "content": "ok"}]),
        &tool_uses,
        &mut edited,
    );
    assert!(edited.is_empty());
}

/// `file-change-audit.test.ts` > "native agent file-change report" > "uses
/// Claude checkpoint dry-run and normalizes its paths": the path guards (U-48)
/// reject out-of-root, control-character and over-long paths and set
/// `truncated`.
#[test]
fn t6a2_path_normalisation_rejects_out_of_root_and_control_characters() {
    let (cwd, _) = temp_workspace("guards");
    let workspace = Workspace::new(&cwd, &[]);
    let canonical = std::fs::canonicalize(&cwd).unwrap();
    let (paths, truncated) = normalize_reported_paths(
        &[
            "inside.txt".to_string(),
            "../outside.txt".to_string(),
            "bad\u{1}name".to_string(),
            format!("{}.txt", "x".repeat(4097)),
            cwd.to_string_lossy().to_string(),
        ],
        &workspace,
    );
    assert_eq!(
        paths,
        vec![canonical.join("inside.txt").to_string_lossy().to_string()]
    );
    assert!(truncated);
    let _ = std::fs::remove_dir_all(&cwd);
}

/// `file-change-audit.test.ts` > "caps path count and serialized report bytes":
/// the fitting loop drops paths until the serialized report fits.
#[test]
fn t6a2_fitting_drops_paths_until_the_report_fits() {
    let paths: Vec<String> = (0..10)
        .map(|index| format!("/work/{index}-{}.txt", "x".repeat(100)))
        .collect();
    let fitted = reported_report("r", paths.clone(), false);
    assert_eq!(fitted["paths"].as_array().unwrap().len(), 10);
    let huge: Vec<String> = (0..1100)
        .map(|index| format!("/work/{index}-{}.txt", "x".repeat(250)))
        .collect();
    let fitted = reported_report("r", huge, false);
    assert_eq!(fitted["truncated"], json!(true));
    assert!(
        serde_json::to_string(&fitted).unwrap().len() <= MAX_REPORT_BYTES,
        "the fitted report fits the wire limit"
    );
}

/// `acp-agent.test.ts` > "advertises canonical AIR sessionFailure support during
/// initialize" and "...even before client negotiation" (C-12): the AIR client
/// gets the eight-entry capability list under `_meta.jetbrains.air`, plus the
/// goal payload, independent of what it declared.
#[test]
fn t6a2_advertises_the_air_capability_list_during_initialize() {
    let air = crate::air::AirClient::from_client_capabilities(&json!({
        "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["sessionFailure"]}}}
    }))
    .expect("an AIR client");
    let response = crate::agent::initialize_response(
        Some(&air),
        &crate::client_caps::ClientCapabilities::default(),
        &crate::env_snapshot::EnvSnapshot::empty(),
        "/bin/adapter",
        &[],
        &[],
        false,
    );
    assert_eq!(
        response.pointer("/_meta/jetbrains/air"),
        Some(&json!({
            "version": 1,
            "capabilities": [
                "sessionFailure",
                "agentFileChangeReport",
                "nativeSubagentSessions",
                "asyncTasks",
                "customInstructions",
                "recommendedValue",
                "diffPatch",
                "planFile",
            ],
            "goal": { "version": 1, "controlMethod": "_session/goal", "actions": ["set", "clear"] },
        }))
    );
    // A client that is not AIR gets no `jetbrains` key.
    let plain = crate::agent::initialize_response(
        None,
        &crate::client_caps::ClientCapabilities::default(),
        &crate::env_snapshot::EnvSnapshot::empty(),
        "/bin/adapter",
        &[],
        &[],
        false,
    );
    assert!(plain
        .get("_meta")
        .and_then(|m| m.get("jetbrains"))
        .is_none());
}

/// M8 (`review-T7`): a cancelled or failed report's job is marked done, so the
/// next prompt's prune drops it from `collecting` instead of leaking one entry
/// per cancelled/failed reported turn (`note_prompt`, `session.rs`). Node
/// finishes the report and forgets the turn's request (`settleActive`,
/// `acp-agent.ts:5491-5492`; `file-change-audit.ts:235-238`).
#[test]
fn m8_a_finished_cancelled_report_marks_the_job_done_so_collecting_prunes() {
    use std::sync::atomic::Ordering;
    use tokio::sync::mpsc;

    let (tx, _rx) = mpsc::unbounded_channel();
    let mut reports = crate::session::FileChangeReports::new(
        true,
        &std::env::temp_dir(),
        &[],
        None,
        &tx,
        "session-m8",
        None,
    );
    // `note_prompt` receives the prompt's `_meta` value, not the params wrapper.
    let meta = json!({"jetbrains": {"air": {
        "agentFileChangeReportRequest": {"version": 1, "requestId": "m8-request"}
    }}});
    reports.note_prompt("p1", &meta);
    let job = reports.take("p1").expect("a report job");
    assert_eq!(reports.collecting_len(), 1);
    reports.finish_job(&job, "cancelled");
    assert!(
        job.done.load(Ordering::SeqCst),
        "a finished report job is marked done"
    );
    // The next prompt prunes done jobs before registering its own.
    reports.note_prompt("p2", &json!({}));
    assert_eq!(
        reports.collecting_len(),
        0,
        "the cancelled report's entry is pruned"
    );
}

/// `file-change-audit-integration.test.ts` > "publishes providerError without
/// attempting checkpoint inspection": a turn the SDK failed ends its report as
/// `unavailable`/`providerError` without previewing the checkpoint (the finish
/// path never calls the reporter's `run_report`).
#[test]
fn t9b_provider_error_report_is_unavailable_without_a_preview() {
    use std::sync::atomic::Ordering;
    use tokio::sync::mpsc;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut reports = crate::session::FileChangeReports::new(
        true,
        &std::env::temp_dir(),
        &[],
        None,
        &tx,
        "session-t9b",
        None,
    );
    let meta = json!({"jetbrains": {"air": {
        "agentFileChangeReportRequest": {"version": 1, "requestId": "t9b-request"}
    }}});
    reports.note_prompt("p1", &meta);
    let job = reports.take("p1").expect("a report job");
    reports.finish_job(&job, "providerError");
    assert!(job.done.load(Ordering::SeqCst));
    let outbound = rx.try_recv().expect("the unavailable report");
    let crate::session::SessionOutbound::RawUpdate(value) = outbound else {
        panic!("expected a raw update");
    };
    assert_eq!(
        value["update"]["_meta"]["jetbrains"]["air"]["agentFileChangeReport"],
        json!({
            "version": 1,
            "requestId": "t9b-request",
            "status": "unavailable",
            "reason": "providerError",
        }),
        "{value}"
    );
    assert!(rx.try_recv().is_err(), "one terminal only");
}

// --- G4 / T-PORT: permission request cases from acp-agent.test.ts ---

/// `acp-agent.test.ts` > "permission request cancellation" > "omits the durable
/// action when Claude supplies no permission update": a request with no
/// `permission_suggestions` offers Yes/No only, and AIR still gets its title.
#[test]
fn g4_tport_omits_the_durable_action_without_a_permission_update() {
    let input = json!({"command": "ls"});
    let ctx = permission_ctx("Bash", input.clone());
    assert_eq!(
        build_options(&ctx),
        vec![
            json!({"optionId": "allow-once", "name": "Yes", "kind": "allow_once"}),
            json!({"optionId": "reject", "name": "No", "kind": "reject_once"}),
        ]
    );
    let shape = crate::tools::tool_shape("Bash", &input, None);
    assert_eq!(
        crate::tools::air_permission_meta("Bash", &input, None, &shape.title, None, false),
        json!({"jetbrains": {"air": {"version": 1, "permission": {
            "version": 1, "title": "ls"
        }}}})
    );
}

/// `acp-agent.test.ts` > "maps Claude presentation fields without reconstructing
/// provider text or changing input": the AIR meta carries the heading and the
/// `Reason:` description, the request repeats nothing the tool call holds, and
/// an allow returns the provider input unchanged.
#[test]
fn g4_tport_maps_presentation_fields_without_reconstructing_text() {
    let input = json!({"file_path": "/outside/a.ts"});
    let mut request = call("Read", input.clone());
    request.tool_use_id = "tool-1".to_string();
    request.air = true;
    request.title = Some("Read /outside/a.ts".to_string());
    request.blocked_path = Some("/outside/a.ts".to_string());
    let tool_call = build_permission_tool_call(&request);
    assert_eq!(
        tool_call,
        json!({
            "toolCallId": "tool-1",
            "title": "Read /outside/a.ts",
            "rawInput": input,
        }),
        "the request repeats nothing the tool call has; the blocked path is already its own location"
    );

    let meta = crate::tools::air_permission_meta(
        "Read",
        &input,
        Some("Read file"),
        "Read /outside/a.ts",
        Some("Needed to inspect the dependency."),
        false,
    );
    assert_eq!(
        meta,
        json!({"jetbrains": {"air": {"version": 1, "permission": {
            "version": 1,
            "title": "Read /outside/a.ts",
            "description": "Reason: Needed to inspect the dependency.",
        }}}})
    );

    let ctx = permission_ctx("Read", input.clone());
    let response = crate::permission::permission_control_response(
        &json!({"outcome": {"outcome": "selected", "optionId": "allow-once"}}),
        &ctx,
        "req-1",
        "tool-1",
        &input,
    );
    assert_eq!(
        response["response"]["response"]["updatedInput"], input,
        "an allow keeps the provider input byte-for-byte"
    );
}

/// `acp-agent.test.ts` > "applies the exact representable provider updates
/// described by the selected option": the durable label names both the
/// directory and the command, and the allow returns the provider updates
/// verbatim.
#[test]
fn g4_tport_applies_the_exact_representable_provider_updates() {
    let updates = json!([
        {
            "type": "addRules",
            "rules": [{"toolName": "Bash", "ruleContent": "npm test:*"}],
            "behavior": "allow",
            "destination": "session",
        },
        {
            "type": "addDirectories",
            "directories": ["/tmp/work"],
            "destination": "session",
        },
    ]);
    let ctx = with_change_set(
        permission_ctx("Bash", json!({"command": "npm test"})),
        updates.clone(),
    );
    let options = build_options(&ctx);
    let durable = options
        .iter()
        .find(|option| option["optionId"] == json!("allow-with-updates"))
        .expect("the durable option");
    assert_eq!(durable["kind"], json!("allow_always"));
    assert_eq!(
        durable["name"],
        json!("Yes, and allow access to work/ and npm test commands")
    );
    assert_eq!(
        apply_selection(OPTION_ALLOW_WITH_UPDATES, &ctx),
        Ok(SelectionOutcome::Allow {
            updated_permissions: Some(updates.as_array().unwrap().clone()),
            permanent: true,
        })
    );
}

/// `acp-agent.test.ts` > "keeps provider permission effects inside the adapter":
/// no offered option exposes the provider's update bundle in `_meta`.
#[test]
fn g4_tport_keeps_provider_permission_effects_inside_the_adapter() {
    let updates = json!([
        {
            "type": "addRules",
            "rules": [{"toolName": "Bash", "ruleContent": "npm test:*"}],
            "behavior": "allow",
            "destination": "session",
        },
        {
            "type": "addDirectories",
            "directories": ["/tmp/work"],
            "destination": "session",
        },
    ]);
    let ctx = with_change_set(
        permission_ctx("Bash", json!({"command": "npm test"})),
        updates,
    );
    for option in build_options(&ctx) {
        assert!(
            option.get("_meta").is_none(),
            "the provider bundle stays inside the adapter: {option}"
        );
    }
}

/// `acp-agent.test.ts` > "does not expose an update bundle that Claude's Bash
/// dialog cannot label": an unrepresentable shell change set offers Yes/No only.
#[test]
fn g4_tport_does_not_expose_an_unlabelable_update_bundle() {
    let suggestions = json!([
        {
            "type": "replaceRules",
            "rules": [{"toolName": "Bash", "ruleContent": "rm generated.txt"}],
            "behavior": "deny",
            "destination": "projectSettings",
        },
        {
            "type": "removeDirectories",
            "directories": ["/tmp/old"],
            "destination": "localSettings",
        },
        {"type": "setMode", "mode": "default", "destination": "session"},
    ]);
    let ctx = with_change_set(
        permission_ctx("Bash", json!({"command": "rm generated.txt"})),
        suggestions,
    );
    let options = build_options(&ctx);
    let ids: Vec<&str> = options
        .iter()
        .filter_map(|option| option["optionId"].as_str())
        .collect();
    assert_eq!(ids, vec!["allow-once", "reject"]);
}
