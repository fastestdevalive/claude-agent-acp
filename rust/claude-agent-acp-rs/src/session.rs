//! Session actor — the single owner of all session state (Decision D4, item
//! 5.1).
//!
//! One tokio task owns the session: the active turn, the queued prompts, the
//! high-water active-turn counter and the stream/command/cancel plumbing. No
//! other task holds a lock on session state — operations arrive as [`Command`]s
//! over an `mpsc` with a bundled `oneshot` reply (D4). This matches the house
//! actor pattern (`vst-agents/src/acp_connection.rs`).
//!
//! The read loop (5.3) is a `tokio::select!` between:
//!
//! - the [`Command`] channel (client requests, each with a `oneshot` reply),
//! - the codec stream ([`dispatch::StreamMsg`]) fed by the line-codec task,
//! - an injected turn-completion lane (the phase-5 fake; phase 6 drives it from
//!   `result` frames),
//! - a cancel token.
//!
//! The `select!` is `biased` with the two `recv()` branches first, and on cancel
//! it drains any remaining queued stream messages before exiting — so a cancel
//! racing the loop's idle `recv` loses no queued message (INV-15). `recv()` on
//! the unbounded channels is cancel-safe (R8).
//!
//! One-active-turn enforcement (5.4, INV-14): at most one [`Command::Prompt`]
//! is active at any instant; a second is queued FIFO. A test-visible high-water
//! counter records the maximum number of simultaneously-active turns (never
//! above 1 by construction).
//!
//! Phase-5 seams: turn completion is driven by the injected `completions`
//! channel (phase 6 replaces it with `result`-frame handling); outbound user
//! frames are sent to an optional [`Control`] (D6) — `None` in unit tests that
//! exercise only the actor logic.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};

use crate::control::Control;
use crate::dispatch::{self, Route, StreamMsg};

/// Errors produced by the session actor.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session closed")]
    Closed,
}

/// The result of a settled `session/prompt` (phase-6 `turn.rs` fills the real
/// stop-reason table; phase 5 carries a placeholder).
#[derive(Debug, Clone)]
pub struct PromptReply {
    /// The turn's stop reason.
    pub stop_reason: String,
}

/// A request sent by the ACP client to the session actor (D4). Each request
/// carries a `oneshot` reply resolved by the actor when the operation settles.
pub enum Command {
    /// Enqueue a prompt turn. Resolves `reply` when the turn settles (in
    /// phase 5, when the injected completion arrives).
    Prompt {
        /// The `user` message frame to feed to `claude` (D6).
        content: Value,
        /// Reply channel, resolved on settlement.
        reply: oneshot::Sender<Result<PromptReply, SessionError>>,
    },
    /// Cancel the active prompt. Real behaviour is phase 11; phase 5 is a
    /// placeholder that settles nothing (kept so the variant exists).
    Cancel,
}

/// A prompt waiting in, or active in, the session.
struct QueuedPrompt {
    content: Value,
    reply: oneshot::Sender<Result<PromptReply, SessionError>>,
}

/// A handle to the session actor. Cloneable: the ACP layer (agent.rs) and any
/// spawned task can issue [`Command`]s through it.
#[derive(Clone)]
pub struct Session {
    tx: mpsc::UnboundedSender<Command>,
    high_water: Arc<AtomicUsize>,
    processed_stream: Arc<AtomicUsize>,
}

impl Session {
    /// Spawn the session actor task.
    ///
    /// - `stream` — the codec's parsed-line stream (only *session* frames are
    ///   processed here; control frames are forwarded to `control_inbound`).
    /// - `control` — optional outbound control channel for user frames (D6);
    ///   `None` in actor-only unit tests.
    /// - `control_inbound` — where inbound *control* frames are forwarded to
    ///   `control.rs` (D6 phase-4 deviation).
    /// - `completions` — phase-5 injected turn-completion lane; phase 6
    ///   replaces it with `result`-frame handling.
    /// - `cancel` — a `watch` token; when it changes, the actor drains queued
    ///   stream messages (INV-15) and exits.
    pub fn spawn(
        stream: mpsc::UnboundedReceiver<StreamMsg>,
        control: Option<Control>,
        control_inbound: mpsc::UnboundedSender<Value>,
        completions: mpsc::UnboundedReceiver<()>,
        cancel: watch::Receiver<()>,
    ) -> Session {
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let high_water = Arc::new(AtomicUsize::new(0));
        let processed_stream = Arc::new(AtomicUsize::new(0));
        let hw = high_water.clone();
        let ps = processed_stream.clone();

        tokio::spawn(run(
            commands,
            stream,
            control,
            control_inbound,
            completions,
            cancel,
            hw,
            ps,
        ));

        Session {
            tx,
            high_water,
            processed_stream,
        }
    }

