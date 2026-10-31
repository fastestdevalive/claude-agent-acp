//! Session-level tests added by Turn 3 (lane A, T3A): the session failure
//! controller (F25), plan mode (F21), the clear-context restart (F22) and the
//! background command terminals (F15).

use super::tests_t2a::Rig;
use super::*;
use crate::session_failure::ClaudeFailureKind;

fn air_rig(capabilities: &[&str]) -> Rig {
    let mut rig = Rig::new();
    let caps =
        json!({"_meta": {"jetbrains": {"air": {"version": 1, "capabilities": capabilities}}}});
    rig.ext.air = crate::air::AirClient::from_client_capabilities(&caps);
    rig
}

fn session_scoped() -> failures::FailureOptions {
    failures::FailureOptions {
        session_scoped: true,
        ..failures::FailureOptions::default()
    }
}

fn active_kinds(rig: &Rig) -> Vec<ClaudeFailureKind> {
    rig.ext
        .t2a
        .failures
        .active()
        .iter()
        .map(|failure| failure.failure.kind)
        .collect()
}

fn model_answer(rig: &mut Rig) {
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_a","model":"claude-sonnet-4-5","role":"assistant",
        "content":[{"type":"text","text":"ok"}],
        "usage":{"input_tokens":1,"output_tokens":1}}}));
}

fn success_result() -> Value {
    json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})
}

// ---------------------------------------------------------------- F25

/// N-22 — a finished login (`auth_status`, not authenticating, no error)
/// retires the signed-out failures and leaves the others alone; a login still
/// in flight, or one that errored, retires nothing.
#[test]
fn t3a_auth_status_frame_clears_auth_required_failures() {
    let mut rig = air_rig(&["sessionFailure"]);
    for kind in [
        ClaudeFailureKind::AuthRequired,
        ClaudeFailureKind::RateLimited,
    ] {
        rig.ext.t2a.failures.publish("s", kind, &session_scoped());
    }
    rig.feed(json!({"type":"auth_status","isAuthenticating":true,"output":[]}));
    rig.feed(json!({"type":"auth_status","isAuthenticating":false,"error":"denied","output":[]}));
    assert_eq!(
        active_kinds(&rig).len(),
        2,
        "an unfinished login clears nothing"
    );
    rig.feed(json!({"type":"auth_status","isAuthenticating":false,"output":[]}));
    assert_eq!(active_kinds(&rig), vec![ClaudeFailureKind::RateLimited]);
}

/// N-22 — the frame goes through the real routing (`dispatch_stream`), which
/// used to drop `auth_status` as an unknown type.
#[tokio::test]
async fn t3fa_auth_status_frame_is_routed_to_the_session() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.ext
        .t2a
        .failures
        .publish("s", ClaudeFailureKind::AuthRequired, &session_scoped());
    let (control_tx, _control_rx) = mpsc::unbounded_channel();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        dispatch_stream(
            StreamMsg::new(json!({"type":"auth_status","isAuthenticating":false,"output":[]})),
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
    assert!(active_kinds(&rig).is_empty());
}

/// V-15 — a successful result with a real model answer retires the failures
/// that wait for one (a usage limit, a signed-out record); an error result, or
/// a result without a model answer, retires neither.
#[test]
fn t3a_real_model_answer_recovers_quota_and_auth_failures() {
    let mut rig = air_rig(&["sessionFailure"]);
    for kind in [
        ClaudeFailureKind::QuotaExhausted,
        ClaudeFailureKind::AuthRequired,
        ClaudeFailureKind::TransportLost,
    ] {
        rig.ext.t2a.failures.publish("s", kind, &session_scoped());
    }
    rig.echo("p1");
    rig.feed(success_result());
    assert_eq!(
        active_kinds(&rig).len(),
        3,
        "no model answered in this turn: nothing recovers"
    );
    rig.echo("p2");
    model_answer(&mut rig);
    let mut errored = success_result();
    errored["is_error"] = json!(true);
    errored["result"] = json!("boom");
    rig.feed(errored);
    let kinds = active_kinds(&rig);
    assert!(
        kinds.contains(&ClaudeFailureKind::QuotaExhausted)
            && kinds.contains(&ClaudeFailureKind::AuthRequired),
        "an error result recovers nothing: {kinds:?}"
    );
    rig.echo("p3");
    model_answer(&mut rig);
    rig.feed(success_result());
    let kinds = active_kinds(&rig);
    assert!(
        !kinds.contains(&ClaudeFailureKind::QuotaExhausted)
            && !kinds.contains(&ClaudeFailureKind::AuthRequired)
            && kinds.contains(&ClaudeFailureKind::TransportLost),
        "a real answer recovers the quota and auth records only: {kinds:?}"
    );
}

/// V-06 — `worker_shutting_down` only arms the signal: nothing is published and
/// no turn fails until the transport ends.
#[test]
fn t3a_worker_shutting_down_only_arms_the_signal() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.feed(json!({"type":"system","subtype":"worker_shutting_down"}));
    assert!(rig.ext.t2a.pending_worker_shutdown);
    assert!(rig.updates().is_empty());
    assert!(rig.ext.t2a.failures.active().is_empty());
    assert!(rig.machine.has_active());
}

