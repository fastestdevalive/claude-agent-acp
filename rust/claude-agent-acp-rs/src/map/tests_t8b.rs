//! T8B — TEST-MAP classification for the upstream `streamEventToAcpNotifications`
//! partial-input lane, the `toAcpNotifications` messageId/thinking lanes and
//! `messageIdForGrouping` (`src/tests/acp-agent.test.ts`, v0.94.0).
//!
//! The upstream cases drive the Node adapter's pure mapping helpers with an AIR
//! client (`AIR_CLIENT_CAPABILITIES`), so the tests here configure the map as
//! AIR where the upstream expectation depends on it (no `rawInput` while the
//! input streams, `_meta.jetbrains.air.commandTitle`).

use super::*;
use serde_json::json;

fn air_state() -> MapState {
    let mut state = MapState::default();
    state.configure_air(std::path::PathBuf::from("/Users/test/project"));
    state
}

fn stream_frame(event: Value) -> Value {
    json!({
        "type": "stream_event",
        "parent_tool_use_id": null,
        "uuid": "stream-uuid",
        "session_id": "test-session",
        "event": event
    })
}

fn send(state: &mut MapState, event: Value) -> Vec<Value> {
    map_stream_event(&stream_frame(event), state)
        .into_iter()
        .map(|update| serde_json::to_value(update).expect("serialize update"))
        .collect()
}

fn start_tool(id: &str, name: &str, block_type: &str) -> Value {
    json!({
        "type": "content_block_start",
        "index": 0,
        "content_block": {"type": block_type, "id": id, "name": name, "input": {}}
    })
}

fn input_delta(partial_json: &str) -> Value {
    json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "input_json_delta", "partial_json": partial_json}
    })
}

// ------------------------------------------------------------------ partial input

/// upstream: "refines a tool call as soon as a streamed input field is complete"
#[test]
fn t8b_partial_input_refines_a_read_as_soon_as_its_path_completes() {
    let mut state = air_state();
    let mut started = send(&mut state, start_tool("toolu_read", "Read", "tool_use"));
    assert_eq!(started.len(), 1);
    materialize_tool_call_defaults(&mut started[0]);
    assert_eq!(started[0]["sessionUpdate"], "tool_call");
    assert_eq!(started[0]["toolCallId"], "toolu_read");
    assert_eq!(started[0]["title"], "Read File");
    assert_eq!(started[0]["locations"], json!([]));

    let partial = send(&mut state, input_delta("{\"file_"));
    assert!(partial.is_empty(), "{partial:?}");

    let path_available = send(
        &mut state,
        input_delta("path\":\"/Users/test/project/src/ZodiacList.tsx\",\"offset\":"),
    );
    assert_eq!(path_available.len(), 1, "{path_available:?}");
    assert_eq!(path_available[0]["sessionUpdate"], "tool_call_update");
    assert_eq!(path_available[0]["toolCallId"], "toolu_read");
    assert_eq!(path_available[0]["title"], "Read src/ZodiacList.tsx");
    assert_eq!(
        path_available[0]["locations"],
        json!([{"path": "/Users/test/project/src/ZodiacList.tsx", "line": 1}])
    );
    assert!(
        path_available[0].get("rawInput").is_none(),
        "AIR gets no rawInput while streaming"
    );
    assert_eq!(state.streamed_inputs.len(), 1);

    let completed = send(&mut state, input_delta("10}"));
    assert!(
        completed.is_empty(),
        "completion emits nothing: {completed:?}"
    );
    assert!(state.streamed_inputs.is_empty());
}

/// upstream: "scans a large streamed tool input in linear time"
#[test]
fn t8b_partial_input_scan_is_linear_for_a_large_write() {
    let mut state = air_state();
    send(&mut state, start_tool("toolu_write", "Write", "tool_use"));
    let input = serde_json::to_string(&json!({
        "file_path": "/Users/test/project/big.txt",
        "content": "a \"quoted\", {braced} line\n".repeat(40_000),
    }))
    .expect("serialize input");

    let started = std::time::Instant::now();
    let mut updates = Vec::new();
    for start in (0..input.len()).step_by(20) {
        let end = (start + 20).min(input.len());
        updates.extend(send(&mut state, input_delta(&input[start..end])));
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "the incremental scan must stay linear (took {elapsed:?})"
    );
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["sessionUpdate"], "tool_call_update");
    assert_eq!(updates[0]["toolCallId"], "toolu_write");
    assert_eq!(
        updates[0]["locations"],
        json!([{"path": "/Users/test/project/big.txt"}])
    );
    assert!(state.streamed_inputs.is_empty());
}

