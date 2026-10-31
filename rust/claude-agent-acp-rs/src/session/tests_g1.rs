//! G1: the T8A/T8B gaps that T8 left `missing:` in TEST-MAP, ported with the
//! upstream cases' inputs and expected wire values.
//!
//! Session-level cases drive `handle_session_frame` / `handle_permission` with
//! the state a running actor holds ([`tests_t2a::Rig`]); the cases that need
//! the real actor (the prompt intake's task-plan republish, the async-task stop
//! and the child cancel) drive `run_loop` over a duplex control channel.

use super::tests_t2a::Rig;
use super::*;
use crate::acp_subagents::SubagentForm;
use crate::client_caps::ClientCapabilities;
use serde_json::json;
use tokio::time::{timeout, Duration};

// ---------------------------------------------------------------- harness

/// A running actor with its control channel (the child's stdin lines it wrote,
/// and the responses the test injects), its stream, and its outbound updates.
struct Actor {
    tx: mpsc::UnboundedSender<Command>,
    updates: mpsc::UnboundedReceiver<SessionOutbound>,
    stream: mpsc::UnboundedSender<Value>,
    written: mpsc::UnboundedReceiver<String>,
}

impl Actor {
    fn new(
        caps: ClientCapabilities,
        air: Option<AirClient>,
        logger: Option<std::sync::Arc<crate::logger::Logger>>,
    ) -> Self {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(a, control_in_rx, ControlOptions { on_request: None });
        let (update_tx, updates) = mpsc::unbounded_channel::<SessionOutbound>();
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
        let (written_tx, written) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(b);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            let _ = written_tx.send(trimmed.to_string());
                        }
                    }
                }
            }
        });
        tokio::spawn(run_loop(
            commands,
            tx.clone(),
            stream_rx,
            control,
            control_in_tx,
            update_tx,
            Duration::from_secs(30),
            "s".into(),
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            air,
            caps,
            std::env::temp_dir(),
            false,
            false,
            crate::sdk_options::RawSdkMessages::Off,
            crate::env_snapshot::EnvSnapshot::empty(),
            logger,
            Vec::new(),
        ));
        Self {
            tx,
            updates,
            stream: stream_tx,
            written,
        }
    }

    /// Send a frame on the child's stdout stream and collect what it publishes.
    async fn feed(&mut self, frame: Value) -> Vec<Value> {
        let _ = self.stream.send(frame);
        self.collect().await
    }

    /// Drain the updates published within a short window, acknowledging flushes.
    async fn collect(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        loop {
            match timeout(Duration::from_millis(150), self.updates.recv()).await {
                Ok(Some(SessionOutbound::Flush(ack))) => {
                    let _ = ack.send(());
                }
                Ok(Some(SessionOutbound::Update(notif))) => {
                    out.push(serde_json::to_value(&notif).unwrap());
                }
                Ok(Some(SessionOutbound::RawUpdate(value))) => out.push(value),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        out
    }

    /// Await a command's reply while acknowledging the Flush barriers the actor
    /// publishes before it (INV-20: the response never overtakes its updates).
    async fn reply_and_collect(
        &mut self,
        mut reply_rx: oneshot::Receiver<Result<bool, String>>,
    ) -> (Result<bool, String>, Vec<Value>) {
        let mut out = Vec::new();
        loop {
            tokio::select! {
                reply = &mut reply_rx => {
                    return (reply.expect("the actor answers"), out);
                }
                item = self.updates.recv() => {
                    match item {
                        Some(SessionOutbound::Flush(ack)) => {
                            let _ = ack.send(());
                        }
                        Some(SessionOutbound::Update(notif)) => {
                            out.push(serde_json::to_value(&notif).unwrap());
                        }
                        Some(SessionOutbound::RawUpdate(value)) => out.push(value),
                        Some(_) => {}
                        None => return (reply_rx.await.expect("the actor answers"), out),
                    }
                }
            }
        }
    }

    /// The next `control_request` line the actor wrote, parsed.
    async fn next_request(&mut self) -> Value {
        loop {
            let line = timeout(Duration::from_secs(5), self.written.recv())
                .await
                .expect("the actor writes a control_request")
                .expect("the stdin reader is alive");
            let value: Value = serde_json::from_str(&line).expect("a JSON line");
            if value.get("type").and_then(Value::as_str) == Some("control_request") {
                return value;
            }
        }
    }

    fn respond(&self, request_id: &str, result: Result<Value, &str>) {
        let response = match result {
            Ok(body) => json!({
                "type": "control_response",
                "response": {"subtype": "success", "request_id": request_id, "response": body},
            }),
            Err(message) => json!({
                "type": "control_response",
                "response": {"subtype": "error", "request_id": request_id, "error": message},
            }),
        };
        let _ = self.stream.send(response);
    }
}

fn rfd_caps() -> ClientCapabilities {
    ClientCapabilities::from_value(Some(&json!({"subagents": {}})))
}

fn air_caps() -> (ClientCapabilities, Option<AirClient>) {
    let value = json!({
        "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["asyncTasks"]}}}
    });
    (
        ClientCapabilities::from_value(Some(&value)),
        AirClient::from_client_capabilities(&value),
    )
}

/// A `Rig` with the native subagent runtime of `form` rooted at `"s"`.
fn rig_with(form: Option<SubagentForm>) -> Rig {
    let mut rig = Rig::new();
    rig.tasks = AsyncTaskRuntime::new(true).with_subagent_form(form, "s");
    rig.ext.caps.subagent_form = form;
    rig
}

/// Every outbound notification since the last call, as `{"sessionId", "update"}`.
fn notifications(rig: &mut Rig) -> Vec<Value> {
    let mut out = Vec::new();
    while let Ok(item) = rig.rx.try_recv() {
        match item {
            SessionOutbound::Update(notif) => {
                // The forwarder materialises the ACP defaults on the wire.
                let mut value = serde_json::to_value(&notif).unwrap();
                if let Some(update) = value.get_mut("update") {
                    crate::map::materialize_tool_call_defaults(update);
                }
                out.push(value);
            }
            SessionOutbound::RawUpdate(value) => out.push(value),
            _ => {}
        }
    }
    out
}

fn plan_updates(updates: &[Value]) -> Vec<Value> {
    updates
        .iter()
        .filter(|value| value["update"]["sessionUpdate"] == json!("plan"))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------- T-09

/// Seed the actor's task state through a `TaskCreate` frame (its tool use, then
/// its result) and return the updates it published.
async fn create_task(actor: &mut Actor, id: &str, subject: &str) -> Vec<Value> {
    let tool_use_id = format!("toolu_create_{id}");
    let _ = actor
        .feed(json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":format!("msg_{id}"),"role":"assistant","model":"claude-opus-5",
            "content":[{"type":"tool_use","id":tool_use_id,"name":"TaskCreate",
                "input":{"subject":subject,"description":subject}}]}}))
        .await;
    actor
        .feed(
            json!({"type":"user","parent_tool_use_id":null,"uuid":format!("u_{id}"),
            "message":{"role":"user","content":[{"type":"tool_result",
                "tool_use_id":tool_use_id,"content":"created"}]},
            "tool_use_result":{"task":{"id":id}}}),
        )
        .await
}

