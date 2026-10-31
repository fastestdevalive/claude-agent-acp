//! T4A2: the session-side native subagent hooks — the `sendUpdate` routing
//! choke point (U-11/U-68), the late async-task route, the client cancel of a
//! child (B-93 rest), the parent cancel (B-40/B-96), the awaiting-user requests
//! and the SendMessage resume (U-101 rest).
//!
//! The harness drives `handle_session_frame` / `handle_permission` with the
//! state a running actor holds; no child process is involved.

use super::tests_t2a::Rig;
use super::*;
use crate::acp_subagents::SubagentForm;

/// A [`Rig`] with a native subagent runtime of `form` rooted at `"s"`.
struct T4Rig {
    rig: Rig,
}

impl T4Rig {
    fn new(form: Option<SubagentForm>) -> Self {
        let mut rig = Rig::new();
        rig.tasks = AsyncTaskRuntime::new(true).with_subagent_form(form, "s");
        rig.ext.caps.subagent_form = form;
        Self { rig }
    }

    fn air(form: Option<SubagentForm>) -> Self {
        let mut this = Self::new(form);
        this.rig.map_state.configure_air(PathBuf::from("/repo"));
        this
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

    /// The `subagent_spawned` of a child that an Agent call launched, then the
    /// `task_started` of that child.
    /// Every outbound item since the last call, with its channel kind
    /// (`"typed"` for `SessionOutbound::Update`, `"raw"` for `RawUpdate`).
    fn outbound_kinds(&mut self) -> Vec<(&'static str, Value)> {
        let mut out = Vec::new();
        while let Ok(item) = self.rig.rx.try_recv() {
            match item {
                SessionOutbound::Update(notif) => {
                    out.push(("typed", serde_json::to_value(&notif).unwrap()));
                }
                SessionOutbound::RawUpdate(value) => out.push(("raw", value)),
                _ => {}
            }
        }
        out
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

/// U-101 — the RFD announce goes to the parent (`subagent_update` with the
/// child's session and `running`), and the spawn prompt to the child
/// (`session_message`).
#[test]
fn t4a2_rfd_announce_publishes_the_child_and_its_prompt() {
    let mut rig = T4Rig::new(Some(SubagentForm::Rfd));
    rig.rig.echo("p1");
    rig.rig
        .feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_a","role":"assistant",
        "content":[{"type":"tool_use","id":"toolu_A","name":"Agent",
            "input":{"description":"Fix","prompt":"Make it pass"}}]}}));
    rig.rig
        .feed(json!({"type":"system","subtype":"task_started",
        "task_id":"t1","tool_use_id":"toolu_A","subagent_type":"general-purpose",
        "description":"Fix","prompt":"Make it pass"}));
    let notes = rig.notifications();
    let announce = notes
        .iter()
        .find(|v| v["update"]["sessionUpdate"] == "subagent_update")
        .expect("the RFD announce");
    assert_eq!(announce["sessionId"], "s");
    assert_eq!(announce["update"]["sessionId"], "t1");
    assert_eq!(announce["update"]["state"]["state"], "running");
    assert_eq!(announce["update"]["capabilities"], json!({"cancel": {}}));
    let message = notes
        .iter()
        .find(|v| v["update"]["sessionUpdate"] == "session_message")
        .expect("the spawn prompt reaches the child");
    assert_eq!(message["sessionId"], "t1");
    assert_eq!(message["update"]["recipientSessionId"], "t1");
}

/// U-101 — the AIR announce is `subagent_spawned` to the parent session.
#[test]
fn t4a2_air_announce_publishes_subagent_spawned() {
    let mut rig = T4Rig::air(Some(SubagentForm::Air));
    rig.rig.echo("p1");
    rig.rig
        .feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_a","role":"assistant",
        "content":[{"type":"tool_use","id":"toolu_A","name":"Agent",
            "input":{"description":"Fix","prompt":"Make it pass"}}]}}));
    rig.rig
        .feed(json!({"type":"system","subtype":"task_started",
        "task_id":"t1","tool_use_id":"toolu_A","subagent_type":"general-purpose",
        "description":"Fix","prompt":"Make it pass"}));
    let notes = rig.notifications();
    let spawned = notes
        .iter()
        .find(|v| v["update"]["sessionUpdate"] == "subagent_spawned")
        .expect("AIR announces the child");
    assert_eq!(spawned["sessionId"], "s");
    assert_eq!(spawned["update"]["subagentSessionId"], "t1");
    assert_eq!(spawned["update"]["name"], "Fix");
}

