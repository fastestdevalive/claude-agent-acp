//! Session-level tests added by Turn 2 (lane A, T2A): frames that describe a
//! tool call (F7a), notices and refusal frames (F7b), background tasks (F14),
//! the cancelled-turn tool closing (F17) and the session title (F30).

use super::*;

/// Drives `handle_session_frame` with the state a running actor would hold.
pub(super) struct Rig {
    pub(super) machine: TurnMachine,
    pub(super) map_state: MapState,
    pub(super) live_background: HashMap<String, String>,
    pub(super) pending: HashMap<String, oneshot::Sender<Result<PromptReply, PromptFailure>>>,
    pub(super) emitted_assistant_text: bool,
    pub(super) assistant_had_error: bool,
    pub(super) tx: mpsc::UnboundedSender<SessionOutbound>,
    pub(super) rx: mpsc::UnboundedReceiver<SessionOutbound>,
    pub(super) high_water: Arc<AtomicUsize>,
    pub(super) tasks: AsyncTaskRuntime,
    pub(super) ext: SessionExt,
    pub(super) replies: HashMap<String, oneshot::Receiver<Result<PromptReply, PromptFailure>>>,
}

impl Rig {
    pub(super) fn new() -> Self {
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

    pub(super) fn feed(&mut self, line: Value) {
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

    pub(super) fn echo(&mut self, uuid: &str) {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.pending.insert(uuid.to_string(), reply_tx);
        self.replies.insert(uuid.to_string(), reply_rx);
        self.machine.enqueue(Turn::new(uuid.into(), false));
        self.feed(json!({"type":"user","uuid":uuid,"message":{"role":"user",
            "content":[{"type":"text","text":"hi"}]}}));
    }

    /// Every `session/update` payload sent since the last call, typed and raw.
    pub(super) fn updates(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(item) = self.rx.try_recv() {
            match item {
                SessionOutbound::Update(notif) => {
                    out.push(serde_json::to_value(&notif.update).unwrap());
                }
                SessionOutbound::RawUpdate(value) => out.push(value["update"].clone()),
                _ => {}
            }
        }
        out
    }

    /// A tool use the client was given (assistant frame with a `tool_use`).
    pub(super) fn tool_use(&mut self, id: &str, name: &str, input: Value) {
        self.feed(json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg_t","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"tool_use","id":id,"name":name,"input":input}]}}));
    }
}

fn kinds(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .map(|u| u["sessionUpdate"].as_str().unwrap_or("").to_string())
        .collect()
}

// ---------------------------------------------------------------- F7a

/// N-20 — a `tool_progress` beat reaches the client as an `in_progress`
/// `tool_call_update` (the frame type is routed to the session, not ignored).
#[test]
fn t2a_tool_progress_frame_updates_the_call() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.tool_use("toolu_A", "Bash", json!({"command": "sleep 5"}));
    rig.updates();
    rig.feed(
        json!({"type": "tool_progress", "tool_use_id": "toolu_A", "tool_name": "Bash",
        "parent_tool_use_id": null, "elapsed_time_seconds": 4}),
    );
    let updates = rig.updates();
    assert_eq!(kinds(&updates), vec!["tool_call_update"]);
    assert_eq!(updates[0]["toolCallId"], "toolu_A");
    assert_eq!(updates[0]["status"], "in_progress");
    assert_eq!(
        updates[0]["_meta"]["claudeCode"]["toolResponse"]["elapsedTimeSeconds"],
        4
    );
}

/// Hazard E — the tool frames never reach the turn machine, so they cannot
/// activate a queued head ahead of its echo.
#[test]
fn t2a_tool_frames_do_not_activate_a_queued_head() {
    let mut rig = Rig::new();
    rig.machine.enqueue(Turn::new("queued".into(), false));
    assert!(!rig.machine.has_active());
    rig.map_state.mark_emitted(
        "toolu_X".to_string(),
        json!({"type":"tool_use","id":"toolu_X","name":"Bash","input":{}}),
    );
    rig.feed(
        json!({"type":"tool_progress","tool_use_id":"toolu_X","tool_name":"Bash",
        "parent_tool_use_id":null,"elapsed_time_seconds":1}),
    );
    rig.feed(
        json!({"type":"system","subtype":"memory_recall","uuid":"m1","mode":"select",
        "memories":[{"path":"/m/a.md"}]}),
    );
    rig.feed(
        json!({"type":"system","subtype":"permission_denied","tool_use_id":"toolu_X",
        "tool_name":"Bash","decision_reason_type":"rule","decision_reason":"no"}),
    );
    assert!(
        !rig.machine.has_active(),
        "no frame promoted the queued head"
    );
    assert_eq!(kinds(&rig.updates()).len(), 3);
}

/// T-05 / T-82 — a `memory_recall` frame becomes a completed memory tool call.
#[test]
fn t2a_memory_recall_frame_becomes_a_read_tool_call() {
    let mut rig = Rig::new();
    rig.feed(
        json!({"type":"system","subtype":"memory_recall","uuid":"mem-1","mode":"select",
        "memories":[{"path":"/m/a.md"},{"path":"/m/b.md"}]}),
    );
    let updates = rig.updates();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["sessionUpdate"], "tool_call");
    assert_eq!(updates[0]["toolCallId"], "mem-1");
    assert_eq!(updates[0]["title"], "Recalled 2 memories");
    assert_eq!(updates[0]["kind"], "read");
}

/// P-01 / T-81 — a `permission_denied` frame fails the surfaced call.
#[test]
fn t2a_permission_denied_frame_fails_the_call() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.tool_use("toolu_D", "Bash", json!({"command": "rm -rf x"}));
    rig.updates();
    rig.feed(
        json!({"type":"system","subtype":"permission_denied","tool_use_id":"toolu_D",
        "tool_name":"Bash","decision_reason_type":"rule","decision_reason":"Denied by rule"}),
    );
    let updates = rig.updates();
    assert_eq!(kinds(&updates), vec!["tool_call_update"]);
    assert_eq!(updates[0]["status"], "failed");
    assert_eq!(
        updates[0]["content"][0]["content"]["text"],
        "Permission denied: Denied by rule"
    );
    assert!(!rig.map_state.pending_hooks.contains("toolu_D"));

    // Once the call resolved, a late denial for it is dropped.
    rig.feed(json!({"type":"user","parent_tool_use_id":null,
        "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_D",
            "is_error":true,"content":"denied"}]}}));
    rig.updates();
    rig.feed(
        json!({"type":"system","subtype":"permission_denied","tool_use_id":"toolu_D",
        "tool_name":"Bash","decision_reason":"late"}),
    );
    assert!(rig.updates().is_empty(), "a resolved call is not reopened");
}

/// T-17 — the message-level `tool_use_result` is honoured only when the message
/// carries exactly one `tool_result`.
#[test]
fn t2a_structured_result_needs_exactly_one_tool_result() {
    let structured = json!({"stdout": "from structured", "stderr": ""});
    let result = |id: &str| json!({"type":"tool_result","tool_use_id":id,"content":"raw text"});
    let mut one = Rig::new();
    one.echo("p1");
    one.tool_use("toolu_1", "Bash", json!({"command": "echo"}));
    one.updates();
    one.feed(json!({"type":"user","parent_tool_use_id":null,
        "message":{"role":"user","content":[result("toolu_1")]},"tool_use_result":structured}));
    let text = one.updates()[0]["content"][0]["content"]["text"].clone();
    assert_eq!(text, "```console\nfrom structured\n```");

    let mut two = Rig::new();
    two.echo("p1");
    two.tool_use("toolu_1", "Bash", json!({"command": "echo"}));
    two.tool_use("toolu_2", "Bash", json!({"command": "echo"}));
    two.updates();
    two.feed(json!({"type":"user","parent_tool_use_id":null,
        "message":{"role":"user","content":[result("toolu_1"), result("toolu_2")]},
        "tool_use_result":structured}));
    let updates = two.updates();
    assert_eq!(updates.len(), 2);
    assert_eq!(
        updates[0]["content"][0]["content"]["text"],
        "```console\nraw text\n```"
    );
}

/// T-31 / T-32 / C-23 — a client that renders terminals gets the command's
/// output on the terminal channel, then the exit with the status.
#[test]
fn t2a_terminal_client_gets_the_command_on_the_terminal_channel() {
    let mut rig = Rig::new();
    rig.map_state
        .set_client_caps(crate::client_caps::ClientCapabilities {
            terminal_output: true,
            ..Default::default()
        });
    rig.echo("p1");
    rig.tool_use(
        "toolu_T",
        "Bash",
        json!({"command": "false", "description": "Fail"}),
    );
    let started = rig.updates();
    assert_eq!(started[0]["sessionUpdate"], "tool_call");
    assert_eq!(
        started[0]["_meta"]["terminal_info"],
        json!({"terminal_id": "toolu_T"})
    );
    assert_eq!(
        started[0]["content"],
        json!([{"type": "terminal", "terminalId": "toolu_T"}])
    );

    rig.feed(json!({"type":"user","parent_tool_use_id":null,
        "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_T",
            "is_error":true,"content":"Exit code 3\nboom"}]}}));
    let updates = rig.updates();
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert_eq!(
        updates[0]["_meta"]["terminal_output"],
        json!({
        "terminal_id": "toolu_T", "data": "Exit code 3\nboom"})
    );
    assert!(updates[0].get("status").is_none());
    assert_eq!(updates[1]["status"], "failed");
    assert_eq!(updates[1]["_meta"]["claudeCode"]["toolName"], "Bash");
    assert_eq!(
        updates[1]["_meta"]["terminal_exit"],
        json!({
        "terminal_id": "toolu_T", "exit_code": 3, "signal": null})
    );
    assert!(
        updates[1].get("rawOutput").is_none(),
        "the terminal carried the output"
    );
}

/// C-23 — a non-terminal client keeps the code block rendering.
#[test]
fn t2a_plain_client_keeps_the_console_block() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.tool_use("toolu_T", "Bash", json!({"command": "echo hi"}));
    assert!(rig.updates()[0]["_meta"].get("terminal_info").is_none());
    rig.feed(json!({"type":"user","parent_tool_use_id":null,
        "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_T",
            "content":"hi"}]}}));
    let updates = rig.updates();
    assert_eq!(updates.len(), 1);
    assert_eq!(
        updates[0]["content"][0]["content"]["text"],
        "```console\nhi\n```"
    );
}

