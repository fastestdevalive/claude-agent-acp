//! T4FD fix round 2: the elicitation/subagent hazards found by the T4A2 and
//! T4A3 reviews — the cancel signal, the AskUserQuestion control responses, a
//! withdrawn request, the awaiting-user token, the async-task route, the parent
//! cancel of the RFD form and the verbatim hook updates.
//!
//! The harness drives `handle_permission` and the actor's helpers with the state
//! a running actor holds; no child process is involved.

use super::tests_t2a::Rig;
use super::*;
use crate::acp_subagents::SubagentForm;

/// A [`Rig`] with a native subagent runtime of `form` rooted at `"s"`.
struct FdRig {
    rig: Rig,
}

impl FdRig {
    fn new(form: Option<SubagentForm>) -> Self {
        let mut rig = Rig::new();
        rig.tasks = AsyncTaskRuntime::new(true).with_subagent_form(form, "s");
        rig.ext.caps.subagent_form = form;
        Self { rig }
    }

    /// Every `session/update` notification since the last call, as
    /// `{"sessionId", "update"}` values.
    fn notifications(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(item) = self.rig.rx.try_recv() {
            match item {
                SessionOutbound::Update(notif) => {
                    out.push(serde_json::to_value(&notif).unwrap());
                }
                SessionOutbound::RawUpdate(value) => out.push(value),
                _ => {}
            }
        }
        out
    }

    /// The `CreateElicitation` outbound, with its reply sender.
    async fn create_elicitation(&mut self) -> oneshot::Sender<Result<Value, String>> {
        loop {
            let item = tokio::time::timeout(Duration::from_secs(2), self.rig.rx.recv())
                .await
                .expect("the client form is sent")
                .expect("the outbound channel stays open");
            if let SessionOutbound::CreateElicitation { reply, .. } = item {
                return reply;
            }
        }
    }

    /// The `RequestPermission` outbound, with its reply sender.
    async fn request_permission(&mut self) -> oneshot::Sender<Result<Value, String>> {
        loop {
            let item = tokio::time::timeout(Duration::from_secs(2), self.rig.rx.recv())
                .await
                .expect("the permission request is sent")
                .expect("the outbound channel stays open");
            if let SessionOutbound::RequestPermission { reply, .. } = item {
                return reply;
            }
        }
    }

    fn start_child(&mut self, task_id: &str, tool_use_id: &str) {
        self.rig
            .feed(json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_a","role":"assistant",
            "content":[{"type":"tool_use","id":tool_use_id,"name":"Agent",
                "input":{"description":"Fix","prompt":"Make it pass"}}]}}));
        self.rig
            .feed(json!({"type":"system","subtype":"task_started",
            "task_id": task_id, "tool_use_id": tool_use_id,
            "subagent_type": "general-purpose", "description": "Fix",
            "prompt": "Make it pass"}));
        let _ = self.notifications();
    }

    fn child_tool_call(&mut self, parent: &str, id: &str) {
        self.rig
            .feed(json!({"type":"assistant","parent_tool_use_id":parent,
            "message":{"id":"msg_c","role":"assistant",
            "content":[{"type":"tool_use","id":id,"name":"Bash","input":{"command":"ls"}}]}}));
    }
}

fn ask_input() -> Value {
    json!({
        "questions": [{
            "question": "Which framework?",
            "header": "Framework",
            "multiSelect": false,
            "options": [{ "label": "Actix" }, { "label": "Axum" }]
        }]
    })
}

fn ask_frame() -> Value {
    json!({"type":"control_request","request_id":"ask-1","request":{
        "subtype":"can_use_tool","tool_name":"AskUserQuestion",
        "input": ask_input(),"tool_use_id":"toolu_ask","permission_suggestions":[]}})
}

/// Run `handle_permission` for `frame` on `rig`, returning the actor state it
/// filled (the pending registries and the command receiver).
struct PermissionRun {
    pending_permissions: HashMap<String, tokio::task::JoinHandle<()>>,
    tokens: HashMap<String, crate::native_subagents::AwaitToken>,
    pending_ask: HashMap<String, (oneshot::Sender<Value>, Value)>,
    command_rx: mpsc::UnboundedReceiver<Command>,
    /// Keeps the session's abort signal open: a dropped sender would read as a
    /// close and settle every round-trip as aborted.
    cancel_tx: watch::Sender<bool>,
}

