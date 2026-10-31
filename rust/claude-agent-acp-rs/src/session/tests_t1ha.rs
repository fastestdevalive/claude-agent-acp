//! Session-level tests added by the Turn 1 fix round 3 (lane A, T1HA): the SDK's
//! error-text bookkeeping behind the process-death verdict and the error-reply
//! ordering.

use super::*;

/// A `run_loop` actor without a child process.
struct Rig {
    tx: mpsc::UnboundedSender<Command>,
    stream_tx: Option<mpsc::UnboundedSender<Value>>,
    update_rx: mpsc::UnboundedReceiver<SessionOutbound>,
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
            air,
            crate::client_caps::ClientCapabilities::default(),
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

fn error_result(text: &str) -> Value {
    json!({"type":"result","subtype":"success","is_error":true,
        "stop_reason":null,"result":text,"usage":{}})
}

fn assistant_chunk() -> Value {
    json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_1","model":"claude-sonnet-4-5","role":"assistant",
        "content":[{"type":"text","text":"streaming"}]}})
}

/// Finding 1: the SDK clears `lastErrorResultText` on every frame but a result
/// or a state change. An error result, then the next turn streaming, then the
/// child's death is a plain `processDied`: the session is evicted and the queued
/// turn gets the "exited unexpectedly" message, not the stale error text.
#[tokio::test]
async fn t1ha_death_after_a_streaming_turn_is_not_blamed_on_the_old_error_result() {
    let mut rig = Rig::spawn(None);
    let p1 = rig.prompt("p1");
    rig.echo("p1");
    rig.send(error_result("overloaded"));
    let _ = reply_of(p1).await;
    let p2 = rig.prompt("p2");
    let p3 = rig.prompt("p3");
    rig.echo("p2");
    rig.send(assistant_chunk());
    rig.end_stream(Some(true));

    for reply in [p2, p3] {
        match reply_of(reply).await {
            Err(SessionError::PromptFailed { message, .. }) => {
                assert_eq!(message, PROCESS_DIED_MESSAGE);
            }
            other => panic!("expected the process-died rejection, got {other:?}"),
        }
    }
    let items = rig.outbound_until_end().await;
    assert!(
        items
            .iter()
            .any(|item| matches!(item, SessionOutbound::ProcessDied)),
        "the session is evicted"
    );
}

/// Finding 1: an AIR client's queued turns get the process-died message, not
/// `transport_lost`, once the stale error text is gone.
#[tokio::test]
async fn t1ha_air_death_after_a_streaming_turn_rejects_queued_as_process_died() {
    let mut rig = Rig::spawn(Some(&["sessionFailure"]));
    let p1 = rig.prompt("p1");
    rig.echo("p1");
    rig.send(error_result("overloaded"));
    let _ = reply_of(p1).await;
    let _p2 = rig.prompt("p2");
    let p3 = rig.prompt("p3");
    rig.echo("p2");
    rig.send(assistant_chunk());
    rig.end_stream(Some(true));
    match reply_of(p3).await {
        Err(SessionError::PromptFailed { message, .. }) => {
            assert_eq!(message, PROCESS_DIED_MESSAGE);
        }
        other => panic!("expected the process-died rejection, got {other:?}"),
    }
}

/// A state change in between does not clear the text (`sdk.mjs` skips it).
#[test]
fn t1ha_session_state_changes_keep_the_error_text() {
    let mut tracker = ErrorTextTracker::default();
    tracker.observe(&error_result("boom"));
    tracker.observe(&json!({"type":"system","subtype":"session_state_changed","state":"idle"}));
    assert_eq!(tracker.exit_error_text().as_deref(), Some("boom"));
    tracker.observe(&json!({"type":"control_request","request_id":"r","request":{}}));
    assert_eq!(tracker.exit_error_text().as_deref(), Some("boom"));
    tracker.observe(&assistant_chunk());
    assert_eq!(tracker.exit_error_text(), None);
}