// ---------------------------------------------------------------- F7b

fn air_rig(capabilities: &[&str]) -> Rig {
    let mut rig = Rig::new();
    let caps =
        json!({"_meta": {"jetbrains": {"air": {"version": 1, "capabilities": capabilities}}}});
    rig.ext.air = crate::air::AirClient::from_client_capabilities(&caps);
    rig
}

fn notice_rig() -> Rig {
    let mut rig = Rig::new();
    rig.ext.caps.notices = true;
    rig
}

fn chunk_texts(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .map(|u| u["content"]["text"].as_str().unwrap_or("").to_string())
        .collect()
}

fn result_frame(text: &str, output_tokens: u64) -> Value {
    json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn",
        "result":text,"usage":{"input_tokens":1,"output_tokens":output_tokens}})
}

impl Rig {
    /// The chunks that ride on `uuid`'s prompt reply (the result-text fallback
    /// and the refusal explanation).
    fn reply_chunks(&mut self, uuid: &str) -> Vec<String> {
        let mut reply = self.replies.remove(uuid).expect("prompt was echoed");
        match reply.try_recv().ok() {
            Some(Ok(reply)) => reply
                .updates
                .iter()
                .map(|n| {
                    let update = serde_json::to_value(&n.update).unwrap();
                    update["content"]["text"].as_str().unwrap_or("").to_string()
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// N-14 — a client without notices gets the transcript line, level folded in,
/// marked with the informational meta; `info` stays plain.
#[test]
fn t2a_informational_transcript_line_for_a_plain_client() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.feed(
        json!({"type":"system","subtype":"informational","level":"warning",
        "content":"Stop hook blocked: tests failing","tool_use_id":"toolu_H"}),
    );
    rig.feed(
        json!({"type":"system","subtype":"informational","level":"info",
        "content":"plain info"}),
    );
    let updates = rig.updates();
    assert_eq!(
        chunk_texts(&updates),
        vec![
            "**Warning:** Stop hook blocked: tests failing",
            "plain info"
        ]
    );
    assert_eq!(
        updates[0]["_meta"],
        json!({"claudeCode": {"kind": "informational", "level": "warning"}})
    );
    assert!(
        rig.emitted_assistant_text,
        "the line is delivered assistant text"
    );
}

/// N-14 — a notice client gets a `notice` at the SDK's level, one per tool use,
/// none for `info`; a result that only repeats the text is not forwarded again.
#[test]
fn t2a_informational_notice_dedupes_and_owns_the_result_text() {
    let mut rig = notice_rig();
    rig.echo("p1");
    for _ in 0..2 {
        rig.feed(
            json!({"type":"system","subtype":"informational","level":"warning",
            "content":"Blocked by hook: do it again","tool_use_id":"toolu_H"}),
        );
    }
    rig.feed(json!({"type":"system","subtype":"informational","level":"info","content":"quiet"}));
    rig.feed(
        json!({"type":"system","subtype":"informational","level":"notice",
        "content":"Heads  up"}),
    );
    let updates = rig.updates();
    let kinds_seen = kinds(&updates);
    assert_eq!(kinds_seen, vec!["notice", "notice"], "{updates:?}");
    assert_eq!(updates[0]["severity"], "warning");
    assert_eq!(updates[0]["title"], "Blocked by hook: do it again");
    assert_eq!(updates[1]["severity"], "info");
    assert!(!rig.emitted_assistant_text, "a notice is not an answer");

    // A hook-blocked turn's result repeats the block reason with zero output
    // tokens: the #453 fallback must not emit it a second time.
    rig.feed(result_frame("Blocked by hook:   do it again", 0));
    assert_eq!(rig.reply_chunks("p1"), Vec::<String>::new());
}

/// N-14 — without a matching notice the same zero-token result is forwarded.
#[test]
fn t2a_result_text_without_a_matching_notice_is_forwarded() {
    let mut rig = notice_rig();
    rig.echo("p1");
    rig.feed(
        json!({"type":"system","subtype":"informational","level":"warning",
        "content":"something else"}),
    );
    rig.feed(result_frame("the cached answer", 0));
    assert_eq!(rig.reply_chunks("p1"), vec!["the cached answer"]);
}

/// N-13 — a mirror failure is logged with the session and the error.
#[test]
fn t2a_mirror_error_is_logged() {
    let dir = std::env::temp_dir().join(format!("t2a-mirror-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut rig = Rig::new();
    rig.ext.logger = Some(Arc::new(crate::logger::Logger::to_dir(&dir)));
    rig.feed(
        json!({"type":"system","subtype":"mirror_error","session_id":"sess-9",
        "error":"append timed out","key":{}}),
    );
    let log = std::fs::read_to_string(dir.join("agent.log")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        log.contains("Session sess-9: failed to persist history: append timed out"),
        "{log}"
    );
}

/// N-38 — an AIR client with session failures gets the retry as a warning on
/// the turn, its revision rising per attempt; a 401 retry publishes nothing; a
/// plain client gets nothing.
#[test]
fn t2a_api_retry_publishes_a_turn_warning_for_air() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.updates();
    rig.feed(
        json!({"type":"system","subtype":"api_retry","attempt":1,"max_retries":3,
        "error_status":429,"error":"rate_limit"}),
    );
    rig.feed(
        json!({"type":"system","subtype":"api_retry","attempt":2,"max_retries":3,
        "error_status":null,"error":"unknown",
        "no_response":{"waited_ms":29500,"retry_wait_ms":120000}}),
    );
    rig.feed(
        json!({"type":"system","subtype":"api_retry","attempt":1,"max_retries":3,
        "error_status":401,"error":"authentication_failed"}),
    );
    let updates = rig.updates();
    assert_eq!(updates.len(), 2, "{updates:?}");
    let sf = |i: usize| updates[i]["_meta"]["jetbrains"]["air"]["sessionFailure"].clone();
    assert_eq!(sf(0)["id"], "p1:error");
    assert_eq!(sf(0)["revision"], 1);
    assert_eq!(sf(0)["severity"], "warning");
    assert_eq!(sf(0)["category"], "limit");
    assert_eq!(sf(0)["title"], "Retrying Claude, attempt 1 of 3.");
    assert_eq!(sf(0)["actions"], json!([]));
    assert_eq!(sf(1)["id"], "p1:error");
    assert_eq!(sf(1)["revision"], 2);
    assert_eq!(sf(1)["category"], "connection");
    assert_eq!(
        sf(1)["title"],
        "Reconnecting to Claude, attempt 2 of 3. No response after 30s; waiting up to 120s."
    );

    let mut plain = Rig::new();
    plain.echo("p1");
    plain.updates();
    plain.feed(
        json!({"type":"system","subtype":"api_retry","attempt":1,"max_retries":3,
        "error_status":429,"error":"rate_limit"}),
    );
    assert!(plain.updates().is_empty());
}

/// N-38 / T2FZ-1 — the terminal failure of a turn that already published an
/// `api_retry` warning takes the next revision of the same `<turn>:error` id
/// (one `SessionFailureController`), so a client keeping the newest revision per
/// id shows the error, not the stale warning.
#[test]
fn t2a_error_after_api_retry_takes_the_next_revision() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.updates();
    rig.feed(
        json!({"type":"system","subtype":"api_retry","attempt":1,"max_retries":3,
        "error_status":529,"error":"server_error"}),
    );
    let updates = rig.updates();
    let warning = &updates[0]["_meta"]["jetbrains"]["air"]["sessionFailure"];
    assert_eq!(warning["id"], "p1:error");
    assert_eq!(warning["revision"], 1);
    rig.feed(
        json!({"type":"result","subtype":"error_during_execution","is_error":true,
        "stop_reason":"end_turn","result":"API Error: 529 overloaded",
        "errors":["API Error: 529 overloaded"],
        "usage":{"input_tokens":1,"output_tokens":0}}),
    );
    let reply = rig
        .replies
        .get_mut("p1")
        .expect("p1 reply")
        .try_recv()
        .expect("answered")
        .expect("AIR failures ride a successful reply");
    let sf = &reply.quota.expect("quota")["jetbrains"]["air"]["sessionFailure"];
    assert_eq!(sf["id"], "p1:error");
    assert_eq!(sf["severity"], "error");
    assert_eq!(sf["revision"], 2, "{sf}");
}

/// T2FZ-2 — a result sweeps the `next_attempt` failures of earlier turns out of
/// the active set (`clearFailuresFromEarlierTurns`); an advisory stays.
#[test]
fn t2a_result_clears_next_attempt_failures_of_earlier_turns() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.feed(
        json!({"type":"system","subtype":"api_retry","attempt":1,"max_retries":3,
        "error_status":429,"error":"rate_limit"}),
    );
    rig.feed(result_frame("one", 5));
    assert!(rig.answered("p1"));
    rig.ext.t2a.failures.publish(
        "s",
        crate::session_failure::ClaudeFailureKind::Advisory,
        &failures::FailureOptions {
            title: Some("Model fallback".into()),
            ..failures::FailureOptions::default()
        },
    );
    // p1's own warning is of the active turn when its result lands, so it
    // survives that result; the next turn's result retires it.
    assert_eq!(rig.ext.t2a.failures.active().len(), 2);
    rig.echo("p2");
    rig.feed(result_frame("two", 5));
    let active = rig.ext.t2a.failures.active();
    assert_eq!(active.len(), 1, "{active:?}");
    assert_eq!(
        active[0].failure.kind,
        crate::session_failure::ClaudeFailureKind::Advisory
    );
}

/// T2GA-2 — an autonomous result neither sweeps the failures nor forgets the
/// last advisory notice: two identical advisories of one turn with an
/// autonomous result between stay one advisory, updated in place.
#[test]
fn t2a_autonomous_result_does_not_clear_failures() {
    let advisory = || failures::FailureOptions {
        title: Some("Model fallback".into()),
        ..failures::FailureOptions::default()
    };
    let publish = |rig: &mut Rig| {
        rig.ext.t2a.failures.publish(
            "s",
            crate::session_failure::ClaudeFailureKind::Advisory,
            &advisory(),
        )
    };
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    let first = publish(&mut rig);
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","origin":{"kind":"task-notification"},
        "usage":{"input_tokens":1,"output_tokens":1}}));
    let second = publish(&mut rig);
    let id = |v: &Value| v["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"]["id"].clone();
    assert_eq!(id(&first), id(&second), "{first} / {second}");
    assert_eq!(rig.ext.t2a.failures.active().len(), 1);
    assert_eq!(
        second["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"]["revision"],
        2
    );
}