fn run_permission(rig: &mut FdRig, frame: Value) -> PermissionRun {
    let (reply_tx, _reply_rx) = oneshot::channel();
    let mut pending_permissions = HashMap::new();
    let mut tokens = HashMap::new();
    let mut pending_ask = HashMap::new();
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    handle_permission(
        &mut rig.rig.tasks,
        frame,
        reply_tx,
        None,
        &mut rig.rig.map_state,
        &mut rig.rig.live_background,
        &rig.rig.tx,
        "s",
        &mut pending_permissions,
        &mut tokens,
        &mut pending_ask,
        &cancel_rx,
        rig.rig.ext.caps.elicitation_form,
        std::path::Path::new("/repo"),
        &[],
        None,
        None,
        None,
        &command_tx,
    );
    PermissionRun {
        pending_permissions,
        tokens,
        pending_ask,
        command_rx,
        cancel_tx,
    }
}

/// The first `AskUserQuestionAnswered` the spawned task sends.
async fn answered(run: &mut PermissionRun) -> Value {
    let _keep_alive = &run.cancel_tx;
    loop {
        let command = tokio::time::timeout(Duration::from_secs(2), run.command_rx.recv())
            .await
            .expect("the answer is produced")
            .expect("the actor channel stays open");
        if let Command::AskUserQuestionAnswered { response, .. } = command {
            return response;
        }
    }
}

/// F2 — the allow answer is exactly upstream's
/// `{behavior:"allow", updatedInput, toolUseID}`, with no
/// `decisionClassification` (the SDK appends only `toolUseID`).
#[tokio::test]
async fn t4fd_ask_question_allow_frame_is_exact() {
    let mut rig = FdRig::new(None);
    rig.rig.ext.caps.elicitation_form = true;
    let mut run = run_permission(&mut rig, ask_frame());
    assert_eq!(run.pending_ask.len(), 1, "the question waits for an answer");

    let reply = rig.create_elicitation().await;
    reply
        .send(Ok(
            json!({"action":"accept","content":{"question_0":"Axum"}}),
        ))
        .expect("the form answers");

    let mut expected = ask_input();
    expected["answers"] = json!({"Which framework?": "Axum"});
    assert_eq!(
        answered(&mut run).await,
        json!({"type":"control_response","response":{
            "subtype":"success","request_id":"ask-1",
            "response":{"behavior":"allow","updatedInput":expected,"toolUseID":"toolu_ask"}}}),
        "the allow frame has no decisionClassification"
    );
}

/// F2 — a client error denies with the plain `{behavior:"deny", message,
/// toolUseID}` frame, as upstream's thrown deny does.
#[tokio::test]
async fn t4fd_ask_question_client_error_denies_without_classification() {
    let mut rig = FdRig::new(None);
    rig.rig.ext.caps.elicitation_form = true;
    let mut run = run_permission(&mut rig, ask_frame());
    let reply = rig.create_elicitation().await;
    // The client refused the dialog (a request error, not a dropped channel).
    reply
        .send(Err("client refused the dialog".to_string()))
        .expect("the client answers with an error");
    assert_eq!(
        answered(&mut run).await,
        json!({"type":"control_response","response":{
            "subtype":"success","request_id":"ask-1",
            "response":{"behavior":"deny",
                "message":"Could not present the question to the user.",
                "toolUseID":"toolu_ask"}}}),
        "the deny frame has no decisionClassification"
    );
}

/// F2 — the forwarding side dropping the reply (the session is gone) is also
/// the aborted tool use.
#[tokio::test]
async fn t4fd_ask_question_dropped_reply_is_the_aborted_frame() {
    let mut rig = FdRig::new(None);
    rig.rig.ext.caps.elicitation_form = true;
    let mut run = run_permission(&mut rig, ask_frame());
    let reply = rig.create_elicitation().await;
    drop(reply);
    assert_eq!(
        answered(&mut run).await,
        json!({"type":"control_response","response":{
            "subtype":"error","request_id":"ask-1","error":"Tool use aborted"}})
    );
}