/// T8A — a restored quota failure (as `session/load` replay leaves it) is
/// cleared after a follow-up whose real model answer proves recovery.
#[test]
fn t8a_replayed_failure_clears_after_a_successful_follow_up() {
    let mut rig = air_rig(&["sessionFailure"]);
    let restored = rig.ext.t2a.failures.restore(
        "s",
        "p1:error",
        ClaudeFailureKind::QuotaExhausted,
        "You've hit your limit",
        true,
    );
    assert_eq!(
        restored["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"]["id"],
        "p1:error"
    );
    assert_eq!(active_kinds(&rig), vec![ClaudeFailureKind::QuotaExhausted]);
    rig.echo("p2");
    model_answer(&mut rig);
    rig.feed(success_result());
    assert!(
        active_kinds(&rig).is_empty(),
        "the recovered failure is no longer active"
    );
}

/// T8A — a synthetic local-command frame does not prove recovery: a restored
/// quota failure stays active, and its zero usage is not published.
#[test]
fn t8a_a_local_command_does_not_clear_restored_quota() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.ext.t2a.failures.restore(
        "s",
        "p1:error",
        ClaudeFailureKind::QuotaExhausted,
        "You've hit your limit",
        true,
    );
    rig.echo("p2");
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"api-local-command-result","model":"<synthetic>",
            "role":"assistant","content":[{"type":"text","text":"No response requested."}],
            "usage":{"input_tokens":0,"output_tokens":0,
                "cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}));
    rig.feed(success_result());
    let updates = rig.updates();
    assert!(
        updates.iter().all(|u| u["sessionUpdate"] != "usage_update"),
        "the synthetic zero usage is not published: {updates:?}"
    );
    assert_eq!(active_kinds(&rig), vec![ClaudeFailureKind::QuotaExhausted]);
}

// ---------------------------------------------------------------- F21

fn exit_plan_use(rig: &mut Rig, plan: &str) {
    rig.tool_use("toolu_PLAN", "ExitPlanMode", json!({"plan": plan}));
}

fn rejected_user_frame() -> Value {
    json!({"type":"user","parent_tool_use_id":null,
        "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_PLAN",
            "is_error":true,"content":"User chose to keep planning"}]},
        "tool_result_meta":[{"id":"toolu_PLAN","non_execution_kind":"user-rejected"}]})
}

fn interruption_diagnostic() -> Value {
    json!({"type":"result","subtype":"error_during_execution","is_error":true,
        "stop_reason":null,"errors":["[ede_diagnostic] result_type=user last_content_type=tool_result stop_reason=tool_use"],
        "usage":{"input_tokens":3,"output_tokens":4}})
}

fn stop_reason_of(rig: &mut Rig, uuid: &str) -> String {
    let reply = rig
        .replies
        .get_mut(uuid)
        .unwrap()
        .try_recv()
        .expect("replied");
    reply
        .map(|r| r.stop_reason)
        .unwrap_or_else(|f| format!("error: {:?}", f.error))
}

/// B-76 / P-02 — "No, keep planning": the CLI stops the cycle with an
/// error-shaped diagnostic; paired with the recorded permission answer and the
/// tool result it ends the prompt `cancelled`, not as a provider failure.
#[test]
fn t3a_keep_planning_interrupt_ends_the_turn_cancelled() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    exit_plan_use(&mut rig, "the plan");
    rig.ext.t2a.exit_plan.note_answer("toolu_PLAN", None, true);
    rig.feed(rejected_user_frame());
    assert!(
        rig.ext
            .t2a
            .exit_plan
            .interruption
            .as_ref()
            .unwrap()
            .tool_result_seen
    );
    rig.feed(interruption_diagnostic());
    assert_eq!(stop_reason_of(&mut rig, "p1"), "cancelled");
    assert!(
        rig.ext.t2a.exit_plan.interruption.is_none(),
        "the marker is spent"
    );
    assert!(
        rig.ext.t2a.failures.active().is_empty(),
        "no failure is published"
    );
}

/// B-76 — the same diagnostic without the recorded answer is an ordinary error
/// result: the turn fails as it always did.
#[test]
fn t3a_uncorrelated_interrupt_diagnostic_is_still_a_failure() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.feed(interruption_diagnostic());
    assert_ne!(stop_reason_of(&mut rig, "p1"), "cancelled");
}

/// B-76 — a diagnostic that arrives before the tool result was seen does not
/// pair with the marker, and a non-matching result drops the marker so it never
/// leaks into a later turn.
#[test]
fn t3a_interrupt_marker_needs_the_tool_result_and_never_leaks() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.ext.t2a.exit_plan.note_answer("toolu_PLAN", None, true);
    rig.feed(interruption_diagnostic());
    assert_ne!(stop_reason_of(&mut rig, "p1"), "cancelled");
    assert!(
        rig.ext.t2a.exit_plan.interruption.is_none(),
        "dropped with the result"
    );
}