/// upstream: "refines a pending Write title when its file path arrives"
#[test]
fn t8b_partial_input_refines_a_pending_write_title() {
    let mut state = air_state();
    let mut started = send(&mut state, start_tool("toolu_partial", "Write", "tool_use"));
    assert_eq!(started.len(), 1);
    materialize_tool_call_defaults(&mut started[0]);
    assert_eq!(started[0]["title"], "Preparing file…");
    assert_eq!(started[0]["kind"], "edit");
    assert_eq!(started[0]["status"], "pending");
    assert_eq!(started[0]["locations"], json!([]));

    let refined = send(
        &mut state,
        input_delta("{\"file_path\":\"/Users/test/project/src/write.ts\",\"content\":"),
    );
    assert_eq!(refined.len(), 1, "{refined:?}");
    assert_eq!(refined[0]["sessionUpdate"], "tool_call_update");
    assert_eq!(refined[0]["toolCallId"], "toolu_partial");
    assert_eq!(refined[0]["title"], "Write src/write.ts");
    assert_eq!(
        refined[0]["locations"],
        json!([{"path": "/Users/test/project/src/write.ts"}])
    );
    assert!(refined[0].get("content").is_none(), "{:?}", refined[0]);
    assert!(refined[0].get("rawInput").is_none(), "{:?}", refined[0]);
}

/// upstream: "refines a pending Write from the consolidated block after complete
/// streamed input"
#[test]
fn t8b_partial_input_write_waits_for_the_consolidated_block_when_complete() {
    let mut state = air_state();
    let started = send(&mut state, start_tool("toolu_partial", "Write", "tool_use"));
    assert_eq!(started[0]["title"], "Preparing file…");

    let refined = send(
        &mut state,
        input_delta("{\"file_path\":\"/Users/test/project/src/write.ts\",\"content\":\"hello\"}"),
    );
    assert!(refined.is_empty(), "{refined:?}");
    assert!(state.streamed_inputs.is_empty());

    let consolidated = json!([
        {
            "type": "tool_use",
            "id": "toolu_partial",
            "name": "Write",
            "input": {"file_path": "/Users/test/project/src/write.ts", "content": "hello"}
        }
    ]);
    let updates: Vec<Value> = map_consolidated(&consolidated, MsgRole::Assistant, &mut state)
        .into_iter()
        .map(|update| serde_json::to_value(update).expect("serialize"))
        .collect();
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["sessionUpdate"], "tool_call_update");
    assert_eq!(updates[0]["toolCallId"], "toolu_partial");
    assert_eq!(updates[0]["title"], "Write src/write.ts");
    // The diff holds the file text, so rawInput leaves it out.
    assert_eq!(
        updates[0]["rawInput"],
        json!({"file_path": "/Users/test/project/src/write.ts"})
    );
    assert_eq!(
        updates[0]["locations"],
        json!([{"path": "/Users/test/project/src/write.ts"}])
    );
}