/// U-11/U-68 — a tool call of a child frame reaches the child session, not the
/// root: the runtime's `route` moves it and `sendUpdate` sends what survived.
#[test]
fn t4a2_a_child_tool_call_is_routed_to_the_child_session() {
    let mut rig = T4Rig::new(Some(SubagentForm::Rfd));
    rig.rig.echo("p1");
    rig.start_child("t1", "toolu_A");
    rig.child_tool_call("toolu_A", "toolu_C");
    let notes = rig.notifications();
    let call = notes
        .iter()
        .find(|value| value["update"]["toolCallId"] == "toolu_C")
        .expect("the child's tool call was published");
    assert_eq!(
        call["sessionId"], "t1",
        "a child's tool call goes to the child"
    );
    assert_eq!(call["update"]["sessionUpdate"], "tool_call");
    assert_eq!(
        call["update"]["_meta"]["claudeCode"]["parentToolUseId"],
        "toolu_A"
    );
}

/// The same frame for a client WITHOUT native subagent sessions stays on the
/// root session, byte for byte the pre-routing behaviour: the exact typed
/// `SessionNotification` the mapper builds, not a re-materialised raw value.
#[test]
fn t4a2_without_native_subagent_sessions_nothing_is_routed() {
    let mut rig = T4Rig::new(None);
    rig.rig.echo("p1");
    let frame = json!({"type":"assistant","parent_tool_use_id":"toolu_A",
        "message":{"id":"msg_c","role":"assistant",
        "content":[{"type":"tool_use","id":"toolu_C","name":"Bash","input":{"command":"ls"}}]}});
    // The pre-routing notification: the mapper's own typed updates.
    let mut state = MapState::default();
    let mapped = crate::map::map_consolidated_assistant(&frame, &mut state);
    let expected: Vec<Value> = state
        .filter_updates(mapped)
        .iter()
        .map(|update| {
            serde_json::to_value(SessionNotification::new("s".to_string(), update.clone())).unwrap()
        })
        .collect();
    rig.rig.feed(frame);
    let out = rig.outbound_kinds();
    let calls: Vec<&(&'static str, Value)> = out
        .iter()
        .filter(|(_, value)| value["update"]["toolCallId"] == "toolu_C")
        .collect();
    assert_eq!(calls.len(), 1, "one call: {out:?}");
    assert_eq!(calls[0].0, "typed", "it keeps the typed channel");
    assert_eq!(
        calls[0].1, expected[0],
        "the routed notification is exactly the mapper's"
    );
    assert_eq!(calls[0].1["sessionId"], "s");
}

/// U-11 — a tool result of a child call closes it in the child session.
#[test]
fn t4a2_a_child_tool_result_is_routed_to_the_child_session() {
    let mut rig = T4Rig::new(Some(SubagentForm::Rfd));
    rig.rig.echo("p1");
    rig.start_child("t1", "toolu_A");
    rig.child_tool_call("toolu_A", "toolu_C");
    let _ = rig.notifications();
    rig.rig
        .feed(json!({"type":"user","parent_tool_use_id":"toolu_A",
        "message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_C","content":"out"}]}}));
    let notes = rig.notifications();
    let done = notes
        .iter()
        .find(|value| value["update"]["toolCallId"] == "toolu_C")
        .expect("the child's tool result was published");
    assert_eq!(done["sessionId"], "t1");
    assert_eq!(done["update"]["status"], "completed");
}

