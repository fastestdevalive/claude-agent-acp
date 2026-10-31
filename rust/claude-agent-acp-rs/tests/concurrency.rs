//! Concurrency fidelity and runtime benchmark suite (CONC-1, ROADMAP F43).
//!
//! Covers:
//! 1. `inv_74_cancel_not_starved_by_backlog`: proves that an in-flight prompt
//!    with 10,000 buffered streaming frames handles `session/cancel` immediately
//!    without waiting for the backlog to drain, guaranteed by Tokio's
//!    `select! { biased; ... }` cooperative scheduling.
//! 2. `bench_runtime_comparison`: measured benchmark of single-threaded
//!    (`current_thread`) vs multi-threaded (`multi_thread`) Tokio runtimes for
//!    streaming chunk latency and steering/cancel latency under backlog.
//! 3. `inv_panic_isolation`: verifies that a task failure / panic unwinds cleanly
//!    under `panic = "unwind"`, returns a clean error to the caller, does not
//!    hang the pending prompt, and does not terminate the process.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::{Channel, Client, ConnectionTo, UntypedMessage};
use claude_agent_acp_rs::agent::{serve, ServeOptions};
use common::{fake_claude_binary, repo_root};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

/// Build a 10,000-frame backlog transcript:
/// - Step 1: initialize handshake
/// - Step 2: user prompt -> emits 10,000 streaming chunks
fn make_10k_backlog_transcript(dir: &Path) -> PathBuf {
    let transcript_path = dir.join("backlog-10k.transcript.jsonl");
    let mut file = std::fs::File::create(&transcript_path).expect("create transcript");
    use std::io::Write;

    // Step 1: initialize
    let init_step = json!({
        "expect": {
            "type": "control_request",
            "request": {"subtype": "initialize"}
        },
        "emit": [{
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "$REQ",
                "response": {
                    "models": [{"value": "claude-sonnet-4-5"}],
                    "commands": [],
                    "account": {}
                }
            }
        }]
    });
    writeln!(file, "{}", serde_json::to_string(&init_step).unwrap()).unwrap();

    // Step 2: user prompt emitting 10,000 stream events
    let mut emit_frames = Vec::with_capacity(10_000);
    for i in 0..10_000 {
        emit_frames.push(json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_delta",
                "index": 0,
                "delta": {
                    "type": "text_delta",
                    "text": format!("token_{i} ")
                }
            }
        }));
    }
    let prompt_step = json!({
        "expect": {"type": "user"},
        "emit": emit_frames
    });
    writeln!(file, "{}", serde_json::to_string(&prompt_step).unwrap()).unwrap();

    transcript_path
}

fn fake_opts_for_transcript(
    _root: &Path,
    transcript: &Path,
    argv_out: &Path,
    cwd: &Path,
) -> ServeOptions {
    ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                transcript.to_string_lossy().into_owned(),
            ),
            (
                "FAKE_CLAUDE_ARGV_OUT".to_string(),
                argv_out.to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(cwd.to_path_buf()),
        ..Default::default()
    }
}

