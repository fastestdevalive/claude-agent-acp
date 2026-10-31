//! Session-level tests added by the Turn 1 fix round (lane A, T1FA).

use super::*;

/// Drives `handle_session_frame` with the state a running actor would hold.
struct Rig {
    machine: TurnMachine,
    map_state: MapState,
    live_background: HashMap<String, String>,
    pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>>,
    emitted_assistant_text: bool,
    assistant_had_error: bool,
    tx: mpsc::UnboundedSender<SessionOutbound>,
    rx: mpsc::UnboundedReceiver<SessionOutbound>,
    high_water: Arc<AtomicUsize>,
    tasks: AsyncTaskRuntime,
    ext: SessionExt,
    replies: HashMap<String, oneshot::Receiver<Result<PromptReply, PromptFailure>>>,
}

impl Rig {
    fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            machine: TurnMachine::new(),
            map_state: MapState::default(),
            live_background: HashMap::new(),
            pending: HashMap::new(),
            emitted_assistant_text: false,
            assistant_had_error: false,
            tx,
            rx,
            high_water: Arc::new(AtomicUsize::new(0)),
            tasks: AsyncTaskRuntime::new(false),
            ext: SessionExt::new(
                None,
                PathBuf::new(),
                crate::env_snapshot::EnvSnapshot::empty(),
            ),
            replies: HashMap::new(),
        }
    }

    fn feed(&mut self, line: Value) {
        handle_session_frame(
            line,
            &mut self.machine,
            &mut self.map_state,
            &mut self.live_background,
            &mut self.pending,
            &mut self.emitted_assistant_text,
            &mut self.assistant_had_error,
            &self.tx,
            "s",
            &self.high_water,
            &mut self.tasks,
            &mut self.ext,
        );
    }

    fn echo(&mut self, uuid: &str) {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.pending.insert(uuid.to_string(), reply_tx);
        self.replies.insert(uuid.to_string(), reply_rx);
        self.machine.enqueue(Turn::new(uuid.into(), false));
        self.feed(json!({"type":"user","uuid":uuid,"message":{"role":"user",
            "content":[{"type":"text","text":"hi"}]}}));
    }

    fn idle(&mut self) {
        self.feed(json!({"type":"system","subtype":"session_state_changed","state":"idle"}));
    }

    fn text(&mut self, text: &str) {
        self.feed(json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_1","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"text","text":text}]}}));
    }

    /// The text of every `agent_message_chunk` sent since the last call.
    fn delivered(&mut self) -> String {
        let mut text = String::new();
        while let Ok(item) = self.rx.try_recv() {
            if let SessionOutbound::Update(notif) = item {
                text.push_str(&chunk_text(&notif));
            }
        }
        text
    }

    /// The result-text fallback chunks that ride on `uuid`'s prompt reply.
    fn reply_text(&mut self, uuid: &str) -> String {
        let mut reply = self.replies.remove(uuid).expect("prompt was echoed");
        match reply.try_recv().ok() {
            Some(Ok(reply)) => reply.updates.iter().map(chunk_text).collect(),
            _ => String::new(),
        }
    }
}

fn chunk_text(notif: &SessionNotification) -> String {
    let update = serde_json::to_value(&notif.update).unwrap();
    if update["sessionUpdate"] == "agent_message_chunk" {
        update["content"]["text"].as_str().unwrap_or("").to_string()
    } else {
        String::new()
    }
}

/// A cache-replayed result: its text lives only in the result frame.
fn cached_result(text: &str) -> Value {
    json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn",
        "result":text,"usage":{"input_tokens":1,"output_tokens":0}})
}

/// Finding 1 (N-12) — prompt A streams partial text, the stream drops and idle
/// fails A with "no result": prompt B, answered from the cache with its text
/// only in the result, must still get the result-text fallback.
#[test]
fn t1fa_failed_turn_does_not_suppress_the_next_prompts_fallback() {
    let mut rig = Rig::new();
    rig.echo("pa");
    rig.text("partial");
    assert!(rig.emitted_assistant_text, "A's text was delivered");
    rig.idle();
    assert!(
        !rig.emitted_assistant_text,
        "the failed turn closed the stretch"
    );
    rig.delivered();

    rig.echo("pb");
    rig.feed(cached_result("cached answer"));
    assert_eq!(rig.reply_text("pb"), "cached answer");
}

/// Finding 1 — a held turn drained at idle closes the stretch as well: text
/// streamed by its follow-ups must not suppress the next prompt's fallback.
#[test]
fn t1fa_held_drain_at_idle_does_not_suppress_the_next_fallback() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.machine.on_task_started("t1", true);
    rig.feed(cached_result("done"));
    rig.machine.on_task_ended("t1");
    rig.text("follow-up prose");
    assert!(rig.emitted_assistant_text);
    rig.idle();
    assert!(
        !rig.emitted_assistant_text,
        "the held drain closed the stretch"
    );
    rig.delivered();

    rig.echo("p2");
    rig.feed(cached_result("answer two"));
    assert_eq!(rig.reply_text("p2"), "answer two");
}

/// Finding 1 — a held turn handed off at the next prompt's echo closes the
/// stretch too.
#[test]
fn t1fa_held_handoff_at_echo_closes_the_stretch() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.machine.on_task_started("t1", true);
    rig.feed(cached_result("done"));
    rig.text("follow-up prose");
    assert!(rig.emitted_assistant_text);
    rig.echo("p2");
    assert!(!rig.emitted_assistant_text);
}