/// B-76 — a cancel forgets both markers and ends the turn cancelled
/// (`acp-agent.test.ts`: "clears pending ExitPlanMode state even after the query
/// has closed"). A husk's actor is gone, so its markers are gone with it and a
/// cancel is refused (`SESSION_GONE`): the client never sees a stale marker.
#[test]
fn t3a_cancel_forgets_the_pending_plan_markers() {
    let mut rig = Rig::new();
    rig.ext
        .t2a
        .exit_plan
        .note_answer("toolu_PLAN", Some(("plan", "auto")), true);
    rig.echo("p1");
    rig.machine.cancel();
    rig.feed(interruption_diagnostic());
    assert!(rig.ext.t2a.exit_plan.interruption.is_none());
    assert!(rig.ext.t2a.exit_plan.context_reset.is_none());
    assert_eq!(
        stop_reason_of(&mut rig, "p1"),
        "cancelled",
        "the cancel also ends the turn"
    );
}

/// U-24 / T-88 — an approved plan of a client that takes plan operations is
/// published as a markdown `plan_update` after the tool result; the output's
/// plan (the user's edits) wins over the input; a client without the
/// capability, a failed result and a subagent's plan publish nothing.
#[test]
fn t3a_approved_plan_is_published_after_the_tool_result() {
    let result = |plan_output: Value| {
        json!({"type":"user","parent_tool_use_id":null,
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_PLAN",
                "content":"User has approved your plan."}]},
            "tool_use_result": plan_output})
    };
    let plan_updates = |updates: &[Value]| -> Vec<Value> {
        updates
            .iter()
            .filter(|u| u["sessionUpdate"] == "plan_update")
            .cloned()
            .collect()
    };
    let mut rig = Rig::new();
    rig.ext.caps.plan = true;
    rig.echo("p1");
    exit_plan_use(&mut rig, " input plan ");
    rig.updates();
    rig.feed(result(json!({"plan": "edited plan"})));
    let updates = rig.updates();
    let kinds: Vec<&str> = updates
        .iter()
        .filter_map(|u| u["sessionUpdate"].as_str())
        .collect();
    assert_eq!(
        kinds.last(),
        Some(&"plan_update"),
        "after the result: {kinds:?}"
    );
    assert_eq!(
        plan_updates(&updates)[0]["plan"],
        json!({"type":"markdown","planId":"plan-mode","content":"edited plan"})
    );

    let mut plain = Rig::new();
    plain.echo("p1");
    exit_plan_use(&mut plain, "p");
    plain.feed(result(json!({"plan": "p"})));
    assert!(
        plan_updates(&plain.updates()).is_empty(),
        "no plan capability"
    );

    let mut agent = Rig::new();
    agent.ext.caps.plan = true;
    agent.echo("p1");
    exit_plan_use(&mut agent, "p");
    agent.feed(result(json!({"plan": "p", "isAgent": true})));
    assert!(plan_updates(&agent.updates()).is_empty(), "a subagent plan");
}

/// T-87 — the accepted clear-context plan's own tool result ends `completed`
/// with neither raw output nor content, though the result is an error.
#[test]
fn t3a_accepted_context_reset_plan_result_is_completed() {
    let mut rig = Rig::new();
    rig.echo("p1");
    exit_plan_use(&mut rig, "the plan");
    rig.ext
        .t2a
        .exit_plan
        .note_answer("toolu_PLAN", Some(("the plan", "auto")), true);
    rig.updates();
    rig.feed(rejected_user_frame());
    let updates = rig.updates();
    let call = updates
        .iter()
        .find(|u| u["toolCallId"] == "toolu_PLAN")
        .expect("the plan call is reported");
    assert_eq!(call["status"], "completed");
    assert!(
        call.get("rawOutput").is_none() && call.get("content").is_none(),
        "{call}"
    );
}

/// P-10 — entering plan mode through the EnterPlanMode hook remembers the mode
/// it left, which the ExitPlanMode options lead with.
#[test]
fn t3a_enter_plan_mode_hook_remembers_the_previous_mode() {
    let mut rig = Rig::new();
    rig.ext.config.modes.current_mode_id = "bypassPermissions".into();
    let input = json!({"hook_event_name":"PostToolUse","tool_name":"EnterPlanMode",
        "tool_input":{},"tool_response":{}});
    let (command_tx, _command_rx) = mpsc::unbounded_channel();
    let (reply, _reply_rx) = oneshot::channel();
    let mut map_state = MapState::default();
    handle_hook_callback(
        &mut rig.tasks,
        HookCall {
            request_id: "r1",
            callback_id: crate::hooks::POST_TOOL_USE_HOOK_ID,
            input: &input,
            tool_use_id: None,
        },
        &mut map_state,
        &mut rig.ext,
        HookIo {
            update_tx: &rig.tx,
            command_tx: &command_tx,
            session_id: "s",
            live_background: &HashMap::new(),
        },
        reply,
    );
    assert_eq!(rig.ext.config.modes.current_mode_id, "plan");
    assert_eq!(
        rig.ext.config.pre_plan_mode.as_deref(),
        Some("bypassPermissions")
    );
}