/// The model-less fallback: a turn that never reached the model (a slash command
/// result with `num_turns == 0` and an empty result) keeps the error text from
/// before it, until a frame that is not a system frame resets it.
#[test]
fn t1ha_model_less_turn_falls_back_to_the_error_text_before_it() {
    let mut tracker = ErrorTextTracker::default();
    tracker.observe(&error_result("boom"));
    tracker.observe(&json!({"type":"system","subtype":"init"}));
    tracker.observe(&json!({"type":"system","subtype":"local_command_output"}));
    tracker.observe(
        &json!({"type":"result","subtype":"success","is_error":false,
        "num_turns":0,"result":""}),
    );
    assert_eq!(tracker.exit_error_text().as_deref(), Some("boom"));
    // A model turn resets the fallback.
    tracker.observe(&assistant_chunk());
    tracker.observe(
        &json!({"type":"result","subtype":"success","is_error":false,
        "num_turns":0,"result":""}),
    );
    assert_eq!(tracker.exit_error_text(), None);
}

/// Serves the outbound channel the way the agent's forwarding task does: a
/// barrier resolves at once, an acknowledged one then waits (bounded) for the
/// prompt task's acknowledgement before the next item is served.
fn serve_outbound(rig: &mut Rig) -> tokio::task::JoinHandle<()> {
    let mut update_rx = std::mem::replace(&mut rig.update_rx, mpsc::unbounded_channel().1);
    tokio::spawn(async move {
        while let Some(out) = update_rx.recv().await {
            match out {
                SessionOutbound::Flush(done) => {
                    let _ = done.send(());
                }
                SessionOutbound::FlushAcked { done, ack } => {
                    let _ = done.send(());
                    let _ = tokio::time::timeout(Duration::from_secs(1), ack).await;
                }
                _ => {}
            }
        }
    })
}

fn success_result() -> Value {
    json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn",
        "result":"ok","num_turns":1,"usage":{}})
}

/// A response as the prompt task sees it; `arrive` writes it (and returns the
/// acknowledgement the forwarder waits for, which the caller fires when it likes).
async fn arrive(
    rx: oneshot::Receiver<Result<PromptReply, PromptFailure>>,
) -> (bool, Option<oneshot::Sender<()>>) {
    let outcome = tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .expect("the prompt settles")
        .expect("the reply is sent");
    match outcome {
        Ok(reply) => {
            // The prompt task writes the response only once the barrier resolved.
            if let Some(flush_rx) = reply.flush_rx {
                let _ = flush_rx.await;
            }
            (true, reply.ack)
        }
        Err(failure) => (false, failure.ack),
    }
}

/// Finding 3a: two rejections of one batch. The second is not released until
/// the first one's response was written and acknowledged: no timing involved.
#[tokio::test]
async fn t1ha_a_rejection_waits_for_the_acknowledgement_of_the_one_before_it() {
    for _ in 0..30 {
        let mut rig = Rig::spawn(None);
        let _forwarder = serve_outbound(&mut rig);
        let p1 = rig.prompt("p1");
        let p2 = tokio::spawn(arrive(rig.prompt("p2")));
        rig.echo("p1");
        rig.end_stream(Some(true));

        let (ok, first_ack) = arrive(p1).await;
        assert!(!ok);
        // The first response is not acknowledged yet; the second must not have
        // been handed over, however long the prompt tasks take to be scheduled.
        let mut p2 = p2;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut p2)
                .await
                .is_err(),
            "the second rejection overtook the first"
        );
        first_ack
            .expect("a rejection carries an acknowledgement")
            .send(())
            .unwrap();
        let (ok, _) = p2.await.expect("the second prompt task ends");
        assert!(!ok);
    }
}

/// Finding 3b: across batches. The error result of turn 1 and the whole of turn
/// 2 arrive in one burst; turn 2's response (settled in a later batch) must not
/// reach the wire before turn 1's error, as in Node where the rejection is
/// synchronous.
#[tokio::test]
async fn t1ha_an_error_reply_stays_ahead_of_the_next_batchs_response() {
    for _ in 0..30 {
        let mut rig = Rig::spawn(None);
        let _forwarder = serve_outbound(&mut rig);
        let p1 = rig.prompt("p1");
        let mut p2 = tokio::spawn(arrive(rig.prompt("p2")));
        rig.echo("p1");
        rig.send(error_result("boom"));
        rig.echo("p2");
        rig.send(success_result());

        let (ok, first_ack) = arrive(p1).await;
        assert!(!ok, "turn 1 is rejected");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut p2)
                .await
                .is_err(),
            "turn 2's response overtook turn 1's error"
        );
        first_ack
            .expect("a rejection carries an acknowledgement")
            .send(())
            .unwrap();
        let (ok, _) = p2.await.expect("the second prompt task ends");
        assert!(ok, "turn 2 settles");
    }
}
