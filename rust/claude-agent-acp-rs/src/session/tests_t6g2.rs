//! T6G2: the v2 behaviour fixes of turn 6, fix round 2 (NEW-1..NEW-10 and the
//! surviving mutants M2/M4/M5 of `review-T6G`), driven through the session
//! actor's own functions.

use super::tests_t2a::Rig;
use super::*;
use crate::turn::StopReason;
use crate::turn_events::{JsonRpcError, V2IntakeError, V2TurnEnd};

/// A [`Rig`] on a v2 connection.
fn rig_v2() -> Rig {
    let mut rig = Rig::new();
    rig.map_state.v2.set_reporting(true);
    rig
}

fn trace(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .map(|update| {
            let kind = update["sessionUpdate"].as_str().unwrap_or("");
            match kind {
                "state_update" => match update["stopReason"].as_str() {
                    Some(reason) => format!("{} {reason}", update["state"].as_str().unwrap_or("")),
                    None => update["state"].as_str().unwrap_or("").to_string(),
                },
                _ => kind.to_string(),
            }
        })
        .collect()
}

fn v2_out(rig: &mut Rig) -> (Vec<Value>, Vec<Result<String, V2IntakeError>>) {
    let mut updates = Vec::new();
    let mut answers = Vec::new();
    while let Ok(item) = rig.rx.try_recv() {
        match item {
            SessionOutbound::RawUpdate(value) => updates.push(value["update"].clone()),
            SessionOutbound::V2Intake {
                prompt_uuid: _,
                reply,
                result,
            } => {
                let _ = reply.send(result.clone());
                answers.push(result);
            }
            SessionOutbound::Update(notif) => {
                updates.push(serde_json::to_value(&notif.update).unwrap());
            }
            _ => {}
        }
    }
    (updates, answers)
}

/// Register and enqueue a v2 prompt once (the caller feeds its echo).
fn queue_v2(rig: &mut Rig, uuid: &str) -> oneshot::Receiver<Result<String, V2IntakeError>> {
    let (tx, rx) = oneshot::channel();
    rig.map_state
        .v2
        .register(uuid, json!([{"type":"text","text":"hi"}]), tx);
    rig.machine.enqueue(Turn::new(uuid.into(), false));
    rx
}

fn echo(rig: &mut Rig, uuid: &str) {
    rig.feed(json!({"type":"user","uuid":uuid,"message":{"role":"user",
        "content":[{"type":"text","text":"hi"}]}}));
}

fn result_frame(stop_reason: &str) -> Value {
    json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":stop_reason,"result":"ok",
        "usage":{"input_tokens":3,"output_tokens":2}})
}

fn subagent_started(task: &str) -> Value {
    json!({"type":"system","subtype":"task_started","task_id":task,
        "tool_use_id":"toolu_AG","description":"Research","subagent_type":"general-purpose",
        "task_type":"local_agent","is_backgrounded":true})
}

fn failed_event(kind: FailureKind, message: &str) -> TurnEvent {
    TurnEvent::Failed {
        prompt_uuid: "p1".into(),
        kind,
        message: message.to_string(),
        usage: crate::turn::Usage::default(),
        model_rows: std::sync::Arc::new(crate::quota::ModelRows::default()),
        is_structured_output_retries: false,
    }
}

/// Run one batch through the real `settle_events` path: the gate, the v1
/// answers, the v2 states, in that order.
fn settle(rig: &mut Rig, events: Vec<TurnEvent>) -> Vec<Value> {
    let mut titles = crate::session_titles::SessionTitles::default();
    let mut usage_turns = crate::usage::UsageTurns::default();
    settle_events(
        events,
        &mut rig.tasks,
        &mut rig.map_state,
        &rig.live_background,
        &mut rig.pending,
        &rig.tx,
        "s",
        &rig.high_water,
        &crate::session_failure::AssistantFailureCtx::default(),
        None,
        None,
        &mut titles,
        &mut crate::session::failures::SessionFailures::default(),
        &mut usage_turns,
        &mut crate::session::FileChangeReports::default(),
    );
    let (updates, _) = v2_out(rig);
    updates
}

// ---------------------------------------------------------------- NEW-1