/// F2 — a client `cancel` is upstream's thrown `Tool use aborted`, which the SDK
/// writes as an `error` control response.
#[tokio::test]
async fn t4fd_ask_question_cancel_is_the_aborted_error_frame() {
    let mut rig = FdRig::new(None);
    rig.rig.ext.caps.elicitation_form = true;
    let mut run = run_permission(&mut rig, ask_frame());
    let reply = rig.create_elicitation().await;
    reply
        .send(Ok(json!({"action":"cancel"})))
        .expect("the client cancels");
    assert_eq!(
        answered(&mut run).await,
        json!({"type":"control_response","response":{
            "subtype":"error","request_id":"ask-1","error":"Tool use aborted"}})
    );
}

/// F3 — a dropped inbound handler (a `control_cancel_request`, or a clear-context
/// rebind) sends `Command::ElicitationWithdrawn`; a completed one does not.
#[tokio::test]
async fn t4fd_a_dropped_control_handler_withdraws_the_request() {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let handler = build_permission_request_handler(
        command_tx.clone(),
        crate::elicitation::ElicitationSupport {
            form: true,
            url: false,
        },
    );
    let frame = json!({"type":"control_request","request_id":"elicit-1","request":{
        "subtype":"elicitation","serverName":"srv","message":"Enter a value",
        "mode":"form","requestedSchema":{"type":"object","properties":{}}}});
    let task = tokio::spawn(handler(frame));
    let first = tokio::time::timeout(Duration::from_secs(2), command_rx.recv())
        .await
        .expect("the request is routed")
        .expect("the actor channel stays open");
    let Command::ElicitationControl { .. } = first else {
        panic!("expected ElicitationControl");
    };
    task.abort();
    let _ = task.await;
    let withdrawn = tokio::time::timeout(Duration::from_secs(2), command_rx.recv())
        .await
        .expect("the withdraw is sent")
        .expect("the actor channel stays open");
    assert!(
        matches!(&withdrawn, Command::ElicitationWithdrawn { request_id } if request_id == "elicit-1"),
        "the withdrawn request id is forwarded"
    );

    // A completed handler owes no withdraw.
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let handler = build_permission_request_handler(
        command_tx,
        crate::elicitation::ElicitationSupport {
            form: true,
            url: false,
        },
    );
    let frame = json!({"type":"control_request","request_id":"elicit-2","request":{
        "subtype":"elicitation","serverName":"srv","message":"Enter a value",
        "mode":"form","requestedSchema":{"type":"object","properties":{}}}});
    let task = tokio::spawn(handler(frame));
    let Command::ElicitationControl { reply, .. } =
        tokio::time::timeout(Duration::from_secs(2), command_rx.recv())
            .await
            .expect("the request is routed")
            .expect("the actor channel stays open")
    else {
        panic!("expected ElicitationControl");
    };
    reply
        .send(json!({"type":"control_response","response":{
            "subtype":"success","request_id":"elicit-2",
            "response":{"action":"decline"}}}))
        .expect("the actor answers");
    let _ = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("the handler completes");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), command_rx.recv())
            .await
            .is_err(),
        "a completed handler sends no withdraw"
    );
}

