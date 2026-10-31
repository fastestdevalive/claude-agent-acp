//! T5A — F20 context compaction at the session level: the `compacting` status,
//! `compact_result`, `compact_boundary`, the API compaction block's stream
//! heartbeat, the PostCompact summary and the turn-boundary lifecycle.
//!
//! The frame-level tests drive `handle_session_frame` with the state a running
//! actor would hold; the cancel test drives the real `run_loop` actor.

use super::tests_t2a::Rig;
use super::*;
use crate::context_compaction::{CompactionPresentation, ContextCompactionLifecycle};

fn rig(presentation: CompactionPresentation, air_client: bool) -> Rig {
    let mut rig = Rig::new();
    if air_client {
        rig.ext = SessionExt::new(
            crate::air::AirClient::from_client_capabilities(&json!({
                "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": []}}}
            })),
            PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        rig.map_state = fresh_map_state(&rig.ext, false);
    }
    rig.ext.t2a.compaction = ContextCompactionLifecycle::new("s", presentation, air_client);
    rig
}

fn air_meta(facts: Value) -> Value {
    json!({"jetbrains": {"air": {"version": 1, "contextCompaction": facts}}})
}

fn kinds(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .map(|u| u["sessionUpdate"].as_str().unwrap_or("").to_string())
        .collect()
}

fn post_compact(rig: &mut Rig, summary: Value) {
    let (reply, _reply_rx) = oneshot::channel();
    handle_hook_callback(
        &mut AsyncTaskRuntime::new(false),
        HookCall {
            request_id: "r",
            callback_id: crate::hooks::POST_COMPACT_HOOK_ID,
            input: &json!({"compact_summary": summary}),
            tool_use_id: None,
        },
        &mut rig.map_state,
        &mut rig.ext,
        HookIo {
            update_tx: &rig.tx,
            command_tx: &mpsc::unbounded_channel().0,
            session_id: "s",
        },
        reply,
    );
}

fn status(frame: Value) -> Value {
    json!({"type": "system", "subtype": "status", "uuid": "c1", "status": frame})
}

// ---------------------------------------------------------------- N-07

#[test]
fn t5a_status_compacting_opens_the_legacy_tool_call() {
    let mut rig = rig(CompactionPresentation::ToolCall, false);
    rig.feed(status(json!("compacting")));
    let updates = rig.updates();
    assert_eq!(kinds(&updates), vec!["tool_call"]);
    assert_eq!(updates[0]["toolCallId"], "c1");
    assert_eq!(updates[0]["title"], "Compact conversation");
    assert_eq!(updates[0]["kind"], "think");
    assert_eq!(updates[0]["status"], "in_progress");
    assert_eq!(
        updates[0]["_meta"],
        json!({"claudeCode": {"toolName": "compact"}})
    );
    assert!(rig.ext.t2a.compaction_output_delivered);
}

#[test]
fn t5a_compact_result_success_completes_the_open_tool_call() {
    let mut rig = rig(CompactionPresentation::ToolCall, false);
    rig.feed(status(json!("compacting")));
    rig.updates();
    rig.feed(json!({"type": "system", "subtype": "status", "uuid": "c1",
        "compact_result": "success"}));
    let updates = rig.updates();
    assert_eq!(kinds(&updates), vec!["tool_call_update"]);
    assert_eq!(updates[0]["toolCallId"], "c1");
    assert_eq!(updates[0]["status"], "completed");
    assert_eq!(
        updates[0]["_meta"],
        json!({"claudeCode": {"toolName": "compact"}})
    );
}

#[test]
fn t5a_compact_result_failed_carries_the_error_and_suppresses_its_stdout() {
    let mut rig = rig(CompactionPresentation::ToolCall, false);
    rig.feed(status(json!("compacting")));
    rig.updates();
    rig.feed(json!({"type": "system", "subtype": "status", "uuid": "c1",
        "compact_result": "failed", "compact_error": "summary rejected"}));
    let updates = rig.updates();
    assert_eq!(kinds(&updates), vec!["tool_call_update"]);
    assert_eq!(updates[0]["status"], "failed");
    assert_eq!(
        updates[0]["content"],
        json!([{"type": "content", "content": {"type": "text",
            "text": "Compaction failed: summary rejected"}}])
    );
    // The duplicate stdout of the same error is consumed once.
    rig.feed(json!({"type": "system", "subtype": "local_command_output",
        "content": "summary rejected\n"}));
    assert!(rig.updates().is_empty());
    // Unrelated command output still renders.
    rig.feed(json!({"type": "system", "subtype": "local_command_output",
        "content": "other output"}));
    assert_eq!(
        rig.updates()
            .iter()
            .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
            .count(),
        1
    );
}

