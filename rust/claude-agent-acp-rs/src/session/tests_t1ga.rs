//! Session-level tests added by the Turn 1 fix round 2 (lane A, T1GA): the
//! process-death paths and the error-reply flush barrier.

use super::*;

/// A `run_loop` actor without a child process; `air` lists the AIR capabilities
/// the client declared (none: a plain client).
struct Rig {
    tx: mpsc::UnboundedSender<Command>,
    stream_tx: Option<mpsc::UnboundedSender<Value>>,
    update_rx: mpsc::UnboundedReceiver<SessionOutbound>,
    actor: tokio::task::JoinHandle<()>,
    _writer: tokio::io::DuplexStream,
}

impl Rig {
    fn spawn(air: Option<&[&str]>) -> Self {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(a, control_in_rx, ControlOptions { on_request: None });
        let (update_tx, update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
        let air = air.and_then(|capabilities| {
            AirClient::from_client_capabilities(&json!({
                "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": capabilities}}}
            }))
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
            crate::client_caps::ClientCapabilities::default(),
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

    fn end_stream(&mut self, abnormal: Option<bool>) {
        if let Some(abnormal) = abnormal {
            self.send(child_exit_marker(abnormal));
        }
        self.stream_tx = None;
    }

    /// Everything the actor sent outward until it ended (the channel closes).
    async fn outbound_until_end(&mut self) -> Vec<SessionOutbound> {
        let mut items = Vec::new();
        let drained = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(item) = self.update_rx.recv().await {
                items.push(item);
            }
        })
        .await;
        assert!(drained.is_ok(), "the actor ends and closes its channel");
        items
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

fn failure_of(meta: &Value) -> &Value {
    &meta["jetbrains"]["air"]["sessionFailure"]
}

/// Finding 1: only a `processDied` exit tells the agent layer to forget the
/// session. A clean exit and a death after an error result (a stream error the
/// SDK words differently) keep the husk (`acp-agent.ts:8300-8335`).
#[tokio::test]
async fn t1ga_only_process_death_reports_the_eviction() {
    for (verdict, error_result, evicts) in [
        (Some(true), false, true),
        (Some(true), true, false),
        (Some(false), false, false),
    ] {
        let mut rig = Rig::spawn(None);
        let p1 = rig.prompt("p1");
        rig.echo("p1");
        if error_result {
            rig.send(json!({"type":"result","subtype":"success","is_error":true,
                "stop_reason":null,"result":"boom","usage":{}}));
            let _ = reply_of(p1).await;
        }
        rig.end_stream(verdict);
        let items = rig.outbound_until_end().await;
        let reported = items
            .iter()
            .any(|item| matches!(item, SessionOutbound::ProcessDied));
        assert_eq!(
            reported, evicts,
            "verdict {verdict:?}, error result {error_result}"
        );
    }
}

/// Finding 2: an AIR client's ACTIVE turn ends `end_turn` with the
/// `transport_lost` failure on its response; only the queued turn is rejected
/// (`failActiveWithSessionFailure`, `acp-agent.ts:8275-8288`).
#[tokio::test]
async fn t1ga_air_child_death_ends_the_active_turn_with_transport_lost() {
    let mut rig = Rig::spawn(Some(&["sessionFailure"]));
    let p1 = rig.prompt("p1");
    let p2 = rig.prompt("p2");
    rig.echo("p1");
    rig.end_stream(Some(true));

    let reply = reply_of(p1).await.expect("the active turn resolves");
    assert_eq!(reply.stop_reason, "end_turn");
    let quota = reply
        .quota
        .expect("the failure rides in the response _meta");
    let failure = failure_of(&quota);
    assert_eq!(failure["id"], "p1:error");
    assert_eq!(failure["category"], "connection");
    assert_eq!(failure["actions"], json!(["new_session"]));
    assert_eq!(failure["title"], "The connection to Claude was lost.");

    match reply_of(p2).await {
        Err(SessionError::PromptFailed { kind, message, .. }) => {
            assert_eq!(kind, FailureKind::SessionEnded);
            assert_eq!(message, PROCESS_DIED_MESSAGE);
        }
        other => panic!("the queued turn is rejected, got {other:?}"),
    }
    // A turn-scoped failure is not published a second time on the session.
    let items = rig.outbound_until_end().await;
    assert!(
        !items.iter().any(|item| matches!(
            item,
            SessionOutbound::RawUpdate(v) if v["update"]["sessionUpdate"] == "session_info_update"
        )),
        "no session-scoped failure beside the turn-scoped one"
    );
}

/// Finding 2: with no turn to carry it (idle session, or a queued turn that was
/// never echoed) the AIR client gets the failure as a session-scoped
/// `session_info_update` before the rejection.
#[tokio::test]
async fn t1ga_air_child_death_without_an_active_turn_publishes_the_failure_on_the_session() {
    let mut rig = Rig::spawn(Some(&["sessionFailure"]));
    let p1 = rig.prompt("p1");
    rig.end_stream(Some(true));
    assert!(matches!(
        reply_of(p1).await,
        Err(SessionError::PromptFailed {
            kind: FailureKind::SessionEnded,
            ..
        })
    ));
    let items = rig.outbound_until_end().await;
    let published: Vec<&Value> = items
        .iter()
        .filter_map(|item| match item {
            SessionOutbound::RawUpdate(v)
                if v["update"]["sessionUpdate"] == "session_info_update" =>
            {
                Some(&v["update"]["_meta"])
            }
            _ => None,
        })
        .collect();
    assert_eq!(published.len(), 1, "one session-scoped failure");
    let failure = failure_of(published[0]);
    assert!(failure["id"]
        .as_str()
        .is_some_and(|id| id.starts_with("s1:session-error:") && id.ends_with(":1")));
    assert_eq!(failure["category"], "connection");
}

/// A plain client sees no failure payload at all: the rejection only.
#[tokio::test]
async fn t1ga_plain_client_child_death_publishes_no_failure() {
    let mut rig = Rig::spawn(None);
    let p1 = rig.prompt("p1");
    rig.echo("p1");
    rig.end_stream(Some(true));
    assert!(reply_of(p1).await.is_err());
    let items = rig.outbound_until_end().await;
    assert!(!items.iter().any(|item| matches!(
        item,
        SessionOutbound::RawUpdate(v) if v["update"]["sessionUpdate"] == "session_info_update"
    )));
}

/// Finding 2: after an error result the SDK throws `Claude Code returned an
/// error result: <text>` (not a `processDied` message). An AIR client's queued
/// turn is rejected `internalErrorForClient({errorKind:"transport_lost"})`.
#[tokio::test]
async fn t1ga_air_child_death_after_an_error_result_rejects_queued_as_transport_lost() {
    let mut rig = Rig::spawn(Some(&["sessionFailure"]));
    let p1 = rig.prompt("p1");
    let p2 = rig.prompt("p2");
    rig.echo("p1");
    rig.send(json!({"type":"result","subtype":"success","is_error":true,
        "stop_reason":null,"result":"boom","usage":{}}));
    let _ = reply_of(p1).await;
    rig.end_stream(Some(true));
    assert!(matches!(
        reply_of(p2).await,
        Err(SessionError::TransportLost)
    ));
}

/// Finding 3: an error reply waits for the updates queued ahead of it. The
/// forwarder has not reached the turn's last chunk yet, so the failure is held
/// back until the barrier is served; Node awaits every `sendUpdate` before
/// `turn.reject`.
#[tokio::test]
async fn t1ga_error_reply_waits_for_the_updates_ahead_of_it() {
    let mut rig = Rig::spawn(None);
    let mut p1 = rig.prompt("p1");
    rig.echo("p1");
    rig.send(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_1","model":"claude-sonnet-4-5","role":"assistant",
        "content":[{"type":"text","text":"last words"}]}}));
    rig.send(json!({"type":"result","subtype":"success","is_error":true,
        "stop_reason":null,"result":"boom","usage":{}}));

    // The chunk is queued first, then the barrier; nothing serves them yet.
    let mut seen_chunk = false;
    let flush = loop {
        match tokio::time::timeout(Duration::from_secs(10), rig.update_rx.recv())
            .await
            .expect("the actor emits the chunk and the barrier")
            .expect("the channel is open")
        {
            SessionOutbound::Update(_) => seen_chunk = true,
            SessionOutbound::FlushAcked { done, .. } => break done,
            _ => {}
        }
    };
    assert!(seen_chunk, "the last chunk precedes the barrier");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut p1)
            .await
            .is_err(),
        "the error must not overtake the queued updates"
    );
    let _ = flush.send(());
    assert!(matches!(
        reply_of(p1).await,
        Err(SessionError::PromptFailed { .. })
    ));
}