/// A2-8/F3 — a withdrawn request aborts the pending round-trip and ends the
/// child's token as withdrawn (no `running` after the cancel).
#[tokio::test]
async fn t4fd_withdrawn_request_ends_the_token_as_withdrawn() {
    let mut rig = FdRig::new(Some(SubagentForm::Rfd));
    rig.start_child("t1", "toolu_A");
    let mut run = run_permission(
        &mut rig,
        json!({"type":"control_request","request_id":"perm1","request":{
            "subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},
            "tool_use_id":"toolu_P","agent_id":"t1","permission_suggestions":[]}}),
    );
    assert_eq!(run.tokens.len(), 1, "the child's request is open");
    let _ = rig.request_permission().await;
    // A parent cancel came first; the CLI then withdrew the request.
    let outs = rig.rig.tasks.subagents().parent_cancelled();
    publish_subagent_outs(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        outs,
        &rig.rig.tx,
        "s",
        None,
    );
    let _ = rig.notifications();

    withdraw_request(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        &mut PendingClientRequests {
            permissions: &mut run.pending_permissions,
            elicitations: &mut HashMap::new(),
            ask: &mut run.pending_ask,
            await_tokens: &mut run.tokens,
        },
        &rig.rig.tx,
        "s",
        "perm1",
    );
    assert!(run.tokens.is_empty(), "the token ends");
    assert!(
        run.pending_permissions.is_empty(),
        "the client round-trip is aborted"
    );
    assert!(
        !rig.notifications()
            .iter()
            .any(|n| n["update"]["sessionUpdate"] == "subagent_update"),
        "a withdrawn request after a cancel reports no running"
    );
}

/// A2-1 — a permission round-trip that errors or is dropped sends
/// `Command::AwaitEnded`, which ends the child's token and reports `running`.
#[tokio::test]
async fn t4fd_a_dropped_permission_ends_the_token() {
    let mut rig = FdRig::new(Some(SubagentForm::Rfd));
    rig.start_child("t1", "toolu_A");
    let mut run = run_permission(
        &mut rig,
        json!({"type":"control_request","request_id":"perm1","request":{
            "subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},
            "tool_use_id":"toolu_P","agent_id":"t1","permission_suggestions":[]}}),
    );
    let request = rig.request_permission().await;
    drop(request);
    // The spawned task refuses the tool and reports the token's end.
    let mut ended = None;
    while let Some(command) = tokio::time::timeout(Duration::from_secs(2), run.command_rx.recv())
        .await
        .expect("the request settles")
    {
        if let Command::AwaitEnded { request_id } = command {
            ended = Some(request_id);
            break;
        }
    }
    assert_eq!(ended.as_deref(), Some("perm1"), "the token ends");
    let _ = rig.notifications();
    end_await_request(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        &mut run.tokens,
        &rig.rig.tx,
        "s",
        "perm1",
    );
    let running = rig
        .notifications()
        .into_iter()
        .find(|n| n["update"]["sessionUpdate"] == "subagent_update")
        .expect("the child reports running again");
    assert_eq!(running["update"]["state"]["state"], "running");
}

/// A2-4 — a parent cancel keeps an RFD child's open request (the child can keep
/// running, issue #2308) and withdraws every MCP elicitation.
#[tokio::test]
async fn t4fd_parent_cancel_keeps_a_rfd_childs_open_request() {
    let mut rig = FdRig::new(Some(SubagentForm::Rfd));
    rig.start_child("t1", "toolu_A");
    let mut run = run_permission(
        &mut rig,
        json!({"type":"control_request","request_id":"perm1","request":{
            "subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},
            "tool_use_id":"toolu_P","agent_id":"t1","permission_suggestions":[]}}),
    );
    let _ = rig.request_permission().await;
    // An open question of the same child, and an MCP elicitation of the root.
    let (ask_reply, _ask_rx) = oneshot::channel();
    run.pending_ask
        .insert("perm1".to_string(), (ask_reply, json!({"subtype":"error"})));
    let (elicitation_reply, elicitation_rx) = oneshot::channel();
    let elicitation_handle = tokio::spawn(async {
        std::future::pending::<()>().await;
    });
    let mut elicitations = HashMap::new();
    elicitations.insert(
        "elicit-1".to_string(),
        (
            elicitation_reply,
            json!({"type":"control_response","response":{
                "subtype":"success","request_id":"elicit-1",
                "response":{"action":"cancel"}}}),
            elicitation_handle,
        ),
    );

    cancel_parent_round_trips(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        &mut PendingClientRequests {
            permissions: &mut run.pending_permissions,
            elicitations: &mut elicitations,
            ask: &mut run.pending_ask,
            await_tokens: &mut run.tokens,
        },
        &rig.rig.tx,
        "s",
    );

    assert_eq!(run.tokens.len(), 1, "the child's request stays open");
    assert_eq!(
        run.pending_permissions.len(),
        1,
        "the client round-trip is not aborted"
    );
    assert!(
        run.pending_ask.contains_key("perm1"),
        "the child's question stays open"
    );
    assert!(elicitations.is_empty(), "the MCP elicitation is withdrawn");
    let response = tokio::time::timeout(Duration::from_secs(2), elicitation_rx)
        .await
        .expect("the CANCEL outcome is written")
        .expect("the reply channel stays open");
    assert_eq!(response["response"]["response"]["action"], "cancel");
}