/// Invariant 74 (F43 / CONC-1): A backlog of 10,000 buffered frames does NOT
/// starve `session/cancel`.
///
/// In Node upstream, `EventLoopYielder` was required because async iterators in
/// V8 drain microtasks continuously without reaching macrotasks (I/O). Under
/// Tokio, `run_loop` uses `tokio::select! { biased; cmd = commands.recv() => ..., msg = stream.recv() => ... }`
/// which polls `commands` before `stream` on every iteration, guaranteeing that
/// a cancel notification is processed without draining the backlog.
#[tokio::test]
async fn inv_74_cancel_not_starved_by_backlog() {
    let root = repo_root();
    let tmp = std::env::temp_dir().join(format!("acp-conc-starv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");

    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let transcript = make_10k_backlog_transcript(&tmp);
    let argv_out = tmp.join("argv.json");

    let opts = fake_opts_for_transcript(&root, &transcript, &argv_out, &cwd);
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });

    let cwd_str = cwd.to_string_lossy().into_owned();
    let client_task = Client.builder().connect_with(
        client_channel,
        move |connection: ConnectionTo<agent_client_protocol::Agent>| async move {
            // 1. Create session
            let init_resp = connection
                .send_request(UntypedMessage::new(
                    "session/new",
                    json!({ "cwd": cwd_str }),
                )?)
                .block_task()
                .await?;
            let session_id = init_resp["sessionId"]
                .as_str()
                .expect("sessionId string")
                .to_string();

            // 2. Submit prompt (fake-claude will start writing 10,000 frames)
            let prompt_conn = connection.clone();
            let prompt_session_id = session_id.clone();
            let prompt_fut = tokio::spawn(async move {
                prompt_conn
                    .send_request(UntypedMessage::new(
                        "session/prompt",
                        json!({
                            "sessionId": prompt_session_id,
                            "prompt": [{"type": "text", "text": "start 10k stream"}]
                        }),
                    )?)
                    .block_task()
                    .await
            });

            // Allow the child to begin streaming
            tokio::time::sleep(Duration::from_millis(20)).await;

            // 3. Send session/cancel while 10,000 frames are buffered/streaming
            let cancel_start = Instant::now();
            connection.send_notification(UntypedMessage::new(
                "session/cancel",
                json!({ "sessionId": session_id }),
            )?)?;

            // 4. Await prompt response: must resolve cancelled swiftly
            let prompt_res = tokio::time::timeout(Duration::from_secs(5), prompt_fut)
                .await
                .expect("prompt must not hang during backlog")
                .expect("task join ok")
                .expect("prompt request ok");

            let elapsed = cancel_start.elapsed();

            let stop_reason = prompt_res["stopReason"]
                .as_str()
                .expect("stopReason present");
            assert_eq!(
                stop_reason, "cancelled",
                "stopReason must be cancelled, not starved by backlog"
            );

            // The bound CONCURRENCY.md §1.3 states: a cancel behind a 10k
            // backlog settles in < 250 ms (measured well under 1 ms here; a
            // starved cancel would take seconds).
            assert!(
                elapsed < Duration::from_millis(250),
                "Cancel latency {elapsed:?} took too long, indication of starvation"
            );

            Ok::<(), agent_client_protocol::Error>(())
        },
    );

    let client_result = tokio::time::timeout(Duration::from_secs(10), client_task)
        .await
        .expect("test must not timeout");
    assert!(client_result.is_ok(), "client session completed cleanly");

    drop(client_result);
    let _ = tokio::time::timeout(Duration::from_secs(2), serve_task).await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Simulated actor loop reproducing the exact `select! { biased; ... }` construct
/// from `session.rs:run_loop`. `processed` publishes the frames drained so far, so
/// the benchmark can start from a running actor instead of racing `tokio::spawn`
/// against its first command.
async fn run_simulated_actor(
    mut commands: mpsc::UnboundedReceiver<&'static str>,
    mut stream: mpsc::UnboundedReceiver<u32>,
    settle_tx: oneshot::Sender<(&'static str, usize)>,
    processed: Arc<AtomicUsize>,
) {
    let mut frames_drained = 0;
    loop {
        tokio::select! {
            biased;
            cmd = commands.recv() => {
                match cmd {
                    Some("cancel") => {
                        let _ = settle_tx.send(("cancelled", frames_drained));
                        break;
                    }
                    Some("steer") => {
                        let _ = settle_tx.send(("steered", frames_drained));
                        break;
                    }
                    _ => break,
                }
            }
            msg = stream.recv() => {
                match msg {
                    Some(_) => {
                        frames_drained += 1;
                        processed.fetch_add(1, Ordering::Relaxed);
                    }
                    None => {
                        let _ = settle_tx.send(("eof", frames_drained));
                        break;
                    }
                }
            }
        }
    }
}

/// Wait (bounded) until the actor drained at least one backlog frame, so the
/// cancel is sent to a RUNNING actor (not racing `tokio::spawn`).
async fn wait_draining(processed: &AtomicUsize) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if processed.load(Ordering::Relaxed) >= 1 {
            return true;
        }
        tokio::task::yield_now().await;
    }
    false
}