/// Finding 3: a harness that never serves the outbound channel does not hang
/// the prompt: the barrier is bounded.
#[tokio::test]
async fn t1ga_error_reply_is_released_when_nothing_serves_the_barrier() {
    let mut rig = Rig::spawn(None);
    let p1 = rig.prompt("p1");
    rig.echo("p1");
    rig.send(json!({"type":"result","subtype":"success","is_error":true,
        "stop_reason":null,"result":"boom","usage":{}}));
    let started = tokio::time::Instant::now();
    assert!(reply_of(p1).await.is_err());
    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "the reply went through the barrier"
    );
    // And when the receiver is gone altogether the reply is immediate.
    drop(std::mem::replace(
        &mut rig.update_rx,
        mpsc::unbounded_channel().1,
    ));
    let p2 = rig.prompt("p2");
    rig.echo("p2");
    rig.send(json!({"type":"result","subtype":"success","is_error":true,
        "stop_reason":null,"result":"boom","usage":{}}));
    let started = tokio::time::Instant::now();
    assert!(reply_of(p2).await.is_err());
    assert!(started.elapsed() < Duration::from_millis(1000));
    let _ = tokio::time::timeout(Duration::from_secs(10), &mut rig.actor).await;
}

/// Finding 3: the rejections of one batch are handed to their prompts in order
/// (active turn first, then the queue), the way Node rejects them, however the
/// prompt tasks are scheduled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t1ga_rejections_of_one_batch_arrive_in_order() {
    let mut rig = Rig::spawn(None);
    // Serve every barrier the way the forwarding task does.
    let update_rx = std::mem::replace(&mut rig.update_rx, mpsc::unbounded_channel().1);
    let _forwarder = tokio::spawn(async move {
        let mut update_rx = update_rx;
        while let Some(out) = update_rx.recv().await {
            match out {
                SessionOutbound::Flush(done) => {
                    let _ = done.send(());
                }
                SessionOutbound::FlushAcked { done, ack } => {
                    let _ = done.send(());
                    let _ = ack.await;
                }
                _ => {}
            }
        }
    });
    let arrivals = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut waiters = Vec::new();
    for uuid in ["p1", "p2", "p3", "p4"] {
        let reply = rig.prompt(uuid);
        let arrivals = arrivals.clone();
        waiters.push(tokio::spawn(async move {
            // Like a prompt task: the response is "written" (counted) before the
            // acknowledgement that lets the forwarder release the next one.
            let outcome = tokio::time::timeout(Duration::from_secs(10), reply)
                .await
                .expect("the prompt settles")
                .expect("the reply is sent");
            let arrived_at = arrivals.fetch_add(1, Ordering::SeqCst);
            let rejected = outcome.is_err();
            if let Err(PromptFailure { ack: Some(ack), .. }) = outcome {
                let _ = ack.send(());
            }
            (rejected, arrived_at)
        }));
    }
    rig.echo("p1");
    rig.end_stream(Some(true));
    for (index, waiter) in waiters.into_iter().enumerate() {
        let (rejected, arrived_at) = waiter.await.expect("the waiter ends");
        assert!(rejected, "prompt {index} is rejected");
        assert_eq!(arrived_at, index, "prompt {index} arrives in queue order");
    }
}
