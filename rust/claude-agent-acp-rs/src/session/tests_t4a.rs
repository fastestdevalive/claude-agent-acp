//! Session-level tests added by Turn 4 (lane A, T4A): the follow-up prompt
//! the CLI predicts at turn end (F24: U-08, N-21).

use super::tests_t2a::Rig;
use super::*;

fn air_rig() -> Rig {
    let mut rig = Rig::new();
    let caps = json!({"_meta": {"jetbrains": {"air": {"version": 1, "capabilities": []}}}});
    rig.ext.air = crate::air::AirClient::from_client_capabilities(&caps);
    rig
}

fn success_result() -> Value {
    json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})
}

fn suggestion_frame(text: &str) -> Value {
    json!({"type":"prompt_suggestion","suggestion":text,"uuid":"uuid","session_id":"s"})
}

/// Every `_meta.promptSuggestions` payload among the drained updates.
fn suggestions(updates: &[Value]) -> Vec<Value> {
    updates
        .iter()
        .filter(|update| update["sessionUpdate"] == "session_info_update")
        .filter_map(|update| update["_meta"].get("promptSuggestions").cloned())
        .collect()
}

/// U-08/N-21 — the frame goes through the real routing (`dispatch_stream`),
/// which used to drop `prompt_suggestion` as an unknown type, and is published
/// to an AIR client when the result opened the window.
#[tokio::test]
async fn t4a_prompt_suggestion_frame_is_routed_to_the_session() {
    let mut rig = air_rig();
    rig.ext.t2a.prompt_suggestion_expected = true;
    let (control_tx, _control_rx) = mpsc::unbounded_channel();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        dispatch_stream(
            StreamMsg::new(suggestion_frame("Run the tests")),
            &control_tx,
            &mut rig.machine,
            &mut rig.map_state,
            &mut rig.live_background,
            &mut rig.pending,
            &mut rig.emitted_assistant_text,
            &mut rig.assistant_had_error,
            &rig.tx,
            "s",
            &rig.high_water,
            &mut rig.tasks,
            &mut rig.ext,
        ),
    )
    .await
    .expect("dispatch_stream returns");
    assert_eq!(
        suggestions(&rig.updates()),
        vec![json!({"suggestions":[{"text":"Run the tests"}]})]
    );
}

/// U-08 — a result opens the prediction window; the arriving suggestion closes
/// it and is published once. A prediction after a turn that predicted nothing
/// is not re-sent (dedupe), and a whitespace-only prediction publishes nothing.
#[test]
fn t4a_suggestion_window_opens_at_result_and_publishes_once() {
    let mut rig = air_rig();
    rig.echo("p1");
    rig.feed(success_result());
    assert!(rig.ext.t2a.prompt_suggestion_expected, "result opens it");
    rig.feed(suggestion_frame("Run the tests"));
    assert!(
        !rig.ext.t2a.prompt_suggestion_expected,
        "suggestion closes it"
    );
    rig.updates();

    // The next result re-opens the window; the same prediction is not re-sent.
    rig.echo("p2");
    rig.feed(success_result());
    rig.feed(suggestion_frame("Run the tests"));
    assert!(suggestions(&rig.updates()).is_empty());

    // A whitespace prediction says nothing: on a fresh session it never
    // reaches the client.
    let mut fresh = air_rig();
    fresh.ext.t2a.prompt_suggestion_expected = true;
    fresh.feed(suggestion_frame("   "));
    assert!(suggestions(&fresh.updates()).is_empty());
    assert!(fresh.ext.t2a.last_published_prompt_suggestion.is_none());
}

/// N-21 — the expected gate: a suggestion the user outran (no result opened
/// the window, or the next prompt closed it) is dropped.
#[test]
fn t4a_suggestion_is_dropped_when_the_gate_is_off() {
    let mut rig = air_rig();
    rig.feed(suggestion_frame("Run the tests"));
    assert!(suggestions(&rig.updates()).is_empty());
    assert!(rig.ext.t2a.last_published_prompt_suggestion.is_none());

    // A prompt arriving before the consumer reads the prediction closes the
    // window (`acp-agent.ts:3893-3898`), so the prediction is dropped.
    rig.ext.t2a.prompt_suggestion_expected = false;
    rig.feed(suggestion_frame("Run the tests"));
    assert!(suggestions(&rig.updates()).is_empty());
}