/// Benchmark metrics container
#[derive(Debug, Clone, Copy)]
pub struct BenchMetrics {
    pub streaming_10k_ms: f64,
    pub chunk_latency_us: f64,
    pub cancel_latency_us: f64,
    pub steer_latency_us: f64,
    pub frames_drained_before_cancel: usize,
}

/// Run actor backlog and streaming benchmark under the current tokio runtime
pub async fn run_runtime_benchmark() -> BenchMetrics {
    const CHUNK_COUNT: usize = 10_000;

    // 1. Measure pure chunk streaming throughput and latency
    let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel::<u32>();
    let stream_start = Instant::now();
    let stream_producer = tokio::spawn(async move {
        for i in 0..CHUNK_COUNT {
            chunk_tx.send(i as u32).unwrap();
        }
    });
    let stream_consumer = tokio::spawn(async move {
        let mut count = 0;
        while chunk_rx.recv().await.is_some() {
            count += 1;
            if count == CHUNK_COUNT {
                break;
            }
        }
        count
    });

    let _ = tokio::join!(stream_producer, stream_consumer);
    let stream_elapsed = stream_start.elapsed();
    let streaming_10k_ms = stream_elapsed.as_secs_f64() * 1000.0;
    let chunk_latency_us = (stream_elapsed.as_secs_f64() * 1_000_000.0) / (CHUNK_COUNT as f64);

    // 2. Measure cancel latency under 10k buffered backlog
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<&'static str>();
    let (stream_tx, stream_rx) = mpsc::unbounded_channel::<u32>();
    let (settle_tx, settle_rx) = oneshot::channel();

    // Pre-fill 10,000 frames in stream queue
    for i in 0..CHUNK_COUNT {
        stream_tx.send(i as u32).unwrap();
    }

    let processed = Arc::new(AtomicUsize::new(0));
    tokio::spawn(run_simulated_actor(
        cmd_rx,
        stream_rx,
        settle_tx,
        processed.clone(),
    ));
    assert!(
        wait_draining(&processed).await,
        "the simulated actor never started draining the backlog"
    );

    let cancel_start = Instant::now();
    cmd_tx.send("cancel").unwrap();
    let (reason, frames_drained) = settle_rx.await.unwrap();
    let cancel_latency_us = cancel_start.elapsed().as_secs_f64() * 1_000_000.0;
    assert_eq!(reason, "cancelled");
    // The actor was already draining when the cancel was sent: the count is not
    // a spawn/send race. Its latency is the property (a starved cancel would take
    // milliseconds per frame, not microseconds).
    assert!(
        frames_drained >= 1,
        "the simulated actor was not draining before the cancel"
    );
    assert!(
        cancel_latency_us < 50_000.0,
        "cancel took {cancel_latency_us} µs behind a 10k backlog"
    );

    // 3. Measure steer latency under 10k buffered backlog
    let (cmd_tx2, cmd_rx2) = mpsc::unbounded_channel::<&'static str>();
    let (stream_tx2, stream_rx2) = mpsc::unbounded_channel::<u32>();
    let (settle_tx2, settle_rx2) = oneshot::channel();

    for i in 0..CHUNK_COUNT {
        stream_tx2.send(i as u32).unwrap();
    }

    let processed2 = Arc::new(AtomicUsize::new(0));
    tokio::spawn(run_simulated_actor(
        cmd_rx2,
        stream_rx2,
        settle_tx2,
        processed2.clone(),
    ));
    assert!(
        wait_draining(&processed2).await,
        "the simulated actor never started draining the backlog"
    );

    let steer_start = Instant::now();
    cmd_tx2.send("steer").unwrap();
    let (steer_reason, steer_drained) = settle_rx2.await.unwrap();
    let steer_latency_us = steer_start.elapsed().as_secs_f64() * 1_000_000.0;
    assert_eq!(steer_reason, "steered");
    assert!(
        steer_drained >= 1,
        "the simulated actor was not draining before the steer"
    );
    assert!(
        steer_latency_us < 50_000.0,
        "steer took {steer_latency_us} µs behind a 10k backlog"
    );

    BenchMetrics {
        streaming_10k_ms,
        chunk_latency_us,
        cancel_latency_us,
        steer_latency_us,
        frames_drained_before_cancel: frames_drained,
    }
}

