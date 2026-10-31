//! T7A (CONC-2, B-17/B-63): the message-loop yielder in the session actor's
//! stream branch, and the backlog races of cancel and teardown.
//!
//! Upstream `messageLoopYielder.maybeYield()` (`acp-agent.ts:5651-5660`,
//! `utils.ts:114-145`) gives the event loop a turn when a stretch of buffered
//! messages ran past its budget, so `session/cancel` is never starved behind a
//! long backlog. The actor yields in its stream branch; these tests drive a
//! real 5,000-frame backlog through the actor and prove the pause is what lets
//! another task run mid-drain.

use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

fn user_frame(uuid: &str) -> Value {
    json!({"type":"user","uuid":uuid,"message":{"role":"user",
        "content":[{"type":"text","text":"hi"}]}})
}

fn delta(i: usize) -> Value {
    json!({
        "type": "stream_event",
        "event": {
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": format!("token_{i} ")}
        }
    })
}

/// A live actor with its stream channel exposed, so the test can queue a
/// backlog before the actor reads any of it.
fn spawn_actor() -> (
    mpsc::UnboundedSender<Command>,
    mpsc::UnboundedReceiver<SessionOutbound>,
    tokio::io::DuplexStream,
    mpsc::UnboundedSender<Value>,
) {
    let (writer, far_end) = tokio::io::duplex(64 * 1024);
    let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
    let (tx, commands) = mpsc::unbounded_channel::<Command>();
    let control = Control::spawn(writer, control_in_rx, ControlOptions { on_request: None });
    let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
    let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
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
        Vec::new(),
    ));
    (tx, update_rx, far_end, stream_tx)
}

fn prompt(
    tx: &mpsc::UnboundedSender<Command>,
    uuid: &str,
) -> oneshot::Receiver<Result<PromptReply, PromptFailure>> {
    let (reply, reply_rx) = oneshot::channel();
    tx.send(Command::Prompt {
        uuid: uuid.into(),
        frame: user_frame(uuid),
        is_local_only: false,
        prompt_text: "hi".into(),
        reply,
    })
    .expect("actor alive");
    reply_rx
}

/// The `state_update` that settled a turn, as the actor published it.
fn settle_update(update: &Value) -> Option<&str> {
    if update["sessionUpdate"] != "state_update" {
        return None;
    }
    match update["state"].as_str()? {
        "idle" | "cancelled" | "error" => update["stopReason"].as_str(),
        _ => None,
    }
}

/// B-63 — a cancel mid-backlog settles the turn `cancelled` without waiting
/// for the 5,000 queued frames, and it is handled within a bounded number of
/// processed frames.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t7a_b63_cancel_settles_behind_a_5000_frame_backlog() {
    let (tx, mut rx, _far_end, stream_tx) = spawn_actor();
    let reply_rx = prompt(&tx, "p1");
    // The backlog is queued before the actor starts draining it.
    for i in 0..5_000 {
        stream_tx.send(delta(i)).expect("stream open");
    }
    tx.send(Command::Cancel).expect("actor alive");
    let settled = tokio::time::timeout(Duration::from_secs(5), reply_rx)
        .await
        .expect("a cancel mid-backlog settles the turn")
        .expect("actor alive");
    let reply = settled.expect("a cancel settles the turn, it does not fail it");
    assert_eq!(reply.stop_reason, "cancelled");

    // Frames processed before the cancel was handled: the cancelled state
    // update is the marker; the backlog's own deltas arrive after it (their
    // text is dropped by the stopped-work window).
    let mut before = 0usize;
    while let Ok(item) = rx.try_recv() {
        let update = match item {
            SessionOutbound::RawUpdate(value) => value["update"].clone(),
            SessionOutbound::Update(notif) => serde_json::to_value(&notif.update).unwrap(),
            _ => continue,
        };
        before += 1;
        if settle_update(&update).is_some() {
            break;
        }
    }
    assert!(
        before < 1_000,
        "the cancel was handled after {before} frames, not behind the whole backlog"
    );
}