/// U-08 — only AIR renders the follow-up prompt: a plain client gets no update
/// and the book remembers nothing.
#[test]
fn t4a_suggestion_is_not_published_to_a_plain_client() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.feed(success_result());
    rig.feed(suggestion_frame("Run the tests"));
    assert!(suggestions(&rig.updates()).is_empty());
    assert!(rig.ext.t2a.last_published_prompt_suggestion.is_none());
}

/// N-21 — `conversation_reset` publishes the clear (`publishPromptSuggestion`
/// with `null`) because the prediction came from the transcript the reset
/// dropped.
#[test]
fn t4a_conversation_reset_clears_the_prediction() {
    let mut rig = air_rig();
    rig.ext.t2a.prompt_suggestion_expected = true;
    rig.feed(suggestion_frame("Run the tests"));
    assert_eq!(
        suggestions(&rig.updates()),
        vec![json!({"suggestions":[{"text":"Run the tests"}]})]
    );
    rig.feed(json!({"type":"conversation_reset","uuid":"uuid"}));
    assert_eq!(suggestions(&rig.updates()), vec![json!({"suggestions":[]})]);
    assert!(rig.ext.t2a.last_published_prompt_suggestion.is_none());
}

/// U-08 — the prompt path clears what the previous turn published, and a clear
/// before the first prediction goes nowhere (`(undefined ?? null) === null`).
#[test]
fn t4a_clear_publishes_only_when_something_was_published() {
    let mut rig = air_rig();
    publish_prompt_suggestion(rig.ext.air.is_some(), &mut rig.ext.t2a, None, &rig.tx, "s");
    assert!(suggestions(&rig.updates()).is_empty(), "nothing to clear");

    publish_prompt_suggestion(
        rig.ext.air.is_some(),
        &mut rig.ext.t2a,
        Some("Run the tests".to_string()),
        &rig.tx,
        "s",
    );
    publish_prompt_suggestion(rig.ext.air.is_some(), &mut rig.ext.t2a, None, &rig.tx, "s");
    assert_eq!(
        suggestions(&rig.updates()),
        vec![
            json!({"suggestions":[{"text":"Run the tests"}]}),
            json!({"suggestions":[]}),
        ]
    );
    // A second clear is already true on the client and is not re-sent.
    publish_prompt_suggestion(rig.ext.air.is_some(), &mut rig.ext.t2a, None, &rig.tx, "s");
    assert!(suggestions(&rig.updates()).is_empty());
}

// ---------------------------------------------------------------- S-01/S-05/S-06/S-07/N-10

/// S-01 — the structured replacement is published once; a mirror of the same
/// output shape is suppressed, and a later distinct diagnostic still goes out
/// (`takeLocalCommandMarkdown` + `local_command_output`).
#[test]
fn t4a_local_command_output_replaces_and_dedupes() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.ext.t2a.usage_turns.insert("p1");
    {
        let turn = rig.ext.t2a.usage_turns.get_mut("p1").unwrap();
        turn.run_started = true;
        turn.prepared = Some(Some("## Usage".to_string()));
    }
    rig.feed(json!({"type":"system","subtype":"local_command_output","content":"raw"}));
    assert_eq!(
        rig.updates(),
        vec![json!({"sessionUpdate":"agent_message_chunk",
            "content":{"type":"text","text":"## Usage"}})]
    );
    // The exact mirror is not published twice.
    rig.feed(json!({"type":"system","subtype":"local_command_output","content":"raw"}));
    assert!(rig.updates().is_empty());
    // A later, distinct frame follows the normal path.
    rig.feed(
        json!({"type":"system","subtype":"local_command_output","content":"Interrupted by user"}),
    );
    let updates = rig.updates();
    assert_eq!(texts(&updates), vec!["Interrupted by user"]);
}