/// Finding 6 (B-18) through the real frame handler: after a cancel settled the
/// turn, the stopped work's tool result updates nothing on the client.
#[test]
fn t1fa_stopped_work_tool_result_reaches_no_client_update() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"m1","model":"claude-sonnet-4-5","role":"assistant",
        "content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]}}));
    rig.machine.cancel();
    rig.map_state.begin_stopped_work();
    rig.idle();
    assert!(rig.machine.work_end_reported());
    while rig.rx.try_recv().is_ok() {}

    rig.feed(json!({"type":"user","parent_tool_use_id":null,
        "message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}}));
    assert!(
        rig.rx.try_recv().is_err(),
        "nothing the stopped work sends reaches the client"
    );
}

// ---------------------------------------------------------------- finding 7

/// A `run_loop` actor without a child process.
struct LoopRig {
    tx: mpsc::UnboundedSender<Command>,
    stream_tx: Option<mpsc::UnboundedSender<Value>>,
    update_rx: mpsc::UnboundedReceiver<SessionOutbound>,
    written: mpsc::UnboundedReceiver<String>,
    actor: tokio::task::JoinHandle<()>,
}

impl LoopRig {
    fn spawn() -> Self {
        Self::spawn_with(None, crate::client_caps::ClientCapabilities::default())
    }

    fn spawn_with(air: Option<AirClient>, caps: crate::client_caps::ClientCapabilities) -> Self {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(a, control_in_rx, ControlOptions { on_request: None });
        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
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
        let actor = tokio::spawn(run_loop(
            commands,
            tx.clone(),
            stream_rx,
            control,
            control_in_tx,
            update_tx,
            Duration::from_secs(30),
            "s1".into(),
            Arc::new(AtomicUsize::new(0)),
            air,
            caps,
            std::env::temp_dir(),
            false,
            false,
            crate::sdk_options::RawSdkMessages::Off,
            crate::env_snapshot::EnvSnapshot::empty(),
            None,
            Vec::new(),
        ));
        Self {
            tx,
            stream_tx: Some(stream_tx),
            update_rx,
            written,
            actor,
        }
    }

    /// The next user frame the actor wrote to the child (an injected steer or a
    /// prompt), parsed.
    async fn next_written_user(&mut self) -> Value {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(10), self.written.recv())
                .await
                .expect("the actor writes a user frame")
                .expect("the stdin reader is alive");
            let value: Value = serde_json::from_str(&line).expect("a JSON line");
            if value.get("type").and_then(Value::as_str) == Some("user") {
                return value;
            }
        }
    }

    /// The next `control_request` of `subtype` the actor wrote to the child.
    async fn next_written_control_request(&mut self, subtype: &str) -> Value {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(10), self.written.recv())
                .await
                .expect("the actor writes a control request")
                .expect("the stdin reader is alive");
            let value: Value = serde_json::from_str(&line).expect("a JSON line");
            if value.get("type").and_then(Value::as_str) == Some("control_request")
                && value.pointer("/request/subtype").and_then(Value::as_str) == Some(subtype)
            {
                return value;
            }
        }
    }

    fn permission(&self, tool_use_id: &str) -> oneshot::Receiver<Value> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::Permission {
                frame: json!({"type":"control_request","request_id":format!("req-{tool_use_id}"),
                    "request":{"subtype":"can_use_tool","tool_name":"Bash",
                    "input":{"command":"ls"},"tool_use_id":tool_use_id,
                    "permission_suggestions":[]}}),
                reply: reply_tx,
            })
            .unwrap();
        reply_rx
    }

    /// The reply channel of the next `session/request_permission` the client gets.
    async fn next_request(&mut self) -> oneshot::Sender<Result<Value, String>> {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), self.update_rx.recv())
                .await
                .expect("a permission request reaches the client")
            {
                Some(SessionOutbound::RequestPermission { reply, .. }) => return reply,
                Some(_) => {}
                None => panic!("the update channel closed"),
            }
        }
    }
}

/// When the CLI exits, the dialog still open on the client is withdrawn (its
/// round-trip is aborted, so the forwarding side sends `$/cancel_request`).
#[tokio::test]
async fn t1fa_open_permission_is_withdrawn_when_the_stream_ends() {
    let mut rig = LoopRig::spawn();
    let _answer = rig.permission("toolu_open");
    let mut client_side = rig.next_request().await;
    assert!(!client_side.is_closed(), "the dialog is open");

    rig.stream_tx = None;
    tokio::time::timeout(Duration::from_secs(10), client_side.closed())
        .await
        .expect("the open dialog is withdrawn when the session ends");
    let _ = tokio::time::timeout(Duration::from_secs(10), &mut rig.actor).await;
}

/// A request that arrives during the close window is withdrawn when the close
/// finishes, not detached.
#[tokio::test]
async fn t1fa_permission_in_the_close_window_is_withdrawn() {
    let mut rig = LoopRig::spawn();
    let (reply_tx, _reply_rx) = oneshot::channel();
    rig.tx.send(Command::Close { reply: reply_tx }).unwrap();
    let _answer = rig.permission("toolu_late");
    // Once the close finishes the actor is gone and nothing may keep a dialog
    // alive: the client sees the request withdrawn, or never sees it at all
    // (the update channel closes once every sender is dropped).
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(out) = rig.update_rx.recv().await {
            if let SessionOutbound::RequestPermission { mut reply, .. } = out {
                reply.closed().await;
            }
        }
    })
    .await;
    assert!(
        outcome.is_ok(),
        "a dialog opened in the close window outlives the session"
    );
    let _ = tokio::time::timeout(Duration::from_secs(10), &mut rig.actor).await;
}

// ---------------------------------------------------------------- T8A eager permission tool calls

impl LoopRig {
    /// Send a `can_use_tool` control request through the actor.
    fn permission_frame(&self, frame: Value) {
        let (reply_tx, _reply_rx) = oneshot::channel();
        self.tx
            .send(Command::Permission {
                frame,
                reply: reply_tx,
            })
            .unwrap();
    }

    /// The next `session/request_permission`, with the updates the actor sent
    /// before it.
    async fn next_request_after(&mut self) -> (Vec<Value>, oneshot::Sender<Result<Value, String>>) {
        let mut updates = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), self.update_rx.recv())
                .await
                .expect("an outbound arrives")
            {
                Some(SessionOutbound::RequestPermission { reply, .. }) => {
                    return (updates, reply);
                }
                Some(SessionOutbound::Update(notif)) => {
                    let mut value = serde_json::to_value(&notif.update).unwrap();
                    crate::map::materialize_tool_call_defaults(&mut value);
                    crate::map::materialize_diff_old_text(&mut value);
                    updates.push(value);
                }
                Some(SessionOutbound::RawUpdate(value)) => {
                    updates.push(value["update"].clone());
                }
                Some(_) => {}
                None => panic!("the update channel closed"),
            }
        }
    }
}

