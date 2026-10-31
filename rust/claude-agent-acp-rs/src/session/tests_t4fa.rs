//! Turn 4 fix round (lane FA, T4FA): the session/turn settle hazards — the
//! `/usage` error lanes, the result-frame order, replayed local-command output,
//! the producer's completion guarantee, the stale `task_notification` end, the
//! stream-end state and the review's low findings.

use super::tests_t2a::Rig;
use super::*;

fn usage_rig(prepared: Option<String>) -> Rig {
    let mut rig = Rig::new();
    rig.ext.t2a.usage_turns.insert("p1");
    rig.ext
        .t2a
        .usage_turns
        .get_mut("p1")
        .expect("registered")
        .prepared = Some(prepared);
    rig.echo("p1");
    rig.updates();
    rig
}

fn success_result(result: &str, output_tokens: u64) -> Value {
    json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":result,
        "usage":{"input_tokens":1,"output_tokens":output_tokens}})
}

fn chunk_texts(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|u| u["content"]["text"].as_str().map(str::to_string))
        .collect()
}

fn reply(rig: &mut Rig, uuid: &str) -> Result<PromptReply, PromptFailure> {
    let mut reply = rig.replies.remove(uuid).expect("prompt was echoed");
    reply.try_recv().expect("turn settled")
}

// ---------------------------------------------------------------- STEP 1

/// Finding 1: only a plain `success` result reaches the replacement lane. A
/// `/usage` turn's structured markdown must not eat an error result's text: a
/// login result still fails the turn as `auth_required`.
#[test]
fn t4fa_usage_login_error_keeps_its_result() {
    let mut rig = usage_rig(Some("## Usage\nstructured".to_string()));
    rig.feed(json!({"type":"result","subtype":"success","is_error":true,
            "stop_reason":"error","result":"Please run /login to continue",
            "usage":{"input_tokens":1,"output_tokens":0}}));
    let updates = rig.updates();
    assert!(
        chunk_texts(&updates).is_empty(),
        "no structured chunk on an error result: {updates:?}"
    );
    match reply(&mut rig, "p1") {
        Err(failure) => assert!(
            matches!(
                &*failure,
                SessionError::PromptFailed {
                    kind: FailureKind::AuthRequired,
                    message,
                    ..
                } if message.contains("Please run /login")
            ),
            "{failure:?}"
        ),
        Ok(reply) => panic!("the login result must fail the turn, got {reply:?}"),
    }
}

/// Finding 1: `max_tokens` exits before the replacement lane and keeps its
/// stop reason.
#[test]
fn t4fa_usage_max_tokens_is_untouched() {
    let mut rig = usage_rig(Some("## Usage\nstructured".to_string()));
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
            "stop_reason":"max_tokens","result":"",
            "usage":{"input_tokens":1,"output_tokens":0}}));
    assert!(chunk_texts(&rig.updates()).is_empty());
    let reply = reply(&mut rig, "p1").expect("settled");
    assert_eq!(reply.stop_reason, "max_tokens");
}

/// Finding 1: a refusal exits before the replacement lane.
#[test]
fn t4fa_usage_refusal_is_untouched() {
    let mut rig = usage_rig(Some("## Usage\nstructured".to_string()));
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
            "stop_reason":"refusal","result":"",
            "usage":{"input_tokens":1,"output_tokens":0}}));
    assert!(chunk_texts(&rig.updates()).is_empty());
    let reply = reply(&mut rig, "p1").expect("settled");
    assert_eq!(reply.stop_reason, "refusal");
}

/// Finding 1: `error_during_execution` keeps its provider failure and its text.
#[test]
fn t4fa_usage_error_during_execution_is_untouched() {
    let mut rig = usage_rig(Some("## Usage\nstructured".to_string()));
    rig.feed(
        json!({"type":"result","subtype":"error_during_execution","is_error":true,
            "stop_reason":"error","errors":["boom"],
            "usage":{"input_tokens":1,"output_tokens":0}}),
    );
    assert!(chunk_texts(&rig.updates()).is_empty());
    match reply(&mut rig, "p1") {
        Err(failure) => assert!(
            matches!(
                &*failure,
                SessionError::PromptFailed {
                    kind: FailureKind::ProviderError,
                    ..
                }
            ),
            "{failure:?}"
        ),
        Ok(reply) => panic!("the error result must fail the turn, got {reply:?}"),
    }
}