/// T8A gap (43): upstream "republishes the session task snapshot when a
/// follow-up prompt starts" — the plan goes out at intake, before the user
/// frame, in task order.
#[tokio::test]
async fn prompt_republishes_the_session_task_snapshot() {
    let mut actor = Actor::new(ClientCapabilities::default(), None, None);
    let _ = create_task(&mut actor, "task-1", "Run tests").await;
    let _ = create_task(&mut actor, "task-2", "Write release notes").await;
    let _ = actor.collect().await;

    let (reply, _reply_rx) = oneshot::channel();
    let _ = actor.tx.send(Command::Prompt {
        uuid: "p1".into(),
        frame: json!({"type":"user","parent_tool_use_id":null,"uuid":"p1",
            "message":{"role":"user","content":"Continue"}}),
        is_local_only: false,
        prompt_text: "Continue".into(),
        reply,
    });
    let updates = actor.collect().await;
    let plans = plan_updates(&updates);
    assert_eq!(plans.len(), 1, "{updates:?}");
    assert_eq!(
        serde_json::to_string(&plans[0]["update"]).unwrap(),
        json!({
            "sessionUpdate": "plan",
            "entries": [
                {"content": "Run tests", "status": "pending", "priority": "medium"},
                {"content": "Write release notes", "status": "pending", "priority": "medium"}
            ]
        })
        .to_string()
    );
    // The snapshot precedes the user frame.
    let plan_at = updates
        .iter()
        .position(|value| value["update"]["sessionUpdate"] == json!("plan"))
        .expect("a plan");
    let user_at = updates
        .iter()
        .position(|value| value["update"]["sessionUpdate"] == json!("user_message_chunk"));
    assert!(
        user_at.is_none_or(|user_at| plan_at < user_at),
        "the plan goes out before the user frame: {updates:?}"
    );
}