/// NEW-1: a turn whose text is only in its `result` reaches a v2 client as a
/// chunk before the `idle` (`sendUpdate`, `acp-agent.ts:5301`). A v2 prompt has
/// no `pending` entry, so the v1 `PromptReply` path never carried it.
#[test]
fn t6g2_the_result_only_text_reaches_a_v2_client() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    // No streamed assistant text and no output tokens: the #453 fallback.
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"ONLY-IN-RESULT","uuid":"res-uuid-1",
        "usage":{"input_tokens":0,"output_tokens":0}}));
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(
        trace(&updates),
        vec!["agent_message_chunk", "idle end_turn"],
        "the result text precedes the idle: {updates:?}"
    );
    let chunk = &updates[0];
    assert_eq!(chunk["content"]["text"], "ONLY-IN-RESULT");
    // D11: the chunk is grouped under the result frame's uuid, and the v2 form
    // puts `messageId` last (`{...chunk, messageId}`).
    assert_eq!(chunk["messageId"], "res-uuid-1");
    assert_eq!(
        serde_json::to_string(chunk).unwrap(),
        r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ONLY-IN-RESULT"},"messageId":"res-uuid-1"}"#
    );
}

// ---------------------------------------------------------------- NEW-5

/// NEW-5: a child-exit failure is the error v1 answers with —
/// `Internal error` with `data.details` — on both versions.
#[test]
fn t6g2_a_child_exit_failure_reports_internal_error_with_details() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    let message = format!("{CHILD_EXIT_PREFIX}boom");
    let updates = settle(
        &mut rig,
        vec![failed_event(FailureKind::ProviderError, &message)],
    );
    assert_eq!(trace(&updates), vec!["idle error"], "{updates:?}");
    let error = &updates[0]["error"];
    assert_eq!(error["code"], -32603);
    assert_eq!(error["message"], "Internal error");
    assert_eq!(
        error["data"],
        json!({"details": "Claude Code returned an error result: boom"}),
        "the child-exit detail rides in data.details: {error}"
    );
}

/// NEW-5: a worker shutdown is `Internal error` with
/// `{errorKind:"worker_shutdown"}`, exactly as v1 answers it.
#[test]
fn t6g2_a_worker_shutdown_reports_the_worker_shutdown_kind() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    let updates = settle(
        &mut rig,
        vec![failed_event(FailureKind::WorkerShutdown, "shutting down")],
    );
    assert_eq!(trace(&updates), vec!["idle error"], "{updates:?}");
    let error = &updates[0]["error"];
    assert_eq!(error["message"], "Internal error");
    assert_eq!(error["data"], json!({"errorKind": "worker_shutdown"}));
    // The v1 answer of the same failure is the same identity.
    let v1 = crate::agent::session_rpc_error(&failed_turn_error(
        FailureKind::WorkerShutdown,
        "shutting down",
        false,
        false,
        false,
    ));
    assert_eq!(v1.message, "Internal error");
    assert_eq!(v1.data, Some(json!({"errorKind": "worker_shutdown"})));
}

/// NEW-5: an auth failure keeps the `-32000` code on the v2 `idle`, and the
/// turn end carries the kind the respawn reads.
#[test]
fn t6g2_an_auth_failure_reports_the_auth_code_and_its_kind() {
    let mut rig = rig_v2();
    let (end_tx, mut end_rx) = oneshot::channel();
    let _intake = queue_v2(&mut rig, "p1");
    rig.map_state.v2.set_turn_end("p1", end_tx);
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    let updates = settle(
        &mut rig,
        vec![failed_event(FailureKind::AuthRequired, "Please run /login")],
    );
    assert_eq!(trace(&updates), vec!["idle error"], "{updates:?}");
    assert_eq!(updates[0]["error"]["code"], -32000);
    assert_eq!(updates[0]["error"]["message"], "Authentication required");
    let end = end_rx.try_recv().expect("the turn end is reported");
    assert_eq!(
        end,
        V2TurnEnd::Failed {
            kind: FailureKind::AuthRequired,
            error: JsonRpcError {
                code: -32000,
                message: "Authentication required".to_string(),
                data: None,
            },
        }
    );
}

/// NEW-3: a settled v2 turn reports its end to the task that runs the v1
/// bookkeeping, with the stop reason.
#[test]
fn t6g2_a_settled_v2_turn_reports_its_end() {
    let mut rig = rig_v2();
    let (end_tx, mut end_rx) = oneshot::channel();
    let _intake = queue_v2(&mut rig, "p1");
    rig.map_state.v2.set_turn_end("p1", end_tx);
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    let events = rig.machine.on_result(&result_frame("end_turn"), false);
    let updates = settle(&mut rig, events);
    assert_eq!(trace(&updates), vec!["idle end_turn"], "{updates:?}");
    assert_eq!(
        end_rx.try_recv().expect("the turn end is reported"),
        V2TurnEnd::Settled(StopReason::EndTurn)
    );
}