/// S-01 — a cancelled `/usage` turn publishes nothing from its local-command
/// lane: the producer is aborted and the output shape is dropped.
#[test]
fn t4a_cancelled_usage_turn_publishes_nothing() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.ext.t2a.usage_turns.insert("p1");
    {
        let turn = rig.ext.t2a.usage_turns.get_mut("p1").unwrap();
        turn.run_started = true;
        turn.prepared = Some(Some("## Usage".to_string()));
    }
    rig.machine.cancel();
    rig.ext.t2a.usage_turns.abort_all();
    rig.feed(json!({"type":"system","subtype":"local_command_output","content":"raw"}));
    assert!(rig.updates().is_empty());
}

/// S-05/S-01 — a result-only `/usage` turn (the CLI never streamed the output
/// shape) is replaced at the result, and the result text is not forwarded
/// twice.
#[test]
fn t4a_result_only_usage_turn_is_replaced() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.ext.t2a.usage_turns.insert("p1");
    {
        let turn = rig.ext.t2a.usage_turns.get_mut("p1").unwrap();
        turn.run_started = true;
        turn.prepared = Some(Some("## Usage".to_string()));
    }
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"raw usage text","uuid":"r1",
        "usage":{"input_tokens":0,"output_tokens":0}}));
    let updates = rig.updates();
    assert_eq!(texts(&updates), vec!["## Usage"]);
}

/// S-05 — `deliveredCompactionOutput` suppresses the #453 result fallback: a
/// compaction delivered this turn's output, so its result text is not the
/// answer.
#[test]
fn t4a_compaction_output_suppresses_the_result_fallback() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.ext.t2a.compaction_output_delivered = true;
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"compaction result text","uuid":"r1",
        "usage":{"input_tokens":0,"output_tokens":0}}));
    let updates = rig.updates();
    assert!(
        texts(&updates)
            .iter()
            .all(|text| text != "compaction result text"),
        "the result text is not forwarded when a compaction output was delivered: {updates:?}"
    );
}

/// S-06 — a live user frame carrying prose alongside `<local-command-stdout>`
/// markers is stripped and rendered as one user chunk; the marker text and the
/// CLI's own output body never reach the client.
#[test]
fn t4a_local_command_stdout_in_a_user_frame_is_rendered_once() {
    let mut rig = Rig::new();
    rig.feed(json!({"type":"user","uuid":"u1","message":{"role":"user",
        "content":"hello <local-command-stdout>raw output</local-command-stdout>"}}));
    let updates = rig.updates();
    assert_eq!(texts(&updates), vec!["hello "]);
    assert_eq!(updates[0]["sessionUpdate"], "user_message_chunk");
}