// ---------------------------------------------------------------- N-08 / N-73

#[test]
fn t5a_compact_boundary_without_metadata_still_reports_the_terminal() {
    let mut rig = rig(CompactionPresentation::ToolCall, false);
    rig.feed(json!({"type": "system", "subtype": "compact_boundary", "uuid": "b1"}));
    let updates = rig.updates();
    let call = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "tool_call")
        .expect("a terminal tool call");
    assert_eq!(call["toolCallId"], "b1");
    assert_eq!(call["status"], "completed");
    assert!(call.get("rawOutput").is_none());
    assert_eq!(
        call["_meta"],
        json!({"claudeCode": {"toolName": "compact"}})
    );
    assert!(updates.iter().any(|u| u["sessionUpdate"] == "usage_update"));
    assert!(rig.ext.t2a.compaction_output_delivered);
}

#[test]
fn t5a_compact_boundary_air_tool_call_carries_the_air_facts() {
    let mut rig = rig(CompactionPresentation::ToolCall, true);
    rig.feed(
        json!({"type": "system", "subtype": "compact_boundary", "uuid": "b1",
        "compact_metadata": {"trigger": "auto", "pre_tokens": 500,
            "post_tokens": 120, "duration_ms": 7}}),
    );
    let call = rig
        .updates()
        .into_iter()
        .find(|u| u["sessionUpdate"] == "tool_call")
        .expect("a terminal tool call");
    assert_eq!(call["status"], "completed");
    // AIR gets no rawOutput; the facts are the AIR extension.
    assert!(call.get("rawOutput").is_none());
    assert_eq!(
        call["_meta"],
        air_meta(
            json!({"version": 1, "trigger": "automatic", "preTokens": 500,
            "postTokens": 120, "durationMs": 7})
        )
    );
}

#[test]
fn t5a_compact_boundary_enriches_an_opened_tool_call_without_a_status() {
    let mut rig = rig(CompactionPresentation::ToolCall, false);
    rig.feed(status(json!("compacting")));
    rig.updates();
    rig.feed(
        json!({"type": "system", "subtype": "compact_boundary", "uuid": "c1",
        "compact_metadata": {"trigger": "manual", "pre_tokens": 9}}),
    );
    let updates = rig.updates();
    let update = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "tool_call_update")
        .expect("the boundary enriches the open call");
    // The opening status never carried a terminal, so the boundary is the
    // entity's first terminal: it reports `completed` (`firstTerminal`).
    assert_eq!(update["status"], "completed");
    assert_eq!(
        update["rawOutput"],
        json!({"trigger": "manual", "preTokens": 9})
    );
}

#[test]
fn t5a_air_tool_call_updates_carry_only_the_changed_meta_keys() {
    let mut rig = rig(CompactionPresentation::ToolCall, true);
    rig.feed(status(json!("compacting")));
    let opened = rig.updates();
    assert_eq!(opened[0]["_meta"], air_meta(json!({"version": 1})));
    // The stream block's heartbeat re-asserts in_progress: every AIR key is
    // unchanged, so the update carries no `_meta` at all.
    rig.feed(
        json!({"type": "stream_event", "uuid": "c1", "parent_tool_use_id": null,
        "event": {"type": "content_block_start", "content_block": {"type": "compaction",
            "content": null}}}),
    );
    let heartbeat = rig.updates();
    assert_eq!(kinds(&heartbeat), vec!["tool_call_update"]);
    assert_eq!(heartbeat[0]["status"], "in_progress");
    assert!(heartbeat[0].get("_meta").is_none());
    // The boundary changes only the facts: `air.version` is unchanged and
    // stripped, the facts object is kept whole (with its own version).
    rig.feed(
        json!({"type": "system", "subtype": "compact_boundary", "uuid": "c1",
        "compact_metadata": {"trigger": "auto", "pre_tokens": 500, "post_tokens": 120}}),
    );
    let terminal = rig
        .updates()
        .into_iter()
        .find(|u| u["sessionUpdate"] == "tool_call_update")
        .expect("the terminal tool call update");
    assert_eq!(
        terminal["_meta"],
        json!({"jetbrains": {"air": {"contextCompaction": {
            "version": 1, "trigger": "automatic", "preTokens": 500, "postTokens": 120
        }}}})
    );
}