/// Finding 1: a plain success still swaps in the replacement.
#[test]
fn t4fa_usage_success_swaps_in_the_markdown() {
    let mut rig = usage_rig(Some("## Usage\nstructured".to_string()));
    rig.feed(success_result("Raw usage text", 0));
    assert_eq!(chunk_texts(&rig.updates()), vec!["## Usage\nstructured"]);
    let reply = reply(&mut rig, "p1").expect("settled");
    assert_eq!(reply.stop_reason, "end_turn");
    assert!(
        rig.ext.t2a.usage_turns.get("p1").is_none(),
        "the turn's producer entry is spent at its result"
    );
}

/// Finding 1 (unit): the gate itself mirrors upstream's exits.
#[test]
fn t4fa_result_reaches_forward_only_for_a_plain_success() {
    let machine = TurnMachine::new();
    assert!(machine.result_reaches_forward(&success_result("x", 0)));
    assert!(!machine.result_reaches_forward(&json!({
        "type":"result","subtype":"success","is_error":false,"stop_reason":"refusal"})));
    assert!(!machine.result_reaches_forward(&json!({
        "type":"result","subtype":"success","is_error":false,"stop_reason":"max_tokens"})));
    assert!(!machine.result_reaches_forward(&json!({
        "type":"result","subtype":"success","is_error":true,"result":"Please run /login"})));
    assert!(!machine.result_reaches_forward(&json!({
        "type":"result","subtype":"error_during_execution","is_error":true})));
    assert!(!machine.result_reaches_forward(&json!({
        "type":"result","subtype":"success","is_error":false,
        "origin":{"kind":"task-notification"}})));
    let mut cancelled = TurnMachine::new();
    cancelled.cancel();
    assert!(!cancelled.result_reaches_forward(&success_result("x", 0)));
}

// ---------------------------------------------------------------- STEP 2