/// S-06 — a marker-only stdout frame leaves nothing to publish: it is logged
/// and never rendered (`stripLocalCommandMetadata` → null).
#[test]
fn t4a_marker_only_stdout_is_logged_not_rendered() {
    let mut rig = Rig::new();
    let dir = std::env::temp_dir().join(format!("acp-t4a-log2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("log dir");
    let log = dir.join("agent.log");
    rig.ext.logger = Some(std::sync::Arc::new(crate::logger::Logger::to_file(&log)));
    rig.feed(json!({"type":"user","uuid":"u1","message":{"role":"user",
        "content":"<local-command-stdout>raw output</local-command-stdout>"}}));
    assert!(rig.updates().is_empty());
    let text = std::fs::read_to_string(&log).expect("log written");
    assert!(text.contains("local-command-stdout"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// S-07 — a live user frame carrying `<local-command-stderr>` is logged as an
/// error and never published.
#[test]
fn t4a_local_command_stderr_is_logged_not_published() {
    let mut rig = Rig::new();
    let dir = std::env::temp_dir().join(format!("acp-t4a-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("log dir");
    let log = dir.join("agent.log");
    rig.ext.logger = Some(std::sync::Arc::new(crate::logger::Logger::to_file(&log)));
    rig.feed(json!({"type":"user","uuid":"u1","message":{"role":"user",
        "content":"<local-command-stderr>boom</local-command-stderr>"}}));
    assert!(rig.updates().is_empty());
    let text = std::fs::read_to_string(&log).expect("log written");
    assert!(text.contains("boom"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Text of every `agent_message_chunk` / `user_message_chunk` in `updates`.
fn texts(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .filter(|update| {
            matches!(
                update["sessionUpdate"].as_str(),
                Some("agent_message_chunk" | "user_message_chunk")
            )
        })
        .filter_map(|update| update["content"]["text"].as_str().map(str::to_string))
        .collect()
}

// ---------------------------------------------------------------- S-22/S-31

/// S-22 — an AIR client gets `_meta.jetbrains.air.{kind:"skill", skillPath}` on
/// each skill command whose `SKILL.md` exists; a built-in command and an MCP
/// prompt carry none. A command whose directory declares another name is
/// resolved through the declared-name scan.
#[test]
fn t4a_air_skill_commands_carry_kind_and_path() {
    let root = std::env::temp_dir().join(format!("acp-t4a-skills-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cwd = root.join("cwd");
    let write = |path: std::path::PathBuf, text: &str| {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    };
    write(
        cwd.join(".claude/skills/my-skill/SKILL.md"),
        "---\nname: my-skill\n---\nbody",
    );
    write(
        cwd.join(".claude/skills/other-dir/SKILL.md"),
        "---\nname: declared-name\n---\n",
    );
    write(
        cwd.join(".agents/skills/agents-skill/SKILL.md"),
        "---\nname: agents-skill\n---\n",
    );

    let commands = json!([
        {"name": "my-skill", "description": "Mine", "builtin": false},
        {"name": "declared-name", "description": "Elsewhere", "builtin": false},
        {"name": "agents-skill", "description": "Agents", "builtin": false},
        {"name": "compact", "description": "Built in", "builtin": true},
        {"name": "missing-skill", "description": "No file", "builtin": false},
        {"name": "server:prompt (MCP)", "description": "An MCP prompt"},
    ]);
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("home");
    let update =
        crate::map::map_commands_changed_with_skills(&commands, &[], Some(&cwd), Some(&home));
    let wire = crate::map::available_commands_json(&update);
    let commands = wire["availableCommands"].as_array().expect("array");
    let find = |name: &str| {
        commands
            .iter()
            .find(|command| command["name"] == name)
            .unwrap_or_else(|| panic!("{name} listed: {wire}"))
    };

    let mine = find("my-skill");
    assert_eq!(
        mine["_meta"]["jetbrains"]["air"],
        json!({
            "version": 1,
            "kind": "skill",
            "skillPath": cwd.join(".claude/skills/my-skill/SKILL.md").to_string_lossy(),
        }),
        "{mine}"
    );
    // The declared-name scan resolves the command to the directory that
    // declares it, not the directory that carries the name.
    let declared = find("declared-name");
    assert_eq!(
        declared["_meta"]["jetbrains"]["air"]["skillPath"],
        json!(cwd
            .join(".claude/skills/other-dir/SKILL.md")
            .to_string_lossy()),
        "{declared}"
    );
    let agents = find("agents-skill");
    assert_eq!(
        agents["_meta"]["jetbrains"]["air"]["skillPath"],
        json!(cwd
            .join(".agents/skills/agents-skill/SKILL.md")
            .to_string_lossy()),
        "{agents}"
    );
    assert!(
        find("compact").get("_meta").is_none(),
        "built-in has no meta"
    );
    assert!(find("missing-skill").get("_meta").is_none());
    // S-31: an MCP prompt is renamed and carries no skill meta.
    let mcp = find("mcp:server:prompt");
    assert!(mcp.get("_meta").is_none(), "{mcp}");

    // A non-AIR client (no skill cwd) gets no skill meta at all.
    let plain = crate::map::available_commands_json(&crate::map::map_commands_changed_with_skills(
        &commands_json(),
        &[],
        None,
        Some(&home),
    ));
    assert!(
        plain["availableCommands"]
            .as_array()
            .expect("array")
            .iter()
            .all(|command| command.get("_meta").is_none()),
        "{plain}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn commands_json() -> Value {
    json!([{"name": "my-skill", "description": "Mine", "builtin": false}])
}