/// `sendUpdate`'s parent tracking (`acp-agent.ts:4562-4575`): a later update of
/// an emitted call records the Agent call that owns it, so a cancel can keep the
/// call with its subagent. A final status drops the record.
#[test]
fn t4a2_a_later_update_records_the_tool_call_parent() {
    let mut rig = T4Rig::new(Some(SubagentForm::Rfd));
    rig.rig.echo("p1");
    rig.rig.map_state.mark_emitted(
        "toolu_X".to_string(),
        json!({"type":"tool_use","id":"toolu_X","name":"Bash","input":{}}),
    );
    let notification = json!({"sessionId":"s","update":{
        "sessionUpdate":"tool_call_update","toolCallId":"toolu_X","status":"in_progress",
        "_meta":{"claudeCode":{"parentToolUseId":"toolu_A"}}}});
    send_update(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        notification,
        None,
        &rig.rig.tx,
        "s",
        None,
    );
    assert_eq!(
        rig.rig.map_state.tool_call_parent("toolu_X"),
        Some("toolu_A")
    );
    // A final status forgets it (`session.toolCallParents?.delete`).
    send_update(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        json!({"sessionId":"s","update":{
            "sessionUpdate":"tool_call_update","toolCallId":"toolu_X","status":"completed"}}),
        None,
        &rig.rig.tx,
        "s",
        None,
    );
    assert_eq!(rig.rig.map_state.tool_call_parent("toolu_X"), None);
}

/// U-11 — the AIR form suppresses a native Agent/Task control call (it is not a
/// transcript tool), and the mapper's pre-send dedupe mark must not survive, so
/// a later permission request can still surface it.
#[test]
fn t4a2_a_suppressed_control_call_is_unmarked() {
    let mut rig = T4Rig::air(Some(SubagentForm::Air));
    rig.rig.echo("p1");
    rig.rig
        .feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_a","role":"assistant",
        "content":[{"type":"tool_use","id":"toolu_A","name":"Agent",
            "input":{"description":"Fix","prompt":"Make it pass"}}]}}));
    let notes = rig.notifications();
    assert!(
        !notes.iter().any(|v| v["update"]["toolCallId"] == "toolu_A"),
        "the AIR control call is not a transcript tool: {notes:?}"
    );
    assert!(!rig.rig.map_state.has_emitted("toolu_A"));
}

/// U-11 — a call that reaches a final status drops its eager session
/// (`session.eagerToolCallSessions?.delete`), so a later update no longer
/// forces the old session.
#[test]
fn t4a2_a_final_status_drops_the_eager_session() {
    let mut rig = T4Rig::new(Some(SubagentForm::Rfd));
    rig.rig.echo("p1");
    rig.rig.map_state.record_eager_session("toolu_X", "t1");
    assert_eq!(
        rig.rig.map_state.eager_session("toolu_X").as_deref(),
        Some("t1")
    );
    send_update(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        json!({"sessionId":"t1","update":{
            "sessionUpdate":"tool_call_update","toolCallId":"toolu_X","status":"failed"}}),
        None,
        &rig.rig.tx,
        "s",
        None,
    );
    assert_eq!(rig.rig.map_state.eager_session("toolu_X"), None);
}

/// U-11 — the eager session a permission request created the call in forces the
/// call's later updates into that session, even before the runtime knows the
/// child owns it (`route(.., forcedSessionId)`).
#[test]
fn t4a2_an_eager_call_keeps_its_session() {
    let mut rig = T4Rig::new(Some(SubagentForm::Rfd));
    rig.start_child("t1", "toolu_A");
    rig.rig.echo("p1");
    rig.rig.map_state.mark_emitted(
        "toolu_E".to_string(),
        json!({"type":"tool_use","id":"toolu_E","name":"Bash","input":{}}),
    );
    // A NON-root session: without the forced lookup the call would go to "s".
    rig.rig.map_state.record_eager_session("toolu_E", "t1");
    send_update(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        json!({"sessionId":"s","update":{
            "sessionUpdate":"tool_call_update","toolCallId":"toolu_E","status":"in_progress"}}),
        None,
        &rig.rig.tx,
        "s",
        None,
    );
    let notes = rig.notifications();
    assert_eq!(notes.len(), 1);
    assert_eq!(
        notes[0]["sessionId"], "t1",
        "the eager session is forced, not the root"
    );
}