fn fallback_frame() -> Value {
    json!({"type":"system","subtype":"model_refusal_fallback",
        "original_model":"claude-opus-4-5","fallback_model":"claude-sonnet-4-5",
        "direction":"retry","api_refusal_category":"cyber"})
}

/// N-39 — a plain client gets the bold-label transcript line and the model sync
/// effect; a notice client a warning notice with the explanation as description.
#[test]
fn t2a_model_refusal_fallback_notice_and_transcript() {
    let mut plain = Rig::new();
    plain.feed(fallback_frame());
    let updates = plain.updates();
    assert_eq!(
        chunk_texts(&updates),
        vec!["**Model fallback:** claude-opus-4-5 declined this request (cyber); retried with claude-sonnet-4-5. The session will continue on claude-sonnet-4-5."]
    );
    assert_eq!(
        plain.ext.t2a.effects,
        vec![state::Effect::SyncModel("claude-sonnet-4-5".into())]
    );

    let mut notice = notice_rig();
    let mut frame = fallback_frame();
    frame["api_refusal_explanation"] = json!("Because of the content.");
    notice.feed(frame);
    let updates = notice.updates();
    assert_eq!(kinds(&updates), vec!["notice"]);
    assert_eq!(updates[0]["severity"], "warning");
    assert!(updates[0]["title"]
        .as_str()
        .unwrap()
        .starts_with("claude-opus-4-5 declined"));
    assert_eq!(updates[0]["description"], "Because of the content.");
}

/// N-39 — a turn-only (`revert`) or local-scope fallback keeps the session model:
/// the wording changes and no model sync is asked for.
#[test]
fn t2a_model_refusal_fallback_revert_and_local_do_not_sync() {
    let mut reverted = Rig::new();
    let mut frame = fallback_frame();
    frame["direction"] = json!("revert");
    reverted.feed(frame);
    assert_eq!(
        chunk_texts(&reverted.updates()),
        vec!["**Model fallback:** claude-opus-4-5 declined this request (cyber); retried with claude-sonnet-4-5. The session stays on claude-opus-4-5."]
    );
    assert!(reverted.ext.t2a.effects.is_empty());

    let mut local = Rig::new();
    let mut frame = fallback_frame();
    frame["scope"] = json!("local");
    local.feed(frame);
    assert_eq!(
        chunk_texts(&local.updates()),
        vec!["**Model fallback:** claude-opus-4-5 declined this request (cyber); retried with claude-sonnet-4-5. Only that response came from claude-sonnet-4-5; the session stays on claude-opus-4-5."]
    );
    assert!(local.ext.t2a.effects.is_empty());
}

/// N-39 — an AIR client with session failures (and no notices) gets the fallback
/// as a warning advisory; a long explanation moves into the details.
#[test]
fn t2a_model_refusal_fallback_advisory_for_air() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.feed(fallback_frame());
    let updates = rig.updates();
    let sf = updates[0]["_meta"]["jetbrains"]["air"]["sessionFailure"].clone();
    assert_eq!(sf["severity"], "warning");
    assert_eq!(sf["category"], "unknown");
    assert!(sf["title"]
        .as_str()
        .unwrap()
        .contains("The session will continue on claude-sonnet-4-5."));
    assert!(sf.get("details").is_none());

    let mut long = fallback_frame();
    long["api_refusal_explanation"] = json!("x".repeat(300));
    rig.feed(long);
    let sf = rig.updates()[0]["_meta"]["jetbrains"]["air"]["sessionFailure"].clone();
    assert_eq!(sf["details"].as_str().unwrap().len(), 300);
    assert!(!sf["title"].as_str().unwrap().contains("xxx"));
}

/// N-40 / U-13 — the banner seeds the explanation the refusing result streams
/// ahead of the prompt's reply; a banner for a non-human turn does not.
#[test]
fn t2a_refusal_explanation_streams_at_the_result() {
    let refusal = |rig: &mut Rig| {
        rig.feed(
            json!({"type":"result","subtype":"success","is_error":true,"stop_reason":"refusal",
            "result":"refused","uuid":"res-1","usage":{"input_tokens":1,"output_tokens":0}}),
        );
    };
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.feed(
        json!({"type":"system","subtype":"model_refusal_no_fallback",
        "content":"banner text","api_refusal_explanation":"classifier says no",
        "refused_user_message_uuid":"p1"}),
    );
    refusal(&mut rig);
    assert_eq!(chunk_texts(&rig.updates()), vec!["classifier says no"]);

    // No structured explanation: the banner content is the explanation.
    let mut banner = Rig::new();
    banner.echo("p1");
    banner.feed(
        json!({"type":"system","subtype":"model_refusal_no_fallback","content":"banner text"}),
    );
    refusal(&mut banner);
    assert_eq!(chunk_texts(&banner.updates()), vec!["banner text"]);

    // A non-human-authored refusal (explicit null uuid) seeds nothing.
    let mut bg = Rig::new();
    bg.echo("p1");
    bg.feed(
        json!({"type":"system","subtype":"model_refusal_no_fallback","content":"banner text",
        "refused_user_message_uuid":null}),
    );
    refusal(&mut bg);
    assert_eq!(chunk_texts(&bg.updates()), Vec::<String>::new());
}

/// U-13 — the refused assistant frame's `stop_details` is the primary source and
/// a banner never overwrites it; a later turn does not inherit it.
#[test]
fn t2a_refusal_stop_details_win_and_do_not_leak_to_the_next_turn() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"m1","model":"claude-sonnet-4-5","role":"assistant","content":[],
        "stop_reason":"refusal","stop_details":{"explanation":"from the frame"},"usage":{}}}));
    rig.feed(json!({"type":"system","subtype":"model_refusal_no_fallback","content":"banner"}));
    rig.feed(
        json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"refusal",
        "result":"","uuid":"r1","usage":{"input_tokens":1,"output_tokens":0}}),
    );
    assert_eq!(chunk_texts(&rig.updates()), vec!["from the frame"]);
    assert!(!rig.machine.take_text_stretch_closed());

    rig.echo("p2");
    rig.feed(
        json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"refusal",
        "result":"","uuid":"r2","usage":{"input_tokens":1,"output_tokens":0}}),
    );
    assert_eq!(chunk_texts(&rig.updates()), Vec::<String>::new());
}

/// V-04 — the CLI's synthetic "Please run /login" reply fails the live turn as
/// auth_required instead of rendering as chat text.
#[test]
fn t2a_synthetic_login_message_fails_the_turn_as_auth_required() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"m1","model":"<synthetic>","role":"assistant",
        "content":[{"type":"text","text":"Not logged in · Please run /login"}],"usage":{}}}));
    assert!(
        chunk_texts(&rig.updates()).is_empty(),
        "the login text is not chat text"
    );
    let mut reply = rig.replies.remove("p1").unwrap();
    match reply.try_recv().expect("settled") {
        Err(failure) => assert!(
            matches!(
                &*failure,
                SessionError::PromptFailed {
                    kind: FailureKind::AuthRequired,
                    ..
                }
            ),
            "{failure:?}"
        ),
        Ok(_) => panic!("the turn must fail"),
    }
}

/// N-44 — a conversation reset clears the task plan, the title, the live async
/// tasks, the remembered tool names and the tracked tool calls.
#[test]
fn t2a_conversation_reset_clears_the_session_books() {
    let mut rig = air_rig(&["asyncTasks"]);
    rig.tasks = AsyncTaskRuntime::new(true);
    rig.echo("p1");
    rig.tool_use("toolu_A", "Bash", json!({"command": "ls"}));
    rig.feed(
        json!({"type":"system","subtype":"task_started","task_id":"b1",
        "task_type":"local_workflow","description":"Deploy"}),
    );
    rig.ext.titles.on_prompt("hello there world");
    assert!(rig.ext.titles.context.is_some());
    rig.ext
        .hooks
        .register_once("toolu_A".into(), "Bash".into(), None);
    rig.updates();

    rig.feed(json!({"type":"conversation_reset","new_conversation_id":"c2"}));
    let updates = rig.updates();
    assert!(
        updates
            .iter()
            .any(|u| u["sessionUpdate"] == "async_task_state_update" && u["state"] == "failed"),
        "live tasks end as failed: {updates:?}"
    );
    assert!(rig.ext.titles.context.is_none(), "the title state is fresh");
    assert!(!rig.ext.hooks.has("toolu_A"));
    assert_eq!(rig.map_state.tracked_tool_calls(), 0);
}

/// U-13 — a banner seeded first survives a refused frame that carries no
/// `stop_details`; the refused turn's explanation does not suppress the next
/// turn's #453 result-text fallback.
#[test]
fn t2a_refusal_banner_survives_a_frame_without_details_and_does_not_taint_the_next_turn() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.feed(
        json!({"type":"system","subtype":"model_refusal_no_fallback","content":"banner first"}),
    );
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"m1","model":"claude-sonnet-4-5","role":"assistant","content":[],
        "stop_reason":"refusal","usage":{}}}));
    rig.feed(
        json!({"type":"result","subtype":"success","is_error":true,"stop_reason":"refusal",
        "result":"","uuid":"r1","usage":{"input_tokens":1,"output_tokens":0}}),
    );
    assert_eq!(chunk_texts(&rig.updates()), vec!["banner first"]);

    rig.echo("p2");
    rig.feed(result_frame("cached answer", 0));
    assert_eq!(rig.reply_chunks("p2"), vec!["cached answer"]);
}

/// N-39 — a fallback to a model the session does not list is tracked raw; one
/// to the model it already runs asks for no sync at all.
#[test]
fn t2a_model_refusal_fallback_sync_skips_the_tracked_model() {
    let mut rig = Rig::new();
    rig.ext.config.current_model_id = "claude-sonnet-4-5".to_string();
    let infos = &rig.ext.config.model_infos;
    assert_eq!(
        external_switch_target(infos, "claude-sonnet-4-5", "claude-sonnet-4-5"),
        None
    );
    assert_eq!(
        external_switch_target(infos, "claude-sonnet-4-5", "custom-unlisted"),
        Some("custom-unlisted".to_string())
    );
    // The frame asks the run loop for the sync either way: the loop decides.
    let mut frame = fallback_frame();
    frame["fallback_model"] = json!("claude-sonnet-4-5");
    rig.feed(frame);
    assert_eq!(
        rig.ext.t2a.effects,
        vec![state::Effect::SyncModel("claude-sonnet-4-5".into())]
    );
}