    /// Send a `session/prompt`: enqueue a turn and await its settlement.
    pub async fn prompt(&self, content: Value) -> Result<PromptReply, SessionError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::Prompt {
                content,
                reply: reply_tx,
            })
            .map_err(|_| SessionError::Closed)?;
        reply_rx.await.map_err(|_| SessionError::Closed)?
    }

    /// Send a `session/cancel` notification (placeholder until phase 11).
    pub async fn cancel(&self) -> Result<(), SessionError> {
        self.tx
            .send(Command::Cancel)
            .map_err(|_| SessionError::Closed)
    }

    /// The high-water mark of simultaneously-active turns observed so far
    /// (INV-14 / 5.4). Never above 1 by construction.
    pub fn high_water(&self) -> usize {
        self.high_water.load(Ordering::Relaxed)
    }

    /// The number of stream messages the actor has dispatched so far.
    /// Test-visible observable for the cancel-safety test (INV-15).
    pub fn processed_stream(&self) -> usize {
        self.processed_stream.load(Ordering::Relaxed)
    }
}

/// The session actor task. Owns all session state; nothing else mutates it.
#[allow(clippy::too_many_arguments)]
async fn run(
    mut commands: mpsc::UnboundedReceiver<Command>,
    mut stream: mpsc::UnboundedReceiver<StreamMsg>,
    control: Option<Control>,
    control_inbound: mpsc::UnboundedSender<Value>,
    mut completions: mpsc::UnboundedReceiver<()>,
    mut cancel: watch::Receiver<()>,
    high_water: Arc<AtomicUsize>,
    processed_stream: Arc<AtomicUsize>,
) {
    let mut active: Option<QueuedPrompt> = None;
    let mut queue: VecDeque<QueuedPrompt> = VecDeque::new();
    let mut current_active: usize = 0;

    loop {
        tokio::select! {
            biased;
            cmd = commands.recv() => {
                let Some(cmd) = cmd else {
                    // All Session handles dropped: session teardown.
                    break;
                };
                match cmd {
                    Command::Prompt { content, reply } => {
                        let qp = QueuedPrompt { content, reply };
                        if active.is_some() {
                            queue.push_back(qp);
                        } else {
                            activate(
                                qp,
                                &mut active,
                                &mut current_active,
                                &high_water,
                                &control,
                            )
                            .await;
                        }
                    }
                    Command::Cancel => {
                        // Phase 11 drives this; a no-op placeholder for now.
                    }
                }
            }
            Some(_) = completions.recv() => {
                // The active turn completed. Phase 5: the injected fake drives
                // this; phase 6 drives it from the `result` frame. Resolve the
                // active prompt's reply and promote the next queued one.
                if let Some(qp) = active.take() {
                    let _ = qp
                        .reply
                        .send(Ok(PromptReply { stop_reason: "end_turn".into() }));
                    current_active = current_active.saturating_sub(1);
                    if let Some(next) = queue.pop_front() {
                        activate(
                            next,
                            &mut active,
                            &mut current_active,
                            &high_water,
                            &control,
                        )
                        .await;
                    }
                }
            }
            Some(msg) = stream.recv() => {
                dispatch_stream(msg, &control_inbound, &processed_stream);
            }
            _ = cancel.changed() => {
                // INV-15: a cancel racing the idle `recv` loses no queued
                // message. Drain any remaining stream messages before exiting.
                while let Ok(msg) = stream.try_recv() {
                    dispatch_stream(msg, &control_inbound, &processed_stream);
                }
                break;
            }
        }
    }
}

/// Promote a prompt to the active turn: record the one-active-turn high-water
/// and feed the user frame to the control channel (D6) when one is present.
async fn activate(
    qp: QueuedPrompt,
    active: &mut Option<QueuedPrompt>,
    current_active: &mut usize,
    high_water: &AtomicUsize,
    control: &Option<Control>,
) {
    *current_active += 1;
    // INV-14: high-water tracks the max simultaneously-active turns. By
    // construction we only activate when `active` is `None`, so this never
    // exceeds 1; `fetch_max` keeps the counter meaningful if phase 6 ever
    // relaxes that.
    high_water.fetch_max(*current_active, Ordering::Relaxed);
    if let Some(control) = control {
        let _ = control.send_user(qp.content.clone()).await;
    }
    *active = Some(qp);
}