// ---------------------------------------------------------------- NEW-7

/// NEW-7: a dropped intake reply is a session that ended — the same error v1
/// answers with (`internalError(undefined, SESSION_ENDED_MESSAGE)`,
/// `acp-agent.ts:3805-3807`), never `-32800`.
#[test]
fn t6g2_a_dropped_intake_is_a_session_ended_error() {
    let dropped = crate::agent::dropped_intake_error();
    assert_eq!(
        dropped,
        V2IntakeError::Failed(crate::agent::session_error_jsonrpc(&SessionError::Closed))
    );
    let error = crate::v2::prompt::intake_error(&dropped);
    assert_eq!(i32::from(error.code), -32603);
    assert_eq!(
        error.message,
        "Internal error: The Claude Agent session has ended. Please start a new session."
    );
    assert_eq!(error.data, None);
    // Only an explicit cancel stays -32800.
    let cancelled = crate::v2::prompt::intake_error(&V2IntakeError::Cancelled);
    assert_eq!(i32::from(cancelled.code), -32800);
}

// ---------------------------------------------------------------- M2

/// M2 (`review-T6G`): the cancel's parent-round-trip prelude closes a withdrawn
/// request with `aborted_by_cli`, so the work reports no `running` before its
/// `idle cancelled` (`acp-agent.ts:9595-9603`).
#[tokio::test]
async fn t6g2_a_cancel_withdraws_the_open_permission_before_the_idle() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.map_state.v2.open_input("req-1", None);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["requires_action"]);
    // The cancel marks every open request before it withdraws them
    // (`cancelTurns`: `input.cancelled = true`).
    rig.map_state.v2.cancel_inputs();

    // The round-trip the client has open when the cancel arrives.
    let mut permissions: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    permissions.insert(
        "req-1".to_string(),
        tokio::spawn(async {
            std::future::pending::<()>().await;
        }),
    );
    let mut elicitations = HashMap::new();
    let mut ask = HashMap::new();
    let mut await_tokens = HashMap::new();
    cancel_parent_round_trips(
        &mut rig.tasks,
        &mut rig.map_state,
        &mut PendingClientRequests {
            permissions: &mut permissions,
            elicitations: &mut elicitations,
            ask: &mut ask,
            await_tokens: &mut await_tokens,
        },
        &rig.tx,
        "s",
    );
    let (updates, _) = v2_out(&mut rig);
    assert!(
        updates.is_empty(),
        "a withdrawn request reports no running: {updates:?}"
    );
    assert!(permissions.is_empty(), "the round-trip was aborted");

    // The cancel then ends the turn at the interrupt's idle: `idle cancelled`,
    // with no `running` in between.
    let events = rig.machine.cancel();
    let _ = settle(&mut rig, events);
    let events = rig.machine.on_idle();
    let updates = settle(&mut rig, events);
    assert_eq!(trace(&updates), vec!["idle cancelled"], "{updates:?}");
}

// ---------------------------------------------------------------- M4

/// M4 (`review-T6G`): a steered held turn ends its foreground work at the
/// steered cycle's idle (`acp-agent.ts:6188-6226`).
#[test]
fn t6g2_a_steered_held_turn_ends_its_work_at_the_steered_idle() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.tool_use(
        "toolu_AG",
        "Agent",
        json!({"description":"Research","prompt":"x"}),
    );
    rig.feed(subagent_started("sub1"));
    rig.feed(result_frame("end_turn"));
    let (updates, _) = v2_out(&mut rig);
    let states: Vec<String> = trace(&updates)
        .into_iter()
        .filter(|kind| kind == "running" || kind.starts_with("idle"))
        .collect();
    assert_eq!(states, vec!["idle end_turn"], "{updates:?}");

    // A steer of the held turn: its continuation becomes the work again, and
    // the settlement moves to the steer lane.
    assert!(rig.machine.mark_steered("steer-1"));
    assert_eq!(rig.machine.take_steer_resumed().as_deref(), Some("p1"));
    rig.map_state.v2.note_resumed("p1");
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["running"], "{updates:?}");

    // The steered message is picked up and its result arrives; the next idle
    // settles the steered cycle and ends the work (`pushHeld`).
    rig.feed(
        json!({"type":"user","uuid":"steer-1","message":{"role":"user",
        "content":[{"type":"text","text":"more"}]}}),
    );
    rig.feed(result_frame("end_turn"));
    v2_out(&mut rig);
    let events = rig.machine.on_idle();
    let updates = settle(&mut rig, events);
    assert_eq!(
        trace(&updates),
        vec!["idle end_turn"],
        "the steered idle ends the work: {updates:?}"
    );
    assert!(rig.machine.has_active(), "v1 still holds the turn open");
}