fn exit_plan_options(pre_plan: Option<&str>) -> crate::permission_options::OptionsContext {
    crate::permission_options::OptionsContext {
        tool_name: "ExitPlanMode".to_string(),
        display_name: None,
        input: json!({"plan": "  ship it "}),
        cwd: PathBuf::from("/repo"),
        change_set: None,
        no_persistent_rule: false,
        default_to_no: false,
        available_modes: [
            "default",
            "acceptEdits",
            "plan",
            "auto",
            "bypassPermissions",
        ]
        .map(String::from)
        .to_vec(),
        auto_unavailable: false,
        pre_plan_mode: pre_plan.map(String::from),
        context_used_percent: None,
        home_dir: None,
    }
}

fn answer_exit_plan(rig: &mut Rig, option_id: &str) -> Value {
    let ctx = exit_plan_options(None);
    let input = json!({"plan": "  ship it "});
    let value = json!({"outcome":{"outcome":"selected","optionId":option_id}});
    resolve_permission_answer(
        PermissionAnswer {
            value: &value,
            ctx: &ctx,
            request_id: "r1",
            tool_use_id: "toolu_PLAN",
            tool_input: &input,
        },
        &mut rig.ext,
        &rig.tx,
        "s",
    )
}

/// B-76 / P-34 — the clear-context answer denies with its own interrupt message
/// and records both the reset (trimmed plan, the option's mode) and the
/// interruption; "keep planning" records the interruption alone, with its own
/// message; an allow records neither.
#[test]
fn t3a_exit_plan_answers_record_the_pending_lanes() {
    let mut rig = Rig::new();
    let response = answer_exit_plan(&mut rig, "exit-plan-clear-auto");
    let body = &response["response"]["response"];
    assert_eq!(body["behavior"], "deny");
    assert_eq!(body["interrupt"], true);
    assert_eq!(
        body["message"],
        "User accepted the plan and requested a fresh context"
    );
    assert_eq!(
        rig.ext.t2a.exit_plan.context_reset,
        Some(exit_plan::ContextReset {
            tool_use_id: "toolu_PLAN".into(),
            plan: "ship it".into(),
            mode: "auto".into(),
        })
    );
    assert_eq!(
        rig.ext
            .t2a
            .exit_plan
            .interruption
            .as_ref()
            .unwrap()
            .tool_use_id,
        "toolu_PLAN"
    );

    let mut rig = Rig::new();
    let response = answer_exit_plan(&mut rig, "reject");
    assert_eq!(
        response["response"]["response"]["message"],
        "User chose to keep planning"
    );
    assert!(rig.ext.t2a.exit_plan.context_reset.is_none());
    assert!(rig.ext.t2a.exit_plan.interruption.is_some());

    let mut rig = Rig::new();
    let response = answer_exit_plan(&mut rig, "exit-plan-default");
    assert_eq!(response["response"]["response"]["behavior"], "allow");
    assert!(rig.ext.t2a.exit_plan.context_reset.is_none());
    assert!(rig.ext.t2a.exit_plan.interruption.is_none());
}

/// P-10 — approving the plan leaves plan mode and forgets the remembered mode.
#[test]
fn t3a_exit_plan_allow_forgets_the_pre_plan_mode() {
    let mut rig = Rig::new();
    rig.ext.config.modes.current_mode_id = "plan".into();
    rig.ext.config.pre_plan_mode = Some("bypassPermissions".into());
    answer_exit_plan(&mut rig, "exit-plan-auto");
    assert_eq!(rig.ext.config.modes.current_mode_id, "auto");
    assert_eq!(rig.ext.config.pre_plan_mode, None);
}

// ---------------------------------------------------------------- F15

fn terminal_client() -> crate::client_caps::ClientCapabilities {
    crate::client_caps::ClientCapabilities {
        terminal_output: true,
        ..crate::client_caps::ClientCapabilities::default()
    }
}

fn background_bash_result() -> Value {
    json!({"type":"user","parent_tool_use_id":null,
        "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_BG",
            "content":"Command running in background with ID: b1. Output is being written to: /tmp/claude/tasks/b1.output"}]},
        "tool_use_result":{"stdout":"","stderr":"","backgroundTaskId":"b1","isImage":false}})
}

fn background_bash_use(rig: &mut Rig) {
    rig.tool_use(
        "toolu_BG",
        "Bash",
        json!({"command":"sleep 30","run_in_background":true}),
    );
}

fn by_id<'a>(updates: &'a [Value], id: &str) -> Vec<&'a Value> {
    updates.iter().filter(|u| u["toolCallId"] == id).collect()
}