/// upstream `.each`: "refines $case before the full JSON object completes"
#[test]
fn t8b_partial_input_refines_each_tool_family_before_the_object_completes() {
    struct Case {
        name: &'static str,
        block_type: &'static str,
        partial: &'static str,
        title: &'static str,
        meta_title: Option<&'static str>,
    }

    let cases = [
        Case {
            name: "Agent",
            block_type: "tool_use",
            partial: "{\"description\":\"Investigate issue\",\"prompt\":",
            title: "Investigate issue",
            meta_title: None,
        },
        Case {
            name: "Task",
            block_type: "tool_use",
            partial: "{\"description\":\"Research dependencies\",\"prompt\":",
            title: "Research dependencies",
            meta_title: None,
        },
        Case {
            name: "Bash",
            block_type: "tool_use",
            partial: "{\"command\":\"git diff\",\"description\":\"Show current diff\",\"timeout\":",
            title: "git diff",
            meta_title: Some("Show current diff"),
        },
        Case {
            name: "Bash",
            block_type: "tool_use",
            partial: "{\"command\":\"sleep 900, then echo \\\"done\\\"\",\"timeout\":",
            title: "sleep 900, then echo \"done\"",
            meta_title: None,
        },
        Case {
            name: "Read",
            block_type: "tool_use",
            partial: "{\"file_path\":\"/Users/test/project/src/read.ts\",\"offset\":",
            title: "Read src/read.ts",
            meta_title: None,
        },
        Case {
            name: "Write",
            block_type: "tool_use",
            partial: "{\"file_path\":\"/Users/test/project/src/write.ts\",\"content\":",
            title: "Write src/write.ts",
            meta_title: None,
        },
        Case {
            name: "Edit",
            block_type: "tool_use",
            partial: "{\"file_path\":\"/Users/test/project/src/edit.ts\",\"old_string\":",
            title: "Edit src/edit.ts",
            meta_title: None,
        },
        Case {
            name: "Glob",
            block_type: "tool_use",
            partial: "{\"pattern\":\"**/*.ts\",\"path\":",
            title: "Find `**/*.ts`",
            meta_title: None,
        },
        Case {
            name: "Grep",
            block_type: "tool_use",
            partial: "{\"pattern\":\"TODO, FIXME\",\"-i\":true,\"-n\":true,\"-A\":2,\"-B\":1,\"-C\":3,\"output_mode\":\"files_with_matches\",\"head_limit\":10,\"glob\":\"*.ts\",\"type\":\"ts\",\"multiline\":true,\"path\":",
            title: "grep -i -n -A 2 -B 1 -C 3 -l | head -10 --include=\"*.ts\" --type=ts -P \"TODO, FIXME\"",
            meta_title: None,
        },
        Case {
            name: "WebFetch",
            block_type: "tool_use",
            partial: "{\"url\":\"https://example.com/docs\",\"prompt\":",
            title: "Fetch https://example.com/docs",
            meta_title: None,
        },
        Case {
            name: "WebSearch",
            block_type: "server_tool_use",
            partial: "{\"query\":\"ACP tools\",\"allowed_domains\":[\"agentclientprotocol.com\",\"github.com\"],\"blocked_domains\":",
            title: "Search \"ACP tools\"",
            meta_title: None,
        },
        Case {
            name: "ReportFindings",
            block_type: "tool_use",
            partial: "{\"findings\":[{\"file\":\"src/a.ts\",\"line\":7,\"summary\":\"Broken\",\"failure_scenario\":\"Fails\"}],\"level\":",
            title: "Report 1 finding",
            meta_title: None,
        },
    ];

    for case in cases {
        let mut state = air_state();
        send(
            &mut state,
            start_tool("toolu_partial", case.name, case.block_type),
        );
        let refined = send(&mut state, input_delta(case.partial));
        assert_eq!(refined.len(), 1, "{}: {refined:?}", case.name);
        assert_eq!(refined[0]["sessionUpdate"], "tool_call_update");
        assert_eq!(refined[0]["toolCallId"], "toolu_partial");
        assert_eq!(refined[0]["title"], case.title, "{}", case.name);
        assert!(
            refined[0].get("rawInput").is_none(),
            "{}: the input is not complete yet: {:?}",
            case.name,
            refined[0]
        );
        if let Some(meta_title) = case.meta_title {
            assert_eq!(
                refined[0]["_meta"]["jetbrains"]["air"]["commandTitle"], meta_title,
                "{}",
                case.name
            );
        }
        assert!(
            refined[0].get("content").is_none(),
            "{}: refinements never carry content: {:?}",
            case.name,
            refined[0]
        );
        assert_eq!(state.streamed_inputs.len(), 1, "{}", case.name);
    }

    // The generic families keep their title/kind and AIR gets no rawInput, so
    // the production field tracker drops the refinement (Node's helper in this
    // upstream test omits the tracker and therefore re-sends it). See the
    // TEST-MAP note for this `.each` row.
    for (name, block_type, partial) in [
        (
            "Other",
            "tool_use",
            "{\"query\":\"custom query\",\"options\":",
        ),
        (
            "mcp__demo__search",
            "mcp_tool_use",
            "{\"query\":\"custom MCP query\",\"options\":",
        ),
    ] {
        let mut state = air_state();
        send(&mut state, start_tool("toolu_partial", name, block_type));
        let refined = send(&mut state, input_delta(partial));
        assert!(refined.is_empty(), "{name}: {refined:?}");
        assert_eq!(state.streamed_inputs.len(), 1, "{name}");
    }
}