/// STEP 2 — an async-task update of a finished child still reaches that child
/// (`routeOfToolCall` + `apply_route`); the route survives the child's end.
#[test]
fn t4a2_a_late_async_task_update_reaches_the_finished_child() {
    let mut rig = T4Rig::new(Some(SubagentForm::Rfd));
    rig.start_child("t1", "toolu_A");
    // A Bash call of the child, routed to it (so the runtime remembers the owner).
    rig.child_tool_call("toolu_A", "toolu_B");
    let _ = rig.notifications();
    // Its background task is registered and announced.
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
    // The child ends before the task reports again.
    rig.rig.tasks.subagent_task_notification(&json!({
        "type":"system","subtype":"task_notification","task_id":"t1",
        "status":"completed","tool_use_id":"toolu_A"}));
    let updates = rig.rig.tasks.task_progress(&json!({
        "task_id":"bg","description":"d","tool_use_id":"toolu_B","summary":"out"}));
    publish_task_updates(
        &mut rig.rig.tasks,
        &rig.rig.map_state,
        updates,
        &rig.rig.tx,
        "s",
    );
    let notes = rig.notifications();
    let progress = notes
        .iter()
        .find(|v| v["update"]["sessionUpdate"] == "async_task_progress")
        .expect("the progress update was published");
    assert_eq!(
        progress["sessionId"], "t1",
        "an async-task update of a child tool call stays in the child"
    );
}

/// STEP 2 — the SendMessage text that resumed the child is found in the
/// tool-use cache (`sendMessageResumePrompt`, latest SendMessage first); a
/// `running` patch then resumes the finished RFD child and reports `running`.
/// Upstream's `taskResumed` passes no prompt in the RFD form (the SendMessage
/// result is the delegation that carries it).
#[test]
fn t4a2_a_resumed_task_carries_the_send_message_text() {
    let mut rig = T4Rig::new(Some(SubagentForm::Rfd));
    rig.start_child("t1", "toolu_A");
    rig.rig.map_state.mark_emitted(
        "toolu_S".to_string(),
        json!({"type":"tool_use","id":"toolu_S","name":"SendMessage",
            "input":{"to":"t1","message":"keep going"}}),
    );
    let prompt = crate::native_subagents::send_message_resume_prompt(
        &rig.rig.map_state.tool_uses_in_order(),
        "t1",
        &[],
    );
    assert_eq!(prompt.as_deref(), Some("keep going"));
    rig.rig.tasks.subagent_task_notification(&json!({
        "type":"system","subtype":"task_notification","task_id":"t1",
        "status":"completed","tool_use_id":"toolu_A"}));
    let _ = rig.notifications();
    rig.rig
        .feed(json!({"type":"system","subtype":"task_updated",
        "task_id":"t1","patch":{"status":"running"}}));
    let notes = rig.notifications();
    assert!(
        !notes
            .iter()
            .any(|value| value["update"]["sessionUpdate"] == "session_message"),
        "the RFD task resume carries no prompt: {notes:?}"
    );
    let state = notes
        .iter()
        .find(|value| value["update"]["sessionUpdate"] == "subagent_update")
        .expect("the resumed child reports its state");
    assert_eq!(state["sessionId"], "s");
    assert_eq!(state["update"]["sessionId"], "t1");
    assert_eq!(state["update"]["state"]["state"], "running");
}

/// STEP 3 — `cancelChild`: an announced child's session names the task whose
/// stop the caller must request; any other id is not a child.
#[test]
fn t4a2_cancel_child_returns_the_task_of_an_announced_child() {
    let mut rig = T4Rig::new(Some(SubagentForm::Rfd));
    rig.start_child("t1", "toolu_A");
    assert_eq!(
        rig.rig.tasks.subagents().cancel_child("t1"),
        Some("t1".to_string())
    );
    assert_eq!(rig.rig.tasks.subagents().cancel_child("nope"), None);
}