/// N-39 — an over-long summary moves under a generic notice title, and only a
/// genuinely long explanation goes to the AIR details.
#[test]
fn t2a_model_refusal_fallback_long_text_layout() {
    let mut frame = fallback_frame();
    frame["original_model"] = json!("m".repeat(300));
    let mut notice = notice_rig();
    notice.feed(frame.clone());
    let updates = notice.updates();
    assert_eq!(updates[0]["title"], "Model fallback");
    assert!(updates[0]["description"]
        .as_str()
        .unwrap()
        .contains(&"m".repeat(300)));

    let mut air = air_rig(&["sessionFailure"]);
    let mut short = fallback_frame();
    short["api_refusal_explanation"] = json!("short reason");
    air.feed(short);
    let sf = air.updates()[0]["_meta"]["jetbrains"]["air"]["sessionFailure"].clone();
    assert!(
        sf.get("details").is_none(),
        "a short explanation stays in the title"
    );
    assert!(sf["title"].as_str().unwrap().ends_with("short reason"));
}

// ---------------------------------------------------------------- F14

fn subagent_started(task: &str, run: Option<&str>) -> Value {
    let mut frame = json!({"type":"system","subtype":"task_started","task_id":task,
        "tool_use_id":"toolu_AG","description":"Research","subagent_type":"general-purpose",
        "task_type":"local_agent","is_backgrounded":true});
    if let Some(run) = run {
        frame["run_id"] = json!(run);
    }
    frame
}

fn idle() -> Value {
    json!({"type":"system","subtype":"session_state_changed","state":"idle"})
}

impl Rig {
    /// Whether `uuid`'s prompt was answered (not failed), without consuming it.
    fn answered(&mut self, uuid: &str) -> bool {
        let Some(reply) = self.replies.get_mut(uuid) else {
            return false;
        };
        matches!(reply.try_recv(), Ok(Ok(_)))
    }

    /// A turn held open for the background subagent `sub1`, its own trailing
    /// idle absorbed.
    fn held_for_subagent() -> Rig {
        let mut rig = Rig::new();
        rig.echo("p1");
        rig.tool_use(
            "toolu_AG",
            "Agent",
            json!({"description":"Research","prompt":"x"}),
        );
        rig.feed(subagent_started("sub1", None));
        rig.feed(result_frame("spawned", 5));
        rig.feed(idle());
        rig.updates();
        assert!(rig.machine.has_active(), "p1 is held for sub1");
        rig
    }
}

/// N-18 / N-36 — a terminal status in a `task_updated` PATCH settles the live
/// subagent, so the held turn drains at the next idle (the status is not a
/// top-level field of the frame).
#[test]
fn t2a_terminal_task_updated_patch_releases_the_hold() {
    let mut rig = Rig::held_for_subagent();
    rig.feed(idle());
    assert!(!rig.answered("p1"), "the subagent still runs");
    rig.feed(
        json!({"type":"system","subtype":"task_updated","task_id":"sub1",
        "patch":{"status":"completed"}}),
    );
    rig.feed(idle());
    assert!(rig.answered("p1"), "drained at the idle after the patch");
}

/// N-18 — a `task_notification` settles the task regardless of its status, and
/// ends the display terminal of a backgrounded command.
#[test]
fn t2a_task_notification_settles_and_ends_the_background_terminal() {
    let mut rig = Rig::held_for_subagent();
    rig.ext.t2a.background_terminals.insert(
        "sub1".to_string(),
        state::BackgroundTerminal {
            tool_call_id: "toolu_BASH".to_string(),
            parent_tool_use_id: Some("toolu_AG".to_string()),
        },
    );
    rig.feed(
        json!({"type":"system","subtype":"task_notification","task_id":"sub1",
        "status":"stopped","tool_use_id":"toolu_AG"}),
    );
    let updates = rig.updates();
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["toolCallId"], "toolu_BASH");
    assert_eq!(
        updates[0]["_meta"],
        json!({"claudeCode":{"parentToolUseId":"toolu_AG"},
            "terminal_exit":{"terminal_id":"toolu_BASH","exit_code":null,"signal":null}})
    );
    assert!(rig.ext.t2a.background_terminals.is_empty());
    rig.feed(idle());
    assert!(rig.answered("p1"));
}

/// N-33 — a frame of an earlier run is ignored (and logged); the resumed run's
/// own notification settles.
#[test]
fn t2a_late_task_frames_of_an_earlier_run_are_ignored() {
    let dir = std::env::temp_dir().join(format!("t2a-late-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut rig = Rig::new();
    rig.ext.logger = Some(Arc::new(crate::logger::Logger::to_dir(&dir)));
    rig.echo("p1");
    rig.tool_use(
        "toolu_AG",
        "Agent",
        json!({"description":"Research","prompt":"x"}),
    );
    rig.feed(subagent_started("sub1", Some("run-01")));
    rig.feed(result_frame("spawned", 5));
    rig.feed(idle());
    rig.feed(subagent_started("sub1", Some("run-02")));
    rig.feed(
        json!({"type":"system","subtype":"task_notification","task_id":"sub1",
        "run_id":"run-01","status":"completed","tool_use_id":"toolu_AG"}),
    );
    rig.feed(idle());
    assert!(!rig.answered("p1"), "the late notification closed nothing");
    rig.feed(
        json!({"type":"system","subtype":"task_notification","task_id":"sub1",
        "run_id":"run-02","status":"completed","tool_use_id":"toolu_AG"}),
    );
    rig.feed(idle());
    assert!(rig.answered("p1"));
    let log = std::fs::read_to_string(dir.join("agent.log")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        log.contains("Ignoring task_notification for task sub1 from earlier run run-01"),
        "{log}"
    );
}

/// N-36 — a `running` patch resumes a finished subagent without a new
/// `task_started`, and the turn that resumed it holds again.
#[test]
fn t2a_running_patch_resumes_a_finished_subagent() {
    let mut rig = Rig::held_for_subagent();
    rig.feed(
        json!({"type":"system","subtype":"task_notification","task_id":"sub1",
        "status":"completed","tool_use_id":"toolu_AG"}),
    );
    assert!(rig.live_background.is_empty());
    rig.feed(
        json!({"type":"system","subtype":"task_updated","task_id":"sub1",
        "patch":{"status":"running"}}),
    );
    assert_eq!(
        rig.live_background.get("sub1").map(String::as_str),
        Some("toolu_AG")
    );
    rig.feed(idle());
    assert!(!rig.answered("p1"), "the resumed subagent holds the turn");
}

/// T-12 — a SendMessage result naming a resumed agent registers it live again.
#[test]
fn t2a_send_message_result_resumes_the_named_subagent() {
    let mut rig = Rig::held_for_subagent();
    rig.feed(
        json!({"type":"system","subtype":"task_notification","task_id":"sub1",
        "status":"completed","tool_use_id":"toolu_AG"}),
    );
    assert!(rig.live_background.is_empty());
    rig.tool_use(
        "toolu_SM",
        "SendMessage",
        json!({"to":"sub1","message":"go on"}),
    );
    rig.feed(json!({"type":"user","parent_tool_use_id":null,
        "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_SM",
            "content":"resumed"}]},
        "tool_use_result":{"success":true,"resumedAgentId":"sub1"}}));
    assert_eq!(
        rig.live_background.get("sub1").map(String::as_str),
        Some("toolu_AG")
    );
    // A failed or unrelated result resumes nothing.
    assert_eq!(
        frames::resumed_native_subagent_id(Some(&json!({"success":false,"resumedAgentId":"x"}))),
        None
    );
    assert_eq!(
        frames::resumed_native_subagent_id(Some(&json!({"success":true,"resumedAgentId":"  "}))),
        None
    );
    assert_eq!(frames::resumed_native_subagent_id(None), None);
}

/// N-43 — `background_tasks_changed` ends the hold of a subagent the level no
/// longer lists, drops an absent shell task, and the next idle drains the turn.
#[test]
fn t2a_background_tasks_changed_reconciles_the_live_set() {
    let mut rig = Rig::held_for_subagent();
    rig.feed(
        json!({"type":"system","subtype":"task_started","task_id":"sh1",
        "task_type":"local_bash","tool_use_id":"toolu_SH"}),
    );
    rig.feed(json!({"type":"system","subtype":"background_tasks_changed","tasks":[]}));
    assert_eq!(
        rig.live_background.get("sub1").map(String::as_str),
        Some("toolu_AG"),
        "attribution stays for the ended subagent"
    );
    rig.feed(idle());
    assert!(
        rig.answered("p1"),
        "no hold waits on a level-ended subagent"
    );
}

/// T2FZ-5b / N-43 — the activation sweep that deletes a level-ended subagent
/// also removes it from the attribution map (`settleLiveBackgroundTask`), not
/// only from the machine.
#[test]
fn t2a_activation_sweep_refreshes_the_attribution_map() {
    let mut rig = Rig::held_for_subagent();
    rig.feed(json!({"type":"system","subtype":"background_tasks_changed","tasks":[]}));
    rig.feed(idle());
    assert!(rig.answered("p1"));
    assert_eq!(
        rig.live_background.get("sub1").map(String::as_str),
        Some("toolu_AG"),
        "the ended subagent keeps its attribution until swept"
    );
    for (turn, text) in [("p2", "two"), ("p3", "three")] {
        rig.echo(turn);
        rig.feed(result_frame(text, 5));
    }
    assert_eq!(
        rig.live_background.get("sub1"),
        None,
        "the second activation swept the entry and the map follows"
    );
}