#[test]
fn t5a_compact_boundary_uses_the_compaction_contract_when_advertised() {
    let mut rig = rig(CompactionPresentation::CompactionUpdate, true);
    rig.feed(status(json!("compacting")));
    let opened = rig.updates();
    assert_eq!(kinds(&opened), vec!["compaction_update"]);
    assert_eq!(opened[0]["compactionId"], "c1");
    assert_eq!(opened[0]["status"], "in_progress");
    assert_eq!(opened[0]["_meta"], air_meta(json!({"version": 1})));
    rig.feed(
        json!({"type": "system", "subtype": "compact_boundary", "uuid": "c1",
        "compact_metadata": {"trigger": "manual", "pre_tokens": 50, "post_tokens": 5}}),
    );
    let updates = rig.updates();
    assert_eq!(
        kinds(&updates),
        vec!["compaction_update", "usage_update"],
        "the boundary's usage update follows the compaction update"
    );
    let update = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "compaction_update")
        .expect("the terminal update");
    assert_eq!(update["compactionId"], "c1");
    assert_eq!(update["status"], "completed");
    assert_eq!(
        update["_meta"],
        air_meta(json!({"version": 1, "trigger": "manual", "preTokens": 50,
            "postTokens": 5}))
    );
}

// ---------------------------------------------------------------- R-03 / U-79

#[test]
fn t5a_post_compact_summary_rides_the_terminal_update() {
    let mut rig = rig(CompactionPresentation::CompactionUpdate, true);
    rig.feed(status(json!("compacting")));
    rig.updates();
    post_compact(
        &mut rig,
        json!("<analysis>x</analysis><summary>\nRetained.\n</summary>"),
    );
    assert!(rig.updates().is_empty());
    rig.feed(json!({"type": "system", "subtype": "compact_boundary", "uuid": "c1"}));
    let update = rig
        .updates()
        .into_iter()
        .find(|u| u["sessionUpdate"] == "compaction_update")
        .expect("the terminal update");
    assert_eq!(
        update["summary"],
        json!([{"type": "text", "text": "Retained."}])
    );
}

#[test]
fn t5a_summary_before_the_opening_status_waits_for_it() {
    let mut rig = rig(CompactionPresentation::CompactionUpdate, true);
    post_compact(&mut rig, json!("<summary>Early.</summary>"));
    assert!(rig.updates().is_empty());
    rig.feed(status(json!("compacting")));
    rig.updates();
    rig.feed(json!({"type": "system", "subtype": "compact_boundary", "uuid": "c1"}));
    let update = rig
        .updates()
        .into_iter()
        .find(|u| u["sessionUpdate"] == "compaction_update")
        .expect("the terminal update");
    assert_eq!(
        update["summary"],
        json!([{"type": "text", "text": "Early."}])
    );
}

#[test]
fn t5a_pending_summary_survives_a_turn_boundary() {
    let mut rig = rig(CompactionPresentation::CompactionUpdate, true);
    post_compact(&mut rig, json!("<summary>Next turn's.</summary>"));
    rig.feed(
        json!({"type": "result", "subtype": "success", "is_error": false,
        "stop_reason": "end_turn", "uuid": "r1", "result": "done",
        "usage": {"input_tokens": 0, "output_tokens": 0}}),
    );
    rig.updates();
    rig.feed(status(json!("compacting")));
    rig.updates();
    rig.feed(json!({"type": "system", "subtype": "compact_boundary", "uuid": "c1"}));
    let update = rig
        .updates()
        .into_iter()
        .find(|u| u["sessionUpdate"] == "compaction_update")
        .expect("the terminal update");
    assert_eq!(
        update["summary"],
        json!([{"type": "text", "text": "Next turn's."}])
    );
}

// ---------------------------------------------------------------- N-32 / U-78