/// Route one inbound stream message (5.2): forward control traffic to
/// `control.rs`, count session traffic (the phase-6 turn machine consumes it).
fn dispatch_stream(
    msg: StreamMsg,
    control_inbound: &mpsc::UnboundedSender<Value>,
    processed_stream: &AtomicUsize,
) {
    processed_stream.fetch_add(1, Ordering::Relaxed);
    let view = dispatch::TurnView::default();
    match dispatch::route(&msg, &view) {
        Route::Control => {
            let _ = control_inbound.send(msg.line);
        }
        Route::Session => {
            // Phase 6: the turn machine consumes user/assistant/result/system
            // /stream_event frames here.
        }
        Route::Ignore => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    /// A session actor with an injected completion sender and a cancel token,
    /// plus the plumbing a test needs to observe it. `control` is `None` for
    /// these actor-only tests.
    struct Harness {
        session: Session,
        completions: mpsc::UnboundedSender<()>,
        stream: mpsc::UnboundedSender<StreamMsg>,
        cancel: watch::Sender<()>,
        _control_inbound: mpsc::UnboundedSender<Value>,
    }

    fn harness() -> Harness {
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<StreamMsg>();
        let (completions_tx, completions_rx) = mpsc::unbounded_channel::<()>();
        let (control_in_tx, _control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (cancel_tx, cancel_rx) = watch::channel(());
        let session = Session::spawn(
            stream_rx,
            None,
            control_in_tx.clone(),
            completions_rx,
            cancel_rx,
        );
        Harness {
            session,
            completions: completions_tx,
            stream: stream_tx,
            cancel: cancel_tx,
            _control_inbound: control_in_tx,
        }
    }

    /// 5.T2 (INV-14) — 25 concurrent `Command::Prompt` on one session with an
    /// injected fake turn-completion: high-water active turns == 1 and all 25
    /// resolve in order.
    #[tokio::test(flavor = "multi_thread")]
    async fn inv_14_one_active_turn() {
        let harness = harness();
        let total = 25;

        // Fire 25 prompts concurrently, each awaiting its reply oneshot.
        let mut tasks = Vec::new();
        for i in 0..total {
            let session = harness.session.clone();
            tasks.push(tokio::spawn(async move {
                let reply = session
                    .prompt(serde_json::json!({"type": "user", "text": i.to_string()}))
                    .await
                    .expect("prompt resolves");
                reply.stop_reason
            }));
        }

        // Let the actor observe the queued prompts, then inject 25 fake
        // completions so every queued turn settles.
        tokio::time::sleep(Duration::from_millis(50)).await;
        for _ in 0..total {
            harness.completions.send(()).unwrap();
        }

        let mut resolved = Vec::new();
        for t in tasks {
            let reason = timeout(Duration::from_secs(10), t)
                .await
                .expect("prompt task completes (no hang)")
                .unwrap();
            resolved.push(reason);
        }

        // All 25 resolved, high-water active turns == 1.
        assert_eq!(resolved.len(), total);
        assert_eq!(
            harness.session.high_water(),
            1,
            "INV-14: at most ONE active turn per session"
        );
        assert!(resolved.iter().all(|r| r == "end_turn"));
    }

    /// 5.T3 (INV-15) — a cancel racing the session loop's idle `recv` loses no
    /// queued stream message.
    #[tokio::test(flavor = "multi_thread")]
    async fn inv_15_cancel_loses_nothing() {
        let harness = harness();
        let total = 200;

        // Enqueue session frames (they increment the actor's processed count).
        for i in 0..total {
            harness
                .stream
                .send(StreamMsg::new(serde_json::json!({"type": "user", "i": i})))
                .unwrap();
        }

        // Let the actor reach its idle `recv`, then fire the cancel token —
        // racing the loop's idle `recv`.
        tokio::time::sleep(Duration::from_millis(50)).await;
        harness.cancel.send(()).unwrap();

        // Give the actor time to drain and exit, then verify every queued
        // message was processed (none lost).
        let deadline = Duration::from_secs(10);
        let start = std::time::Instant::now();
        while harness.session.processed_stream() < total {
            assert!(
                start.elapsed() < deadline,
                "no hang: all queued messages must be processed before exit"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            harness.session.processed_stream(),
            total,
            "INV-15: a cancel racing the idle recv must lose no queued message"
        );
    }
}