/// B-63 — a teardown mid-backlog settles the running turn and ends the actor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t7a_b63_close_settles_behind_a_5000_frame_backlog() {
    let (tx, mut rx, _far_end, stream_tx) = spawn_actor();
    // The production forwarder acks the close's flush barrier; here a drain
    // task does it so the close reply can be observed.
    tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            if let SessionOutbound::Flush(ack) = item {
                let _ = ack.send(());
            }
        }
    });
    let reply_rx = prompt(&tx, "p1");
    for i in 0..5_000 {
        stream_tx.send(delta(i)).expect("stream open");
    }
    let (close_tx, close_rx) = oneshot::channel();
    tx.send(Command::Close { reply: close_tx })
        .expect("actor alive");
    tokio::time::timeout(Duration::from_secs(5), close_rx)
        .await
        .expect("a close mid-backlog completes")
        .expect("the close reply is sent");
    let settled = tokio::time::timeout(Duration::from_secs(5), reply_rx)
        .await
        .expect("the running turn settles at teardown")
        .expect("the reply survives the actor ending");
    assert!(
        settled.is_err() || matches!(settled.unwrap().stop_reason.as_str(), "cancelled"),
        "a turn swept by teardown ends cancelled (or the session-gone failure)"
    );
}

/// B-17 — the yielder is what lets another task run during a synchronous drain
/// of a buffered backlog. With the pause the cancel task is seen within a few
/// frames; without it the drain starves the task for the whole backlog.
#[tokio::test(flavor = "current_thread")]
async fn t7a_b17_the_pause_lets_a_cancel_task_run_mid_backlog() {
    async fn drain(
        cancel: &AtomicBool,
        yielder: &mut crate::event_loop_yield::EventLoopYielder,
        pause: bool,
    ) -> usize {
        for i in 0..5_000 {
            if pause {
                yielder.maybe_yield().await;
            }
            if cancel.load(Ordering::SeqCst) {
                return i;
            }
        }
        5_000
    }

    // Without the pause the spawned task cannot run: the drain owns the
    // single-threaded runtime until it is done.
    let cancel = AtomicBool::new(false);
    let mut yielder = crate::event_loop_yield::EventLoopYielder::with_clock(0.0);
    assert_eq!(
        drain(&cancel, &mut yielder, false).await,
        5_000,
        "a drain that never pauses starves the cancel task"
    );

    // With the pause (budget 0: every call after the first yields) the task
    // runs and the drain stops within a few frames.
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        flag.store(true, Ordering::SeqCst);
    });
    let mut yielder = crate::event_loop_yield::EventLoopYielder::with_clock(0.0);
    let at = drain(&cancel, &mut yielder, true).await;
    assert!(
        at < 50,
        "the pause lets the cancel task run after {at} frames"
    );
}

/// B-39 (`acp-agent.ts:8441-8460`): a cancel resets the per-turn state a later
/// frame could still publish — here the `/usage` producer's late replacement
/// (`localCommandAbort.abort()` / `usage_turns.abort_all()`); the compaction and
/// ExitPlanMode halves are covered by `t5a_cancel_interrupts_the_compaction_*`
/// and `t3a_cancel_forgets_the_pending_plan_markers`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t7a_b39_cancel_drops_a_usage_producers_late_output() {
    let (tx, mut rx, _far_end, _stream_tx) = spawn_actor();
    let reply_rx = prompt(&tx, "p1");
    tx.send(Command::StartUsage { uuid: "p1".into() })
        .expect("actor alive");
    tx.send(Command::Cancel).expect("actor alive");
    tx.send(Command::UsagePrepared {
        uuid: "p1".into(),
        markdown: Some("## Usage\nstructured".into()),
    })
    .expect("actor alive");
    let mut reply = tokio::time::timeout(Duration::from_secs(5), reply_rx)
        .await
        .expect("the turn settles")
        .expect("the reply is sent")
        .expect("a cancel settles the turn");
    let _ = reply.flush_rx.take();
    if let Some(ack) = reply.ack.take() {
        let _ = ack.send(());
    }
    assert_eq!(reply.stop_reason, "cancelled");
    let mut texts = Vec::new();
    while let Ok(item) = rx.try_recv() {
        let update = match item {
            SessionOutbound::RawUpdate(value) => value["update"].clone(),
            SessionOutbound::Update(notif) => serde_json::to_value(&notif.update).unwrap(),
            _ => continue,
        };
        if let Some(text) = update.pointer("/content/text").and_then(Value::as_str) {
            texts.push(text.to_string());
        }
    }
    assert!(
        !texts.iter().any(|text| text.contains("## Usage")),
        "the cancelled producer's replacement must not be published: {texts:?}"
    );
}