fn can_use_tool(tool: &str, input: Value, id: &str, extra: Value) -> Value {
    let mut request = json!({
        "subtype": "can_use_tool",
        "tool_name": tool,
        "input": input,
        "tool_use_id": id,
        "permission_suggestions": [],
    });
    if let Value::Object(extra) = extra {
        for (key, value) in extra {
            request[key] = value;
        }
    }
    json!({"type": "control_request", "request_id": format!("req-{id}"), "request": request})
}

fn air_client() -> AirClient {
    AirClient::from_client_capabilities(&json!({
        "_meta": {"jetbrains": {"air": {"version": 1}}}
    }))
    .expect("an AIR client")
}

/// T8A — the eager `tool_call` of an AIR session precedes the permission request
/// and carries the command title.
#[tokio::test]
async fn t8a_eager_tool_call_precedes_the_permission_request() {
    let mut rig = LoopRig::spawn_with(Some(air_client()), Default::default());
    rig.permission_frame(can_use_tool(
        "Bash",
        json!({"command": "git diff", "description": "Show current diff"}),
        "tool-1",
        json!({"decision_reason": "This action requires interactive safety approval."}),
    ));
    let (updates, reply) = rig.next_request_after().await;
    assert_eq!(
        updates.len(),
        1,
        "only the tool_call precedes it: {updates:?}"
    );
    assert_eq!(updates[0]["sessionUpdate"], "tool_call");
    assert_eq!(updates[0]["toolCallId"], "tool-1");
    assert_eq!(updates[0]["status"], "pending");
    assert_eq!(updates[0]["title"], "git diff");
    assert_eq!(updates[0]["_meta"]["claudeCode"]["toolName"], "Bash");
    assert_eq!(
        updates[0]["_meta"]["jetbrains"]["air"]["commandTitle"],
        "Show current diff"
    );
    let _ = reply.send(Ok(json!({"outcome": "selected", "optionId": "allow-once"})));
}

/// T8A — PowerShell's eager call carries its description like Bash.
#[tokio::test]
async fn t8a_eager_powershell_carries_its_command_title() {
    let mut rig = LoopRig::spawn_with(Some(air_client()), Default::default());
    rig.permission_frame(can_use_tool(
        "PowerShell",
        json!({"command": "Get-ChildItem", "description": "List files"}),
        "tool-1",
        json!({}),
    ));
    let (updates, reply) = rig.next_request_after().await;
    assert_eq!(updates[0]["sessionUpdate"], "tool_call");
    assert_eq!(updates[0]["toolCallId"], "tool-1");
    assert_eq!(updates[0]["title"], "Get-ChildItem");
    assert_eq!(updates[0]["_meta"]["claudeCode"]["toolName"], "PowerShell");
    assert_eq!(
        updates[0]["_meta"]["jetbrains"]["air"]["commandTitle"],
        "List files"
    );
    let _ = reply.send(Ok(json!({"outcome": "selected", "optionId": "allow-once"})));
}

/// T8A — a tool call the eager path already surfaced is not emitted again.
#[tokio::test]
async fn t8a_eager_call_is_not_reemitted() {
    let mut rig = LoopRig::spawn_with(Some(air_client()), Default::default());
    rig.permission_frame(can_use_tool(
        "Bash",
        json!({"command": "ls"}),
        "tool-1",
        json!({}),
    ));
    let (first, reply) = rig.next_request_after().await;
    assert_eq!(first[0]["sessionUpdate"], "tool_call");
    let _ = reply.send(Ok(json!({"outcome": "selected", "optionId": "allow-once"})));

    rig.permission_frame(can_use_tool(
        "Bash",
        json!({"command": "ls"}),
        "tool-1",
        json!({}),
    ));
    let (second, reply) = rig.next_request_after().await;
    assert!(
        second
            .iter()
            .all(|u| u["sessionUpdate"] != "tool_call" && u["sessionUpdate"] != "tool_call_update"),
        "no duplicate call: {second:?}"
    );
    let _ = reply.send(Ok(json!({"outcome": "selected", "optionId": "allow-once"})));
}

/// T8A — a plan-rendered TodoWrite still surfaces a `tool_call` before its
/// permission request (issue #851).
#[tokio::test]
async fn t8a_eager_todowrite_call_precedes_the_permission_request() {
    let mut rig = LoopRig::spawn();
    rig.permission_frame(can_use_tool(
        "TodoWrite",
        json!({"todos": [{"content": "x", "status": "pending"}]}),
        "todo-1",
        json!({}),
    ));
    let (updates, reply) = rig.next_request_after().await;
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["sessionUpdate"], "tool_call");
    assert_eq!(updates[0]["toolCallId"], "todo-1");
    let _ = reply.send(Ok(json!({"outcome": "selected", "optionId": "allow-once"})));
}

/// T8A — a terminal-capable client's eager Bash call carries
/// `_meta.terminal_info` so the later terminal output can attach.
#[tokio::test]
async fn t8a_eager_bash_call_carries_terminal_info() {
    let caps = crate::client_caps::ClientCapabilities {
        terminal_output: true,
        ..Default::default()
    };
    let mut rig = LoopRig::spawn_with(None, caps);
    rig.permission_frame(can_use_tool(
        "Bash",
        json!({"command": "ls"}),
        "tool-1",
        json!({}),
    ));
    let (updates, reply) = rig.next_request_after().await;
    assert_eq!(updates[0]["sessionUpdate"], "tool_call");
    assert_eq!(updates[0]["toolCallId"], "tool-1");
    assert_eq!(
        updates[0]["_meta"]["terminal_info"],
        json!({"terminal_id": "tool-1"})
    );
    let _ = reply.send(Ok(json!({"outcome": "selected", "optionId": "allow-once"})));
}