/// B-40/B-96 — a parent cancel ends AIR's children as cancelled and publishes
/// their state; the RFD form only counts (nothing is reported until the SDK
/// ends the child).
#[test]
fn t4a2_parent_cancel_reports_air_children_and_counts_rfd() {
    let mut air = T4Rig::new(Some(SubagentForm::Air));
    air.start_child("t1", "toolu_A");
    let outs = air.rig.tasks.subagents().parent_cancelled();
    publish_subagent_outs(
        &mut air.rig.tasks,
        &mut air.rig.map_state,
        outs,
        &air.rig.tx,
        "s",
        None,
    );
    let notes = air.notifications();
    let ended = notes
        .iter()
        .find(|v| v["update"]["sessionUpdate"] == "subagent_state_update")
        .expect("AIR reports the cancelled child");
    assert_eq!(ended["sessionId"], "s");
    assert_eq!(ended["update"]["subagentSessionId"], "t1");
    assert_eq!(ended["update"]["state"], "cancelled");

    let mut rfd = T4Rig::new(Some(SubagentForm::Rfd));
    rfd.start_child("t1", "toolu_A");
    let outs = rfd.rig.tasks.subagents().parent_cancelled();
    publish_subagent_outs(
        &mut rfd.rig.tasks,
        &mut rfd.rig.map_state,
        outs,
        &rfd.rig.tx,
        "s",
        None,
    );
    assert!(
        rfd.notifications().is_empty(),
        "the RFD form reports nothing on a parent cancel"
    );
}

/// STEP 5 — a permission request of an announced child goes to the child
/// session and makes it `requires_action` until the request ends.
#[tokio::test]
async fn t4a2_a_child_permission_request_is_awaiting_user() {
    let mut rig = T4Rig::new(Some(SubagentForm::Rfd));
    rig.start_child("t1", "toolu_A");
    let (reply_tx, _reply_rx) = oneshot::channel();
    let mut pending_permissions: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut tokens: HashMap<String, crate::native_subagents::AwaitToken> = HashMap::new();
    let (command_tx, _command_rx) = mpsc::unbounded_channel::<Command>();
    handle_permission(
        &mut rig.rig.tasks,
        json!({"type":"control_request","request_id":"perm1","request":{
            "subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},
            "tool_use_id":"toolu_P","agent_id":"t1","permission_suggestions":[]}}),
        reply_tx,
        None,
        &mut rig.rig.map_state,
        &mut rig.rig.live_background,
        &rig.rig.tx,
        "s",
        &mut pending_permissions,
        &mut tokens,
        &mut HashMap::new(),
        &watch::channel(false).1,
        false,
        std::path::Path::new("/repo"),
        None,
        &[],
        None,
        None,
        None,
        &command_tx,
    );
    assert_eq!(tokens.len(), 1, "the child's request is open");
    let notes = rig.notifications();
    let state = notes
        .iter()
        .find(|v| v["update"]["sessionUpdate"] == "subagent_update")
        .expect("the waiting child reports requires_action");
    assert_eq!(state["sessionId"], "s");
    assert_eq!(state["update"]["sessionId"], "t1");
    assert_eq!(state["update"]["state"]["state"], "requires_action");
    // The surfaced tool call went to the child session.
    let call = notes
        .iter()
        .find(|v| v["update"]["toolCallId"] == "toolu_P")
        .expect("the request's tool call surfaced");
    assert_eq!(call["sessionId"], "t1");
    // Ending the request reports the child running again.
    let token = tokens.drain().next().expect("a token").1;
    let outs = rig.rig.tasks.subagents().awaiting_user_end(token, false);
    publish_subagent_outs(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        outs,
        &rig.rig.tx,
        "s",
        None,
    );
    let notes = rig.notifications();
    let running = notes
        .iter()
        .find(|v| v["update"]["sessionUpdate"] == "subagent_update")
        .expect("the child reports running again");
    assert_eq!(running["update"]["state"]["state"], "running");
    pending_permissions.clear();
}