/// T2FZ-5c / U-75 — the live path: when the update channel is gone, the task
/// call's published records are rolled back, so the same notification
/// published after the channel is back carries its fields again.
#[test]
fn t2a_failed_task_send_rolls_back_the_published_fields() {
    let mut rig = air_rig(&["asyncTasks"]);
    rig.tasks = AsyncTaskRuntime::new(true);
    rig.echo("p1");
    rig.feed(
        json!({"type":"system","subtype":"task_started","task_id":"build",
        "task_type":"local_bash","description":"build","is_backgrounded":true,
        "tool_use_id":"bash-tool"}),
    );
    rig.updates();
    let note = json!({"type":"system","subtype":"task_notification","task_id":"build",
        "status":"completed","summary":"Done","output_file":"/tmp/build.output"});

    // The client connection is gone for this frame.
    let (dead_tx, dead_rx) = mpsc::unbounded_channel();
    drop(dead_rx);
    let live_tx = std::mem::replace(&mut rig.tx, dead_tx);
    rig.feed(note.clone());
    rig.tx = live_tx;

    rig.feed(note);
    let updates = rig.updates();
    let state: Vec<&Value> = updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "async_task_state_update")
        .collect();
    assert_eq!(state.len(), 1, "{updates:?}");
    assert_eq!(state[0]["state"], "completed");
    assert_eq!(state[0]["outputFilePath"], "/tmp/build.output");
}

/// N-36 — a backgrounded patch lets a cancel's output through for the subagent.
#[test]
fn t2a_backgrounded_patch_moves_the_subagent_to_the_background() {
    let mut rig = Rig::new();
    rig.echo("p1");
    let mut frame = subagent_started("sub1", None);
    frame["is_backgrounded"] = json!(false);
    rig.feed(frame);
    assert!(!rig.machine.runs_in_background("toolu_AG"));
    rig.feed(
        json!({"type":"system","subtype":"task_updated","task_id":"sub1",
        "patch":{"is_backgrounded":true}}),
    );
    assert!(rig.machine.runs_in_background("toolu_AG"));
}

// ---------------------------------------------------------------- F17

impl Rig {
    /// `session/cancel` as the actor handles it: the swept turns settle and the
    /// stopped-work window opens.
    fn cancel(&mut self) {
        let events = self.machine.cancel();
        self.map_state.begin_stopped_work();
        settle_events(
            events,
            &mut self.map_state,
            &self.live_background,
            &mut self.pending,
            &self.tx,
            "s",
            &self.high_water,
            &self.ext.failure_ctx,
            self.ext.air.as_ref(),
            None,
            &mut self.ext.titles,
            &mut self.ext.t2a.failures,
        );
    }

    /// A tool use that streamed in without a complete assistant message (never
    /// dispatched).
    fn streamed_tool_use(&mut self, id: &str, name: &str, parent: Value) {
        self.feed(json!({"type":"stream_event","parent_tool_use_id":parent,
            "event":{"type":"content_block_start","index":0,
                "content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}}));
    }

    /// A tool use of a subagent (a complete assistant message under `parent`).
    fn subagent_tool_use(&mut self, id: &str, parent: &str) {
        self.feed(json!({"type":"assistant","parent_tool_use_id":parent,
            "message":{"id":"msg_sub","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"tool_use","id":id,"name":"Bash","input":{"command":"ls"}}]}}));
    }

    fn interrupt_result(&mut self) {
        self.feed(
            json!({"type":"result","subtype":"error_during_execution","is_error":true,
            "stop_reason":"end_turn","terminal_reason":"aborted_tools","result":"",
            "usage":{"input_tokens":1,"output_tokens":0}}),
        );
    }
}

fn status_and_text(update: &Value) -> (String, String) {
    (
        update["status"].as_str().unwrap_or("").to_string(),
        update["content"][0]["content"]["text"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    )
}

/// B-08 / B-73 — a cancelled turn settles with every tool call the work left
/// open closed first: one that ran says the turn was cancelled, one Claude
/// abandoned before it ran says so; in the order they were announced.
#[test]
fn t2a_cancelled_turn_closes_its_open_tool_calls_before_the_response() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.tool_use("toolu_A", "Bash", json!({"command": "sleep 100"}));
    rig.streamed_tool_use("toolu_B", "Read", Value::Null);
    rig.updates();
    rig.cancel();
    assert!(
        rig.updates().is_empty(),
        "nothing closes before the turn settles"
    );
    rig.interrupt_result();
    let updates = rig.updates();
    let ids: Vec<&str> = updates
        .iter()
        .map(|u| u["toolCallId"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(ids, vec!["toolu_A", "toolu_B"], "{updates:?}");
    assert_eq!(
        status_and_text(&updates[0]),
        (
            "failed".into(),
            "The turn was cancelled before this tool call returned a result.".into()
        )
    );
    assert_eq!(
        status_and_text(&updates[1]),
        (
            "failed".into(),
            "Claude stopped this tool call before it ran.".into()
        )
    );
    assert!(rig.answered("p1"), "the response follows the closing");
    assert!(
        !rig.map_state.has_emitted("toolu_A"),
        "the calls are forgotten"
    );
}

/// B-08 — a normal end_turn settle keeps its own wording and a cancelled turn
/// that had nothing open closes nothing.
#[test]
fn t2a_cancel_with_nothing_open_closes_nothing() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.cancel();
    rig.interrupt_result();
    assert!(rig.updates().is_empty());
    assert!(rig.answered("p1"));
}

/// B-09 — the interrupted result of a running tool reaches the client like any
/// other result, with the reason it never ran, before the work end is reported;
/// a result for a call the client does not hold is dropped.
#[test]
fn t2a_interrupted_tool_result_is_delivered_before_the_cancelled_response() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.tool_use("toolu_A", "Bash", json!({"command": "sleep 100"}));
    rig.updates();
    rig.cancel();
    rig.feed(json!({"type":"user","parent_tool_use_id":null,
        "message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_A","is_error":true,
                "content":"[Request interrupted by user for tool use]"},
            {"type":"tool_result","tool_use_id":"toolu_STRANGER","is_error":true,"content":"x"}]},
        "tool_result_meta":[{"id":"toolu_A","non_execution_kind":"interrupted"}]}));
    let updates = rig.updates();
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["toolCallId"], "toolu_A");
    assert_eq!(updates[0]["status"], "failed");
    assert_eq!(
        updates[0]["_meta"]["claudeCode"]["nonExecutionKind"],
        "interrupted"
    );
    assert!(
        !rig.map_state.has_emitted("toolu_A"),
        "the call has its result"
    );

    // The turn's result ends the work and the client is told; nothing is left open.
    rig.interrupt_result();
    assert!(rig.updates().is_empty());
    assert!(rig.answered("p1"));
    // After that report, a late result is dropped.
    rig.tool_use("toolu_LATE", "Bash", json!({"command": "ls"}));
    rig.updates();
    rig.feed(json!({"type":"user","parent_tool_use_id":null,
        "message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_LATE","content":"late"}]}}));
    assert!(
        rig.updates().is_empty(),
        "the stopped work's output stays dropped"
    );
}

/// B-09 — the other effects of an interrupted result's message stay dropped: a
/// task plan is not touched.
#[test]
fn t2a_interrupted_results_apply_no_other_effect_of_the_message() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.tool_use(
        "toolu_T",
        "TaskCreate",
        json!({"subject": "x", "description": "d"}),
    );
    rig.tool_use("toolu_A", "Bash", json!({"command": "sleep 100"}));
    rig.updates();
    rig.cancel();
    rig.feed(json!({"type":"user","parent_tool_use_id":null,
        "message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_T",
                "content":{"task":{"id":"t1","subject":"x","status":"pending"}}},
            {"type":"tool_result","tool_use_id":"toolu_A","is_error":true,"content":"interrupted"}]}}));
    let updates = rig.updates();
    assert!(
        updates.iter().all(|u| u["sessionUpdate"] != "plan"),
        "no plan from the stopped work: {updates:?}"
    );
    assert!(rig.ext.task_state.is_empty());
}

/// The ids a cancel closes for a subagent started with `started_in_background`
/// whose `is_backgrounded` is then patched (no status) to `patched`.
fn closed_at_cancel_after_patch(started_in_background: bool, patched: bool) -> Vec<String> {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.tool_use("toolu_AG", "Agent", json!({"description":"a","prompt":"x"}));
    rig.feed(
        json!({"type":"system","subtype":"task_started","task_id":"sub1",
        "tool_use_id":"toolu_AG","subagent_type":"general-purpose",
        "is_backgrounded":started_in_background}),
    );
    rig.subagent_tool_use("toolu_CMD", "toolu_AG");
    rig.feed(
        json!({"type":"system","subtype":"task_updated","task_id":"sub1",
        "patch":{"is_backgrounded":patched}}),
    );
    rig.updates();
    rig.cancel();
    rig.interrupt_result();
    rig.updates()
        .iter()
        .filter_map(|u| u["toolCallId"].as_str().map(str::to_string))
        .collect()
}

/// T2FZ-3 — a `task_updated` patch that only flips `is_backgrounded` refreshes
/// the live background set at once (`runsInBackground` reads it live): a
/// foreground subagent sent to the background keeps its calls open at cancel,
/// and one brought back to the foreground has them closed.
#[test]
fn t2a_backgrounded_patch_without_status_moves_the_cancel_close_set() {
    let to_background = closed_at_cancel_after_patch(false, true);
    assert!(
        !to_background.contains(&"toolu_CMD".to_string()),
        "backgrounded: stays open: {to_background:?}"
    );
    let to_foreground = closed_at_cancel_after_patch(true, false);
    assert!(
        to_foreground.contains(&"toolu_CMD".to_string()),
        "foregrounded: closed: {to_foreground:?}"
    );
}