#[test]
fn t5a_stream_compaction_block_streams_summary_chunks_into_an_announced_entity() {
    let mut rig = rig(CompactionPresentation::CompactionUpdate, true);
    // The block alone never opens an entity.
    rig.feed(
        json!({"type": "stream_event", "uuid": "st", "parent_tool_use_id": null,
        "event": {"type": "content_block_delta", "delta": {"type": "compaction_delta",
            "content": "orphan"}}}),
    );
    assert!(rig.updates().is_empty());
    rig.feed(status(json!("compacting")));
    rig.updates();
    rig.feed(
        json!({"type": "stream_event", "uuid": "st", "parent_tool_use_id": null,
        "event": {"type": "content_block_start", "content_block": {"type": "compaction",
            "content": null}}}),
    );
    assert!(rig.updates().is_empty());
    rig.feed(
        json!({"type": "stream_event", "uuid": "st", "parent_tool_use_id": null,
        "event": {"type": "content_block_delta", "delta": {"type": "compaction_delta",
            "content": "Part"}}}),
    );
    rig.feed(
        json!({"type": "stream_event", "uuid": "st", "parent_tool_use_id": null,
        "event": {"type": "content_block_delta", "delta": {"type": "compaction_delta",
            "content": " two"}}}),
    );
    let chunks = rig.updates();
    assert_eq!(
        kinds(&chunks),
        vec!["compaction_summary_chunk", "compaction_summary_chunk"]
    );
    assert_eq!(chunks[0]["compactionId"], "c1");
    assert_eq!(
        chunks[0]["content"],
        json!({"type": "text", "text": "Part"})
    );
    assert_eq!(
        chunks[1]["content"],
        json!({"type": "text", "text": " two"})
    );
    // The hook's cleaned summary equals the streamed text: not repeated.
    post_compact(&mut rig, json!("<summary>\nPart two\n</summary>"));
    rig.feed(json!({"type": "system", "subtype": "compact_boundary", "uuid": "c1"}));
    let terminal = rig
        .updates()
        .into_iter()
        .find(|u| u["sessionUpdate"] == "compaction_update")
        .expect("the terminal update");
    assert!(terminal.get("summary").is_none());
}

#[test]
fn t5a_stream_compaction_block_opens_the_legacy_tool_call() {
    let mut rig = rig(CompactionPresentation::ToolCall, false);
    rig.feed(
        json!({"type": "stream_event", "uuid": "st", "parent_tool_use_id": null,
        "event": {"type": "content_block_start", "content_block": {"type": "compaction",
            "content": null}}}),
    );
    let updates = rig.updates();
    // The block opens the call and re-asserts `in_progress` once
    // (`heartbeat`: start, then the one inProgress).
    assert_eq!(kinds(&updates), vec!["tool_call", "tool_call_update"]);
    assert_eq!(updates[0]["toolCallId"], "st");
    assert_eq!(updates[0]["status"], "in_progress");
    assert_eq!(updates[1]["status"], "in_progress");
    // Later chunks are silent.
    rig.feed(
        json!({"type": "stream_event", "uuid": "st", "parent_tool_use_id": null,
        "event": {"type": "content_block_delta", "delta": {"type": "compaction_delta",
            "content": "text"}}}),
    );
    rig.feed(
        json!({"type": "stream_event", "uuid": "st", "parent_tool_use_id": null,
        "event": {"type": "content_block_delta", "delta": {"type": "compaction_delta",
            "content": "more"}}}),
    );
    assert!(rig.updates().is_empty());
}

#[test]
fn t5a_nested_compaction_block_does_not_touch_the_session() {
    let mut rig = rig(CompactionPresentation::ToolCall, false);
    rig.feed(json!({"type": "stream_event", "uuid": "st",
        "parent_tool_use_id": "toolu_agent",
        "event": {"type": "content_block_start", "content_block": {"type": "compaction",
            "content": null}}}));
    assert!(rig.updates().is_empty());
    assert!(!rig.ext.t2a.compaction_output_delivered);
}

// ---------------------------------------------------------------- result / idle

#[test]
fn t5a_result_closes_an_open_compaction_update_as_cancelled() {
    let mut rig = rig(CompactionPresentation::CompactionUpdate, true);
    rig.feed(status(json!("compacting")));
    rig.updates();
    rig.feed(
        json!({"type": "result", "subtype": "success", "is_error": false,
        "stop_reason": "end_turn", "uuid": "r1", "result": "done",
        "usage": {"input_tokens": 0, "output_tokens": 0}}),
    );
    let updates = rig.updates();
    let cancelled = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "compaction_update")
        .expect("the cancelled terminal");
    assert_eq!(cancelled["compactionId"], "c1");
    assert_eq!(cancelled["status"], "cancelled");
    assert!(!rig.ext.t2a.compaction_output_delivered);
}