/// T8A — the PostToolUse hook update of a child's Edit goes to the child's
/// session through the parent attribution it carries.
#[test]
fn t8a_a_child_hook_update_routes_to_the_child() {
    let mut rig = T4Rig::new(Some(SubagentForm::Rfd));
    rig.rig.echo("p1");
    rig.start_child("t1", "toolu_A");
    rig.child_tool_call("toolu_A", "toolu_C");
    let _ = rig.notifications();
    let update = crate::hooks::build_post_tool_use_update(
        "toolu_C",
        "Edit",
        Some("toolu_A"),
        &json!({
            "filePath": "/repo/x.ts",
            "oldString": "a",
            "newString": "b",
            "structuredPatch": [{"oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1,
                "lines": ["-a", "+b"]}],
        }),
    );
    send_update(
        &mut rig.rig.tasks,
        &mut rig.rig.map_state,
        json!({"sessionId": "s", "update": update}),
        None,
        &rig.rig.tx,
        "s",
        None,
    );
    let notes = rig.notifications();
    let hook = notes
        .iter()
        .find(|v| v["update"]["toolCallId"] == "toolu_C");
    assert!(hook.is_some(), "{notes:?}");
    let hook = hook.unwrap();
    assert_eq!(hook["sessionId"], "t1", "the child owns the Edit");
    assert!(hook["update"]["content"].is_array());
}

/// T8A — the RFD form sends no AIR draft (`subagent_spawned` /
/// `subagent_state_update`), only its `subagent_update` lifecycle.
#[test]
fn t8a_rfd_sends_no_air_draft() {
    let mut rig = T4Rig::new(Some(SubagentForm::Rfd));
    rig.rig.echo("p1");
    rig.rig
        .feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_a","role":"assistant",
        "content":[{"type":"tool_use","id":"toolu_A","name":"Agent",
            "input":{"description":"Fix","prompt":"Make it pass"}}]}}));
    rig.rig
        .feed(json!({"type":"system","subtype":"task_started",
        "task_id":"t1","tool_use_id":"toolu_A","subagent_type":"general-purpose",
        "description":"Fix","prompt":"Make it pass"}));
    let notes = rig.notifications();
    assert!(
        notes
            .iter()
            .any(|v| v["update"]["sessionUpdate"] == "subagent_update"),
        "{notes:?}"
    );
    assert!(
        notes.iter().all(|v| {
            v["update"]["sessionUpdate"] != "subagent_spawned"
                && v["update"]["sessionUpdate"] != "subagent_state_update"
        }),
        "no AIR draft for an RFD client: {notes:?}"
    );
}

/// T8A — a permission request of an announced child goes to the child session,
/// and so does the tool call it surfaces.
#[tokio::test]
async fn t8a_child_permission_request_session_is_the_child() {
    let mut rig = T4Rig::new(Some(SubagentForm::Rfd));
    rig.start_child("t1", "toolu_A");
    let (reply_tx, _reply_rx) = oneshot::channel();
    let mut pending_permissions: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut tokens: HashMap<String, crate::native_subagents::AwaitToken> = HashMap::new();
    let (command_tx, _command_rx) = mpsc::unbounded_channel::<Command>();
    handle_permission(
        &mut rig.rig.tasks,
        json!({"type":"control_request","request_id":"perm1","request":{
            "subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},
            "tool_use_id":"toolu_P","agent_id":"t1","permission_suggestions":[]}}),
        reply_tx,
        None,
        &mut rig.rig.map_state,
        &mut rig.rig.live_background,
        &rig.rig.tx,
        "s",
        &mut pending_permissions,
        &mut tokens,
        &mut HashMap::new(),
        &watch::channel(false).1,
        false,
        std::path::Path::new("/repo"),
        None,
        &[],
        None,
        None,
        None,
        &command_tx,
    );
    let mut request_session = None;
    let mut call_session = None;
    while let Ok(item) = rig.rig.rx.try_recv() {
        match item {
            SessionOutbound::RequestPermission { params, .. } => {
                request_session = params["sessionId"].as_str().map(str::to_string);
            }
            SessionOutbound::Update(notif) => {
                let value = serde_json::to_value(&notif).unwrap();
                if value["update"]["toolCallId"] == "toolu_P" {
                    call_session = Some(notif.session_id.to_string());
                }
            }
            _ => {}
        }
    }
    assert_eq!(request_session.as_deref(), Some("t1"));
    assert_eq!(call_session.as_deref(), Some("t1"));
    pending_permissions.clear();
}