/// B-08 / T-15 — the tool call of a foreground subagent, for which Claude Code
/// writes no result to the stream, closes with the turn and names its Agent
/// call; a background subagent's call stays open.
#[test]
fn t2a_cancel_closes_foreground_subagent_calls_and_keeps_background_ones() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.tool_use(
        "toolu_AG",
        "Agent",
        json!({"description":"fg","prompt":"x"}),
    );
    rig.tool_use(
        "toolu_BG",
        "Agent",
        json!({"description":"bg","prompt":"y"}),
    );
    rig.feed(
        json!({"type":"system","subtype":"task_started","task_id":"fg1",
        "tool_use_id":"toolu_AG","subagent_type":"general-purpose","is_backgrounded":false}),
    );
    rig.feed(
        json!({"type":"system","subtype":"task_started","task_id":"bg1",
        "tool_use_id":"toolu_BG","subagent_type":"general-purpose","is_backgrounded":true}),
    );
    rig.subagent_tool_use("toolu_FGCMD", "toolu_AG");
    rig.subagent_tool_use("toolu_BGCMD", "toolu_BG");
    rig.updates();
    assert_eq!(
        rig.map_state.tool_call_parent("toolu_FGCMD"),
        Some("toolu_AG")
    );

    rig.cancel();
    rig.interrupt_result();
    let updates = rig.updates();
    let closed: Vec<&str> = updates
        .iter()
        .map(|u| u["toolCallId"].as_str().unwrap_or(""))
        .collect();
    assert!(
        closed.contains(&"toolu_AG") && closed.contains(&"toolu_FGCMD"),
        "{closed:?}"
    );
    assert!(
        !closed.contains(&"toolu_BGCMD"),
        "the background subagent's command stays open"
    );
    let fg = updates
        .iter()
        .find(|u| u["toolCallId"] == "toolu_FGCMD")
        .unwrap();
    assert_eq!(fg["_meta"]["claudeCode"]["parentToolUseId"], "toolu_AG");
    assert_eq!(
        status_and_text(fg).1,
        "The turn was cancelled before this tool call returned a result.",
        "the subagent's command was dispatched, so it ran"
    );
    assert!(rig.map_state.has_emitted("toolu_BGCMD"));
}

/// B-08 — a background subagent's call is closed once the SDK reports it stopped
/// after the work ended.
#[test]
fn t2a_stopped_subagent_closes_its_calls_after_the_work_end() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.tool_use(
        "toolu_BG",
        "Agent",
        json!({"description":"bg","prompt":"y"}),
    );
    rig.feed(
        json!({"type":"system","subtype":"task_started","task_id":"bg1",
        "tool_use_id":"toolu_BG","subagent_type":"general-purpose","is_backgrounded":true}),
    );
    rig.subagent_tool_use("toolu_BGCMD", "toolu_BG");
    rig.updates();
    rig.cancel();
    rig.interrupt_result();
    rig.updates();
    assert!(
        rig.map_state.has_emitted("toolu_BGCMD"),
        "still open while the subagent runs"
    );

    rig.feed(
        json!({"type":"system","subtype":"task_notification","task_id":"bg1",
        "status":"stopped","tool_use_id":"toolu_BG"}),
    );
    let updates = rig.updates();
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["toolCallId"], "toolu_BGCMD");
    assert_eq!(
        updates[0]["_meta"]["claudeCode"]["parentToolUseId"],
        "toolu_BG"
    );
    assert!(!rig.map_state.has_emitted("toolu_BGCMD"));

    // A stop without a preceding cancel closes nothing.
    let mut calm = Rig::new();
    calm.echo("p1");
    calm.tool_use(
        "toolu_BG",
        "Agent",
        json!({"description":"bg","prompt":"y"}),
    );
    calm.feed(
        json!({"type":"system","subtype":"task_started","task_id":"bg1",
        "tool_use_id":"toolu_BG","subagent_type":"general-purpose","is_backgrounded":true}),
    );
    calm.subagent_tool_use("toolu_BGCMD", "toolu_BG");
    calm.updates();
    calm.feed(
        json!({"type":"system","subtype":"task_updated","task_id":"bg1",
        "patch":{"status":"killed"}}),
    );
    assert!(calm.updates().is_empty());
    assert!(calm.map_state.has_emitted("toolu_BGCMD"));
}

/// B-08 — the model cycle Claude Code started on its own closes the call it
/// abandoned when it ends at an idle with no result; a dispatched call and a
/// subagent's call are left alone.
#[test]
fn t2a_autonomous_cycle_end_closes_abandoned_calls() {
    let mut rig = Rig::new();
    rig.feed(json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"message_start","message":{"id":"m1","model":"claude-sonnet-4-5",
        "usage":{"input_tokens":1,"output_tokens":0}}}}));
    rig.streamed_tool_use("toolu_ABANDONED", "Read", Value::Null);
    rig.tool_use("toolu_RAN", "Bash", json!({"command": "ls"}));
    rig.updates();
    assert!(rig.machine.runs_autonomous_cycle());
    rig.feed(idle());
    let updates = rig.updates();
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["toolCallId"], "toolu_ABANDONED");
    assert_eq!(
        status_and_text(&updates[0]).1,
        "Claude stopped this tool call before it ran."
    );
    assert!(rig.map_state.has_emitted("toolu_RAN"));
    assert!(!rig.machine.runs_autonomous_cycle());
}

/// T-84 — the closing report ends the display terminal of a command for a client
/// that renders terminals: `null` for a plain terminal client, `1` for AIR; a
/// tool that is not a command, and a client without terminals, get none.
#[test]
fn t2a_closing_report_ends_the_display_terminal() {
    let terminal = || crate::client_caps::ClientCapabilities {
        terminal_output: true,
        ..Default::default()
    };
    let run = |mut rig: Rig, name: &str| {
        rig.echo("p1");
        rig.tool_use(
            "toolu_C",
            name,
            json!({"command": "sleep 5", "file_path": "/x"}),
        );
        rig.updates();
        rig.cancel();
        rig.interrupt_result();
        rig.updates()
    };
    let mut plain_terminal = Rig::new();
    plain_terminal.map_state.set_client_caps(terminal());
    let updates = run(plain_terminal, "Bash");
    assert_eq!(
        updates[0]["_meta"]["terminal_exit"],
        json!({"terminal_id": "toolu_C", "exit_code": null, "signal": null})
    );

    let mut air = air_rig(&[]);
    air.map_state
        .set_client_caps(crate::client_caps::ClientCapabilities {
            terminal_output: true,
            air_client: true,
            ..Default::default()
        });
    let updates = run(air, "Bash");
    assert_eq!(updates[0]["_meta"]["terminal_exit"]["exit_code"], 1);

    assert!(
        run(Rig::new(), "Bash")[0].get("_meta").is_none(),
        "no terminal client"
    );
    let mut other = Rig::new();
    other.map_state.set_client_caps(terminal());
    assert!(
        run(other, "Read")[0].get("_meta").is_none(),
        "not a command"
    );
}

/// T-84 / B-73 — a command the turn ended without a result for (ran or abandoned
/// before it ran) also ends its terminal, whatever ended the turn.
#[test]
fn t2a_end_turn_closing_report_ends_the_terminal_too() {
    let mut rig = Rig::new();
    rig.map_state
        .set_client_caps(crate::client_caps::ClientCapabilities {
            terminal_output: true,
            ..Default::default()
        });
    rig.echo("p1");
    rig.tool_use("toolu_RAN", "Bash", json!({"command": "sleep 5"}));
    rig.streamed_tool_use("toolu_ABANDONED", "Bash", Value::Null);
    rig.updates();
    rig.feed(result_frame("done", 5));
    let updates = rig.updates();
    let by_id = |id: &str| {
        updates
            .iter()
            .find(|u| u["toolCallId"] == id)
            .cloned()
            .unwrap()
    };
    for (id, text) in [
        (
            "toolu_ABANDONED",
            "Claude stopped this tool call before it ran.",
        ),
        (
            "toolu_RAN",
            "Claude ended the turn without returning a result for this tool.",
        ),
    ] {
        let update = by_id(id);
        assert_eq!(status_and_text(&update), ("failed".into(), text.into()));
        assert_eq!(update["_meta"]["terminal_exit"]["terminal_id"], id);
    }
}

/// B-08 — the force-cancel backstop ends a turn that gets no results: every open
/// call closes with it.
#[test]
fn t2a_force_cancel_closes_every_open_tool_call() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.tool_use("toolu_A", "Bash", json!({"command": "sleep 100"}));
    rig.updates();
    rig.cancel();
    let events = rig.machine.force_cancel();
    settle_events(
        events,
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
    );
    let updates = rig.updates();
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["toolCallId"], "toolu_A");
    assert!(rig.answered("p1"));
}

/// B-66 — a subagent's tool call is not the turn's foreground work: the turn
/// ends cleanly without a result for it.
#[test]
fn t2a_subagent_tool_call_is_not_foreground_work() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.subagent_tool_use("toolu_SUB", "toolu_AG");
    rig.updates();
    rig.feed(result_frame("done", 5));
    assert!(
        rig.answered("p1"),
        "the turn is not failed for the subagent's call"
    );
}

/// B-64 — the book of dispatched tool uses keeps 1000, oldest dropped; a repeat
/// moves to the end.
#[test]
fn t2a_dispatched_book_is_capped_at_1000() {
    let mut books = crate::unfinished::ToolBooks::default();
    for index in 0..1001 {
        books.record_dispatched(&json!([{"type":"tool_use","id":format!("t{index}")}]));
    }
    assert!(!books.is_dispatched("t0"), "the oldest was dropped");
    assert!(books.is_dispatched("t1") && books.is_dispatched("t1000"));
    books.record_dispatched(&json!([{"type":"tool_use","id":"t1"}]));
    books.record_dispatched(&json!([{"type":"tool_use","id":"t1001"}]));
    assert!(books.is_dispatched("t1"), "a repeat moved to the end");
    assert!(!books.is_dispatched("t2"));
}

/// T-15 — a call a permission request surfaced for a subagent records its Agent
/// call; a later final status forgets it.
#[test]
fn t2a_permission_surfaced_subagent_call_records_its_parent() {
    let mut rig = Rig::new();
    let surfaced = crate::permission::ensure_tool_call_emitted(
        "Bash",
        &json!({"command": "ls"}),
        "toolu_P",
        Some("toolu_AG"),
        &mut rig.map_state,
        None,
    );
    assert!(surfaced.is_some());
    assert_eq!(rig.map_state.tool_call_parent("toolu_P"), Some("toolu_AG"));
    rig.feed(json!({"type":"user","parent_tool_use_id":"toolu_AG",
        "message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_P","content":"ok"}]}}));
    assert_eq!(rig.map_state.tool_call_parent("toolu_P"), None);
}

impl Rig {
    /// What `handle_permission` does with a request for `id`: surface the call
    /// (the streamed `tool_use` has not arrived) and count it as foreground work.
    fn permission_surfaces(&mut self, id: &str) {
        self.permission_surfaces_tool("Bash", json!({"command": "sleep 100"}), id);
    }