// ---------------------------------------------------------------- B-109 (actor)

/// A `run_loop` actor without a child process, configured for the ACP
/// compaction contract.
struct ActorRig {
    tx: mpsc::UnboundedSender<Command>,
    stream_tx: Option<mpsc::UnboundedSender<Value>>,
    update_rx: mpsc::UnboundedReceiver<SessionOutbound>,
    actor: tokio::task::JoinHandle<()>,
    _writer: tokio::io::DuplexStream,
}

impl ActorRig {
    fn spawn() -> Self {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(a, control_in_rx, ControlOptions { on_request: None });
        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
        let caps = crate::client_caps::ClientCapabilities {
            compaction: true,
            air_client: true,
            ..crate::client_caps::ClientCapabilities::default()
        };
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
            None,
            caps,
            std::env::temp_dir(),
            false,
            false,
            crate::sdk_options::RawSdkMessages::Off,
            crate::env_snapshot::EnvSnapshot::empty(),
            None,
        ));
        Self {
            tx,
            stream_tx: Some(stream_tx),
            update_rx,
            actor,
            _writer: b,
        }
    }

    fn send(&self, frame: Value) {
        self.stream_tx.as_ref().unwrap().send(frame).unwrap();
    }

    fn echo(&self, uuid: &str) {
        self.send(json!({"type": "user", "uuid": uuid, "isReplay": true,
            "message": {"role": "user", "content": [{"type": "text", "text": "a"}]}}));
    }

    async fn updates(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(300), self.update_rx.recv()).await {
                Ok(Some(SessionOutbound::Update(notif))) => {
                    out.push(serde_json::to_value(&notif.update).unwrap());
                }
                Ok(Some(SessionOutbound::RawUpdate(value))) => out.push(value["update"].clone()),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        out
    }
}

#[tokio::test]
async fn t5a_cancel_interrupts_the_compaction_and_drops_late_frames() {
    let mut rig = ActorRig::spawn();
    rig.send(status(json!("compacting")));
    let opened = rig.updates().await;
    assert_eq!(kinds(&opened), vec!["compaction_update"]);
    assert_eq!(opened[0]["status"], "in_progress");

    rig.tx.send(Command::Cancel).unwrap();
    let cancelled = rig.updates().await;
    let cancelled = cancelled
        .iter()
        .find(|u| u["sessionUpdate"] == "compaction_update")
        .expect("the cancel closes the entity");
    assert_eq!(cancelled["compactionId"], "c1");
    assert_eq!(cancelled["status"], "cancelled");

    // The abandoned work's tail is ignored until a new turn begins. (The
    // boundary still refreshes the displayed usage, as upstream's does; no
    // compaction update or chunk may appear.)
    rig.send(status(json!("compacting")));
    rig.send(
        json!({"type": "stream_event", "uuid": "st", "parent_tool_use_id": null,
        "event": {"type": "content_block_delta", "delta": {"type": "compaction_delta",
            "content": "late chunk"}}}),
    );
    rig.send(
        json!({"type": "system", "subtype": "compact_boundary", "uuid": "late",
        "compact_metadata": {"trigger": "manual", "pre_tokens": 100}}),
    );
    rig.send(
        json!({"type": "system", "subtype": "status", "uuid": "late2",
        "compact_result": "success"}),
    );
    let late = rig.updates().await;
    assert!(
        late.iter().all(|u| matches!(
            u["sessionUpdate"].as_str(),
            Some("usage_update" | "session_state_changed" | "available_commands_update")
        )),
        "no compaction update survives the interrupt: {late:?}"
    );

    // A live turn's echo proves the tail drained: compaction is accepted again.
    let (reply_tx, _reply_rx) = oneshot::channel();
    rig.tx
        .send(Command::Prompt {
            uuid: "p2".into(),
            frame: json!({"type": "user", "uuid": "p2", "message": {}}),
            is_local_only: false,
            prompt_text: String::new(),
            reply: reply_tx,
        })
        .unwrap();
    rig.echo("p2");
    rig.updates().await;
    rig.send(json!({"type": "system", "subtype": "status", "uuid": "c2",
        "status": "compacting"}));
    let reopened = rig.updates().await;
    assert_eq!(kinds(&reopened), vec!["compaction_update"]);
    assert_eq!(reopened[0]["compactionId"], "c2");
    assert_eq!(reopened[0]["status"], "in_progress");

    rig.stream_tx = None;
    let _ = tokio::time::timeout(Duration::from_secs(10), &mut rig.actor).await;
}