/// A2-4 — an answered request of an RFD child after a parent cancel reports
/// `running` (the child kept running; upstream's signal was not aborted).
#[tokio::test]
async fn t4fd_answered_request_after_parent_cancel_reports_running() {
    let mut rig = FdRig::new(Some(SubagentForm::Rfd));
    rig.start_child("t1", "toolu_A");
    let mut run = run_permission(
        &mut rig,
        json!({"type":"control_request","request_id":"perm1","request":{
            "subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},
            "tool_use_id":"toolu_P","agent_id":"t1","permission_suggestions":[]}}),
    );
    let _ = rig.request_permission().await;
    let outs = rig.rig.tasks.subagents().parent_cancelled();
    publish_subagent_outs(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        outs,
        &rig.rig.tx,
        "s",
        None,
    );
    let _ = rig.notifications();
    end_await_request(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        &mut run.tokens,
        &rig.rig.tx,
        "s",
        "perm1",
    );
    let running = rig
        .notifications()
        .into_iter()
        .find(|n| n["update"]["sessionUpdate"] == "subagent_update")
        .expect("the child reports running after the answer");
    assert_eq!(running["update"]["state"]["state"], "running");
}

/// A2-8/F3 — a CLI withdrawal that no cancel caused (Claude Code withdrew the
/// request on its own) still reports `running`: the child goes on
/// (`awaitingUser`'s `stopped` is false without a new cancel).
#[tokio::test]
async fn t4fd_a_cli_withdraw_without_a_cancel_reports_running() {
    let mut rig = FdRig::new(Some(SubagentForm::Rfd));
    rig.start_child("t1", "toolu_A");
    let mut run = run_permission(
        &mut rig,
        json!({"type":"control_request","request_id":"perm1","request":{
            "subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},
            "tool_use_id":"toolu_P","agent_id":"t1","permission_suggestions":[]}}),
    );
    let _ = rig.request_permission().await;
    let _ = rig.notifications();
    withdraw_request(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        &mut PendingClientRequests {
            permissions: &mut run.pending_permissions,
            elicitations: &mut HashMap::new(),
            ask: &mut run.pending_ask,
            await_tokens: &mut run.tokens,
        },
        &rig.rig.tx,
        "s",
        "perm1",
    );
    let running = rig
        .notifications()
        .into_iter()
        .find(|n| n["update"]["sessionUpdate"] == "subagent_update")
        .expect("the child reports running");
    assert_eq!(running["update"]["state"]["state"], "running");
}

/// A2-3/A2-10 — a reset clears the eager sessions and the tool tracker.
#[test]
fn t4fd_conversation_reset_clears_eager_sessions() {
    let mut rig = FdRig::new(None);
    rig.rig.map_state.record_eager_session("toolu_X", "t1");
    rig.rig
        .feed(json!({"type":"conversation_reset","new_conversation_id":"c2"}));
    assert_eq!(
        rig.rig.map_state.eager_session("toolu_X"),
        None,
        "the reset clears the eager sessions"
    );
}