// ---------------------------------------------------------------- M5

/// M5 (`review-T6G`): the clear-context restart makes a held turn's
/// continuation the work again (`startForeground`, `acp-agent.ts:7119-7130`).
#[test]
fn t6g2_a_restart_resumes_a_held_turn() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.tool_use(
        "toolu_AG",
        "Agent",
        json!({"description":"Research","prompt":"x"}),
    );
    rig.feed(subagent_started("sub1"));
    rig.feed(result_frame("end_turn"));
    v2_out(&mut rig);
    // The production line of the restart: the machine stamps the adopted turn
    // with its hold (`take_active_for_restart`), and the continuation resumes.
    let turn = rig
        .machine
        .take_active_for_restart()
        .expect("the active turn is taken for the restart");
    let held_at_restart = turn.held_at_restart();
    assert!(held_at_restart, "the restart sees the held turn");
    resume_held_turn_after_restart(&mut rig.map_state, &rig.tx, "s", held_at_restart, "p1");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["running"], "{updates:?}");
}

// ---------------------------------------------------------------- NEW-4

/// NEW-4: a clear-context restart's withdrawn forms are not a cancel, so the
/// work reports `running` for its continuation (`acp-agent.ts:9600-9604`).
#[test]
fn t6g2_a_restart_reports_running_for_the_continuation() {
    let mut rig = rig_v2();
    let _intake = queue_v2(&mut rig, "p1");
    echo(&mut rig, "p1");
    v2_out(&mut rig);
    rig.map_state.v2.open_input("req-1", None);
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["requires_action"]);
    rig.map_state.v2.reset_for_fresh_context();
    drain_v2(&mut rig.map_state, &rig.tx, "s");
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(trace(&updates), vec!["running"], "{updates:?}");
}

// ---------------------------------------------------------------- N5 / T-105