// ---------------------------------------------------------------- P-60 (runtime)

use crate::acp_subagents::SubagentForm;
use crate::native_subagents::{AwaitToken, NativeSubagentRuntime, Out, TaskStarted};

fn rfd_launch(tool: &str, input: Value, parent: Option<&str>) -> Value {
    let mut claude = json!({"toolName": "Agent"});
    if let Some(parent) = parent {
        claude["parentToolUseId"] = json!(parent);
    }
    json!({"sessionId": "root", "update": {
        "sessionUpdate": "tool_call", "toolCallId": tool, "title": "Agent",
        "status": "pending", "rawInput": input, "_meta": {"claudeCode": claude}}})
}

struct RfdWorker {
    rt: NativeSubagentRuntime,
    published: Vec<Value>,
}

impl RfdWorker {
    fn new() -> Self {
        let mut rt = NativeSubagentRuntime::new(Some(SubagentForm::Rfd), "root");
        let _ = rt.route(
            &rfd_launch(
                "launch-1",
                json!({"description": "Fix the build", "prompt": "Make it pass"}),
                None,
            ),
            None,
        );
        let published = rt.task_started(&TaskStarted {
            task_id: "worker-1".into(),
            tool_use_id: Some("launch-1".into()),
            subagent_type: Some("Explore".into()),
            description: Some("Fix the build".into()),
            prompt: Some("Make it pass".into()),
        });
        Self {
            rt,
            published: published
                .into_iter()
                .filter_map(|out| match out {
                    Out::Publish(v) => Some(v),
                    _ => None,
                })
                .collect(),
        }
    }

    fn apply(&mut self, outs: Vec<Out>) {
        self.published
            .extend(outs.into_iter().filter_map(|out| match out {
                Out::Publish(v) => Some(v),
                _ => None,
            }));
    }

    fn begin(&mut self, session: &str) -> Option<AwaitToken> {
        let (token, outs) = self.rt.awaiting_user_begin(session);
        self.apply(outs);
        token
    }

    fn end(&mut self, token: AwaitToken, aborted: bool) {
        let outs = self.rt.awaiting_user_end(token, aborted);
        self.apply(outs);
    }

    fn parent_cancelled(&mut self) {
        let outs = self.rt.parent_cancelled();
        self.apply(outs);
    }

    fn finish(&mut self, task: &str, status: &str) {
        let outs = self.rt.finish_task(task, status, None, None);
        self.apply(outs);
    }

    fn started(&mut self, task: TaskStarted) {
        let outs = self.rt.task_started(&task);
        self.apply(outs);
    }

    fn states(&self) -> Vec<Value> {
        self.published
            .iter()
            .filter(|n| n["update"]["sessionUpdate"] == "subagent_update")
            .map(|n| n["update"]["state"].clone())
            .collect()
    }
}

#[test]
fn t5a_open_requests_keep_the_child_requires_action() {
    let mut worker = RfdWorker::new();
    let first = worker.begin("worker-1");
    let second = worker.begin("worker-1");
    assert_eq!(
        worker.states().last(),
        Some(&json!({"state": "requires_action"}))
    );
    worker.end(first.unwrap(), false);
    assert_eq!(
        worker.states().last(),
        Some(&json!({"state": "requires_action"}))
    );
    worker.end(second.unwrap(), false);
    assert_eq!(
        worker.states(),
        vec![
            json!({"state": "running"}),
            json!({"state": "requires_action"}),
            json!({"state": "running"}),
        ]
    );
    // The root session's own request changes no child.
    let token = worker.begin("root");
    assert!(token.is_none());
    assert_eq!(worker.states().len(), 3);
}

#[test]
fn t5a_child_ended_while_it_waited_does_not_run_again() {
    let mut worker = RfdWorker::new();
    let token = worker.begin("worker-1");
    worker.finish("worker-1", "completed");
    worker.end(token.unwrap(), false);
    assert_eq!(
        worker.states().last(),
        Some(&json!({"state": "idle", "stopReason": "end_turn"}))
    );
}