/// upstream `.each`: "waits for the consolidated message when $case is the only
/// field"
#[test]
fn t8b_partial_input_single_field_tools_wait_for_the_consolidated_message() {
    let cases = [
        ("ExitPlanMode", "{\"plan\":\"Implement streamed input\""),
        (
            "AskUserQuestion",
            "{\"questions\":[{\"question\":\"Which mode?\",\"header\":\"Mode\",\"options\":[{\"label\":\"Fast\",\"description\":\"Fast mode\"},{\"label\":\"Safe\",\"description\":\"Safe mode\"}],\"multiSelect\":false}]",
        ),
    ];
    for (name, partial) in cases {
        let mut state = air_state();
        send(&mut state, start_tool("toolu_partial", name, "tool_use"));
        let refined = send(&mut state, input_delta(partial));
        assert!(refined.is_empty(), "{name}: {refined:?}");
        assert_eq!(state.streamed_inputs.len(), 1, "{name}");
    }
}

/// upstream: "does not publish an unfinished string"
#[test]
fn t8b_partial_input_does_not_publish_an_unfinished_string() {
    let mut state = air_state();
    send(&mut state, start_tool("toolu_partial", "Bash", "tool_use"));
    let refined = send(&mut state, input_delta("{\"command\":\"sleep 900"));
    assert!(refined.is_empty(), "{refined:?}");
}

/// upstream: "does not publish an ambiguous number until its delimiter arrives"
#[test]
fn t8b_partial_input_waits_for_a_number_delimiter() {
    let mut state = air_state();
    send(&mut state, start_tool("toolu_partial", "Bash", "tool_use"));
    let first = send(&mut state, input_delta("{\"timeout\":10"));
    assert!(first.is_empty(), "{first:?}");

    let delimited = send(&mut state, input_delta("0,\"command\":"));
    // The recovered input names only `timeout`, so the Bash title stays
    // "Terminal": the production field tracker drops the unchanged refinement.
    assert!(delimited.is_empty(), "{delimited:?}");
}

/// upstream `.each`: "recovers a completed $label value at a field boundary"
#[test]
fn t8b_partial_input_recovers_completed_values_at_a_field_boundary() {
    let cases = [
        "{\"run_in_background\":true,\"command\":",
        "{\"optional\":null,\"command\":",
        "{\"items\":[1,\"two\",false],\"command\":",
        "{\"options\":{\"limit\":5,\"enabled\":true},\"command\":",
    ];
    for partial in cases {
        let mut state = air_state();
        send(
            &mut state,
            start_tool("toolu_partial", "CustomTool", "tool_use"),
        );
        let refined = send(&mut state, input_delta(partial));
        // CustomTool keeps its title/kind and AIR gets no rawInput, so the
        // production field tracker drops the refinement (the upstream helper
        // omits the tracker).
        assert!(refined.is_empty(), "{partial}: {refined:?}");
    }
}

/// upstream: "survives a ping keep-alive arriving mid-stream"
#[test]
fn t8b_partial_input_survives_a_ping_keep_alive() {
    let mut state = MapState::default();
    send(&mut state, start_tool("toolu_ping", "Bash", "tool_use"));
    send(&mut state, input_delta("{\"command\":\"sleep 900\""));
    let ping = send(&mut state, json!({"type": "ping"}));
    assert!(ping.is_empty(), "{ping:?}");
    assert_eq!(state.streamed_inputs.len(), 1);

    let refined = send(&mut state, input_delta(",\"timeout\":"));
    assert_eq!(refined.len(), 1, "{refined:?}");
    assert_eq!(refined[0]["sessionUpdate"], "tool_call_update");
    assert_eq!(refined[0]["toolCallId"], "toolu_ping");
    assert_eq!(refined[0]["title"], "sleep 900");
}