/// N5 (`review-T6FBFC`) + T-105: on a v2 connection the hook ids are the v2
/// registration order, and `hook_0` (PreToolUse) dispatches the change preview
/// of an Edit whose tool call the client does not hold yet.
#[test]
fn t6g2_a_v2_pretool_use_hook_shows_the_exact_patch() {
    let mut rig = rig_v2();
    let dir = std::env::temp_dir().join(format!("acp-t6g2-preview-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = dir.join("app.ts");
    std::fs::write(&file, "line 1\nconst value = 1;\nline 3\n").expect("write file");
    rig.ext.cwd = dir.clone();
    let input = json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Edit",
        "tool_input": {
            "file_path": file.to_string_lossy(),
            "old_string": "const value = 1;",
            "new_string": "const value = 2;",
        },
        "tool_use_id": "toolu_edit",
    });
    let (reply, _done) = oneshot::channel();
    handle_hook_callback(
        &mut rig.tasks,
        HookCall {
            request_id: "hookreq1",
            callback_id: "hook_0",
            input: &input,
            tool_use_id: Some("toolu_edit"),
        },
        &mut rig.map_state,
        &mut rig.ext,
        HookIo {
            update_tx: &rig.tx,
            command_tx: &mpsc::unbounded_channel().0,
            session_id: "s",
            live_background: &HashMap::new(),
        },
        reply,
    );
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(
        trace(&updates),
        vec!["tool_call"],
        "the tool call goes out with the patch: {updates:?}"
    );
    let patch = updates[0]["content"][0]["patch"]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        patch.contains("-const value = 1;\n+const value = 2;"),
        "the exact patch precedes the Edit: {patch}"
    );
    assert!(rig.map_state.has_emitted("toolu_edit"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// N5: the v2 `hook_1` is PostToolUse. With the v1 constants it ran
/// PostModelSwitch, which produced nothing.
#[test]
fn t6g2_a_v2_hook_1_is_post_tool_use() {
    let mut rig = rig_v2();
    // The streamed tool use registers the PostToolUse entry.
    rig.tool_use("toolu_1", "Bash", json!({"command": "ls"}));
    rig.updates();
    let input = json!({
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "ls"},
        "tool_response": {"stdout": "a.txt"},
        "tool_use_id": "toolu_1",
    });
    let (reply, done) = oneshot::channel();
    handle_hook_callback(
        &mut rig.tasks,
        HookCall {
            request_id: "hookreq2",
            callback_id: "hook_1",
            input: &input,
            tool_use_id: Some("toolu_1"),
        },
        &mut rig.map_state,
        &mut rig.ext,
        HookIo {
            update_tx: &rig.tx,
            command_tx: &mpsc::unbounded_channel().0,
            session_id: "s",
            live_background: &HashMap::new(),
        },
        reply,
    );
    let (updates, _) = v2_out(&mut rig);
    assert_eq!(
        trace(&updates),
        vec!["tool_call_update"],
        "the post-tool-use update: {updates:?}"
    );
    assert_eq!(updates[0]["toolCallId"], "toolu_1");
    let answer = done.blocking_recv().expect("the hook answered");
    assert_eq!(answer["response"]["response"]["continue"], true);
}

/// N5: the v2 `hook_5` is TaskCompleted; with the v1 constants it was unknown
/// and the plan never moved.
#[test]
fn t6g2_a_v2_hook_5_is_task_completed() {
    let mut rig = rig_v2();
    let created = json!({"task_id": "1", "task_subject": "Ship", "task_description": "d"});
    let (reply, _done) = oneshot::channel();
    handle_hook_callback(
        &mut rig.tasks,
        HookCall {
            request_id: "hookreq3",
            callback_id: "hook_4",
            input: &created,
            tool_use_id: None,
        },
        &mut rig.map_state,
        &mut rig.ext,
        HookIo {
            update_tx: &rig.tx,
            command_tx: &mpsc::unbounded_channel().0,
            session_id: "s",
            live_background: &HashMap::new(),
        },
        reply,
    );
    assert_eq!(rig.ext.task_state.len(), 1, "the task was created");
    let done_input = json!({"task_id": "1", "task_subject": "Ship"});
    let (reply, _done) = oneshot::channel();
    handle_hook_callback(
        &mut rig.tasks,
        HookCall {
            request_id: "hookreq4",
            callback_id: "hook_5",
            input: &done_input,
            tool_use_id: None,
        },
        &mut rig.map_state,
        &mut rig.ext,
        HookIo {
            update_tx: &rig.tx,
            command_tx: &mpsc::unbounded_channel().0,
            session_id: "s",
            live_background: &HashMap::new(),
        },
        reply,
    );
    assert_eq!(
        rig.ext.task_state[0].1.status, "completed",
        "hook_5 completed the task"
    );
}

// ---------------------------------------------------------------- LOW

/// The `idle` usage puts `totalTokens` last (`sessionUsage`,
/// `acp-agent.ts:11855-11862`).
#[test]
fn t6g2_the_idle_usage_puts_total_tokens_last() {
    let usage = crate::turn_events::TurnUsage {
        total_tokens: 11,
        input_tokens: 7,
        output_tokens: 4,
        thought_tokens: None,
        cached_read_tokens: Some(2),
        cached_write_tokens: Some(3),
    };
    assert_eq!(
        serde_json::to_string(&usage.to_json()).unwrap(),
        r#"{"inputTokens":7,"outputTokens":4,"cachedReadTokens":2,"cachedWriteTokens":3,"totalTokens":11}"#
    );
    let update = crate::v2::prompt::idle_update(&crate::turn_events::TurnOutcome {
        stop_reason: StopReason::EndTurn,
        usage: Some(usage),
        meta: None,
    });
    assert_eq!(
        serde_json::to_string(&update).unwrap(),
        r#"{"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn","usage":{"inputTokens":7,"outputTokens":4,"cachedReadTokens":2,"cachedWriteTokens":3,"totalTokens":11}}"#
    );
}

/// NEW-9: an update the v2 surface cannot translate is logged rather than
/// dropped silently (`Failed to send a … update`, `v2/agent.ts:243-249`).
#[test]
fn t6g2_an_untranslatable_update_is_logged() {
    let path = std::env::temp_dir().join(format!("acp-t6g2-log-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let logger = std::sync::Arc::new(crate::logger::Logger::to_file(&path));
    let mut out = crate::v2::agent::V2Outbound::new().with_logger(Some(logger));
    let notifications = out.translate("s", json!({"sessionUpdate": "made_up_kind", "x": 1}));
    assert!(notifications.is_empty());
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        text.contains("failed to send a made_up_kind update"),
        "the failure is logged: {text}"
    );
    let _ = std::fs::remove_file(&path);
}