/// Benchmark test comparing multi_thread vs current_thread
#[test]
fn bench_tokio_runtime_comparison() {
    // 1. Run under multi-threaded runtime
    let mt_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build multi_thread runtime");
    let mt_metrics = mt_runtime.block_on(async { run_runtime_benchmark().await });

    // 2. Run under current_thread runtime
    let ct_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current_thread runtime");
    let ct_metrics = ct_runtime.block_on(async { run_runtime_benchmark().await });

    eprintln!("\n=== TOKIO RUNTIME BENCHMARK RESULTS (CONC-1) ===");
    eprintln!(
        "Multi-thread:   10k chunks: {:.2} ms ({:.3} µs/chunk) | Cancel latency: {:.2} µs (drained: {}) | Steer latency: {:.2} µs",
        mt_metrics.streaming_10k_ms,
        mt_metrics.chunk_latency_us,
        mt_metrics.cancel_latency_us,
        mt_metrics.frames_drained_before_cancel,
        mt_metrics.steer_latency_us
    );
    eprintln!(
        "Current-thread: 10k chunks: {:.2} ms ({:.3} µs/chunk) | Cancel latency: {:.2} µs (drained: {}) | Steer latency: {:.2} µs",
        ct_metrics.streaming_10k_ms,
        ct_metrics.chunk_latency_us,
        ct_metrics.cancel_latency_us,
        ct_metrics.frames_drained_before_cancel,
        ct_metrics.steer_latency_us
    );

    // The simulated actor was draining when the cancel/steer were sent, so the
    // counters are not a spawn/send race; the latency bounds inside
    // `run_runtime_benchmark` are the gate. The real-actor guarantee is proven by
    // `inv_74_cancel_not_starved_by_backlog` and the T7 race suite, not here.
    assert!(mt_metrics.frames_drained_before_cancel >= 1);
    assert!(ct_metrics.frames_drained_before_cancel >= 1);

    // Generous gate bound: total test execution well within bounds
    assert!(mt_metrics.streaming_10k_ms < 5000.0);
    assert!(ct_metrics.streaming_10k_ms < 5000.0);
}

/// A task-level panic-containment check: a panicking task yields
/// `JoinError::is_panic` and its dropped oneshot resolves `Err`, never hanging.
/// It is NOT the session actor or the agent layer (deleting `agent.rs`'s
/// `unwrap_or_else(|_| Err(SessionError::Closed))` would not fail it); the
/// agent-level containment is covered by
/// `tests/conc_race_child_exit.rs::conc_race_a_dead_session_leaves_a_sibling_answering`
/// (a dead session's prompt gets -32603, a sibling keeps answering) and
/// `conc_race_child_abnormal_exit_mid_turn_fails_the_turn`.
#[tokio::test]
async fn inv_panic_isolation_and_prompt_settlement() {
    let (reply_tx, reply_rx) = oneshot::channel::<Result<String, String>>();
    let panic_handled = Arc::new(AtomicUsize::new(0));
    let ph = panic_handled.clone();

    let task = tokio::spawn(async move {
        // Drop reply_tx prematurely on panic / error
        let _holder = reply_tx;
        if true {
            panic!("intentional test panic in session actor");
        }
    });

    let join_result = task.await;
    assert!(join_result.is_err(), "panicking task must yield JoinError");
    assert!(
        join_result.unwrap_err().is_panic(),
        "JoinError must be panic"
    );

    // Channel dropped by unwind resolves with RecvError
    let reply_result = reply_rx.await;
    assert!(
        reply_result.is_err(),
        "dropped oneshot sender must resolve Err(RecvError), never hang"
    );
    ph.fetch_add(1, Ordering::SeqCst);
    assert_eq!(panic_handled.load(Ordering::SeqCst), 1);
}