impl LoopRig {
    /// The next `session/request_permission` params, with its reply sender.
    async fn next_request_params(&mut self) -> (Value, oneshot::Sender<Result<Value, String>>) {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), self.update_rx.recv())
                .await
                .expect("an outbound arrives")
            {
                Some(SessionOutbound::RequestPermission { reply, params }) => {
                    return (params, reply);
                }
                Some(_) => {}
                None => panic!("the update channel closed"),
            }
        }
    }
}

/// T8A — an ask that carries a `decision_reason` is still presented to the
/// client (the CLI's bypass evaluation never bypasses a forced ask).
#[tokio::test]
async fn t8a_an_ask_with_a_decision_reason_prompts() {
    let mut rig = LoopRig::spawn();
    rig.permission_frame(can_use_tool(
        "Bash",
        json!({"command": "ls"}),
        "tool-1",
        json!({"decision_reason": "This action requires interactive safety approval."}),
    ));
    let (params, reply) = rig.next_request_params().await;
    assert_eq!(params["sessionId"], "s1");
    assert_eq!(params["toolCall"]["toolCallId"], "tool-1");
    let _ = reply.send(Ok(json!({"outcome": "selected", "optionId": "allow-once"})));
}

/// T8A — an ask forced by a `matchedAskRule` still prompts the client.
#[tokio::test]
async fn t8a_a_matched_ask_rule_prompts() {
    let mut rig = LoopRig::spawn();
    rig.permission_frame(can_use_tool(
        "Bash",
        json!({"command": "terraform destroy"}),
        "tool-1",
        json!({"matched_ask_rule": {
            "source": "projectSettings",
            "toolName": "Bash",
            "ruleContent": "Bash(terraform:*)",
        }}),
    ));
    let (params, reply) = rig.next_request_params().await;
    assert_eq!(params["toolCall"]["toolCallId"], "tool-1");
    let _ = reply.send(Ok(json!({"outcome": "selected", "optionId": "allow-once"})));
}

/// T8A — a suppressed persistent rule offers no always-allow option, and the
/// AIR meta names the command.
#[tokio::test]
async fn t8a_a_suppressed_persistent_rule_offers_no_always_allow() {
    let mut rig = LoopRig::spawn_with(Some(air_client()), Default::default());
    rig.permission_frame(can_use_tool(
        "Bash",
        json!({"command": "rm -rf build"}),
        "tool-1",
        json!({
            "suppress_always_allow_rule": true,
            "permission_suggestions": [{
                "type": "addRules",
                "rules": [{"toolName": "Bash", "ruleContent": "rm:*"}],
                "behavior": "allow",
                "destination": "localSettings",
            }],
        }),
    ));
    let (params, reply) = rig.next_request_params().await;
    let ids: Vec<&str> = params["options"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|option| option["optionId"].as_str())
        .collect();
    assert_eq!(ids, vec!["allow-once", "reject"]);
    assert_eq!(
        params["_meta"]["jetbrains"]["air"]["permission"],
        json!({"version": 1, "title": "rm -rf build"})
    );
    let _ = reply.send(Ok(json!({"outcome": "selected", "optionId": "allow-once"})));
}

/// T8A — `defaultToNo` leads with the reject option and rides the AIR meta.
#[tokio::test]
async fn t8a_default_to_no_leads_with_reject_and_forwards_the_hint() {
    let mut rig = LoopRig::spawn_with(Some(air_client()), Default::default());
    rig.permission_frame(can_use_tool(
        "Bash",
        json!({"command": "rm -rf build"}),
        "tool-1",
        json!({
            "default_to_no": true,
            "permission_suggestions": [{
                "type": "addRules",
                "rules": [{"toolName": "Bash", "ruleContent": "rm:*"}],
                "behavior": "allow",
                "destination": "localSettings",
            }],
        }),
    ));
    let (params, reply) = rig.next_request_params().await;
    let kinds: Vec<&str> = params["options"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|option| option["kind"].as_str())
        .collect();
    assert_eq!(kinds, vec!["reject_once", "allow_once", "allow_always"]);
    // The full AIR meta, `air.version` included, exactly as upstream sends it.
    assert_eq!(
        params["_meta"],
        json!({"jetbrains": {"air": {"version": 1, "permission": {
            "version": 1, "title": "rm -rf build", "defaultToNo": true
        }}}})
    );
    let _ = reply.send(Ok(json!({"outcome": "selected", "optionId": "reject"})));
    // A reject selection maps to the SDK's deny verdict (the same builder the
    // actor uses; the LoopRig control channel has no inbound handler).
    let ctx = crate::permission_options::OptionsContext {
        tool_name: "Bash".into(),
        display_name: None,
        input: json!({"command": "rm -rf build"}),
        cwd: std::env::temp_dir(),
        change_set: None,
        no_persistent_rule: false,
        default_to_no: true,
        available_modes: Vec::new(),
        auto_unavailable: false,
        pre_plan_mode: None,
        context_used_percent: None,
        home_dir: None,
    };
    let mapped = crate::permission::permission_control_response(
        &json!({"outcome": {"outcome": "selected", "optionId": "reject"}}),
        &ctx,
        "req-tool-1",
        "tool-1",
        &json!({"command": "rm -rf build"}),
    );
    assert_eq!(mapped["response"]["response"]["behavior"], "deny");
}

// ---------------------------------------------------------------- finding 8

impl LoopRig {
    /// Enqueue a prompt and return its reply channel.
    fn prompt(&self, uuid: &str) -> oneshot::Receiver<Result<PromptReply, PromptFailure>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::Prompt {
                uuid: uuid.into(),
                frame: json!({"type":"user","uuid":uuid,"message":{}}),
                is_local_only: false,
                prompt_text: String::new(),
                reply: reply_tx,
            })
            .unwrap();
        reply_rx
    }

    fn send(&self, frame: Value) {
        self.stream_tx.as_ref().unwrap().send(frame).unwrap();
    }

    fn echo(&self, uuid: &str) {
        self.send(json!({"type":"user","uuid":uuid,"isReplay":true,
            "message":{"role":"user","content":[{"type":"text","text":"a"}]}}));
    }

    /// The child died (or exited) and its stdout closed.
    fn end_stream(&mut self, abnormal: Option<bool>) {
        if let Some(abnormal) = abnormal {
            self.send(child_exit_marker(abnormal));
        }
        self.stream_tx = None;
    }
}