/// upstream: "treats `ping` keep-alive events as no-ops without logging to stderr"
#[test]
fn t8b_ping_stream_event_is_a_silent_noop() {
    let mut state = MapState::default();
    let ping = send(&mut state, json!({"type": "ping"}));
    assert!(ping.is_empty(), "{ping:?}");
    assert!(state.streamed_inputs.is_empty());
    assert!(state.emitted.ids.is_empty());
}

// ------------------------------------------------------- messageId and thinking

/// upstream: "sets messageId on agent message chunks from string content"
/// upstream: "sets messageId on user message chunks and thought chunks"
/// upstream: "omits messageId when none is supplied"
/// upstream: "never sets messageId on non-chunk updates (tool_call)"
#[test]
fn t8b_message_id_is_set_on_chunks_only() {
    let message_id = "11111111-2222-3333-4444-555555555555";

    let mut state = MapState::default();
    let assistant = map_consolidated_assistant(
        &json!({
            "type": "assistant",
            "parent_tool_use_id": null,
            "uuid": "frame-uuid",
            "message": {"id": message_id, "role": "assistant", "content": "hello world"}
        }),
        &mut state,
    );
    assert_eq!(assistant.len(), 1);
    let value = serde_json::to_value(&assistant[0]).expect("serialize");
    assert_eq!(
        value,
        json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hello world"},
            "messageId": message_id
        })
    );

    let mut state = MapState::default();
    let user = map_user_frame(
        &json!({
            "type": "user",
            "uuid": "user-uuid",
            "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}
        }),
        &mut state,
    );
    assert_eq!(user.len(), 1);
    let value = serde_json::to_value(&user[0]).expect("serialize");
    assert_eq!(value["sessionUpdate"], "user_message_chunk");
    assert_eq!(value["messageId"], "user-uuid");

    let mut state = MapState::default();
    let thought = map_consolidated_assistant(
        &json!({
            "type": "assistant",
            "parent_tool_use_id": null,
            "uuid": "frame-uuid",
            "message": {
                "id": message_id,
                "role": "assistant",
                "content": [{"type": "thinking", "thinking": "hmm", "signature": ""}]
            }
        }),
        &mut state,
    );
    assert_eq!(thought.len(), 1);
    let value = serde_json::to_value(&thought[0]).expect("serialize");
    assert_eq!(value["sessionUpdate"], "agent_thought_chunk");
    assert_eq!(value["messageId"], message_id);

    // No supplied id: no messageId property at all.
    let mut state = MapState::default();
    let no_id = map_consolidated_assistant(
        &json!({
            "type": "assistant",
            "parent_tool_use_id": null,
            "message": {"role": "assistant", "content": "hello"}
        }),
        &mut state,
    );
    assert_eq!(no_id.len(), 1);
    let value = serde_json::to_value(&no_id[0]).expect("serialize");
    assert!(value.get("messageId").is_none(), "{value}");

    // A tool_call is never a chunk and never carries messageId.
    let mut state = MapState::default();
    let tool = map_consolidated_assistant(
        &json!({
            "type": "assistant",
            "parent_tool_use_id": null,
            "uuid": "frame-uuid",
            "message": {
                "id": message_id,
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "toolu_abc", "name": "Read",
                    "input": {"file_path": "/tmp/x"}}]
            }
        }),
        &mut state,
    );
    assert_eq!(tool.len(), 1);
    let value = serde_json::to_value(&tool[0]).expect("serialize");
    assert_eq!(value["sessionUpdate"], "tool_call");
    assert!(value.get("messageId").is_none(), "{value}");
}

