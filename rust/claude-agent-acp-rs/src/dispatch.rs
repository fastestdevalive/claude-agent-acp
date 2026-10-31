//! Injectable routing unit (Decision D5, items 5.2 / 5.3).
//!
//! The child's stdout line stream is one `mpsc` of parsed JSON lines. This
//! module owns the only classification of those lines:
//!
//! - [`route`] — a pure function deciding whether a [`StreamMsg`] is
//!   **control traffic** (hand to `control.rs`: `keep_alive`,
//!   `control_response`, `control_request`, `control_cancel_request`) or
//!   **session traffic** (hand to the session actor: `user`, `assistant`,
//!   `result`, `system`, `stream_event`).
//! - [`run_dispatch`] — a sequential, order-preserving pump that feeds a
//!   [`Handler`] every [`StreamMsg`] with its [`Route`].
//!
//! The ordering guarantee is the point (R13): the prior port's dispatch layer
//! was rewritten three times because it `tokio::spawn`-ed per message and
//! destroyed arrival order. Here nothing is spawned — `run_dispatch` processes
//! one message at a time in send order (INV-13). The session actor's full read
//! loop (5.3) multiplexes the codec stream against its `Command` channel and a
//! cancel token, but routes each stream message through the same [`route`] so
//! the classification is never duplicated.
//!
//! [`TurnView`] is the phase-6 seam: a snapshot of the session's turn state
//! that `route` may consult. Phase 5 fills only the presence of an active turn.

use serde_json::Value;
use tokio::sync::mpsc;

/// A parsed inbound line from the child's stdout.
#[derive(Debug, Clone)]
pub struct StreamMsg {
    /// The full parsed JSON line.
    pub line: Value,
}

impl StreamMsg {
    /// Wrap a parsed JSON line.
    pub fn new(line: Value) -> Self {
        Self { line }
    }

    /// The `type` field of the line, or `""` if absent.
    pub fn frame_type(&self) -> &str {
        self.line.get("type").and_then(Value::as_str).unwrap_or("")
    }
}

/// Where an inbound stream message should be routed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Control traffic — hand to `control.rs` (`keep_alive`, `control_response`,
    /// `control_request`, `control_cancel_request`).
    Control,
    /// Session traffic — hand to the session actor (`user`, `assistant`,
    /// `result`, `system`, `stream_event`).
    Session,
    /// Unknown / not this crate's concern — drop.
    Ignore,
}

/// A snapshot of the session's turn state consulted by [`route`] (phase 6
/// seam; phase 5 records only whether a turn is active).
#[derive(Debug, Clone, Copy, Default)]
pub struct TurnView {
    /// Whether the session currently has an active turn.
    pub has_active_turn: bool,
}

/// Classify an inbound stream message (D5).
///
/// `_view` is the phase-6 seam; phase-5 routing is purely by frame `type`.
pub fn route(msg: &StreamMsg, _view: &TurnView) -> Route {
    match msg.frame_type() {
        "keep_alive" | "control_response" | "control_request" | "control_cancel_request" => {
            Route::Control
        }
        "user" | "assistant" | "result" | "system" | "stream_event" => Route::Session,
        _ => Route::Ignore,
    }
}

/// What [`Handler::handle`] asks the pump to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlFlow {
    /// Continue pumping.
    Continue,
    /// Stop the dispatch run.
    Stop,
}

/// The injectable sink consumed by [`run_dispatch`] (D5, R12).
///
/// A test injects a recording handler to assert arrival order (INV-13); the
/// session actor could implement this trait were it not for needing its own
/// multi-input `select!` loop (5.3).
pub trait Handler {
    /// A snapshot of the handler's turn state used by [`route`].
    fn view(&self) -> TurnView;

    /// Handle one stream message, in send order.
    fn handle(&mut self, msg: StreamMsg, route: Route) -> ControlFlow;
}

/// Run the sequential, order-preserving dispatch pump over `rx` (D5).
///
/// Each [`StreamMsg`] is classified by [`route`] and handed to `handler` in
/// send order. Nothing is spawned (R13); a [`ControlFlow::Stop`] return stops
/// the pump. The loop reads `rx` to closure.
pub async fn run_dispatch<H: Handler>(
    rx: &mut mpsc::UnboundedReceiver<StreamMsg>,
    handler: &mut H,
) {
    while let Some(msg) = rx.recv().await {
        let view = handler.view();
        let route = route(&msg, &view);
        if handler.handle(msg, route) == ControlFlow::Stop {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording [`Handler`] that collects `(frame_type, route)` in arrival
    /// order for the INV-13 ordering test.
    struct Recorder {
        seen: Vec<(String, Route)>,
    }

    impl Handler for Recorder {
        fn view(&self) -> TurnView {
            TurnView {
                has_active_turn: false,
            }
        }
        fn handle(&mut self, msg: StreamMsg, route: Route) -> ControlFlow {
            self.seen.push((msg.frame_type().to_string(), route));
            ControlFlow::Continue
        }
    }

    /// 5.T1 (INV-13) — 1000 messages through `run_dispatch` arrive in send
    /// order, under a multi-threaded runtime.
    #[tokio::test(flavor = "multi_thread")]
    async fn inv_13_dispatch_order() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamMsg>();
        let total = 1000;

        // Enqueue every message from a separate task so the sends can race the
        // pump, then drop the sender so the pump sees EOF.
        let producer = tokio::spawn(async move {
            for i in 0..total {
                tx.send(StreamMsg::new(serde_json::json!({
                    "type": if i % 2 == 0 { "user" } else { "result" },
                    "i": i,
                })))
                .expect("send");
            }
        });

        let mut recorder = Recorder { seen: Vec::new() };
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            run_dispatch(&mut rx, &mut recorder),
        )
        .await
        .expect("run_dispatch completes (no hang)");

        producer.await.expect("producer task ok");

        assert_eq!(
            recorder.seen.len(),
            total,
            "every message must arrive exactly once"
        );
        for (idx, (frame_type, _route)) in recorder.seen.iter().enumerate() {
            let expected = if idx % 2 == 0 { "user" } else { "result" };
            assert_eq!(
                frame_type, expected,
                "message at index {idx} must be the {expected} frame"
            );
        }
    }
}