async fn reply_of(
    rx: oneshot::Receiver<Result<PromptReply, PromptFailure>>,
) -> Result<PromptReply, SessionError> {
    tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .expect("the prompt settles")
        .expect("the reply is sent")
        .map_err(|failure| failure.error)
}

/// V-03 — the child died mid-turn: the SDK throws and `failAllTurns` rejects the
/// in-flight prompt (and the queued one) instead of settling `end_turn`.
#[tokio::test]
async fn t1fa_child_death_rejects_the_in_flight_turn() {
    let mut rig = LoopRig::spawn();
    let p1 = rig.prompt("p1");
    let p2 = rig.prompt("p2");
    rig.echo("p1");
    rig.end_stream(Some(true));
    for reply in [p1, p2] {
        match reply_of(reply).await {
            Err(SessionError::PromptFailed { kind, message, .. }) => {
                assert_eq!(kind, FailureKind::SessionEnded);
                assert_eq!(message, PROCESS_DIED_MESSAGE);
            }
            other => panic!("expected the process-died rejection, got {other:?}"),
        }
    }
}

/// A clean exit ends the iterator: the in-flight turn settles `end_turn`, only
/// the queued one is rejected (unchanged, `acp-agent.ts:5764-5800`).
#[tokio::test]
async fn t1fa_clean_exit_settles_the_in_flight_turn() {
    let mut rig = LoopRig::spawn();
    // The queued turn's rejection waits behind the active turn's response.
    let update_rx = std::mem::replace(&mut rig.update_rx, mpsc::unbounded_channel().1);
    let _forwarder = serve_outbound(update_rx);
    let p1 = rig.prompt("p1");
    let mut p2 = rig.prompt("p2");
    rig.echo("p1");
    rig.end_stream(Some(false));
    let mut reply = reply_of(p1).await.expect("the active turn settles");
    assert_eq!(reply.stop_reason, "end_turn");
    // Finding 10: Node sends the active turn's response first, then rejects the
    // queued one; here the rejection stays behind until p1's response is written.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut p2)
            .await
            .is_err(),
        "the queued turn's rejection must not overtake the active turn's response"
    );
    if let Some(flush) = reply.flush_rx.take() {
        let _ = flush.await;
    }
    if let Some(ack) = reply.ack.take() {
        let _ = ack.send(());
    }
    match reply_of(p2).await {
        Err(SessionError::PromptFailed { kind, message, .. }) => {
            assert_eq!(kind, FailureKind::SessionEnded);
            assert_eq!(message, crate::turn::SESSION_ENDED_MESSAGE);
        }
        other => panic!("expected the session-ended rejection, got {other:?}"),
    }
}

/// After a cancel the abort can kill the stream, and that error is the cancel's:
/// the active turn ends `cancelled`.
#[tokio::test]
async fn t1fa_child_death_after_a_cancel_settles_cancelled() {
    let mut rig = LoopRig::spawn();
    let p1 = rig.prompt("p1");
    rig.echo("p1");
    rig.tx.send(Command::Cancel).unwrap();
    rig.end_stream(Some(true));
    let reply = reply_of(p1).await.expect("cancelled, not rejected");
    assert_eq!(reply.stop_reason, "cancelled");
}

/// A child that dies after an error result: every rejected turn carries the SDK's
/// `Claude Code returned an error result: <text>` error (F3a (f)).
#[tokio::test]
async fn t1fa_child_death_after_an_error_result_carries_its_text() {
    let mut rig = LoopRig::spawn();
    let p1 = rig.prompt("p1");
    let p2 = rig.prompt("p2");
    rig.echo("p1");
    rig.send(json!({"type":"result","subtype":"success","is_error":true,
        "stop_reason":null,"result":"boom","usage":{}}));
    assert!(matches!(
        reply_of(p1).await,
        Err(SessionError::PromptFailed { .. })
    ));
    rig.end_stream(Some(true));
    match reply_of(p2).await {
        Err(SessionError::ChildErrorResult { text }) => assert_eq!(text, "boom"),
        other => panic!("expected the child error result, got {other:?}"),
    }
}

/// The relay appends the exit verdict after the last frame, or nothing when the
/// process gave none.
#[tokio::test]
async fn t1fa_relay_appends_the_exit_verdict() {
    for (verdict, expected) in [
        (Some(true), Some(true)),
        (Some(false), Some(false)),
        (None, None),
    ] {
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
        let (relay_tx, mut relay_rx) = mpsc::unbounded_channel::<Value>();
        let relay = tokio::spawn(relay_stream(stream_rx, relay_tx, async move { verdict }));
        stream_tx.send(json!({"type":"system"})).unwrap();
        drop(stream_tx);
        let mut frames = Vec::new();
        while let Some(frame) = tokio::time::timeout(Duration::from_secs(10), relay_rx.recv())
            .await
            .expect("the relay closes after the verdict")
        {
            frames.push(frame);
        }
        relay.abort();
        assert_eq!(frames[0]["type"], "system");
        assert_eq!(frames.get(1).and_then(child_exit_of), expected);
    }
}

// ---------------------------------------------------------------- finding 9

/// The wire side the agent layer provides: serve every flush barrier the way the
/// forwarding task does, so the order in which responses leave is observable.
fn serve_outbound(
    mut update_rx: mpsc::UnboundedReceiver<SessionOutbound>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(out) = update_rx.recv().await {
            match out {
                SessionOutbound::Flush(done) => {
                    let _ = done.send(());
                }
                SessionOutbound::FlushAcked { done, ack } => {
                    let _ = done.send(());
                    // Later outbound items wait until the response was written.
                    let _ = tokio::time::timeout(Duration::from_secs(5), ack).await;
                }
                _ => {}
            }
        }
    })
}