/// A2-2/A2-6 — an async-task update of a child-owned task goes to that child,
/// through the same helper the stop, the held release and the stream-end finish
/// use.
#[test]
fn t4fd_async_task_updates_route_to_the_child() {
    let mut rig = FdRig::new(Some(SubagentForm::Rfd));
    rig.start_child("t1", "toolu_A");
    rig.child_tool_call("toolu_A", "toolu_B");
    let _ = rig.notifications();
    let spawned = rig.rig.tasks.task_started(&json!({
        "task_id":"bg","task_type":"local_bash","description":"d",
        "is_backgrounded":true,"tool_use_id":"toolu_B"}));
    publish_task_updates(
        &mut rig.rig.tasks,
        &rig.rig.map_state,
        spawned,
        &rig.rig.tx,
        "s",
    );
    let _ = rig.notifications();
    let stopped = rig.rig.tasks.task_stopped("bg");
    emit_task_updates(
        &mut rig.rig.tasks,
        &rig.rig.map_state,
        stopped.updates,
        &rig.rig.tx,
        "s",
    );
    let notes = rig.notifications();
    let update = notes
        .iter()
        .find(|v| v["update"]["sessionUpdate"] == "async_task_state_update")
        .expect("the stopped state is published");
    assert_eq!(update["sessionId"], "t1", "the update stays in the child");
}

/// A2-2 — the stream-end finish routes each terminal update to the child.
#[test]
fn t4fd_finish_async_tasks_routes_terminal_updates() {
    let mut rig = FdRig::new(Some(SubagentForm::Rfd));
    rig.start_child("t1", "toolu_A");
    rig.child_tool_call("toolu_A", "toolu_B");
    let _ = rig.notifications();
    let spawned = rig.rig.tasks.task_started(&json!({
        "task_id":"bg","task_type":"local_bash","description":"d",
        "is_backgrounded":true,"tool_use_id":"toolu_B"}));
    publish_task_updates(
        &mut rig.rig.tasks,
        &rig.rig.map_state,
        spawned,
        &rig.rig.tx,
        "s",
    );
    let _ = rig.notifications();
    finish_async_tasks(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        "stopped",
        &rig.rig.tx,
        "s",
        None,
    );
    let notes = rig.notifications();
    let update = notes
        .iter()
        .find(|v| v["update"]["sessionUpdate"] == "async_task_state_update")
        .expect("the finish publishes the terminal state");
    assert_eq!(update["sessionId"], "t1", "the update stays in the child");
}

/// A2-7 — a hook update stays verbatim (explicit nulls and unknown keys), as it
/// did before the routing hook.
#[test]
fn t4fd_hook_updates_stay_verbatim() {
    let mut rig = FdRig::new(None);
    rig.rig.echo("p1");
    send_update(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        json!({"sessionId":"s","update":{
            "sessionUpdate":"tool_call_update","toolCallId":"toolu_H",
            "status":"completed","_meta":{"unknown":{"keep":null},"x":null}}}),
        None,
        &rig.rig.tx,
        "s",
        None,
    );
    let mut raw = None;
    while let Ok(item) = rig.rig.rx.try_recv() {
        if let SessionOutbound::RawUpdate(value) = item {
            raw = Some(value);
        }
    }
    let raw = raw.expect("the update is sent raw");
    assert!(
        raw["update"]["_meta"]["unknown"]
            .get("keep")
            .is_some_and(Value::is_null),
        "an explicit null survives: {raw}"
    );
    assert!(
        raw["update"]["_meta"].get("x").is_some_and(Value::is_null),
        "an unknown key survives: {raw}"
    );
}

/// A2-5 — the native subagent runtime's diagnostics are drained at the session
/// boundary instead of accumulating.
#[test]
fn t4fd_subagent_logs_are_drained() {
    let mut rig = FdRig::new(Some(SubagentForm::Air));
    rig.rig.map_state.configure_air(PathBuf::from("/repo"));
    rig.rig
        .map_state
        .set_logger(Some(std::sync::Arc::new(crate::logger::Logger::disabled())));
    rig.start_child("t1", "toolu_A");
    rig.child_tool_call("toolu_A", "toolu_B");
    let _ = rig.notifications();
    rig.rig.tasks.subagent_task_notification(&json!({
        "type":"system","subtype":"task_notification","task_id":"t1",
        "status":"completed","tool_use_id":"toolu_A"}));
    let _ = rig.notifications();
    // A late update of the terminal child logs the drop.
    let notification = json!({"sessionId":"s","update":{
        "sessionUpdate":"tool_call_update","toolCallId":"toolu_B","status":"in_progress"}});
    let _ = rig.rig.tasks.subagents().route(&notification, None);
    assert!(
        !rig.rig.tasks.subagents().take_logs().is_empty(),
        "the runtime logged the drop"
    );
    // Drain the runtime again, then the session boundary drains what it holds.
    let _ = rig.rig.tasks.subagents().take_logs();
    let notification = json!({"sessionId":"s","update":{
        "sessionUpdate":"tool_call_update","toolCallId":"toolu_B","status":"in_progress"}});
    let _ = rig.rig.tasks.subagents().route(&notification, None);
    publish_subagent_outs(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        Vec::new(),
        &rig.rig.tx,
        "s",
        None,
    );
    assert!(
        rig.rig.tasks.subagents().take_logs().is_empty(),
        "the boundary drained the logs"
    );
}