/// Finding 2: the replacement chunk goes out after the result `usage_update`
/// (goal rollback, usage_update, then the chunk), not before it.
#[test]
fn t4fa_result_replacement_follows_the_usage_update() {
    let mut rig = usage_rig(Some("## Usage\nstructured".to_string()));
    // The usage snapshot the result reports comes from the streamed message.
    rig.feed(json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"message_start","message":{"model":"claude-sonnet-4-5",
        "usage":{"input_tokens":100,"output_tokens":0,
            "cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}));
    let mut updates = rig.updates();
    updates.extend(rig.updates());
    rig.feed(success_result("Raw usage text", 0));
    updates.extend(rig.updates());
    let kinds: Vec<&str> = updates
        .iter()
        .filter_map(|u| u["sessionUpdate"].as_str())
        .collect();
    let usage = kinds
        .iter()
        .rposition(|k| *k == "usage_update")
        .expect("a result usage_update");
    let chunk = kinds
        .iter()
        .position(|k| *k == "agent_message_chunk")
        .expect("the replacement chunk");
    assert!(
        usage < chunk,
        "the replacement must follow the usage_update: {kinds:?}"
    );
    assert_eq!(
        updates[chunk]["content"]["text"], "## Usage\nstructured",
        "the swapped-in chunk, not the CLI text"
    );
}

// ---------------------------------------------------------------- STEP 3

/// Finding 3: a replayed `local-command-stdout` user frame is dropped before
/// the stdout handling (`acp-agent.ts:7796-7812`), never published again.
#[test]
fn t4fa_replayed_local_command_output_is_not_published() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(json!({"type":"user","uuid":"replay-uuid","isReplay":true,
        "message":{"role":"user","content":"leaked <local-command-stdout>raw usage output</local-command-stdout>"}}));
    let updates = rig.updates();
    assert!(
        updates.iter().all(|u| !u.to_string().contains("leaked")),
        "a replayed stdout frame must not be published: {updates:?}"
    );
    // A live (non-replay) stdout frame still maps (as a user chunk).
    let mut live_rig = Rig::new();
    live_rig.feed(json!({"type":"user","uuid":"live-uuid",
        "message":{"role":"user","content":"hello <local-command-stdout>live output</local-command-stdout>"}}));
    let live = live_rig.updates();
    assert!(
        live.iter().any(|u| u["sessionUpdate"] == "user_message_chunk"
            && u["content"]["text"] == "hello "),
        "the live stdout frame maps: {live:?}"
    );
}

// ---------------------------------------------------------------- STEP 4

/// Finding 4: a producer that panics still re-enters, so the held frames (and
/// the turn) settle instead of hanging.
#[tokio::test]
async fn t4fa_panicking_producer_still_reports_back() {
    let handle: tokio::task::JoinHandle<Option<String>> =
        tokio::spawn(async { panic!("producer exploded") });
    let markdown = join_produced(handle).await.unwrap_or(None);
    assert_eq!(markdown, None, "a panic means keep the CLI text");
}

/// Finding 4: a non-ASCII `resets_at` never panics (the reset line falls back
/// to the escaped raw value).
#[test]
fn t4fa_non_ascii_resets_at_does_not_panic() {
    let zone = crate::usage_markdown::Zone::utc();
    let value = "2026-09-04T16:59:00Z\u{2003}";
    assert_eq!(
        crate::usage_markdown::format_reset(Some(value), &zone),
        format!(
            " · Resets {}",
            crate::usage_markdown::escape_markdown(value)
        )
    );
}

// ---------------------------------------------------------------- STEP 5

/// Finding 3 (T4B review): a stale `task_notification` end returns from the
/// whole branch. It must not settle the live task, notify the async tasks,
/// close stopped tool calls or settle the turn.
#[test]
fn t4fa_stale_task_notification_does_not_settle_the_live_task() {
    use crate::client_caps::SubagentForm;
    use crate::native_subagents::TaskStarted;

    let mut rig = Rig::new();
    rig.tasks = AsyncTaskRuntime::new(false).with_subagent_form(Some(SubagentForm::Rfd), "s");
    // The Agent call that started the child, so the child is announced (a
    // withdrawn, never-exposed child keeps no parent mapping).
    rig.tasks.subagents().route(
        &json!({"sessionId":"s","update":{
            "sessionUpdate":"tool_call","toolCallId":"launch-1","title":"Agent",
            "status":"pending","rawInput":{"description":"Fix","prompt":"Make it pass"},
            "_meta":{"claudeCode":{"toolName":"Agent"}}}}),
        None,
    );
    // The RFD scenario: a child resumed under `send-1` after an earlier
    // delegation under `launch-1`, then a `task_started` names `launch-1` again
    // (a late replay). `send-1` is then a stale end.
    rig.tasks.subagents().task_started(&TaskStarted {
        task_id: "worker-1".into(),
        tool_use_id: Some("launch-1".into()),
        subagent_type: Some("Explore".into()),
        ..TaskStarted::default()
    });
    rig.tasks
        .subagents()
        .finish_task("worker-1", "completed", Some("launch-1"), None);
    rig.tasks.subagents().task_started(&TaskStarted {
        task_id: "worker-1".into(),
        tool_use_id: Some("send-1".into()),
        subagent_type: Some("Explore".into()),
        ..TaskStarted::default()
    });
    rig.tasks
        .subagents()
        .finish_task("worker-1", "completed", Some("send-1"), None);
    rig.tasks.subagents().task_started(&TaskStarted {
        task_id: "worker-1".into(),
        tool_use_id: Some("launch-1".into()),
        subagent_type: Some("Explore".into()),
        ..TaskStarted::default()
    });
    assert!(rig
        .tasks
        .subagents()
        .is_stale_end("worker-1", Some("send-1")));

    rig.machine.enqueue(Turn::new("p1".into(), false));
    rig.echo("p1");
    rig.updates();
    rig.machine.register_live_task(
        "worker-1",
        Some("send-1"),
        true,
        crate::turn::TaskPlacement::default(),
    );
    rig.feed(json!({"type":"system","subtype":"task_notification",
        "task_id":"worker-1","status":"stopped","tool_use_id":"send-1"}));
    assert!(
        rig.machine.is_live_task("worker-1"),
        "the live task still belongs to the new run"
    );
    assert!(
        rig.replies.get("p1").is_some_and(|r| r.is_empty()),
        "the turn was not settled by the stale end"
    );
    assert!(
        rig.updates().is_empty(),
        "nothing was published for the stale end"
    );
}

// ---------------------------------------------------------------- STEP 7

/// Finding 5: a `status:"compacting"` lifecycle alone suppresses the #453
/// fallback (and the stale synthetic frame), without a boundary.
#[test]
fn t4fa_compacting_status_counts_as_delivered_output() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(json!({"type":"system","subtype":"status","status":"compacting"}));
    assert!(rig.ext.t2a.compaction_output_delivered);
    rig.feed(success_result("A stale replay", 0));
    assert!(
        chunk_texts(&rig.updates()).is_empty(),
        "the result fallback stays off"
    );
}

/// Finding 5: `compact_result:"success"` also opens the lifecycle.
#[test]
fn t4fa_compact_result_counts_as_delivered_output() {
    let mut rig = Rig::new();
    rig.feed(json!({"type":"system","subtype":"status","compact_result":"success"}));
    assert!(rig.ext.t2a.compaction_output_delivered);
}

/// Finding 8: `/usage` is detected only when the prompt is exactly one text
/// block (`localCommandMarkdown`, `acp-agent.ts:3722-3726`).
#[test]
fn t4fa_usage_detection_requires_a_single_text_block() {
    let single = json!({"message":{"content":[{"type":"text","text":"/usage"}]}});
    assert_eq!(single_text_prompt(&single), Some("/usage"));
    let with_image = json!({"message":{"content":[
        {"type":"text","text":"/usage"},
        {"type":"image","source":{"type":"base64","data":"aGk=","media_type":"image/png"}}
    ]}});
    assert_eq!(single_text_prompt(&with_image), None);
    let two_texts = json!({"message":{"content":[
        {"type":"text","text":"/usage"},
        {"type":"text","text":""}
    ]}});
    assert_eq!(single_text_prompt(&two_texts), None);
    let link = json!({"message":{"content":[
        {"type":"text","text":"[@a](file:///a)"}
    ]}});
    assert_eq!(single_text_prompt(&link), Some("[@a](file:///a)"));
}

/// Finding 6: an output shape that arrives before the `/usage` producer was
/// started starts it and holds the frame.
#[test]
fn t4fa_output_before_the_producer_starts_holds_and_starts_it() {
    let mut rig = Rig::new();
    rig.ext.t2a.usage_turns.insert("p1");
    rig.machine.enqueue(Turn::new("p1".into(), false));
    let line = json!({"type":"system","subtype":"local_command_output","content":"raw"});
    assert!(
        holds_for_local_command_run(&line, &rig.machine, &mut rig.ext),
        "the frame waits for the replacement"
    );
    let turn = rig.ext.t2a.usage_turns.get("p1").expect("registered");
    assert!(turn.run_started, "the producer was started lazily");
}

/// Finding 9: a turn that settles without a result drops its `/usage` entry, so
/// the map cannot grow for the session's lifetime.
#[test]
fn t4fa_usage_turn_is_removed_when_it_settles_without_a_result() {
    let mut rig = Rig::new();
    rig.ext.t2a.usage_turns.insert("p1");
    // A queued turn the cancel sweeps: it settles without a result.
    rig.machine.enqueue(Turn::new("p1".into(), false));
    let events = rig.machine.cancel();
    settle_events(
        events,
        &mut rig.tasks,
        &mut rig.map_state,
        &rig.live_background,
        &mut rig.pending,
        &rig.tx,
        "s",
        &rig.high_water,
        &rig.ext.failure_ctx,
        rig.ext.air.as_ref(),
        None,
        &mut rig.ext.titles,
        &mut rig.ext.t2a.failures,
        &mut rig.ext.t2a.usage_turns,
        &mut crate::session::FileChangeReports::default(),
    );
    assert!(
        rig.ext.t2a.usage_turns.get("p1").is_none(),
        "the cancelled turn's producer entry is gone"
    );
}