/// T8A gap (44): upstream "does not republish a completed plan on an unrelated
/// follow-up".
#[tokio::test]
async fn prompt_does_not_republish_a_completed_plan() {
    let mut actor = Actor::new(ClientCapabilities::default(), None, None);
    let updates = create_task(&mut actor, "task-1", "Run tests").await;
    assert!(!plan_updates(&updates).is_empty(), "the create publishes");
    // The task completes through TaskUpdate.
    actor
        .feed(json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_update","role":"assistant","model":"claude-opus-5",
            "content":[{"type":"tool_use","id":"toolu_update","name":"TaskUpdate",
                "input":{"taskId":"task-1","status":"completed"}}]}}))
        .await;
    let updates = actor
        .feed(
            json!({"type":"user","parent_tool_use_id":null,"uuid":"u_update",
            "message":{"role":"user","content":[{"type":"tool_result",
                "tool_use_id":"toolu_update","content":"updated"}]}}),
        )
        .await;
    assert!(
        plan_updates(&updates)
            .iter()
            .any(|plan| plan["update"]["entries"][0]["status"] == json!("completed")),
        "the completed plan publishes once: {updates:?}"
    );
    let _ = actor.collect().await;

    let (reply, _reply_rx) = oneshot::channel();
    let _ = actor.tx.send(Command::Prompt {
        uuid: "p2".into(),
        frame: json!({"type":"user","parent_tool_use_id":null,"uuid":"p2",
            "message":{"role":"user","content":"Start something else"}}),
        is_local_only: false,
        prompt_text: "Start something else".into(),
        reply,
    });
    let updates = actor.collect().await;
    assert!(
        plan_updates(&updates).is_empty(),
        "a completed plan is not republished: {updates:?}"
    );
}

// ---------------------------------------------------------------- U-68

