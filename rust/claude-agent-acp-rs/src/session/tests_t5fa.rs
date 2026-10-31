//! T5FA — the F20 (B-109) compaction lifecycle fix round: `resume()` on a
//! `command_lifecycle` "started", `interrupt()` on every abandon lane (stream
//! end, `conversation_reset`, auth-required / error results, the #825 idle),
//! the clear-context restart's fresh lifecycle, the heartbeat's position ahead
//! of the stopped-work gate, the AIR payload key order, the PostCompact
//! `agent_id` truthiness and the summary-regex edge.
//!
//! The frame tests drive `handle_session_frame` with the state a running actor
//! would hold; the actor tests drive the real `run_loop`.

use super::tests_t2a::Rig;
use super::*;
use crate::context_compaction::{CompactionPresentation, ContextCompactionLifecycle};

fn compaction_rig(presentation: CompactionPresentation, air_client: bool) -> Rig {
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

fn kinds(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .map(|u| u["sessionUpdate"].as_str().unwrap_or("").to_string())
        .collect()
}

fn status(uuid: &str, value: Value) -> Value {
    json!({"type": "system", "subtype": "status", "uuid": uuid, "status": value})
}

fn compact_result(uuid: &str, value: &str) -> Value {
    json!({"type": "system", "subtype": "status", "uuid": uuid, "compact_result": value})
}

fn boundary(uuid: &str) -> Value {
    json!({"type": "system", "subtype": "compact_boundary", "uuid": uuid,
        "compact_metadata": {"trigger": "auto", "pre_tokens": 50000, "post_tokens": 12000,
            "duration_ms": 7}})
}

fn idle() -> Value {
    json!({"type": "system", "subtype": "session_state_changed", "state": "idle"})
}

fn compaction_delta(uuid: &str, text: &str) -> Value {
    json!({"type": "stream_event", "uuid": uuid, "parent_tool_use_id": null,
        "event": {"type": "content_block_delta", "index": 0,
            "delta": {"type": "compaction_delta", "content": text}}})
}

fn result_frame(text: &str, is_error: bool) -> Value {
    json!({"type": "result", "subtype": "success", "is_error": is_error,
        "stop_reason": "end_turn", "result": text,
        "usage": {"input_tokens": 10, "output_tokens": 5}})
}

/// The compaction update the lifecycle sent, if any.
fn compaction_update(updates: &[Value]) -> Option<Value> {
    updates
        .iter()
        .find(|u| u["sessionUpdate"] == "compaction_update")
        .cloned()
}

// ---------------------------------------------------------------- F1

/// A real actor: one session, one duplex control channel.
struct ActorRig {
    tx: mpsc::UnboundedSender<Command>,
    stream_tx: Option<mpsc::UnboundedSender<Value>>,
    update_rx: mpsc::UnboundedReceiver<SessionOutbound>,
    actor: tokio::task::JoinHandle<()>,
    _writer: tokio::io::DuplexStream,
}

impl ActorRig {
    fn spawn(compaction: bool) -> Self {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(a, control_in_rx, ControlOptions { on_request: None });
        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
        let caps = crate::client_caps::ClientCapabilities {
            compaction,
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
            Vec::new(),
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

    fn prompt(&self, uuid: &str, is_local_only: bool) {
        let (reply_tx, _reply_rx) = oneshot::channel();
        self.tx
            .send(Command::Prompt {
                uuid: uuid.into(),
                frame: json!({"type": "user", "uuid": uuid, "message": {}}),
                is_local_only,
                prompt_text: String::new(),
                reply: reply_tx,
            })
            .unwrap();
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

    async fn shutdown(mut self) {
        self.stream_tx = None;
        let _ = tokio::time::timeout(Duration::from_secs(10), &mut self.actor).await;
    }
}

/// After a cancel the NEXT `/compact` reports its lifecycle again: the
/// `command_lifecycle` "started" frame of its (echo-less) turn resumes the
/// interrupted lifecycle (`acp-agent.ts:5846-5848`), so its `in_progress`, its
/// streamed summary chunks and its `completed` reach the client.
#[tokio::test]
async fn t5fa_command_lifecycle_started_resumes_after_cancel() {
    let mut rig = ActorRig::spawn(true);
    rig.send(status("c1", json!("compacting")));
    let opened = rig.updates().await;
    assert_eq!(kinds(&opened), vec!["compaction_update"]);
    assert_eq!(opened[0]["status"], "in_progress");

    rig.tx.send(Command::Cancel).unwrap();
    let cancelled = rig.updates().await;
    let cancelled = compaction_update(&cancelled).expect("the cancel closes the entity");
    assert_eq!(cancelled["compactionId"], "c1");
    assert_eq!(cancelled["status"], "cancelled");

    // The next `/compact` is echo-less: its turn is queued, and its dispatch is
    // the only signal that the abandoned tail drained.
    rig.prompt("p2", true);
    rig.updates().await;
    rig.send(json!({"type": "command_lifecycle", "command_uuid": "p2", "state": "started"}));
    rig.send(status("c2", json!("compacting")));
    let reopened = rig.updates().await;
    let reopened = compaction_update(&reopened).expect("started resumes the lifecycle");
    assert_eq!(reopened["compactionId"], "c2");
    assert_eq!(reopened["status"], "in_progress");

    // The block's text still streams into the announced entity, although the
    // cancel's stopped-work window is open (the heartbeat runs ahead of it).
    rig.send(compaction_delta("c2", "## Retained\n"));
    rig.send(compaction_delta("c2", "The user counted."));
    let chunks = rig.updates().await;
    assert_eq!(
        kinds(&chunks),
        vec!["compaction_summary_chunk", "compaction_summary_chunk"],
        "the summary chunks reach the resumed entity: {chunks:?}"
    );
    assert_eq!(chunks[0]["compactionId"], "c2");
    assert_eq!(chunks[1]["content"]["text"], "The user counted.");

    rig.send(boundary("c2"));
    let completed = rig.updates().await;
    let completed = compaction_update(&completed).expect("the boundary ends the compaction");
    assert_eq!(completed["compactionId"], "c2");
    assert_eq!(completed["status"], "completed");
    assert!(
        completed.get("_meta").is_none(),
        "a non-AIR client gets no _meta"
    );
    assert!(
        completed.get("summary").is_none(),
        "no hook summary was recorded"
    );

    rig.shutdown().await;
}

// ---------------------------------------------------------------- F2 (stream end)

/// The CLI dies mid-compaction: the entity ends `cancelled` (never stays
/// `in_progress`) before any turn settles (`finishLifecycle` ->
/// `compaction.interrupt()`, `acp-agent.ts:4645`).
#[tokio::test]
async fn t5fa_stream_end_closes_an_open_entity_as_cancelled() {
    let mut rig = ActorRig::spawn(true);
    rig.send(status("cmp-eof", json!("compacting")));
    let opened = rig.updates().await;
    assert_eq!(opened[0]["status"], "in_progress");

    // The stream ends with the entity open.
    rig.stream_tx = None;
    let after = rig.updates().await;
    let cancelled = compaction_update(&after).expect("the stream end closes the entity");
    assert_eq!(cancelled["compactionId"], "cmp-eof");
    assert_eq!(cancelled["status"], "cancelled");

    let _ = tokio::time::timeout(Duration::from_secs(10), &mut rig.actor).await;
}

// ---------------------------------------------------------------- F2 (conversation_reset)

/// `conversation_reset` closes the old conversation's entity `cancelled` and
/// drops its pending hook summary before the reset clears the rest
/// (`acp-agent.ts:8191-8195`).
#[test]
fn t5fa_conversation_reset_closes_the_entity_and_drops_the_pending_summary() {
    let mut rig = compaction_rig(CompactionPresentation::CompactionUpdate, false);
    rig.feed(status("c1", json!("compacting")));
    assert_eq!(kinds(&rig.updates()), vec!["compaction_update"]);

    rig.feed(json!({"type": "conversation_reset",
        "new_conversation_id": "fresh", "uuid": "reset"}));
    let updates = rig.updates();
    let cancelled = compaction_update(&updates).expect("the reset closes the entity");
    assert_eq!(cancelled["compactionId"], "c1");
    assert_eq!(cancelled["status"], "cancelled");

    // A summary recorded while interrupted is dropped: the next compaction is
    // a fresh entity with no stale summary.
    let (reply, _reply_rx) = oneshot::channel();
    handle_hook_callback(
        &mut AsyncTaskRuntime::new(false),
        HookCall {
            request_id: "r",
            callback_id: crate::hooks::POST_COMPACT_HOOK_ID,
            input: &json!({"compact_summary": "<summary>\nstale summary\n</summary>"}),
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
    assert!(!rig.ext.t2a.compaction.record_summary(Some(&json!("stale"))));

    // A new turn's echo proves the reset's abandoned tail drained and resumes
    // the lifecycle (`activateTurn` -> `compaction.resume()`).
    rig.echo("p2");
    rig.updates();
    rig.feed(status("c2", json!("compacting")));
    rig.feed(compact_result("c2", "success"));
    let updates = rig.updates();
    let completed = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "compaction_update" && u["status"] == "completed")
        .expect("the fresh compaction completes");
    assert!(
        completed.get("summary").is_none(),
        "the stale pending summary must not carry: {completed}"
    );
}

// ---------------------------------------------------------------- F2 (auth required)

/// An error result whose text is the CLI's sign-in prompt fails the turn
/// `auth_required`; `failActiveWithSessionFailure` interrupts the compaction
/// first (`acp-agent.ts:5565`), so the entity is closed `cancelled` before the
/// prompt response.
#[test]
fn t5fa_auth_required_result_interrupts_the_compaction() {
    let mut rig = compaction_rig(CompactionPresentation::CompactionUpdate, false);
    rig.echo("p1");
    rig.updates();
    rig.feed(status("c1", json!("compacting")));
    assert_eq!(kinds(&rig.updates()), vec!["compaction_update"]);

    rig.feed(result_frame("Please run /login", true));
    let updates = rig.updates();
    let cancelled = compaction_update(&updates).expect("the failure closes the entity");
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["compactionId"], "c1");
    let mut reply = rig.replies.remove("p1").expect("the prompt was echoed");
    assert!(
        matches!(reply.try_recv(), Ok(Err(_))),
        "the auth failure rejects the prompt"
    );
}

/// A provider-error result goes through the same failure lane. The interrupt
/// also drops a summary that waits for the next compaction, and the latch
/// ignores a `compacting` status until the next turn resumes the lifecycle.
#[test]
fn t5fa_error_result_interrupts_the_compaction() {
    let mut rig = compaction_rig(CompactionPresentation::CompactionUpdate, false);
    rig.echo("p1");
    rig.updates();
    rig.feed(status("c1", json!("compacting")));
    rig.updates();
    rig.feed(result_frame("provider exploded", true));
    let updates = rig.updates();
    let cancelled = compaction_update(&updates).expect("the failure closes the entity");
    assert_eq!(cancelled["status"], "cancelled");
    let mut reply = rig.replies.remove("p1").expect("the prompt was echoed");
    assert!(matches!(reply.try_recv(), Ok(Err(_))));

    // M8: `failActiveWithSessionFailure` interrupts (not resets,
    // `acp-agent.ts:5565`), so a PostCompact summary recorded after the failure
    // waits for nothing: the latch drops it.
    post_compact(
        &mut rig,
        json!({"compact_summary": "<summary>\nstale\n</summary>"}),
    );
    assert!(
        !rig.ext.t2a.compaction.record_summary(Some(&json!("stale"))),
        "an interrupted lifecycle records no summary"
    );
    // A `compacting` status before any resume is ignored (a reset would open it).
    rig.feed(status("c-late", json!("compacting")));
    assert!(
        rig.updates()
            .iter()
            .all(|u| u["sessionUpdate"] != "compaction_update"),
        "a status before the resume is ignored"
    );
    // The next turn's echo resumes the lifecycle; its compaction carries no
    // stale summary.
    rig.echo("p2");
    rig.updates();
    rig.feed(status("c2", json!("compacting")));
    rig.feed(compact_result("c2", "success"));
    let updates = rig.updates();
    let completed = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "compaction_update" && u["status"] == "completed")
        .expect("the fresh compaction completes");
    assert!(
        completed.get("summary").is_none(),
        "the dropped summary must not carry: {completed}"
    );
}

/// An `end_turn` result that leaves a dispatched tool call unfinished becomes
/// `IncompleteToolCall` in the settle gate — upstream's
/// `failActiveWithSessionFailure`, which interrupts (`acp-agent.ts:5469`,
/// `5565`): a pending PostCompact summary is dropped and late compaction frames
/// are ignored until the next turn resumes the lifecycle.
#[test]
fn t5fa3_an_incomplete_tool_call_result_interrupts_the_compaction() {
    let mut rig = compaction_rig(CompactionPresentation::CompactionUpdate, false);
    rig.echo("p1");
    rig.tool_use("toolu_STUCK", "Bash", json!({"command": "sleep 5"}));
    rig.updates();
    // No entity is open, so the summary waits for the next compaction.
    post_compact(
        &mut rig,
        json!({"compact_summary": "<summary>\nstale\n</summary>"}),
    );
    rig.feed(result_frame("done", false));
    let updates = rig.updates();
    assert!(
        updates
            .iter()
            .all(|u| u["sessionUpdate"] != "compaction_update"),
        "no entity was open to close: {updates:?}"
    );
    let mut reply = rig.replies.remove("p1").expect("the prompt was echoed");
    assert!(
        matches!(reply.try_recv(), Ok(Err(_))),
        "the unfinished tool call fails the prompt"
    );
    // The latch is set: a late `compacting` status is ignored.
    rig.feed(status("c-late", json!("compacting")));
    assert!(
        rig.updates()
            .iter()
            .all(|u| u["sessionUpdate"] != "compaction_update"),
        "late compaction frames are ignored"
    );
    // The next turn's echo resumes the lifecycle; its compaction has no summary.
    rig.echo("p2");
    rig.updates();
    rig.feed(status("c2", json!("compacting")));
    rig.feed(compact_result("c2", "success"));
    let updates = rig.updates();
    let completed = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "compaction_update" && u["status"] == "completed")
        .expect("the fresh compaction completes");
    assert!(
        completed.get("summary").is_none(),
        "the pending summary must not carry: {completed}"
    );
}

/// The CLI's synthetic "Please run /login" assistant message fails the turn
/// `auth_required` from the assistant lane (`acp-agent.ts:7968-7975`), and
/// `failActiveWithSessionFailure` interrupts the open entity first
/// (`acp-agent.ts:5565`).
#[test]
fn t5fa3_an_assistant_auth_message_interrupts_the_compaction() {
    let mut rig = compaction_rig(CompactionPresentation::CompactionUpdate, false);
    rig.echo("p1");
    rig.updates();
    rig.feed(status("c1", json!("compacting")));
    assert_eq!(kinds(&rig.updates()), vec!["compaction_update"]);

    // A nested synthetic login frame: the S-06 replay gate only drops
    // top-level ones, so this one reaches the auth lane with the entity open.
    rig.feed(
        json!({"type": "assistant", "parent_tool_use_id": "toolu_parent",
        "message": {"id": "msg_auth", "model": "<synthetic>", "role": "assistant",
            "content": [{"type": "text", "text": "Please run /login"}]}}),
    );
    let updates = rig.updates();
    let cancelled = compaction_update(&updates).expect("the auth message closes the entity");
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["compactionId"], "c1");
    let mut reply = rig.replies.remove("p1").expect("the prompt was echoed");
    assert!(
        matches!(reply.try_recv(), Ok(Err(_))),
        "the auth failure rejects the prompt"
    );
}

// ---------------------------------------------------------------- F2 (#825 idle) + F7

/// The #825 idle (no result for the active turn) fails the turn through
/// `failActiveWithSessionFailure`, which interrupts the compaction.
#[test]
fn t5fa_no_result_idle_interrupts_the_compaction() {
    let mut rig = compaction_rig(CompactionPresentation::CompactionUpdate, false);
    rig.echo("p1");
    rig.updates();
    rig.feed(status("c1", json!("compacting")));
    rig.updates();
    rig.feed(idle());
    let updates = rig.updates();
    let cancelled = compaction_update(&updates).expect("the #825 fail closes the entity");
    assert_eq!(cancelled["status"], "cancelled");
    let mut reply = rig.replies.remove("p1").expect("the prompt was echoed");
    assert!(matches!(reply.try_recv(), Ok(Err(_))));
}

/// F7: an idle that merely SETTLES a held turn is not a compaction boundary.
/// The old code reset on every `Settled` at idle, emitting an extra `cancelled`
/// for an entity the runtime still owns (`compaction.reset()` runs at the
/// result only, `acp-agent.ts:7478`).
#[test]
fn t5fa_idle_settling_a_held_turn_does_not_reset_the_compaction() {
    let mut rig = compaction_rig(CompactionPresentation::CompactionUpdate, false);
    rig.echo("p1");
    rig.tool_use(
        "toolu_AG",
        "Agent",
        json!({"description": "Research", "prompt": "x"}),
    );
    rig.feed(
        json!({"type": "system", "subtype": "task_started", "task_id": "sub1",
        "tool_use_id": "toolu_AG", "description": "Research",
        "subagent_type": "general-purpose", "task_type": "local_agent",
        "is_backgrounded": true}),
    );
    rig.feed(result_frame("spawned", false));
    rig.feed(idle());
    rig.updates();
    assert!(rig.machine.has_active(), "p1 is held for sub1");

    // The followup cycle's compaction is open while the turn is held.
    rig.feed(status("c1", json!("compacting")));
    assert_eq!(kinds(&rig.updates()), vec!["compaction_update"]);

    // The subagent ends; the next idle settles the held turn.
    rig.feed(
        json!({"type": "system", "subtype": "task_updated", "task_id": "sub1",
        "patch": {"status": "completed"}}),
    );
    rig.feed(idle());
    let settled = rig.updates();
    assert!(
        settled
            .iter()
            .all(|u| u["sessionUpdate"] != "compaction_update"),
        "an idle that settles a held turn must not reset the entity: {settled:?}"
    );
    assert!(
        rig.replies
            .remove("p1")
            .is_some_and(|mut reply| reply.try_recv().is_ok()),
        "the held turn settled at the idle"
    );

    // The entity is still open and ends normally.
    rig.feed(compact_result("c1", "success"));
    let completed = rig.updates();
    let completed = compaction_update(&completed).expect("the entity was still open");
    assert_eq!(completed["status"], "completed");
}

// ---------------------------------------------------------------- F3

/// A clear-context restart replaces the lifecycle with a brand-new one (like
/// the fresh consumer upstream builds): the old pending hook summary does not
/// carry, and compaction frames are accepted again.
#[test]
fn t5fa_fresh_context_starts_a_new_lifecycle() {
    let mut rig = compaction_rig(CompactionPresentation::CompactionUpdate, false);
    // A pending summary of the old context (recorded while the entity was
    // already terminal, so it waits for the next compaction).
    assert!(rig
        .ext
        .t2a
        .compaction
        .record_summary(Some(&json!("stale summary"))));
    // The caller publishes the old context's terminal, then swaps the lifecycle.
    let cancelled = rig.ext.t2a.compaction.interrupt();
    assert!(cancelled.is_empty(), "no entity was open");
    reset_ext_for_fresh_context(&mut rig.ext);

    assert!(
        !rig.ext.t2a.compaction.has_delivered_output(),
        "the fresh lifecycle starts with no delivered output"
    );
    let opened = rig.ext.t2a.compaction.start("c2");
    assert_eq!(opened[0]["status"], "in_progress");
    assert_eq!(opened[0]["compactionId"], "c2");
    let completed = rig.ext.t2a.compaction.finish(
        "c2",
        crate::context_compaction::CompactionStatus::Completed,
        &crate::context_compaction_meta::CompactionFacts::default(),
        false,
    );
    assert_eq!(completed[0]["status"], "completed");
    assert!(
        completed[0].get("summary").is_none(),
        "the old context's pending summary must not carry: {}",
        completed[0]
    );
}

/// The restart publishes the open entity's `cancelled` terminal before the
/// fresh context starts (the old consumer's result `finally`,
/// `acp-agent.ts:7488`).
#[test]
fn t5fa_fresh_context_publishes_the_old_entity_as_cancelled() {
    let mut rig = compaction_rig(CompactionPresentation::CompactionUpdate, false);
    rig.ext.t2a.compaction.start("c1");
    let cancelled = rig.ext.t2a.compaction.interrupt();
    assert_eq!(cancelled[0]["compactionId"], "c1");
    assert_eq!(cancelled[0]["status"], "cancelled");
    reset_ext_for_fresh_context(&mut rig.ext);
    // The fresh lifecycle opens the same id as a new entity.
    let opened = rig.ext.t2a.compaction.start("c1");
    assert_eq!(opened[0]["status"], "in_progress");
}

// ---------------------------------------------------------------- F4

/// A legacy client that cancels mid-compaction still gets the single
/// `in_progress` re-assert: the heartbeat runs before the stopped-work gate
/// (`acp-agent.ts:7493` vs `7612`).
#[tokio::test]
async fn t5fa_legacy_heartbeat_runs_before_the_stopped_work_gate() {
    let mut rig = ActorRig::spawn(false);
    rig.prompt("p1", false);
    rig.send(json!({"type": "user", "uuid": "p1", "isReplay": true,
        "message": {"role": "user", "content": [{"type": "text", "text": "hi"}]}}));
    rig.updates().await;
    rig.send(status("c1", json!("compacting")));
    let opened = rig.updates().await;
    assert_eq!(kinds(&opened), vec!["tool_call"]);
    assert_eq!(opened[0]["status"], "in_progress");

    rig.tx.send(Command::Cancel).unwrap();
    rig.updates().await;
    // The cancelled turn settles at the idle; only then has the client been
    // told the work ended and does the gate drop its output
    // (`dropsStoppedWorkOutput`).
    rig.send(idle());
    rig.updates().await;

    // The cancel's stopped-work window is open; the block's heartbeat still
    // re-asserts the tool call (once).
    rig.send(compaction_delta("c1", "## Retained\n"));
    let beat = rig.updates().await;
    // The cancel's `interrupt()` reset the legacy call (ACP has no cancelled
    // tool status); the heartbeat re-opens it and re-asserts `in_progress`,
    // where the old order dropped the delta at the stopped-work gate.
    assert_eq!(
        kinds(&beat),
        vec!["tool_call", "tool_call_update"],
        "the heartbeat precedes the gate: {beat:?}"
    );
    assert_eq!(beat[0]["toolCallId"], "c1");
    assert_eq!(beat[1]["status"], "in_progress");

    // A plain text delta of the stopped work is still dropped by the gate.
    rig.send(
        json!({"type": "stream_event", "uuid": "st", "parent_tool_use_id": null,
        "event": {"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "late text"}}}),
    );
    let gated = rig.updates().await;
    assert!(
        gated
            .iter()
            .all(|u| u["sessionUpdate"] != "agent_message_chunk"),
        "the stopped work's text is still dropped: {gated:?}"
    );

    rig.shutdown().await;
}

// ---------------------------------------------------------------- F5

/// The wire bytes of the compaction updates: `version` is FIRST in the AIR
/// payload, and the update keys keep Node's insertion order
/// (`context-compaction-meta.ts:31-36`).
#[test]
fn t5fa_compaction_update_wire_key_order() {
    let mut rig = compaction_rig(CompactionPresentation::CompactionUpdate, true);
    rig.feed(status("c1", json!("compacting")));
    let updates = rig.updates();
    assert_eq!(
        serde_json::to_string(&updates[0]).unwrap(),
        r#"{"sessionUpdate":"compaction_update","compactionId":"c1","status":"in_progress","_meta":{"jetbrains":{"air":{"version":1,"contextCompaction":{"version":1}}}}}"#
    );

    rig.feed(compaction_delta("c1", "## Retained\n"));
    let chunks = rig.updates();
    assert_eq!(
        serde_json::to_string(&chunks[0]).unwrap(),
        r###"{"sessionUpdate":"compaction_summary_chunk","compactionId":"c1","content":{"type":"text","text":"## Retained\n"}}"###
    );

    // The terminal carries the hook summary (the chunk text differs after
    // cleaning) and the boundary's facts, with `version` first.
    let (reply, _reply_rx) = oneshot::channel();
    handle_hook_callback(
        &mut AsyncTaskRuntime::new(false),
        HookCall {
            request_id: "r",
            callback_id: crate::hooks::POST_COMPACT_HOOK_ID,
            input: &json!({"compact_summary": "<analysis>\nThinking.\n</analysis>\n<summary>\nRetained.\n</summary>"}),
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
    rig.feed(boundary("c1"));
    let terminal = rig.updates();
    assert_eq!(
        serde_json::to_string(&terminal[0]).unwrap(),
        r#"{"sessionUpdate":"compaction_update","compactionId":"c1","status":"completed","summary":[{"type":"text","text":"Retained."}],"_meta":{"jetbrains":{"air":{"version":1,"contextCompaction":{"version":1,"trigger":"automatic","preTokens":50000,"postTokens":12000,"durationMs":7}}}}}"#
    );
}

// ---------------------------------------------------------------- F8

fn post_compact(rig: &mut Rig, input: Value) {
    let (reply, _reply_rx) = oneshot::channel();
    handle_hook_callback(
        &mut AsyncTaskRuntime::new(false),
        HookCall {
            request_id: "r",
            callback_id: crate::hooks::POST_COMPACT_HOOK_ID,
            input: &input,
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
}

/// `!input.agent_id` is JS truthiness: a null (or empty, or false) agent_id is
/// a ROOT compaction and records its summary; only a truthy one is a subagent's.
#[test]
fn t5fa_post_compact_records_a_summary_for_a_falsy_agent_id() {
    for agent_id in [json!(null), json!(""), json!(false), json!(0)] {
        let mut rig = compaction_rig(CompactionPresentation::CompactionUpdate, false);
        post_compact(
            &mut rig,
            json!({"agent_id": agent_id, "compact_summary": "<summary>\nkept\n</summary>"}),
        );
        assert_eq!(
            rig.ext.last_compaction_summary.as_deref(),
            Some("<summary>\nkept\n</summary>"),
            "agent_id {agent_id} is falsy"
        );
    }
    let mut rig = compaction_rig(CompactionPresentation::CompactionUpdate, false);
    post_compact(
        &mut rig,
        json!({"agent_id": "subagent-1", "compact_summary": "<summary>\nsub\n</summary>"}),
    );
    assert!(
        rig.ext.last_compaction_summary.is_none(),
        "a subagent compaction stays out of the root session"
    );
    // No agent_id at all is the root too.
    let mut rig = compaction_rig(CompactionPresentation::CompactionUpdate, false);
    post_compact(
        &mut rig,
        json!({"compact_summary": "<summary>\nroot\n</summary>"}),
    );
    assert_eq!(
        rig.ext.last_compaction_summary.as_deref(),
        Some("<summary>\nroot\n</summary>")
    );
}

// ---------------------------------------------------------------- F9

/// The own-line analysis close cannot reuse the open line's newline: JS consumed
/// it in the open match (`ANALYSIS_BLOCKS[0]`), so `<analysis>\n</analysis>` is
/// not an own-line block.
#[test]
fn t5fa_analysis_close_does_not_reuse_the_open_line_newline() {
    assert_eq!(
        crate::context_compaction::compaction_summary_text("x\n<analysis>\n</analysis>").as_deref(),
        Some("x\n<analysis>\n</analysis>")
    );
    // A real own-line block (the close on its own later line) is still stripped.
    assert_eq!(
        crate::context_compaction::compaction_summary_text(
            "x\n<analysis>\nreasoning\n</analysis>\nkept"
        )
        .as_deref(),
        Some("x\n\nkept")
    );
}