/// T-11 / T-96 / T-34 — for an AIR client the structured Bash result announces
/// the task (with the output path read from the result text) before the card's
/// update, and the card carries the `backgrounded` marker.
#[test]
fn t3a_background_bash_result_announces_the_task_and_marks_the_card() {
    let mut rig = air_rig(&["asyncTasks"]);
    rig.tasks = crate::async_tasks::AsyncTaskRuntime::new(true);
    rig.echo("p1");
    background_bash_use(&mut rig);
    rig.updates();
    rig.feed(background_bash_result());
    let updates = rig.updates();
    let kinds: Vec<&str> = updates
        .iter()
        .filter_map(|u| u["sessionUpdate"].as_str())
        .collect();
    assert_eq!(
        kinds,
        ["async_task_spawned", "tool_call_update"],
        "{updates:?}"
    );
    assert_eq!(updates[0]["asyncTaskId"], "b1");
    assert_eq!(updates[0]["toolCallId"], "toolu_BG");
    assert_eq!(updates[0]["outputFilePath"], "/tmp/claude/tasks/b1.output");
    assert_eq!(
        updates[1]["_meta"]["jetbrains"]["air"]["asyncTasks"],
        json!({"backgrounded": true})
    );
}

/// T-34 — a client without the async-task lifecycle is never sent the marker, and
/// no task.
#[test]
fn t3a_background_bash_marker_is_for_async_task_clients_only() {
    let mut rig = Rig::new();
    rig.echo("p1");
    background_bash_use(&mut rig);
    rig.updates();
    rig.feed(background_bash_result());
    let updates = rig.updates();
    assert!(updates
        .iter()
        .all(|u| u["sessionUpdate"] == "tool_call_update"));
    assert!(updates
        .iter()
        .all(|u| u.get("_meta").is_none() || u["_meta"].get("jetbrains").is_none()));
}

/// T-06 / T-11 / T-34 — for a terminal client the command's display terminal is
/// registered and the result's `terminal_exit` is withheld; the end of the task
/// ends it with a null exit code.
#[test]
fn t3a_background_command_keeps_its_terminal_running_until_the_task_ends() {
    let mut rig = Rig::new();
    rig.ext.caps = terminal_client();
    rig.map_state.set_client_caps(terminal_client());
    rig.echo("p1");
    rig.feed(
        json!({"type":"system","subtype":"task_started","task_id":"b1",
        "task_type":"local_bash","description":"sleep 30","tool_use_id":"toolu_BG"}),
    );
    background_bash_use(&mut rig);
    rig.updates();
    rig.feed(background_bash_result());
    let updates = rig.updates();
    let card = by_id(&updates, "toolu_BG");
    assert!(!card.is_empty(), "{updates:?}");
    assert!(
        card.iter()
            .all(|u| u["_meta"].get("terminal_exit").is_none()),
        "the exit waits for the task: {updates:?}"
    );
    assert!(rig.ext.t2a.background_terminals.contains_key("b1"));
    rig.feed(
        json!({"type":"system","subtype":"task_notification","task_id":"b1",
        "status":"completed","summary":"done"}),
    );
    let ended = rig.updates();
    let exit = ended
        .iter()
        .find(|u| u["_meta"].get("terminal_exit").is_some())
        .expect("the terminal ends with the task");
    assert_eq!(
        exit["_meta"]["terminal_exit"],
        json!({"terminal_id":"toolu_BG","exit_code":null,"signal":null})
    );
    assert!(rig.ext.t2a.background_terminals.is_empty());
}

/// T-11 — a command that already ended when its result comes has no live task:
/// its terminal exits with the result as for any command.
#[test]
fn t3a_background_command_whose_task_ended_exits_with_its_result() {
    let mut rig = Rig::new();
    rig.ext.caps = terminal_client();
    rig.map_state.set_client_caps(terminal_client());
    rig.echo("p1");
    background_bash_use(&mut rig);
    rig.updates();
    rig.feed(background_bash_result());
    let updates = rig.updates();
    assert!(
        by_id(&updates, "toolu_BG")
            .iter()
            .any(|u| u["_meta"].get("terminal_exit").is_some()),
        "{updates:?}"
    );
    assert!(rig.ext.t2a.background_terminals.is_empty());
}

// ---------------------------------------------------------------- M-65

fn board_events(items: Vec<SessionOutbound>) -> Vec<BoardEvent> {
    items
        .into_iter()
        .filter_map(|item| match item {
            SessionOutbound::Board(event) => Some(event),
            _ => None,
        })
        .collect()
}

fn drain_board_events(rig: &mut Rig) -> Vec<BoardEvent> {
    let mut items = Vec::new();
    while let Ok(item) = rig.rx.try_recv() {
        items.push(item);
    }
    board_events(items)
}

fn state_frame(state: &str) -> Value {
    json!({"type":"system","subtype":"session_state_changed","state":state})
}