/// T8A gap (207): upstream "omits the attribution (and logs the miss) when the
/// agent id has no recorded parent".
#[tokio::test]
async fn permission_without_a_recorded_parent_logs_the_miss() {
    let path = std::env::temp_dir().join(format!(
        "g1-u68-log-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let logger = std::sync::Arc::new(crate::logger::Logger::to_file(&path));
    let mut rig = rig_with(None);
    let (reply_tx, _reply_rx) = oneshot::channel();
    let mut pending_permissions: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let (command_tx, _command_rx) = mpsc::unbounded_channel::<Command>();
    handle_permission(
        &mut rig.tasks,
        json!({"type":"control_request","request_id":"perm1","request":{
            "subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},
            "tool_use_id":"toolu_sub","agent_id":"agent-unknown",
            "permission_suggestions":[]}}),
        reply_tx,
        None,
        &mut rig.map_state,
        &mut rig.live_background,
        &rig.tx,
        "s",
        &mut pending_permissions,
        &mut HashMap::new(),
        &mut HashMap::new(),
        &watch::channel(false).1,
        false,
        std::path::Path::new("/repo"),
        Some(&logger),
        &[],
        None,
        None,
        None,
        &command_tx,
    );
    pending_permissions.clear();
    let log = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    assert!(
        log.contains("agent-unknown"),
        "the attribution miss names the agent id: {log}"
    );
    // The surfaced tool call omits the attribution.
    let notes = notifications(&mut rig);
    let call = notes
        .iter()
        .find(|value| value["update"]["toolCallId"] == json!("toolu_sub"))
        .expect("the tool call is surfaced");
    assert!(call["update"]["_meta"]["claudeCode"]
        .get("parentToolUseId")
        .is_none());
}

// ---------------------------------------------------------------- P-01

/// T8B gap (662): upstream "uses eager child ownership when native lineage has
/// not arrived yet" — a denial whose agent id has no lineage is kept when the
/// eager tool-call session map knows the child, and it lands in that child.
#[test]
fn denial_uses_the_eager_child_ownership_without_lineage() {
    let mut rig = rig_with(Some(SubagentForm::Rfd));
    rig.map_state.mark_emitted(
        "toolu_inner".to_string(),
        json!({"type":"tool_use","id":"toolu_inner","name":"Write","input":{}}),
    );
    rig.map_state
        .record_eager_session("toolu_inner", "child-session");
    rig.feed(json!({"type":"system","subtype":"permission_denied",
        "tool_use_id":"toolu_inner","tool_name":"Write",
        "agent_id":"agent-racing",
        "decision_reason_type":"mode",
        "message":"Permission to use Write has been denied."}));
    let notes = notifications(&mut rig);
    let denials: Vec<&Value> = notes
        .iter()
        .filter(|value| {
            value["update"]["sessionUpdate"] == json!("tool_call_update")
                && value["update"]["toolCallId"] == json!("toolu_inner")
        })
        .collect();
    assert_eq!(denials.len(), 1, "{notes:?}");
    assert_eq!(denials[0]["sessionId"], json!("child-session"));
    assert_eq!(denials[0]["update"]["status"], json!("failed"));
}

// ---------------------------------------------------------------- C-24

/// T8A gap (221): upstream "releases a tool call that ends cancelled on v2, as
/// one that ends failed" — an aborted result is `cancelled` on v2 and `failed`
/// on v1, and the call leaves the books.
#[test]
fn aborted_tool_result_is_cancelled_on_v2_and_failed_on_v1() {
    for (v2, expected) in [(true, "cancelled"), (false, "failed")] {
        let mut rig = rig_with(None);
        rig.map_state.v2.set_reporting(v2);
        rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_bash","role":"assistant","model":"claude-sonnet-4-5",
            "content":[{"type":"tool_use","id":"toolu_bash","name":"Bash",
                "input":{"command":"make"}}]}}));
        rig.map_state
            .books
            .record_foreground(Some("p1"), "toolu_bash");
        rig.feed(json!({"type":"user","parent_tool_use_id":null,"uuid":"u1",
            "tool_result_meta":[{"id":"toolu_bash","non_execution_kind":"interrupted"}],
            "message":{"role":"user","content":[{"type":"tool_result",
                "tool_use_id":"toolu_bash","is_error":true,
                "content":"[Request interrupted by user for tool use]"}]}}));
        let notes = notifications(&mut rig);
        let last = notes
            .iter()
            .rfind(|value| {
                value["update"]["sessionUpdate"] == json!("tool_call_update")
                    && value["update"].get("status").is_some()
                    && value["update"]["toolCallId"] == json!("toolu_bash")
            })
            .expect("the result resolves the call");
        let status = if v2 {
            crate::v2::v2_session_update(&last["update"])
                .expect("a v2 update")
                .expect("a translated update")["status"]
                .clone()
        } else {
            last["update"]["status"].clone()
        };
        assert_eq!(status, json!(expected), "{notes:?}");
        assert!(!rig.map_state.has_emitted("toolu_bash"));
        assert!(!rig.map_state.books.has_foreground());
    }
}

/// T8A gap (221), closing path: the report that closes a call a cancel stopped
/// is `cancelled` on v2 (`AcpToolCallRenderer.unfinished`).
#[test]
fn a_stopped_closing_report_is_cancelled_on_v2() {
    let mut rig = rig_with(None);
    rig.map_state.v2.set_reporting(true);
    rig.map_state.mark_emitted(
        "toolu_a".to_string(),
        json!({"type":"tool_use","id":"toolu_a","name":"Bash","input":{}}),
    );
    rig.map_state
        .books
        .record_dispatched(&json!([{"type":"tool_use","id":"toolu_a","name":"Bash"}]));
    rig.map_state.begin_stopped_work();
    let closing = crate::unfinished::close_cancelled_tool_calls(&mut rig.map_state);
    send_closing(&mut rig.tasks, &mut rig.map_state, closing, &rig.tx, "s");
    let notes = notifications(&mut rig);
    let report = notes
        .iter()
        .find(|value| value["update"]["toolCallId"] == json!("toolu_a"))
        .expect("the closing report");
    assert_eq!(report["update"]["status"], json!("cancelled"), "{notes:?}");
}