#[test]
fn t5a_child_request_withdrawn_by_a_cancel_then_ended_by_the_sdk() {
    let mut worker = RfdWorker::new();
    let token = worker.begin("worker-1");
    assert_eq!(
        worker.rt.cancel_child("worker-1").as_deref(),
        Some("worker-1")
    );
    worker.end(token.unwrap(), true);
    assert_eq!(
        worker.states(),
        vec![
            json!({"state": "running"}),
            json!({"state": "requires_action"}),
        ]
    );
    worker.finish("worker-1", "killed");
    assert_eq!(
        worker.states().last(),
        Some(&json!({"state": "idle", "stopReason": "cancelled"}))
    );
}

#[test]
fn t5a_parent_cancel_withdrawal_reports_no_running() {
    let mut worker = RfdWorker::new();
    let token = worker.begin("worker-1");
    worker.parent_cancelled();
    worker.end(token.unwrap(), true);
    assert_eq!(
        worker.states(),
        vec![
            json!({"state": "running"}),
            json!({"state": "requires_action"}),
        ]
    );
    // The SDK's own task event reports the end of the stopped child.
    worker.finish("worker-1", "killed");
    assert_eq!(
        worker.states().last(),
        Some(&json!({"state": "idle", "stopReason": "cancelled"}))
    );
}

#[test]
fn t5a_parent_cancel_keeps_the_open_request_and_runs_once_answered() {
    let mut worker = RfdWorker::new();
    let token = worker.begin("worker-1");
    worker.parent_cancelled();
    assert_eq!(
        worker.states(),
        vec![
            json!({"state": "running"}),
            json!({"state": "requires_action"}),
        ]
    );
    worker.end(token.unwrap(), false);
    assert_eq!(worker.states().last(), Some(&json!({"state": "running"})));
}

#[test]
fn t5a_cancel_before_the_request_opened_is_not_counted() {
    let mut worker = RfdWorker::new();
    worker.parent_cancelled();
    let token = worker.begin("worker-1");
    worker.end(token.unwrap(), true);
    assert_eq!(worker.states().last(), Some(&json!({"state": "running"})));
}

#[test]
fn t5a_parent_cancel_of_a_foreground_subagent_does_not_report_running() {
    let mut worker = RfdWorker::new();
    let _ = worker.rt.route(
        &rfd_launch(
            "launch-2",
            json!({"description": "Dig", "prompt": "Look around"}),
            Some("launch-1"),
        ),
        None,
    );
    worker.started(TaskStarted {
        task_id: "digger-1".into(),
        tool_use_id: Some("launch-2".into()),
        subagent_type: Some("Explore".into()),
        description: Some("Dig".into()),
        prompt: Some("Look around".into()),
    });
    let token = worker.begin("digger-1");
    assert_eq!(
        worker.states().last(),
        Some(&json!({"state": "requires_action"}))
    );
    // The cancel of worker-1 also stops the subagent it waits on.
    assert_eq!(
        worker.rt.cancel_child("worker-1").as_deref(),
        Some("worker-1")
    );
    worker.end(token.unwrap(), true);
    assert_eq!(
        worker.states().last(),
        Some(&json!({"state": "requires_action"}))
    );
}

#[test]
fn t5a_air_grants_no_child_cancel_and_no_request_state() {
    let mut rt = NativeSubagentRuntime::new(Some(SubagentForm::Air), "root");
    let _ = rt.route(
        &json!({"sessionId": "root", "update": {
            "sessionUpdate": "tool_call", "toolCallId": "agent-tool", "title": "Agent",
            "status": "pending", "_meta": {"claudeCode": {"toolName": "Agent"}}}}),
        None,
    );
    let _ = rt.task_started(&TaskStarted {
        task_id: "child-1".into(),
        tool_use_id: Some("agent-tool".into()),
        subagent_type: Some("Explore".into()),
        ..TaskStarted::default()
    });
    assert_eq!(rt.cancel_child("child-1"), None);
    let (token, outs) = rt.awaiting_user_begin("child-1");
    assert!(token.is_none());
    assert!(outs.is_empty());
    let _ = rt.finish_task("child-1", "completed", None, None);
    assert_eq!(rt.form(), Some(SubagentForm::Air));
}