/// M-65 — the CLI's own state reaches the session index: a permission request that
/// waits (`requires_action`) shows on the session's row, `running` starts a turn,
/// `idle` ends it with an end time.
#[test]
fn t3a_session_state_frames_reach_the_board() {
    use crate::session_index::activity::ActivityState;
    use crate::session_index::connection::OwnSessionBoard;
    let mut rig = Rig::new();
    rig.feed(state_frame("running"));
    rig.feed(state_frame("requires_action"));
    rig.feed(state_frame("bogus"));
    let events = drain_board_events(&mut rig);
    assert_eq!(
        events,
        [
            BoardEvent::State(ActivityState::Running),
            BoardEvent::State(ActivityState::RequiresAction),
        ]
    );
    let board = OwnSessionBoard::new();
    board.begin("s");
    for event in events {
        event.apply(&board, "s");
    }
    assert_eq!(
        board.get("s").unwrap().state,
        Some(ActivityState::RequiresAction)
    );
    rig.feed(state_frame("idle"));
    for event in drain_board_events(&mut rig) {
        event.apply(&board, "s");
    }
    let own = board.get("s").unwrap();
    assert_eq!(own.state, Some(ActivityState::Idle));
    assert!(own.last_turn_ended_at.is_some());
}

/// M-65 — an error result marks the turn failed and the next turn's result clears
/// the mark; a cancelled turn, an autonomous result and the interruption of an
/// ExitPlanMode answer neither mark nor clear it.
#[test]
fn t3a_turn_failure_reaches_the_board_per_result() {
    let error = json!({"type":"result","subtype":"success","is_error":true,"result":"boom",
        "usage":{"input_tokens":1,"output_tokens":1}});
    let mut rig = Rig::new();
    rig.echo("p1");
    drain_board_events(&mut rig);
    rig.feed(error.clone());
    assert_eq!(drain_board_events(&mut rig), [BoardEvent::TurnFailed(true)]);
    rig.echo("p2");
    rig.feed(success_result());
    assert_eq!(
        drain_board_events(&mut rig),
        [BoardEvent::TurnFailed(false)]
    );

    let mut max_turns = Rig::new();
    max_turns.echo("p1");
    drain_board_events(&mut max_turns);
    max_turns.feed(
        json!({"type":"result","subtype":"error_max_turns","is_error":false,
        "usage":{"input_tokens":1,"output_tokens":1}}),
    );
    assert_eq!(
        drain_board_events(&mut max_turns),
        [BoardEvent::TurnFailed(true)]
    );

    let mut cancelled = Rig::new();
    cancelled.echo("p1");
    drain_board_events(&mut cancelled);
    cancelled.machine.cancel();
    cancelled.feed(error.clone());
    assert!(
        drain_board_events(&mut cancelled).is_empty(),
        "a cancel is not a failure"
    );

    let mut autonomous = Rig::new();
    autonomous.echo("p1");
    drain_board_events(&mut autonomous);
    let mut background = error.clone();
    background["origin"] = json!({"kind":"task-notification"});
    autonomous.feed(background);
    assert!(drain_board_events(&mut autonomous).is_empty());

    let mut planning = Rig::new();
    planning.echo("p1");
    planning
        .ext
        .t2a
        .exit_plan
        .note_answer("toolu_PLAN", None, true);
    planning.feed(rejected_user_frame());
    drain_board_events(&mut planning);
    planning.feed(interruption_diagnostic());
    assert!(
        drain_board_events(&mut planning).is_empty(),
        "the keep-planning interrupt is not a failure"
    );

    // A refusal settles before the failure mark upstream, whatever its shape.
    for (is_error, subtype) in [
        (false, "success"),
        (true, "success"),
        (true, "error_during_execution"),
    ] {
        let mut refused = Rig::new();
        refused.echo("p1");
        drain_board_events(&mut refused);
        refused.feed(
            json!({"type":"result","subtype":subtype,"is_error":is_error,
            "stop_reason":"refusal","usage":{"input_tokens":1,"output_tokens":1}}),
        );
        assert!(
            drain_board_events(&mut refused).is_empty(),
            "a refusal is not a failure ({subtype}, is_error {is_error})"
        );
    }

    // The interrupted cycle's diagnostic of a steered turn with echoes pending.
    let mut steered = Rig::new();
    steered.echo("p1");
    drain_board_events(&mut steered);
    assert!(steered.machine.mark_steered("steer1"));
    steered.feed(error.clone());
    assert!(
        drain_board_events(&mut steered).is_empty(),
        "the steer diagnostic is not a failure"
    );
}

/// M-65 — an AIR failure answer (a prompt response with failure metadata) also
/// marks the turn failed.
#[test]
fn t3a_air_failure_answer_marks_the_turn_failed_on_the_board() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    drain_board_events(&mut rig);
    rig.feed(
        json!({"type":"result","subtype":"error_max_budget_usd","is_error":true,
        "errors":["budget"],"usage":{"input_tokens":1,"output_tokens":1}}),
    );
    let events = drain_board_events(&mut rig);
    assert!(events.contains(&BoardEvent::TurnFailed(true)), "{events:?}");
}