// ---------------------------------------------------------------- M-22 (231)

/// T8A gap (231): upstream "logs a stop that Claude Code refuses, and changes
/// nothing" — a refused child `stop_task` is logged and publishes nothing.
#[tokio::test]
async fn refused_child_stop_is_logged_and_changes_nothing() {
    let path = std::env::temp_dir().join(format!(
        "g1-m22a-log-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let logger = std::sync::Arc::new(crate::logger::Logger::to_file(&path));
    let mut actor = Actor::new(rfd_caps(), None, Some(logger));
    // The child starts and is announced by the RFD runtime.
    let _ = actor
        .feed(json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_agent","role":"assistant","model":"claude-opus-5",
            "content":[{"type":"tool_use","id":"toolu_parent","name":"Agent",
                "input":{"description":"Explore","prompt":"Go"}}]}}))
        .await;
    let _ = actor
        .feed(
            json!({"type":"system","subtype":"task_started","task_id":"agent-42",
            "tool_use_id":"toolu_parent","subagent_type":"Explore",
            "description":"Explore","prompt":"Go"}),
        )
        .await;

    let (reply, reply_rx) = oneshot::channel();
    let _ = actor.tx.send(Command::CancelChild {
        session_id: "agent-42".into(),
        reply,
    });
    assert!(reply_rx.await.expect("the actor answers"), "it is a child");
    let request = actor.next_request().await;
    assert_eq!(
        request.pointer("/request/subtype"),
        Some(&json!("stop_task"))
    );
    let request_id = request["request_id"].as_str().expect("an id");
    actor.respond(request_id, Err("No such task"));
    let updates = actor.collect().await;
    assert!(
        updates.is_empty(),
        "accepting the stop reports nothing: {updates:?}"
    );
    let log = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    assert!(
        log.contains("failed to stop the task agent-42 of subagent session agent-42"),
        "the refusal is logged: {log}"
    );
}

// ---------------------------------------------------------------- M-22 (259)

/// T8A gap (259): upstream "stops one advertised async task through the SDK
/// without cancelling the prompt" — a failed stop releases the claim, the retry
/// stops the task, and one transcript line acknowledges it.
#[tokio::test]
async fn a_failed_async_task_stop_releases_the_claim_for_a_retry() {
    let (caps, air) = air_caps();
    let mut actor = Actor::new(caps, air, None);
    let _ = actor
        .feed(json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_workflow","role":"assistant","model":"claude-opus-5",
            "content":[{"type":"tool_use","id":"toolu_workflow","name":"Workflow",
                "input":{"command":"run"}}]}}))
        .await;
    let advertised = actor
        .feed(
            json!({"type":"system","subtype":"task_started","task_id":"workflow-1",
            "tool_use_id":"toolu_workflow","task_type":"local_workflow",
            "is_backgrounded":true}),
        )
        .await;
    assert!(
        advertised
            .iter()
            .any(|value| value["update"]["sessionUpdate"] == json!("async_task_spawned")),
        "the task is advertised: {advertised:?}"
    );

    let (reply, reply_rx) = oneshot::channel();
    let _ = actor.tx.send(Command::StopAsyncTask {
        task_id: "workflow-1".into(),
        reply,
    });
    let request = actor.next_request().await;
    assert_eq!(
        request.pointer("/request/subtype"),
        Some(&json!("stop_task"))
    );
    let request_id = request["request_id"].as_str().expect("an id");
    actor.respond(request_id, Err("SDK stop failed"));
    let (failed, updates) = actor.reply_and_collect(reply_rx).await;
    assert_eq!(failed, Err("SDK stop failed".to_string()));
    assert!(
        !updates
            .iter()
            .any(|value| value["update"]["sessionUpdate"] == json!("async_task_state_update")),
        "the failed stop never reaches a terminal state: {updates:?}"
    );

    let (reply, reply_rx) = oneshot::channel();
    let _ = actor.tx.send(Command::StopAsyncTask {
        task_id: "workflow-1".into(),
        reply,
    });
    let request = actor.next_request().await;
    assert_eq!(
        request.pointer("/request/subtype"),
        Some(&json!("stop_task"))
    );
    let request_id = request["request_id"].as_str().expect("an id");
    actor.respond(request_id, Ok(json!({})));
    let (stopped, updates) = actor.reply_and_collect(reply_rx).await;
    assert_eq!(stopped, Ok(true));
    assert!(
        updates.iter().any(|value| value["update"]["sessionUpdate"]
            == json!("async_task_state_update")
            && value["update"]["state"] == json!("stopped")),
        "{updates:?}"
    );
    // One transcript line for the two attempts: the failed one published none.
    let lines: Vec<&Value> = updates
        .iter()
        .filter(|value| value["update"]["sessionUpdate"] == json!("agent_message_chunk"))
        .collect();
    assert_eq!(lines.len(), 1, "{updates:?}");
}