/// upstream: "emits an agent_thought_chunk for non-empty thinking text"
/// upstream: "skips empty thinking blocks (display: 'omitted' signature-only blocks)"
/// upstream: "skips empty thinking deltas"
/// upstream: "skips thinking chunks without string content"
/// upstream: "skips text deltas without string content"
#[test]
fn t8b_thinking_chunks_skip_empty_and_non_string_content() {
    let mut state = MapState::default();
    let updates = map_consolidated(
        &json!([{"type": "thinking", "thinking": "let me reason", "signature": ""}]),
        MsgRole::Assistant,
        &mut state,
    );
    assert_eq!(updates.len(), 1);
    let value = serde_json::to_value(&updates[0]).expect("serialize");
    assert_eq!(
        value,
        json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": "let me reason"}
        })
    );

    for content in [
        json!([{"type": "thinking", "thinking": "", "signature": "abc"}]),
        json!([{"type": "thinking_delta", "thinking": "", "estimated_tokens": 0}]),
        json!([{"type": "thinking", "signature": "abc"}]),
        json!([{"type": "thinking_delta", "estimated_tokens": 0}]),
        json!([{"type": "thinking_delta", "thinking": null, "estimated_tokens": 0}]),
        json!([{"type": "text_delta"}]),
        json!([{"type": "text_delta", "text": null}]),
    ] {
        let mut state = MapState::default();
        let updates = map_consolidated(&content, MsgRole::Assistant, &mut state);
        assert!(updates.is_empty(), "{content}: {updates:?}");
    }
}

/// upstream: "uses the Anthropic API message id for assistant messages"
/// upstream: "falls back to the uuid for assistant messages without an API id"
/// upstream: "uses the uuid for user messages (they carry no API id and aren't streamed)"
/// upstream: "returns undefined when there is no usable id"
#[test]
fn t8b_message_id_for_grouping() {
    use crate::mcp_command::message_id_of;

    assert_eq!(
        message_id_of(&json!({
            "type": "assistant",
            "uuid": "de242400-cdb3-4af7-9856-d3b114b20af9",
            "message": {"id": "msg_018DQGVuZbGYwVnvDakAP9Do", "role": "assistant"}
        })),
        Some("msg_018DQGVuZbGYwVnvDakAP9Do".to_string())
    );
    assert_eq!(
        message_id_of(&json!({
            "type": "assistant",
            "uuid": "de242400-cdb3-4af7-9856-d3b114b20af9",
            "message": {"role": "assistant"}
        })),
        Some("de242400-cdb3-4af7-9856-d3b114b20af9".to_string())
    );
    assert_eq!(
        message_id_of(&json!({
            "type": "user",
            "uuid": "11111111-2222-3333-4444-555555555555",
            "message": {"id": "msg_should_be_ignored", "role": "user"}
        })),
        Some("11111111-2222-3333-4444-555555555555".to_string())
    );
    assert_eq!(
        message_id_of(&json!({"type": "system", "message": {}})),
        None
    );
    assert_eq!(
        message_id_of(&json!({"type": "assistant", "uuid": "", "message": {}})),
        None
    );
}

// ------------------------------------------------------ assembled text fallback

/// upstream: "dedupes per block type: streamed text is dropped but an
/// un-streamed thinking block in the same message is forwarded"
#[test]
fn t8b_assembled_text_dedupes_per_block_type() {
    let mut state = MapState::default();
    let delta = json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"content_block_delta","index":0,
        "delta":{"type":"text_delta","text":"streamed text"}}});
    let streamed = map_stream_event(&delta, &mut state);
    assert_eq!(streamed.len(), 1);
    let frame = json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg-mixed","model":"claude-sonnet-4-5","role":"assistant",
        "content":[{"type":"text","text":"streamed text"},
            {"type":"thinking","thinking":"private reasoning"}]}});
    let updates = map_consolidated_assistant(&frame, &mut state);
    assert_eq!(updates.len(), 1, "{updates:?}");
    let value = serde_json::to_value(&updates[0]).expect("serialize");
    assert_eq!(value["sessionUpdate"], "agent_thought_chunk");
    assert_eq!(value["content"]["text"], "private reasoning");
}