/// V-08 — `session/close` with prompts in flight: the cancelled prompts' responses
/// (the swept queued one and the active one) leave before the close reply, behind
/// the same barrier, and the close answers once. The test serves the barriers, so
/// a close that skipped them would be seen answering first.
#[tokio::test]
async fn t1fa_close_answers_after_the_cancelled_prompts_responses() {
    let mut rig = LoopRig::spawn();
    let update_rx = std::mem::replace(&mut rig.update_rx, mpsc::unbounded_channel().1);
    let forwarder = serve_outbound(update_rx);
    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<String>();

    for uuid in ["p1", "p2"] {
        let reply_rx = rig.prompt(uuid);
        let log_tx = log_tx.clone();
        tokio::spawn(async move {
            if let Ok(Ok(mut reply)) = reply_rx.await {
                // The prompt task waits for its barrier, writes, then acks.
                if let Some(flush) = reply.flush_rx.take() {
                    let _ = flush.await;
                }
                // Writing the response takes a moment.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _ = log_tx.send(format!("prompt {uuid} {}", reply.stop_reason));
                if let Some(ack) = reply.ack.take() {
                    let _ = ack.send(());
                }
            }
        });
    }
    rig.echo("p1");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (close_tx, close_rx) = oneshot::channel();
    rig.tx.send(Command::Close { reply: close_tx }).unwrap();
    let close_log = log_tx.clone();
    let close_waiter = tokio::spawn(async move {
        let answered = close_rx.await.is_ok();
        let _ = close_log.send(format!("close {answered}"));
    });
    drop(log_tx);

    let mut log = Vec::new();
    while let Some(line) = tokio::time::timeout(Duration::from_secs(10), log_rx.recv())
        .await
        .expect("prompts and close all answer")
    {
        log.push(line);
        if log.len() == 3 {
            break;
        }
    }
    assert_eq!(
        log.len(),
        3,
        "two prompt responses and one close reply: {log:?}"
    );
    assert_eq!(log[2], "close true", "close answers last: {log:?}");
    let mut prompts = log[..2].to_vec();
    prompts.sort();
    assert_eq!(prompts, vec!["prompt p1 cancelled", "prompt p2 cancelled"]);
    // The close reply is sent once; nothing else follows it.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), log_rx.recv())
            .await
            .map(|line| line.is_none())
            .unwrap_or(true),
        "no second close reply"
    );
    let _ = close_waiter.await;
    forwarder.abort();
}

// ---------------------------------------------------------------- finding 13

/// Held frames go back in front of what is still queued, in order: appending them
/// would put a second held `/mcp` result behind the frames that follow it.
#[test]
fn t1fa_released_frames_keep_the_stream_order() {
    let mut replay: std::collections::VecDeque<Value> =
        [json!({"n": 3}), json!({"n": 4})].into_iter().collect();
    let mut held = vec![json!({"n": 1}), json!({"n": 2})];
    release_held_frames(&mut replay, &mut held);
    assert!(held.is_empty());
    let order: Vec<i64> = replay.iter().filter_map(|f| f["n"].as_i64()).collect();
    assert_eq!(order, vec![1, 2, 3, 4]);
}

/// AG-4 — the stream ended while a `/mcp` result waited for its control run: the
/// result is not thrown away; it settles the turn with its own stop reason and
/// usage, not an `end_turn` with zero usage.
#[tokio::test]
async fn t1fa_eof_keeps_a_result_held_behind_a_mcp_run() {
    let mut rig = LoopRig::spawn();
    let update_rx = std::mem::replace(&mut rig.update_rx, mpsc::unbounded_channel().1);
    let _forwarder = serve_outbound(update_rx);
    let command = crate::mcp_command::parse_mcp_command("/mcp").expect("a /mcp prompt");
    rig.tx
        .send(Command::McpPrompt {
            uuid: "p1".into(),
            command,
        })
        .unwrap();
    let p1 = rig.prompt("p1");
    rig.echo("p1");
    rig.send(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"max_tokens","result":"cli text",
        "usage":{"input_tokens":11,"output_tokens":7}}));
    rig.end_stream(Some(false));
    let reply = reply_of(p1)
        .await
        .expect("the held result settles the turn");
    assert_eq!(reply.stop_reason, "max_tokens");
    let usage = reply.usage.expect("usage");
    assert_eq!((usage.input_tokens, usage.output_tokens), (11, 7));
}