// ---------------------------------------------------------------- U-11 (265)

/// T8A gap (265): upstream "completes an Agent card created by a permission
/// after its control frame was suppressed" — the permission re-surfaces nothing
/// (the streamed call already exists) and the launch result completes the card
/// once, in the root session.
#[tokio::test]
async fn a_permission_surfaced_agent_card_completes_after_suppression() {
    let mut rig = rig_with(Some(SubagentForm::Rfd));
    rig.echo("p1");
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_agent","role":"assistant","model":"claude-opus-5",
        "content":[{"type":"tool_use","id":"toolu_agent","name":"Agent",
            "input":{"description":"Investigate","prompt":"Find the bug"}}]}}));
    let first = notifications(&mut rig);

    let (reply_tx, _reply_rx) = oneshot::channel();
    let mut pending_permissions: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let (command_tx, _command_rx) = mpsc::unbounded_channel::<Command>();
    handle_permission(
        &mut rig.tasks,
        json!({"type":"control_request","request_id":"perm1","request":{
            "subtype":"can_use_tool","tool_name":"Agent",
            "input":{"description":"Investigate","prompt":"Find the bug"},
            "tool_use_id":"toolu_agent","permission_suggestions":[]}}),
        reply_tx,
        None,
        &mut rig.map_state,
        &mut rig.live_background,
        &rig.tx,
        "s",
        &mut pending_permissions,
        &mut HashMap::new(),
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
    pending_permissions.clear();
    let suppressed = notifications(&mut rig);
    assert!(
        !suppressed
            .iter()
            .any(|value| value["update"]["toolCallId"] == json!("toolu_agent")),
        "the permission re-surfaces nothing: {suppressed:?}"
    );

    rig.feed(json!({"type":"system","subtype":"task_started",
        "task_id":"agent-1","tool_use_id":"toolu_agent",
        "subagent_type":"Explore","description":"Investigate"}));
    rig.feed(json!({"type":"user","parent_tool_use_id":null,
        "uuid":"u2","message":{"role":"user","content":[{"type":"tool_result",
            "tool_use_id":"toolu_agent","content":"done"}]}}));
    let mut updates = first;
    updates.extend(notifications(&mut rig));
    let control: Vec<&Value> = updates
        .iter()
        .filter(|value| {
            (value["update"]["sessionUpdate"] == json!("tool_call")
                || value["update"]["sessionUpdate"] == json!("tool_call_update"))
                && value["update"]["toolCallId"] == json!("toolu_agent")
        })
        .collect();
    assert_eq!(
        control
            .iter()
            .map(|value| value["sessionId"].as_str().unwrap_or(""))
            .collect::<Vec<_>>(),
        vec!["s", "s"]
    );
    assert_eq!(
        control
            .iter()
            .map(|value| value["update"]
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or(""))
            .collect::<Vec<_>>(),
        vec!["pending", "completed"]
    );
}