/// upstream: "dedupes streamed text even when the consolidated message carries
/// a different id"
#[test]
fn t8b_assembled_text_dedupes_across_message_ids() {
    let mut state = MapState::default();
    let start = json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"message_start","message":{"id":"msg-stream-id","model":"m"}}});
    let _ = map_stream_event(&start, &mut state);
    for text in ["hello ", "world"] {
        let delta = json!({"type":"stream_event","parent_tool_use_id":null,
            "event":{"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":text}}});
        let _ = map_stream_event(&delta, &mut state);
    }
    let frame = json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg-DIFFERENT-id","model":"m","role":"assistant",
        "content":[{"type":"text","text":"hello world"}]}});
    assert!(
        map_consolidated_assistant(&frame, &mut state).is_empty(),
        "the assembled copy is deduped despite the id mismatch"
    );
}

/// upstream: "dedupes a streamed text block even when a thinking delta omits
/// text"
#[test]
fn t8b_assembled_text_missing_thinking_text_does_not_stall_dedupe() {
    let mut state = MapState::default();
    let start = json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"message_start","message":{"id":"msg-missing-thinking","model":"m"}}});
    let _ = map_stream_event(&start, &mut state);
    let missing = json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"content_block_delta","index":0,
        "delta":{"type":"thinking_delta"}}});
    let _ = map_stream_event(&missing, &mut state);
    let delta = json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"content_block_delta","index":0,
        "delta":{"type":"text_delta","text":"real answer"}}});
    let _ = map_stream_event(&delta, &mut state);
    let frame = json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg-missing-thinking","model":"m","role":"assistant",
        "content":[{"type":"text","text":"real answer"}]}});
    assert!(
        map_consolidated_assistant(&frame, &mut state).is_empty(),
        "a thinking delta without text must not stall the diff cursor"
    );
}

/// upstream: "forwards distinct blocks that a gateway splits across same-id
/// messages" and "re-forwards a block a gateway re-delivers (no content-keyed
/// dedupe)"
#[test]
fn t8b_assembled_text_same_id_split_and_redelivery() {
    let mut state = MapState::default();
    let thinking = json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg-split","model":"m","role":"assistant",
        "content":[{"type":"thinking","thinking":""}]}});
    assert!(map_consolidated_assistant(&thinking, &mut state).is_empty());
    let text = json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg-split","model":"m","role":"assistant",
        "content":[{"type":"text","text":"the real answer"}]}});
    let updates = map_consolidated_assistant(&text, &mut state);
    assert_eq!(updates.len(), 1, "{updates:?}");
    let value = serde_json::to_value(&updates[0]).expect("serialize");
    assert_eq!(value["sessionUpdate"], "agent_message_chunk");
    assert_eq!(value["content"]["text"], "the real answer");

    // The fallback keys only on whether the id streamed live, not on content:
    // a re-delivered assembled block is forwarded again.
    let again = map_consolidated_assistant(&text, &mut state);
    assert_eq!(again.len(), 1, "{again:?}");
    let value = serde_json::to_value(&again[0]).expect("serialize");
    assert_eq!(value["content"]["text"], "the real answer");
}

/// upstream: "does not repeat streamed subagent text in the consolidated
/// message"
#[test]
fn t8b_assembled_text_forwarded_subagent_stream_dedupes() {
    let mut state = MapState::default();
    state.set_forward_subagent_text(true);
    for text in ["nested ", "report"] {
        let delta = json!({"type":"stream_event","parent_tool_use_id":"tool_use_1",
            "event":{"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":text}}});
        let updates = map_stream_event(&delta, &mut state);
        assert_eq!(updates.len(), 1, "{text}: {updates:?}");
    }
    let frame = json!({"type":"assistant","parent_tool_use_id":"tool_use_1",
        "message":{"id":"msg-subagent","model":"m","role":"assistant",
        "content":[{"type":"text","text":"nested report!"}]}});
    let updates = map_consolidated_assistant(&frame, &mut state);
    assert_eq!(updates.len(), 1, "{updates:?}");
    let value = serde_json::to_value(&updates[0]).expect("serialize");
    assert_eq!(value["sessionUpdate"], "agent_message_chunk");
    assert_eq!(value["content"]["text"], "!");
    assert_eq!(
        value["_meta"]["claudeCode"]["parentToolUseId"],
        "tool_use_1"
    );
}