/// P1a #3 (`mcp-command.test.ts`: "replaces the result text when no other shape
/// carries it"): when the `/mcp` turn's only output shape is the result, the
/// produced status markdown is published once, at the held result, and the
/// CLI's result text is replaced (not echoed).
#[tokio::test]
async fn t1fa_mcp_result_only_shape_publishes_the_status_markdown() {
    let mut rig = LoopRig::spawn();
    let command = crate::mcp_command::parse_mcp_command("/mcp").expect("a /mcp prompt");
    rig.tx
        .send(Command::McpPrompt {
            uuid: "p1".into(),
            command,
        })
        .unwrap();
    let p1 = rig.prompt("p1");
    rig.echo("p1");
    // The status read is delayed: the result reaches the actor first and is
    // held behind the run.
    let request = rig.next_written_control_request("mcp_status").await;
    let request_id = request["request_id"]
        .as_str()
        .expect("a request id")
        .to_string();
    rig.send(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"cli text",
        "usage":{"input_tokens":1,"output_tokens":0}}));
    // The held result waits for the run: no replacement yet.
    assert!(
        !std::iter::from_fn(|| rig.update_rx.try_recv().ok()).any(|item| {
            matches!(item, SessionOutbound::Update(notif)
                if serde_json::to_value(&notif.update).unwrap()["sessionUpdate"]
                    == "agent_message_chunk")
        }),
        "the result is held until the control run answers"
    );
    rig.send(
        json!({"type":"control_response","response":{"subtype":"success",
        "request_id":request_id,"response":{"mcpServers":[
            {"name":"alpha","status":"connected"}]}}}),
    );
    let reply = reply_of(p1).await.expect("the turn settles");
    assert_eq!(reply.stop_reason, "end_turn");
    assert!(
        reply.updates.is_empty(),
        "the CLI's result text is replaced, not echoed"
    );
    let chunks: Vec<String> = std::iter::from_fn(|| rig.update_rx.try_recv().ok())
        .filter_map(|item| match item {
            SessionOutbound::Update(notif)
                if serde_json::to_value(&notif.update).unwrap()["sessionUpdate"]
                    == "agent_message_chunk" =>
            {
                Some(chunk_text(&notif))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        chunks,
        vec![crate::mcp_command::format_mcp_status(&[json!(
            {"name":"alpha","status":"connected"}
        )])],
        "exactly one replacement chunk"
    );
}

/// P1a #4 (`create-session-options.test.ts`: "asks for the summary context
/// usage so the CLI sends no count_tokens requests"): the model-switch probe
/// sends the summary request verbatim.
#[tokio::test]
async fn t1fa_model_switch_asks_for_the_summary_context_usage() {
    let mut rig = LoopRig::spawn();
    let (reply_tx, _reply_rx) = oneshot::channel();
    rig.tx
        .send(Command::ModelApplied {
            model_val: "claude-opus-4-1".into(),
            result: Ok(()),
            reply: reply_tx,
            notify_client: true,
            inline: false,
        })
        .unwrap();
    let request = rig.next_written_control_request("get_context_usage").await;
    assert_eq!(
        request["request"],
        json!({"subtype":"get_context_usage","detail":"summary"}),
        "the model switch asks for the summary, not the SDK's full default"
    );
}

/// `mcp-command.test.ts`: "publishes nothing when a cancel comes during the
/// status read with %j" — the cancel clears the `/mcp` state, so the late
/// result and the late control run publish nothing and the turn is cancelled.
#[tokio::test]
async fn t1fa_cancel_during_an_mcp_status_read_publishes_nothing() {
    let mut rig = LoopRig::spawn();
    let command = crate::mcp_command::parse_mcp_command("/mcp").expect("a /mcp prompt");
    rig.tx
        .send(Command::McpPrompt {
            uuid: "p1".into(),
            command,
        })
        .unwrap();
    let p1 = rig.prompt("p1");
    rig.echo("p1");
    let request = rig.next_written_control_request("mcp_status").await;
    let request_id = request["request_id"]
        .as_str()
        .expect("a request id")
        .to_string();
    rig.tx.send(Command::Cancel).unwrap();
    // The cancelled turn's result and idle, then the producer's late answer.
    rig.send(
        json!({"type":"result","subtype":"error_during_execution","is_error":true,
        "stop_reason":"end_turn","result":"cli text",
        "usage":{"input_tokens":1,"output_tokens":0}}),
    );
    rig.send(json!({"type":"system","subtype":"session_state_changed","state":"idle"}));
    rig.send(
        json!({"type":"control_response","response":{"subtype":"success",
        "request_id":request_id,"response":{"mcpServers":[
            {"name":"alpha","status":"connected"}]}}}),
    );
    let reply = reply_of(p1).await.expect("the cancel settles the turn");
    assert_eq!(reply.stop_reason, "cancelled");
    let chunks: Vec<String> = std::iter::from_fn(|| rig.update_rx.try_recv().ok())
        .filter_map(|item| match item {
            SessionOutbound::Update(notif)
                if serde_json::to_value(&notif.update).unwrap()["sessionUpdate"]
                    == "agent_message_chunk" =>
            {
                Some(chunk_text(&notif))
            }
            _ => None,
        })
        .collect();
    assert!(
        chunks.is_empty(),
        "a cancelled /mcp turn publishes no replacement: {chunks:?}"
    );
}

/// `mcp-command.test.ts`: "stops the actions and publishes nothing when a cancel
/// comes during a reconnect" — the reconnect's late result is dropped.
#[tokio::test]
async fn t1fa_cancel_during_an_mcp_reconnect_publishes_nothing() {
    let mut rig = LoopRig::spawn();
    let command =
        crate::mcp_command::parse_mcp_command("/mcp reconnect all").expect("a /mcp reconnect");
    rig.tx
        .send(Command::McpPrompt {
            uuid: "p1".into(),
            command,
        })
        .unwrap();
    let p1 = rig.prompt("p1");
    rig.echo("p1");
    let status = rig.next_written_control_request("mcp_status").await;
    let status_id = status["request_id"]
        .as_str()
        .expect("a request id")
        .to_string();
    rig.send(
        json!({"type":"control_response","response":{"subtype":"success",
        "request_id":status_id,"response":{"mcpServers":[
            {"name":"alpha","status":"failed"}]}}}),
    );
    let reconnect = rig.next_written_control_request("mcp_reconnect").await;
    let reconnect_id = reconnect["request_id"]
        .as_str()
        .expect("a request id")
        .to_string();
    rig.tx.send(Command::Cancel).unwrap();
    rig.send(
        json!({"type":"result","subtype":"error_during_execution","is_error":true,
        "stop_reason":"end_turn","result":"cli text",
        "usage":{"input_tokens":1,"output_tokens":0}}),
    );
    rig.send(json!({"type":"system","subtype":"session_state_changed","state":"idle"}));
    rig.send(
        json!({"type":"control_response","response":{"subtype":"success",
        "request_id":reconnect_id,"response":{}}}),
    );
    let reply = reply_of(p1).await.expect("the cancel settles the turn");
    assert_eq!(reply.stop_reason, "cancelled");
    let chunks: Vec<String> = std::iter::from_fn(|| rig.update_rx.try_recv().ok())
        .filter_map(|item| match item {
            SessionOutbound::Update(notif)
                if serde_json::to_value(&notif.update).unwrap()["sessionUpdate"]
                    == "agent_message_chunk" =>
            {
                Some(chunk_text(&notif))
            }
            _ => None,
        })
        .collect();
    assert!(
        chunks.is_empty(),
        "a cancelled /mcp reconnect publishes nothing: {chunks:?}"
    );
}

// ---------------------------------------------------------------- G4 / T-PORT

fn steer_frame(text: &str) -> Value {
    json!({"type":"user","message":{"role":"user",
        "content":[{"type":"text","text":text}]}})
}

fn permission_frame(tool_use_id: &str, request_id: &str, input: Value) -> Value {
    json!({"type":"control_request","request_id":request_id,
        "request":{"subtype":"can_use_tool","tool_name":"Bash","input":input,
        "tool_use_id":tool_use_id,"permission_suggestions":[]}})
}

/// `acp-agent.test.ts` > "keeps concurrent permission requests independent even
/// for the same tool call": two requests for one tool_use_id each carry their
/// own input and settle independently.
#[tokio::test]
async fn g4_tport_concurrent_permission_requests_stay_independent() {
    let mut rig = LoopRig::spawn();
    for (request_id, command) in [("req-one", "one"), ("req-two", "two")] {
        let (reply_tx, reply_rx) = oneshot::channel();
        // Keep the receiver alive so the round trip is not seen as withdrawn.
        std::mem::forget(reply_rx);
        rig.tx
            .send(Command::Permission {
                frame: permission_frame("tool-shared", request_id, json!({"command": command})),
                reply: reply_tx,
            })
            .unwrap();
    }
    let mut raw_inputs = Vec::new();
    while raw_inputs.len() < 2 {
        match tokio::time::timeout(Duration::from_secs(10), rig.update_rx.recv())
            .await
            .expect("both permission requests reach the client")
        {
            Some(SessionOutbound::RequestPermission { params, reply }) => {
                raw_inputs.push(params["toolCall"]["rawInput"].clone());
                let _ = reply.send(Ok(json!({
                    "outcome": {"outcome": "selected", "optionId": "allow-once"}
                })));
            }
            Some(_) => {}
            None => panic!("the update channel closed"),
        }
    }
    assert_eq!(
        raw_inputs,
        vec![json!({"command": "one"}), json!({"command": "two"})]
    );
    let _ = tokio::time::timeout(Duration::from_secs(10), &mut rig.actor).await;
}

/// `acp-agent.test.ts` > "lets the host retry promptRequired content through
/// session/prompt exactly once": the idle steer consumes nothing (no push, no
/// turn), so the same content starts one normal turn through `session/prompt`.
#[tokio::test]
async fn g4_tport_prompt_required_content_is_retried_once_through_prompt() {
    let mut rig = LoopRig::spawn();
    let (reply_tx, reply_rx) = oneshot::channel();
    rig.tx
        .send(Command::Steer {
            uuid: "steer-1".into(),
            frame: steer_frame("late follow-up"),
            is_local_only: false,
            is_prompt_required: true,
            reply: reply_tx,
        })
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), reply_rx)
        .await
        .expect("the steer replies")
        .expect("the actor replies");
    assert_eq!(
        outcome,
        json!({"outcome": "promptRequired", "reason": "noRunningTurn"})
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), rig.written.recv())
            .await
            .is_err(),
        "the idle steer pushes nothing"
    );

    let (preply, _prx) = oneshot::channel();
    std::mem::forget(_prx);
    rig.tx
        .send(Command::Prompt {
            uuid: "p2".into(),
            frame: steer_frame("late follow-up"),
            is_local_only: false,
            prompt_text: "late follow-up".into(),
            reply: preply,
        })
        .unwrap();
    let written = rig.next_written_user().await;
    assert!(
        written.get("priority").is_none(),
        "a normal prompt carries no priority: {written}"
    );
    assert!(
        serde_json::to_string(&written["message"]["content"])
            .unwrap()
            .contains("late follow-up"),
        "{written}"
    );
    let _ = tokio::time::timeout(Duration::from_secs(10), &mut rig.actor).await;
}