/// F6 — a cycle Claude Code started on its own is foreground work even though
/// no turn is queued (`foregroundWork.has(sessionId)`,
/// `acp-agent.ts:3824-3832`). The actor sees the cycle only after its first
/// main-conversation output, so drive a real assistant frame (no prompt) and
/// wait for the update it publishes before sending the prompt: the refusal then
/// has to come from `runs_autonomous_cycle`, not from `has_unsettled`.
#[tokio::test]
async fn t4fd_autonomous_cycle_refuses_remote_control() {
    let (tx, mut update_rx, stream_tx, _control_in_tx) = super::tests::spawn_bare_actor();
    stream_tx
        .send(json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_a","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"text","text":"follow-up after a background subagent"}]}}))
        .expect("the stream stays open");
    // The assistant frame's update is published by the same arm that starts the
    // cycle, so seeing it means the cycle is latched.
    let mut saw_chunk = false;
    for _ in 0..50 {
        match tokio::time::timeout(Duration::from_millis(200), update_rx.recv()).await {
            Ok(Some(SessionOutbound::Update(notif))) => {
                let value = serde_json::to_value(&notif.update).unwrap();
                if value["sessionUpdate"] == "agent_message_chunk" {
                    saw_chunk = true;
                    break;
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    assert!(saw_chunk, "the autonomous frame reached the client");
    let (reply_tx, reply_rx) = oneshot::channel();
    tx.send(Command::RemoteControlPrompt {
        command: crate::control::RemoteControlCommand {
            action: crate::control::RemoteControlAction::Connect { name: None },
        },
        reply: reply_tx,
        accepted: None,
    })
    .expect("the actor is alive");
    let result = tokio::time::timeout(Duration::from_secs(2), reply_rx)
        .await
        .expect("the refusal resolves")
        .expect("the actor stays alive");
    match result
        .expect_err("a prompt during an autonomous cycle is refused")
        .error
    {
        SessionError::InvalidRequest(message) => assert_eq!(
            message,
            "Remote Control can't be changed while Claude is working. Try again when the turn ends."
        ),
        other => panic!("expected an invalid request, got {other:?}"),
    }
}

/// F6/L1 — a `can_use_tool` the CLI withdraws while it is parked in the cancel
/// window is never shown to the client: the parked entry is dropped instead of
/// opening a permission dialog (and a tool call) nobody waits for
/// (`requestPermissionFromClient` throws "Tool use aborted" before sending
/// anything, `acp-agent.ts:9800-9815`).
#[tokio::test]
async fn t4fd_a_withdrawn_parked_request_is_not_presented() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let (a, b) = tokio::io::duplex(64 * 1024);
    let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
    let (tx, commands) = mpsc::unbounded_channel::<Command>();
    let control = Control::spawn(
        a,
        control_in_rx,
        ControlOptions {
            on_request: Some(super::build_permission_request_handler(
                tx.clone(),
                crate::elicitation::ElicitationSupport::default(),
            )),
        },
    );
    let (update_tx, mut update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
    let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(super::run_loop(
        commands,
        tx.clone(),
        stream_rx,
        control,
        control_in_tx.clone(),
        update_tx,
        Duration::from_secs(30),
        "s1".into(),
        Arc::new(AtomicUsize::new(0)),
        None,
        crate::client_caps::ClientCapabilities::default(),
        std::env::temp_dir(),
        false,
        false,
        crate::sdk_options::RawSdkMessages::Off,
        crate::env_snapshot::EnvSnapshot::empty(),
        None,
        Vec::new(),
    ));
    // Drain the child's stdin so the duplex never fills up.
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut reader = BufReader::new(b);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let trimmed = line.trim().to_string();
                    if !trimmed.is_empty() {
                        let _ = line_tx.send(trimmed);
                    }
                }
            }
        }
    });

    // An active turn so a cancel opens the stopped-work window.
    let (preply_tx, _preply_rx) = oneshot::channel();
    tx.send(Command::Prompt {
        uuid: "p1".into(),
        frame: json!({"type":"user","uuid":"p1","message":{}}),
        is_local_only: false,
        prompt_text: String::new(),
        reply: preply_tx,
    })
    .expect("the actor is alive");
    let _echo = tokio::time::timeout(Duration::from_secs(5), line_rx.recv())
        .await
        .expect("the user frame reaches the child")
        .expect("the stream stays open");
    tx.send(Command::Cancel).expect("the actor is alive");

    // The request arrives before the consumer reached its tool use: parked.
    control_in_tx
        .send(json!({
            "type": "control_request",
            "request_id": "permreq1",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "input": {"command": "echo hi"},
                "tool_use_id": "toolu_01PARKED",
                "agent_id": null,
                "permission_suggestions": [],
            }
        }))
        .expect("the control channel stays open");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut shown = false;
    while let Ok(item) = update_rx.try_recv() {
        if matches!(item, SessionOutbound::RequestPermission { .. }) {
            shown = true;
        }
    }
    assert!(!shown, "the request is parked, not presented yet");

    // The CLI withdraws it while it is parked: the handler's reply closes and
    // the actor is told.
    control_in_tx
        .send(json!({"type":"control_cancel_request","request_id":"permreq1"}))
        .expect("the control channel stays open");

    // Wait past `TOOL_USE_WAIT` (2 s) so the bounded release would have run.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let mut shown = false;
    let mut denied = None;
    while let Ok(item) = update_rx.try_recv() {
        if let SessionOutbound::RequestPermission { params, .. } = item {
            shown = true;
            denied = Some(params);
        }
    }
    assert!(
        !shown,
        "the withdrawn request is never presented: {denied:?}"
    );
    // The CLI must not be answered either: it cancelled the request.
    let mut answered = false;
    while let Ok(line) = line_rx.try_recv() {
        if line.contains("\"permreq1\"") && line.contains("control_response") {
            answered = true;
        }
    }
    assert!(!answered, "a withdrawn request gets no response");
    drop(stream_tx);
}