/// T8A — a permission of a child that is known but not yet announced stays on
/// the root until the child is announced.
#[tokio::test]
async fn t8a_raced_permission_stays_on_the_root_until_announced() {
    let mut rig = T4Rig::new(Some(SubagentForm::Rfd));
    rig.rig.echo("p1");
    // The child starts before its launching Agent frame, so it is registered but
    // not announced.
    rig.rig.feed(
        json!({"type":"system","subtype":"task_started","task_id":"t1",
        "tool_use_id":"toolu_A","description":"Investigate",
        "subagent_type":"Explore","task_type":"local_agent"}),
    );
    let (reply_tx, _reply_rx) = oneshot::channel();
    let mut pending_permissions: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut tokens: HashMap<String, crate::native_subagents::AwaitToken> = HashMap::new();
    let (command_tx, _command_rx) = mpsc::unbounded_channel::<Command>();
    handle_permission(
        &mut rig.rig.tasks,
        json!({"type":"control_request","request_id":"perm1","request":{
            "subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},
            "tool_use_id":"toolu_P","agent_id":"t1","permission_suggestions":[]}}),
        reply_tx,
        None,
        &mut rig.rig.map_state,
        &mut rig.rig.live_background,
        &rig.rig.tx,
        "s",
        &mut pending_permissions,
        &mut tokens,
        &mut HashMap::new(),
        &watch::channel(false).1,
        false,
        std::path::Path::new("/repo"),
        None,
        &[],
        None,
        None,
        None,
        &command_tx,
    );
    let mut request_session = None;
    let mut call_session = None;
    while let Ok(item) = rig.rig.rx.try_recv() {
        match item {
            SessionOutbound::RequestPermission { params, .. } => {
                request_session = params["sessionId"].as_str().map(str::to_string);
            }
            SessionOutbound::Update(notif) => {
                let value = serde_json::to_value(&notif).unwrap();
                if value["update"]["toolCallId"] == "toolu_P" {
                    call_session = Some(notif.session_id.to_string());
                }
            }
            _ => {}
        }
    }
    assert_eq!(request_session.as_deref(), Some("s"));
    assert_eq!(call_session.as_deref(), Some("s"));
    pending_permissions.clear();
}

/// STEP 6 — a SendMessage result that resumed a finished child resumes the
/// native generation under the child session.
#[test]
fn t4a2_a_send_message_result_resumes_the_child() {
    let mut rig = T4Rig::new(Some(SubagentForm::Rfd));
    rig.rig.echo("p1");
    rig.start_child("t1", "toolu_A");
    // The SendMessage call itself surfaced (and is cached).
    rig.rig
        .feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_s","role":"assistant",
        "content":[{"type":"tool_use","id":"toolu_S","name":"SendMessage",
            "input":{"to":"t1","message":"keep going"}}]}}));
    let _ = rig.notifications();
    // The child finished before the result arrives.
    rig.rig.tasks.subagent_task_notification(&json!({
        "type":"system","subtype":"task_notification","task_id":"t1",
        "status":"completed","tool_use_id":"toolu_A"}));
    // Its result resumes the agent.
    rig.rig.feed(json!({"type":"user","parent_tool_use_id":null,
        "tool_use_result":{"success":true,"resumedAgentId":"t1"},
        "message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_S","content":"ok"}]}}));
    let notes = rig.notifications();
    let resumed = notes
        .iter()
        .find(|v| v["update"]["sessionUpdate"] == "subagent_update")
        .expect("the resumed child reports its state");
    assert_eq!(resumed["sessionId"], "s");
    assert_eq!(resumed["update"]["sessionId"], "t1");
    assert_eq!(resumed["update"]["state"]["state"], "running");
    let message = notes
        .iter()
        .find(|v| v["update"]["sessionUpdate"] == "session_message")
        .expect("the delegation's text reaches the child");
    assert_eq!(message["sessionId"], "t1");
    assert_eq!(message["update"]["recipientSessionId"], "t1");
    assert_eq!(
        message["update"]["content"][0]["text"], "keep going",
        "the SendMessage text reaches the wire"
    );
}