/// `acp-agent.test.ts` > "uses priority:'now' again once $name is withdrawn,
/// though its client never settles it": while a client round trip is open a
/// steer uses `later`; once the CLI withdraws it the next steer uses `now` again.
#[tokio::test]
async fn g4_tport_steer_uses_now_again_after_the_request_is_withdrawn() {
    let mut rig = LoopRig::spawn();
    let (preply, prx) = oneshot::channel();
    std::mem::forget(prx);
    rig.tx
        .send(Command::Prompt {
            uuid: "p1".into(),
            frame: steer_frame("start"),
            is_local_only: false,
            prompt_text: "start".into(),
            reply: preply,
        })
        .unwrap();
    let _ = rig.next_written_user().await;

    let (perm_reply, perm_rx) = oneshot::channel();
    std::mem::forget(perm_rx);
    rig.tx
        .send(Command::Permission {
            frame: permission_frame("toolu_perm", "req-toolu_perm", json!({"command": "ls"})),
            reply: perm_reply,
        })
        .unwrap();
    let mut client_reply = rig.next_request().await;

    let (reply_tx, reply_rx) = oneshot::channel();
    rig.tx
        .send(Command::Steer {
            uuid: crate::agent::uuid_v4(),
            frame: steer_frame("while open"),
            is_local_only: false,
            is_prompt_required: false,
            reply: reply_tx,
        })
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), reply_rx)
        .await
        .expect("the steer replies")
        .expect("the actor replies");
    assert_eq!(outcome, json!({"outcome": "injected"}));
    assert_eq!(rig.next_written_user().await["priority"], json!("later"));

    // The CLI withdraws the request; the client never settles it.
    rig.tx
        .send(Command::ElicitationWithdrawn {
            request_id: "req-toolu_perm".into(),
        })
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), client_reply.closed()).await;

    let (reply_tx, reply_rx) = oneshot::channel();
    rig.tx
        .send(Command::Steer {
            uuid: crate::agent::uuid_v4(),
            frame: steer_frame("after withdrawal"),
            is_local_only: false,
            is_prompt_required: false,
            reply: reply_tx,
        })
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), reply_rx)
        .await
        .expect("the steer replies")
        .expect("the actor replies");
    assert_eq!(outcome, json!({"outcome": "injected"}));
    assert_eq!(rig.next_written_user().await["priority"], json!("now"));
    let _ = tokio::time::timeout(Duration::from_secs(10), &mut rig.actor).await;
}