/// F23c/L2 — the OAuth `elicitation/complete` reaches the wire before the URL
/// request is dropped (which sends `$/cancel_request`,
/// `acp-agent.ts:2645-2658`): the complete closure waits for the forwarder to
/// process the notification, not merely for it to be queued.
#[tokio::test]
async fn t4fd_oauth_complete_waits_for_the_forwarder() {
    let (tx, mut rx) = mpsc::unbounded_channel::<SessionOutbound>();
    let (release_tx, release_rx) = oneshot::channel::<()>();
    // A forwarder that holds the Flush until the test releases it: the helper
    // must not return before the ack.
    let forwarder = tokio::spawn(async move {
        let mut complete_seen = false;
        while let Some(item) = rx.recv().await {
            match item {
                SessionOutbound::CompleteElicitation { .. } => complete_seen = true,
                SessionOutbound::Flush(ack) => {
                    assert!(complete_seen, "the complete is queued before the flush");
                    let _ = release_rx.await;
                    let _ = ack.send(());
                    return;
                }
                _ => {}
            }
        }
    });
    let done = complete_elicitation_flushed(&tx, json!({"elicitationId": "mcp-oauth-x"}));
    tokio::pin!(done);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut done)
            .await
            .is_err(),
        "the helper waits for the forwarder to process the notification"
    );
    release_tx.send(()).expect("the forwarder is waiting");
    tokio::time::timeout(Duration::from_secs(2), &mut done)
        .await
        .expect("the helper returns once the forwarder acked");
    forwarder.await.expect("the forwarder finishes");
}