/// M-65 — a child that dies mid-turn leaves the session's row at `error` until
/// the next turn starts; a clean exit, and a death after a cancel, do not.
#[tokio::test]
async fn t3a_child_death_keeps_the_error_row_until_the_next_turn() {
    use crate::session::tests_t1ha::Rig as ActorRig;
    use crate::session_index::activity::ActivityState;
    use crate::session_index::connection::OwnSessionBoard;
    let mut rig = ActorRig::spawn(None);
    let reply = rig.prompt("p1");
    rig.echo("p1");
    rig.end_stream(Some(true));
    let events = board_events(rig.outbound_until_end().await);
    drop(reply);
    assert!(events.contains(&BoardEvent::QueryFailed), "{events:?}");
    let board = OwnSessionBoard::new();
    board.begin("s1");
    for event in events {
        event.apply(&board, "s1");
    }
    let own = board.get("s1").unwrap();
    assert!(own.query_failed_at.is_some() && own.last_turn_failed);
    // The next turn starts: the earlier failed query no longer counts.
    BoardEvent::State(ActivityState::Running).apply(&board, "s1");
    assert!(board.get("s1").unwrap().query_failed_at.is_none());

    let mut clean = ActorRig::spawn(None);
    let _reply = clean.prompt("p1");
    clean.echo("p1");
    clean.end_stream(Some(false));
    assert!(!board_events(clean.outbound_until_end().await).contains(&BoardEvent::QueryFailed));

    let mut cancelled = ActorRig::spawn(None);
    let _reply = cancelled.prompt("p1");
    cancelled.echo("p1");
    cancelled.tx.send(Command::Cancel).unwrap();
    cancelled.end_stream(Some(true));
    assert!(
        !board_events(cancelled.outbound_until_end().await).contains(&BoardEvent::QueryFailed),
        "the abort a cancel started can kill the stream: that is not a failure"
    );
}

// ------------------------------------------------------------ Opus review gaps

/// T2GA-2 (review gap) — a swallowed empty-interruption diagnostic reaches
/// neither failure sweep: two identical advisories of one turn with a cancel and
/// that diagnostic between them stay one advisory, updated in place
/// (`acp-agent.ts:7177-7197` breaks before `clearFailuresFromEarlierTurns`).
/// Without the guard at the result's failure sweep the second advisory would get
/// a fresh id.
#[test]
fn t3a_swallowed_diagnostic_does_not_clear_failures() {
    let advisory = || failures::FailureOptions {
        title: Some("Model fallback".into()),
        ..failures::FailureOptions::default()
    };
    let publish = |rig: &mut Rig| {
        rig.ext
            .t2a
            .failures
            .publish("s", ClaudeFailureKind::Advisory, &advisory())
    };
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    let first = publish(&mut rig);
    // A queued prompt is swept by the cancel: the cancelled cycle may still emit
    // its empty-interruption diagnostic, naming that prompt.
    rig.machine.enqueue(Turn::new("p2".into(), false));
    rig.machine.cancel();
    rig.echo("p3");
    rig.feed(
        json!({"type":"result","subtype":"error_during_execution","is_error":true,
        "user_message_uuid":"p2",
        "errors":["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"],
        "usage":{"input_tokens":0,"output_tokens":0}}),
    );
    assert!(
        rig.machine.has_active(),
        "the diagnostic did not settle the replacement"
    );
    let second = publish(&mut rig);
    let id = |v: &Value| v["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"]["id"].clone();
    assert_eq!(id(&first), id(&second), "{first} / {second}");
    assert_eq!(
        second["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"]["revision"],
        2
    );
}

/// U-38 — the fresh context of an accepted plan publishes its mode, the config
/// options with that mode, and, for a client with plan operations, the plan under
/// the fixed id, in that order; the session is out of plan mode for good.
#[test]
fn t3a_restart_publishes_mode_config_and_plan_in_order() {
    let reset = exit_plan::ContextReset {
        tool_use_id: "toolu_PLAN".into(),
        plan: "ship it".into(),
        mode: "acceptEdits".into(),
    };
    for (plan_ops, expected) in [
        (
            true,
            vec!["current_mode_update", "config_option_update", "plan_update"],
        ),
        (false, vec!["current_mode_update", "config_option_update"]),
    ] {
        let mut rig = Rig::new();
        rig.ext.caps.plan = plan_ops;
        rig.ext.config.modes.current_mode_id = "plan".into();
        rig.ext.config.pre_plan_mode = Some("default".into());
        rig.ext.config.config_options = vec![crate::session_config::ConfigOption {
            id: crate::session_config::MODE_CONFIG_ID.into(),
            name: "Mode".into(),
            description: String::new(),
            category: "mode".into(),
            option_type: "select".into(),
            current_value: "plan".into(),
            options: Vec::new(),
            _meta: None,
        }];
        publish_restart_state(&mut rig.ext, &reset, &rig.tx, "s");
        let updates = rig.updates();
        let kinds: Vec<&str> = updates
            .iter()
            .filter_map(|u| u["sessionUpdate"].as_str())
            .collect();
        assert_eq!(kinds, expected);
        assert_eq!(updates[0]["currentModeId"], "acceptEdits");
        assert_eq!(
            updates[1]["configOptions"][0]["currentValue"],
            "acceptEdits"
        );
        if plan_ops {
            assert_eq!(
                updates[2]["plan"],
                json!({"type":"markdown","planId":"plan-mode","content":"ship it"})
            );
        }
        assert_eq!(rig.ext.config.modes.current_mode_id, "acceptEdits");
        assert_eq!(rig.ext.config.pre_plan_mode, None);
    }
}