    fn permission_surfaces_tool(&mut self, tool: &str, input: Value, id: &str) {
        let surfaced = crate::permission::ensure_tool_call_emitted(
            tool,
            &input,
            id,
            None,
            &mut self.map_state,
            None,
        )
        .expect("surfaced");
        note_permission_surfaced(
            &surfaced,
            tool,
            true,
            self.machine.foreground_target(),
            &mut self.map_state,
        );
    }
}

/// T2GA-1 / B-64 — a `SandboxNetworkAccess` approval is recorded as dispatched
/// but never as foreground work (the CLI sends no `tool_result` for its
/// synthetic id): the turn ends `end_turn`. A normal permission-surfaced call
/// left open still fails the prompt (`t2a_permission_surfaced_call_counts_as_dispatched`).
#[test]
fn t2a_sandbox_network_approval_does_not_fail_the_prompt() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.permission_surfaces_tool(
        "SandboxNetworkAccess",
        json!({"host": "example.com"}),
        "toolu_NET",
    );
    assert!(rig.map_state.books.is_dispatched("toolu_NET"));
    rig.updates();
    rig.feed(result_frame("done", 5));
    let reply = rig.replies.get_mut("p1").unwrap().try_recv().unwrap();
    assert!(reply.is_ok(), "{:?}", reply.err().map(|f| f.error));
    // Not foreground work, so the turn's close pass leaves it alone (upstream
    // closes only `foregroundToolCallIds`, `acp-agent.ts:5426-5473`).
    assert!(
        rig.updates().iter().all(|u| u["toolCallId"] != "toolu_NET"),
        "no closing update for a non-foreground call"
    );
}

/// T2FZ-4 / B-64 — a permission request records its call as dispatched (it is
/// about to run), so a cancel closes it as "returned no result" and an
/// `end_turn` with the call open fails the prompt with `incomplete_tool_call`.
#[test]
fn t2a_permission_surfaced_call_counts_as_dispatched() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.permission_surfaces("toolu_PERM");
    assert!(rig.map_state.books.is_dispatched("toolu_PERM"));
    rig.updates();
    rig.cancel();
    rig.interrupt_result();
    let updates = rig.updates();
    let closing = updates
        .iter()
        .find(|u| u["toolCallId"] == "toolu_PERM")
        .expect("closed at cancel");
    assert_eq!(
        status_and_text(closing).1,
        "The turn was cancelled before this tool call returned a result."
    );

    let mut rig = Rig::new();
    rig.echo("p1");
    rig.permission_surfaces("toolu_PERM");
    rig.feed(result_frame("done", 5));
    let reply = rig.replies.get_mut("p1").unwrap().try_recv().unwrap();
    let Err(failure) = reply else {
        panic!("the prompt fails");
    };
    assert!(
        matches!(
            failure.error,
            SessionError::PromptFailed {
                kind: FailureKind::IncompleteToolCall,
                ..
            }
        ),
        "{:?}",
        failure.error
    );
}

/// T2FZ-5a / B-66 — a `tool_call_update` that attributes an eagerly surfaced
/// call to a subagent takes it out of the turn's foreground work: the turn ends
/// cleanly without a result for it.
#[test]
fn t2a_tool_call_update_for_a_child_forgets_the_foreground_call() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.permission_surfaces("toolu_P");
    rig.feed(json!({"type":"assistant","parent_tool_use_id":"toolu_AG",
        "message":{"id":"msg_sub","model":"claude-sonnet-4-5","role":"assistant",
        "content":[{"type":"tool_use","id":"toolu_P","name":"Bash",
            "input":{"command":"sleep 100","description":"wait"}}]}}));
    let updates = rig.updates();
    assert_eq!(kinds(&updates), ["tool_call_update"], "{updates:?}");
    assert_eq!(
        updates[0]["_meta"]["claudeCode"]["parentToolUseId"],
        "toolu_AG"
    );
    rig.feed(result_frame("done", 5));
    let reply = rig.replies.get_mut("p1").unwrap().try_recv().unwrap();
    assert!(reply.is_ok(), "{:?}", reply.err().map(|f| f.error));
}

/// T-16 — a tool name outlives the tool-use cache for 1000 results; a Monitor
/// task is not announced.
#[test]
fn t2a_resolved_tool_names_hide_monitor_tasks() {
    let mut rig = air_rig(&["asyncTasks"]);
    rig.tasks = AsyncTaskRuntime::new(true);
    rig.echo("p1");
    rig.tool_use("toolu_MON", "Monitor", json!({"command": "tail -f log"}));
    rig.feed(
        json!({"type":"system","subtype":"task_started","task_id":"mon1",
        "task_type":"local_workflow","description":"watch","tool_use_id":"toolu_MON"}),
    );
    assert!(
        rig.updates()
            .iter()
            .all(|u| u["sessionUpdate"] != "async_task_spawned"),
        "a task that Monitor started is never announced"
    );
    // The result drops the cache entry; the name is still known.
    rig.feed(json!({"type":"user","parent_tool_use_id":null,
        "message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_MON","content":"started"}]}}));
    assert_eq!(
        rig.map_state.resolved_tool_name("toolu_MON"),
        Some("Monitor")
    );
    rig.feed(
        json!({"type":"system","subtype":"task_started","task_id":"mon2",
        "task_type":"local_workflow","description":"watch again","tool_use_id":"toolu_MON"}),
    );
    assert!(rig
        .updates()
        .iter()
        .all(|u| u["sessionUpdate"] != "async_task_spawned"));
}

// ---------------------------------------------------------------- F30

/// A bare session actor (no child process): its command sender and outbound
/// receiver. The control writer's far end stays open and unread, so a control
/// request the actor sends is written and never answered.
fn spawn_title_actor() -> (
    mpsc::UnboundedSender<Command>,
    mpsc::UnboundedReceiver<SessionOutbound>,
    tokio::io::DuplexStream,
) {
    let (writer, far_end) = tokio::io::duplex(64 * 1024);
    let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
    let (tx, commands) = mpsc::unbounded_channel::<Command>();
    let control = Control::spawn(writer, control_in_rx, ControlOptions { on_request: None });
    let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
    let (_stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
    // The stream stays open for the test's life.
    std::mem::forget(_stream_tx);
    tokio::spawn(run_loop(
        commands,
        tx.clone(),
        stream_rx,
        control,
        control_in_tx,
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
    ));
    (tx, update_rx, far_end)
}

async fn next_session_info(rx: &mut mpsc::UnboundedReceiver<SessionOutbound>) -> Option<Value> {
    let found = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(item) = rx.recv().await {
            let update = match item {
                SessionOutbound::RawUpdate(value) => value["update"].clone(),
                SessionOutbound::Update(notif) => serde_json::to_value(&notif.update).unwrap(),
                _ => continue,
            };
            if update["sessionUpdate"] == "session_info_update" {
                return Some(update);
            }
        }
        None
    })
    .await;
    found.unwrap_or(None)
}

async fn title_barrier(tx: &mpsc::UnboundedSender<Command>) {
    let (reply, rx) = oneshot::channel();
    tx.send(Command::TitleSnapshot { reply })
        .expect("actor alive");
    let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
}

fn persist_ok(stored: &'static str) -> ExplicitPersist {
    Box::new(move || Box::pin(async move { Ok(Some(stored.to_string())) }))
}

fn long_prompt(tx: &mpsc::UnboundedSender<Command>) {
    let (reply, _rx) = oneshot::channel();
    tx.send(Command::Prompt {
        uuid: "p1".into(),
        frame: json!({"type":"user","uuid":"p1","message":{"role":"user","content":[]}}),
        is_local_only: false,
        prompt_text: "please title this long enough session".into(),
        reply,
    })
    .expect("actor alive");
}

/// U-45 — a client-chosen title publishes `{title}` without `updatedAt`, once
/// stored, and the snapshot reports it as shown and as stored.
#[tokio::test]
async fn t2a_explicit_title_publishes_without_updated_at() {
    let (tx, mut rx, _far_end) = spawn_title_actor();
    let (reply, reply_rx) = oneshot::channel();
    tx.send(Command::SetExplicitTitle {
        publish: crate::session_titles::ExplicitPublish::Title("Renamed".into()),
        persist: persist_ok("Renamed"),
        reply,
    })
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), reply_rx)
        .await
        .expect("replied")
        .expect("actor alive")
        .expect("ok");
    let update = next_session_info(&mut rx)
        .await
        .expect("session_info_update");
    assert_eq!(
        update,
        json!({"sessionUpdate": "session_info_update", "title": "Renamed"})
    );
    let (reply, snapshot) = oneshot::channel();
    tx.send(Command::TitleSnapshot { reply }).unwrap();
    assert_eq!(
        snapshot.await.unwrap(),
        TitleSnapshot {
            shown: Some("Renamed".into()),
            stored: Some("Renamed".into())
        }
    );
}

/// B-79 — the publish may follow from what was stored (`archive` prefixes the
/// stored title); a failed change publishes nothing and the title may still be
/// generated later.
#[tokio::test]
async fn t2a_explicit_title_from_stored_and_failed_changes() {
    let (tx, mut rx, _far_end) = spawn_title_actor();
    let (reply, reply_rx) = oneshot::channel();
    tx.send(Command::SetExplicitTitle {
        publish: crate::session_titles::ExplicitPublish::FromStored(Box::new(|stored| {
            format!("shown:{}", stored.unwrap_or("-"))
        })),
        persist: persist_ok("[archived] Old"),
        reply,
    })
    .unwrap();
    reply_rx.await.unwrap().unwrap();
    assert_eq!(
        next_session_info(&mut rx).await.unwrap()["title"],
        "shown:[archived] Old"
    );

    // A failure replies with the error and publishes nothing.
    let (reply, reply_rx) = oneshot::channel();
    tx.send(Command::SetExplicitTitle {
        publish: crate::session_titles::ExplicitPublish::Title("never".into()),
        persist: Box::new(|| Box::pin(async { Err("disk full".to_string()) })),
        reply,
    })
    .unwrap();
    assert_eq!(reply_rx.await.unwrap(), Err("disk full".to_string()));
    title_barrier(&tx).await;
    assert!(rx.try_recv().is_err(), "a failed change publishes nothing");
}