// ---------------------------------------------------------------- T3FA

/// V-15 — a refusal settles before upstream's recovery clears, so a refused
/// answer from a real model retires neither a usage limit nor a signed-out record.
#[test]
fn t3fa_refusal_result_does_not_recover_quota_and_auth_failures() {
    let mut rig = air_rig(&["sessionFailure"]);
    for kind in [
        ClaudeFailureKind::QuotaExhausted,
        ClaudeFailureKind::AuthRequired,
    ] {
        rig.ext.t2a.failures.publish("s", kind, &session_scoped());
    }
    rig.echo("p1");
    model_answer(&mut rig);
    let mut refusal = success_result();
    refusal["stop_reason"] = json!("refusal");
    rig.feed(refusal);
    assert_eq!(active_kinds(&rig).len(), 2, "a refusal recovers nothing");
    rig.echo("p2");
    model_answer(&mut rig);
    rig.feed(success_result());
    assert!(active_kinds(&rig).is_empty(), "a real answer does");
}

/// B-108 — a held task is announced before the rollback of a failed `/goal`
/// (`releaseHeld` is the first statement of the result handler).
#[test]
fn t3fa_held_task_is_released_before_the_failed_goal_rollback() {
    let mut rig = air_rig(&["asyncTasks"]);
    rig.tasks = crate::async_tasks::AsyncTaskRuntime::new(true);
    assert!(rig.ext.goal.note_prompt("/goal ship it", "p1").is_some());
    rig.echo("p1");
    rig.feed(
        json!({"type":"system","subtype":"task_started","task_id":"wf1",
        "task_type":"local_workflow","description":"flow","is_backgrounded":true}),
    );
    assert!(
        rig.updates()
            .iter()
            .all(|u| u["sessionUpdate"] != "async_task_spawned"),
        "the task is held while it has no tool call"
    );
    let mut failed = success_result();
    failed["is_error"] = json!(true);
    failed["result"] = json!("goal rejected");
    rig.feed(failed);
    let kinds: Vec<String> = rig
        .updates()
        .iter()
        .map(|u| u["sessionUpdate"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        kinds.first().map(String::as_str),
        Some("async_task_spawned"),
        "{kinds:?}"
    );
    assert!(kinds.len() > 1, "the goal rollback follows: {kinds:?}");
}

/// F22 — what the fresh context starts without: the old worker's shutdown
/// signal and the background terminals of the old child.
#[test]
fn t3fa_fresh_context_drops_the_old_worker_signals_and_terminals() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.ext.t2a.pending_worker_shutdown = true;
    rig.ext.t2a.background_terminals.insert(
        "b1".to_string(),
        state::BackgroundTerminal {
            tool_call_id: "toolu_BG".to_string(),
            parent_tool_use_id: None,
        },
    );
    rig.ext
        .t2a
        .background
        .backgrounded
        .insert("toolu_BG".to_string());
    reset_ext_for_fresh_context(&mut rig.ext);
    assert!(!rig.ext.t2a.pending_worker_shutdown);
    assert!(rig.ext.t2a.background_terminals.is_empty());
    assert!(rig.ext.t2a.background.backgrounded.is_empty());
}

/// F22 — the fresh context's machine keeps the client's stop-workflows-on-cancel
/// and subagent-text choices.
#[test]
fn t3fa_fresh_context_machine_keeps_the_client_choices() {
    assert!(fresh_context_machine(true, true).stops_workflows_on_cancel());
    assert!(!fresh_context_machine(false, false).stops_workflows_on_cancel());
}

/// F22 — commands a restart held back are handled first, in order, ahead of
/// whatever the client sent since: a prompt held across the restart is not
/// overtaken by a later prompt, and a cancel that follows it sees it.
#[tokio::test]
async fn t3fa_held_commands_come_before_newer_ones() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut held = std::collections::VecDeque::new();
    let prompt = |uuid: &str| {
        let (reply, _rx) = oneshot::channel();
        Command::Prompt {
            uuid: uuid.to_string(),
            frame: json!({}),
            is_local_only: false,
            prompt_text: String::new(),
            reply,
        }
    };
    held.push_back(prompt("held1"));
    held.push_back(prompt("held2"));
    tx.send(prompt("newer")).unwrap();
    tx.send(Command::Cancel).unwrap();
    let mut order = Vec::new();
    for _ in 0..4 {
        let next = timeout_next(&mut held, &mut rx).await;
        order.push(match next {
            Command::Prompt { uuid, .. } => uuid,
            Command::Cancel => "cancel".to_string(),
            _ => "other".to_string(),
        });
    }
    assert_eq!(order, ["held1", "held2", "newer", "cancel"]);
}

async fn timeout_next(
    held: &mut std::collections::VecDeque<Command>,
    rx: &mut mpsc::UnboundedReceiver<Command>,
) -> Command {
    tokio::time::timeout(std::time::Duration::from_secs(5), next_command(held, rx))
        .await
        .expect("a command is ready")
        .expect("the channel is open")
}