/// B-79 / A-13 — a rename that arrives while the title is being generated waits
/// for it: its own title is persisted and published last, and the generated
/// title is never published over it.
#[tokio::test]
async fn t2a_rename_waits_for_a_generation_in_flight() {
    let (tx, mut rx, _far_end) = spawn_title_actor();
    long_prompt(&tx);
    // The turn ends: the generation request goes out (and is never answered here).
    tx.send(Command::TitleInfo {
        transcript: None,
        info: None,
        cancelled: false,
        renames: 0,
    })
    .unwrap();
    title_barrier(&tx).await;

    let persisted = Arc::new(AtomicUsize::new(0));
    let seen = persisted.clone();
    let (reply, mut reply_rx) = oneshot::channel();
    tx.send(Command::SetExplicitTitle {
        publish: crate::session_titles::ExplicitPublish::Title("Mine".into()),
        persist: Box::new(move || {
            Box::pin(async move {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(Some("Mine".to_string()))
            })
        }),
        reply,
    })
    .unwrap();
    title_barrier(&tx).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        persisted.load(Ordering::SeqCst),
        0,
        "the rename waits for the generation"
    );
    assert!(reply_rx.try_recv().is_err());

    // The generation answers: its title is not published over the client's.
    tx.send(Command::TitleGenerated {
        title: Some("Generated".into()),
        fallback: None,
    })
    .unwrap();
    reply_rx.await.unwrap().unwrap();
    assert_eq!(persisted.load(Ordering::SeqCst), 1);
    let update = next_session_info(&mut rx)
        .await
        .expect("the client's title");
    assert_eq!(update["title"], "Mine", "{update}");
    title_barrier(&tx).await;
    assert!(rx.try_recv().is_err(), "no generated title follows");
}

/// B-79 — once a client named the session, no title is generated over it at a
/// turn end; a title stored by someone else is still adopted.
#[tokio::test]
async fn t2a_client_named_session_is_never_generated_over() {
    let (tx, mut rx, _far_end) = spawn_title_actor();
    let (reply, reply_rx) = oneshot::channel();
    tx.send(Command::SetExplicitTitle {
        publish: crate::session_titles::ExplicitPublish::Title("Client title".into()),
        persist: persist_ok("Client title"),
        reply,
    })
    .unwrap();
    reply_rx.await.unwrap().unwrap();
    assert_eq!(
        next_session_info(&mut rx).await.unwrap()["title"],
        "Client title"
    );

    long_prompt(&tx);
    let info = crate::session_store::StoredSessionInfo {
        custom_title: Some("Renamed elsewhere".into()),
        summary: None,
        mtime_ms: 1_700_000_000_000,
    };
    // Read when no rename ran since the request: adopted.
    tx.send(Command::TitleInfo {
        transcript: None,
        info: Some(info.clone()),
        cancelled: false,
        renames: 1,
    })
    .unwrap();
    let update = next_session_info(&mut rx).await.expect("adopted");
    assert_eq!(update["title"], "Renamed elsewhere");
    assert!(update.get("updatedAt").is_some());

    // A rename ran during the read: the stored title is stale, nothing happens.
    let stale = crate::session_store::StoredSessionInfo {
        custom_title: Some("Stale".into()),
        ..info
    };
    tx.send(Command::TitleInfo {
        transcript: None,
        info: Some(stale),
        cancelled: false,
        renames: 0,
    })
    .unwrap();
    title_barrier(&tx).await;
    assert!(rx.try_recv().is_err());
}

/// T2FZ-6 / B-79 — the contract `session_index` relies on for a loaded session:
/// a rename that waited for a generation publishes the client's title once, and
/// the next turn end that reads the same stored title publishes nothing
/// (`last_title` learned the shown rename).
#[tokio::test]
async fn t2a_rename_during_generation_leaves_no_duplicate_at_turn_end() {
    let (tx, mut rx, _far_end) = spawn_title_actor();
    long_prompt(&tx);
    tx.send(Command::TitleInfo {
        transcript: None,
        info: None,
        cancelled: false,
        renames: 0,
    })
    .unwrap();
    title_barrier(&tx).await;

    let (reply, reply_rx) = oneshot::channel();
    tx.send(Command::SetExplicitTitle {
        publish: crate::session_titles::ExplicitPublish::Title("Mine".into()),
        persist: persist_ok("Mine"),
        reply,
    })
    .unwrap();
    tx.send(Command::TitleGenerated {
        title: Some("Generated".into()),
        fallback: None,
    })
    .unwrap();
    reply_rx.await.unwrap().unwrap();
    assert_eq!(next_session_info(&mut rx).await.unwrap()["title"], "Mine");

    // The next turn end reads the title the rename stored.
    tx.send(Command::TitleInfo {
        transcript: None,
        info: Some(crate::session_store::StoredSessionInfo {
            custom_title: Some("Mine".into()),
            summary: None,
            mtime_ms: 1_700_000_000_000,
        }),
        cancelled: false,
        renames: 1,
    })
    .unwrap();
    title_barrier(&tx).await;
    assert!(rx.try_recv().is_err(), "no duplicate session_info_update");
    let (reply, snapshot) = oneshot::channel();
    tx.send(Command::TitleSnapshot { reply }).unwrap();
    assert_eq!(
        snapshot.await.unwrap(),
        TitleSnapshot {
            shown: Some("Mine".into()),
            stored: Some("Mine".into())
        }
    );
}

/// T2FZ-6 / A-13 — `remember_stored_title` and `set_client_title` take effect
/// at once: the snapshot reports the remembered title as stored, and a title
/// adopted afterwards reads through the client mapping.
#[tokio::test]
async fn t2a_remembered_title_and_client_mapping_apply_to_the_next_publish() {
    let (tx, mut rx, _far_end) = spawn_title_actor();
    tx.send(Command::SetClientTitle {
        map: crate::session_titles::ClientTitle(Arc::new(|raw| {
            raw.trim_start_matches("[archived] ").to_string()
        })),
    })
    .unwrap();
    tx.send(Command::RememberStoredTitle {
        title: "[archived] Old".into(),
    })
    .unwrap();
    let (reply, snapshot) = oneshot::channel();
    tx.send(Command::TitleSnapshot { reply }).unwrap();
    assert_eq!(
        snapshot.await.unwrap(),
        TitleSnapshot {
            shown: None,
            stored: Some("[archived] Old".into())
        }
    );
    tx.send(Command::TitleInfo {
        transcript: None,
        info: Some(crate::session_store::StoredSessionInfo {
            custom_title: Some("[archived] Old".into()),
            summary: None,
            mtime_ms: 1_700_000_000_000,
        }),
        cancelled: false,
        renames: 0,
    })
    .unwrap();
    assert_eq!(next_session_info(&mut rx).await.unwrap()["title"], "Old");
}

/// N-51 — a conversation reset releases the title: the client-set guard, the
/// collected text and the shown title all go.
#[test]
fn t2a_conversation_reset_releases_the_explicit_title() {
    let mut rig = Rig::new();
    rig.ext.titles.begin_explicit_change();
    rig.ext.titles.end_explicit_change(Some(Some("Mine")), true);
    rig.ext.titles.publish_explicit("Mine".into());
    assert!(rig.ext.titles.explicit.settled_by_client());
    assert_eq!(rig.ext.titles.stored_title(), Some("Mine"));
    rig.feed(json!({"type":"conversation_reset","new_conversation_id":"c2"}));
    assert!(!rig.ext.titles.explicit.active());
    assert_eq!(rig.ext.titles.stored_title(), None);
    assert_eq!(rig.ext.titles.shown_title(), None);
    assert!(!rig.ext.titles.can_request(false), "no collected text yet");
    rig.ext.titles.on_prompt("a prompt long enough to title");
    assert!(
        rig.ext.titles.can_request(false),
        "the next turn re-evaluates"
    );
}

/// B-79 — a change that fails or names nothing puts the title state back; one
/// that names the session keeps it settled.
#[test]
fn t2a_explicit_change_restores_the_title_state_unless_it_named_the_session() {
    let mut titles = crate::session_titles::SessionTitles::new();
    titles.on_prompt("some collected context here");
    titles.begin_explicit_change();
    assert!(!titles.can_request(false), "settled while the change runs");
    assert!(titles.explicit.active());
    titles.end_explicit_change(None, true);
    assert!(!titles.explicit.active());
    assert!(
        titles.can_request(false),
        "the failed change put the state back"
    );

    titles.begin_explicit_change();
    titles.end_explicit_change(Some(None), false);
    assert!(
        titles.can_request(false),
        "a change that names nothing restores too"
    );

    titles.begin_explicit_change();
    titles.end_explicit_change(Some(Some("T")), false);
    assert!(titles.explicit.settled_by_client());
    assert!(!titles.can_request(false));
}

/// A-13 — the generated title is published sanitised with the client's mapping
/// of the stored form (an AIR client reads an archived title without its prefix).
#[test]
fn t2a_client_title_mapping_applies_before_sanitising() {
    let mut titles = crate::session_titles::SessionTitles::new();
    titles.client_title = Some(crate::session_titles::ClientTitle(Arc::new(|raw| {
        raw.trim_start_matches("[archived] ").to_string()
    })));
    let published =
        titles.publish_stored("[archived]   My\nTitle ", Some("2026-01-01T00:00:00.000Z"));
    assert_eq!(
        published,
        Some((
            "My Title".to_string(),
            Some("2026-01-01T00:00:00.000Z".to_string())
        ))
    );
    assert!(
        titles.publish_stored("[archived] My Title", None).is_none(),
        "unchanged"
    );
}

/// V-04 — only the CLI's single-block synthetic login reply matches.
#[test]
fn t2a_synthetic_login_message_matching() {
    let login =
        json!({"model":"<synthetic>","content":[{"type":"text","text":"x · Please run /login"}]});
    assert!(frames::is_synthetic_login_message(&login));
    assert!(!frames::is_synthetic_login_message(
        &json!({"model":"claude-sonnet-4-5","content":[{"type":"text","text":"Please run /login"}]})
    ));
    assert!(!frames::is_synthetic_login_message(
        &json!({"model":"<synthetic>","content":[{"type":"text","text":"hello"}]})
    ));
    assert!(!frames::is_synthetic_login_message(
        &json!({"model":"<synthetic>","content":
        [{"type":"text","text":"Please run /login"},{"type":"text","text":"more"}]})
    ));
    assert!(!frames::is_synthetic_login_message(&Value::Null));
}
