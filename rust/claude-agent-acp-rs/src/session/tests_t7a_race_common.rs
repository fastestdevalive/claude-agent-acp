//! CONC-2 race-suite harness: a seeded scheduler perturbation, a frame-script
//! builder for turn scripts, and a runner that repeats a scenario and reports
//! failures with the seed that reproduces them.
//!
//! Determinism: every iteration runs from a seed (`CONC_SEED`, default
//! `0x5eed5eed`, plus the iteration index) that drives `yield_now` jitter and
//! the choice of which side of a race goes first. No wall-clock sleep beyond a
//! 5 ms perturbation. Rerun one failure with
//! `CONC_SEED=<seed> CONC_ITERS=1 cargo test -p claude-agent-acp-rs --lib <fn>`.
//!
//! The scenarios drive the real session actor in-process (`run_loop`) over
//! unbounded channels: no child process, no network, no `claude`. The scripts
//! are the frame sequences a fake CLI would print, fed to the actor's stream
//! exactly as the relay would.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// xorshift64: the perturbation stream of one iteration.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self((seed ^ 0x9e37_79b9_7f4a_7c15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }

    pub fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }
}

/// The iteration count (`CONC_ITERS`, default 200).
pub fn iterations() -> usize {
    std::env::var("CONC_ITERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(200)
}

/// The base seed (`CONC_SEED`, default `0x5eed5eed`).
pub fn base_seed() -> u64 {
    std::env::var("CONC_SEED")
        .ok()
        .and_then(|v| {
            v.parse::<u64>()
                .ok()
                .or_else(|| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        })
        .unwrap_or(0x5eed_5eed)
}

/// Perturb the scheduler: 0..=max `yield_now`s, and sometimes a sub-5 ms sleep
/// so the race lands on a different side of the timer wheel.
pub async fn jitter(rng: &mut Rng, max: usize) {
    for _ in 0..rng.below(max + 1) {
        tokio::task::yield_now().await;
    }
    if rng.one_in(8) {
        tokio::time::sleep(Duration::from_micros(rng.below(5_000) as u64)).await;
    }
}

/// Run a scenario `iterations()` times and aggregate failures with their seed.
pub async fn run_race<F, Fut>(name: &str, mut body: F)
where
    F: FnMut(u64) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let n = iterations();
    let base = base_seed();
    let mut failures: Vec<(u64, String)> = Vec::new();
    for i in 0..n {
        let seed = base.wrapping_add(i as u64);
        match tokio::time::timeout(Duration::from_secs(10), body(seed)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failures.push((seed, error)),
            Err(_) => failures.push((seed, "iteration timed out after 10s".into())),
        }
    }
    if !failures.is_empty() {
        let (seed, error) = &failures[0];
        panic!(
            "{name}: {}/{n} iterations failed; first seed {seed}: {error} \
             (rerun with CONC_SEED={seed} CONC_ITERS=1)",
            failures.len()
        );
    }
}

/// What the agent layer would do with one actor outbound item, exposed to the
/// test instead.
pub enum WireItem {
    /// A `session/update` payload (typed or raw).
    Update(Value),
    /// `session/request_permission` waiting for the client's answer.
    Permission {
        reply: oneshot::Sender<Result<Value, String>>,
    },
}

/// A live session actor plus the forwarder a production agent layer would run.
pub struct Actor {
    pub tx: mpsc::UnboundedSender<Command>,
    /// The inbound control channel the child's `control_request` frames take.
    pub control_in_tx: mpsc::UnboundedSender<Value>,
    pub stream_tx: Option<mpsc::UnboundedSender<Value>>,
    pub wire: mpsc::UnboundedReceiver<WireItem>,
    /// Every JSON line the actor wrote to the child's stdin (control frames).
    pub control_lines: mpsc::UnboundedReceiver<Value>,
    pub flushes: Arc<AtomicUsize>,
}

impl Actor {
    /// Spawn the actor with a duplex child stdin (read back as `control_lines`)
    /// and a stream channel the test feeds.
    pub fn spawn() -> Self {
        let (writer, far_end) = tokio::io::duplex(256 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(
            writer,
            control_in_rx,
            ControlOptions {
                on_request: Some(build_permission_request_handler(
                    tx.clone(),
                    crate::elicitation::ElicitationSupport {
                        form: false,
                        url: false,
                    },
                )),
            },
        );
        let (update_tx, mut update_rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(run_loop(
            commands,
            tx.clone(),
            stream_rx,
            control,
            control_in_tx.clone(),
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
        // The production forwarder: acks barriers and hands requests over.
        let (wire_tx, wire) = mpsc::unbounded_channel::<WireItem>();
        let flushes = Arc::new(AtomicUsize::new(0));
        let flush_count = Arc::clone(&flushes);
        tokio::spawn(async move {
            while let Some(item) = update_rx.recv().await {
                match item {
                    SessionOutbound::Flush(ack) => {
                        flush_count.fetch_add(1, Ordering::Relaxed);
                        let _ = ack.send(());
                    }
                    SessionOutbound::FlushAcked { done, ack } => {
                        flush_count.fetch_add(1, Ordering::Relaxed);
                        let _ = done.send(());
                        tokio::spawn(async move {
                            let _ = ack.await;
                        });
                    }
                    SessionOutbound::RawUpdate(value) => {
                        let _ = wire_tx.send(WireItem::Update(value["update"].clone()));
                    }
                    SessionOutbound::Update(notif) => {
                        if let Ok(value) = serde_json::to_value(&notif.update) {
                            let _ = wire_tx.send(WireItem::Update(value));
                        }
                    }
                    SessionOutbound::RequestPermission { reply, .. } => {
                        let _ = wire_tx.send(WireItem::Permission { reply });
                    }
                    _ => {}
                }
            }
        });
        // The child's stdin reader: every control frame the actor writes.
        let (lines_tx, control_lines) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(far_end);
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                    let _ = lines_tx.send(value);
                }
            }
        });
        Self {
            tx,
            control_in_tx,
            stream_tx: Some(stream_tx),
            wire,
            control_lines,
            flushes,
        }
    }

    pub fn prompt(&self, uuid: &str) -> oneshot::Receiver<Result<PromptReply, PromptFailure>> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::Prompt {
                uuid: uuid.into(),
                frame: echo_frame(uuid),
                is_local_only: false,
                prompt_text: "hi".into(),
                reply,
            })
            .expect("actor alive");
        reply_rx
    }

    pub fn steer(&self, uuid: &str) -> oneshot::Receiver<Value> {
        let (reply, reply_rx) = oneshot::channel();
        self.tx
            .send(Command::Steer {
                uuid: uuid.into(),
                frame: echo_frame(uuid),
                is_local_only: false,
                is_prompt_required: false,
                reply,
            })
            .expect("actor alive");
        reply_rx
    }

    pub fn feed(&self, line: Value) {
        if let Some(tx) = &self.stream_tx {
            let _ = tx.send(line);
        }
    }

    /// Close the session and wait (bounded) for the actor to end.
    pub async fn shutdown(&mut self) {
        let (reply, reply_rx) = oneshot::channel();
        if self.tx.send(Command::Close { reply }).is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(2), reply_rx).await;
        }
        self.stream_tx = None;
    }

    /// The CLI's echo of a prompt's user message.
    pub fn echo(&self, uuid: &str) {
        self.feed(json!({"type":"user","uuid":uuid,"isReplay":true,
            "message":{"role":"user","content":[{"type":"text","text":"hi"}]}}));
    }

    /// End the child's stdout (a clean or abnormal exit verdict first).
    pub fn end_stream(&mut self, abnormal: Option<bool>) {
        if let Some(abnormal) = abnormal {
            self.feed(json!({"type":"adapter_child_exit","abnormal":abnormal}));
        }
        self.stream_tx = None;
    }

    /// Collect updates until `within` elapses with nothing new.
    pub async fn updates_until_idle(&mut self, within: Duration) -> Vec<Value> {
        let mut updates = Vec::new();
        loop {
            match tokio::time::timeout(within, self.wire.recv()).await {
                Ok(Some(WireItem::Update(value))) => updates.push(value),
                Ok(Some(WireItem::Permission { reply })) => {
                    let _ = reply.send(Ok(json!({"outcome": {"outcome": "cancelled"}})));
                }
                Ok(None) | Err(_) => break,
            }
        }
        updates
    }

    /// Every update already queued, in emission order (a settled prompt's flush
    /// barrier guarantees they are all here).
    pub fn drain_updates(&mut self) -> Vec<Value> {
        let mut updates = Vec::new();
        loop {
            match self.wire.try_recv() {
                Ok(WireItem::Update(value)) => updates.push(value),
                Ok(WireItem::Permission { reply, .. }) => {
                    let _ = reply.send(Ok(json!({"outcome": {"outcome": "cancelled"}})));
                }
                Err(_) => return updates,
            }
        }
    }
}

/// The CLI's echo frame for a prompt (the user message the SDK replays).
pub fn echo_frame(uuid: &str) -> Value {
    json!({"type":"user","uuid":uuid,"isReplay":true,
        "message":{"role":"user","content":[{"type":"text","text":"hi"}]}})
}

/// A fake-CLI turn script: the frames a child prints for one turn, in order
/// (its echo, usage snapshot and deltas; the result is fed by the scenario so
/// it can race it).
pub struct TurnScript {
    frames: Vec<Value>,
}

impl TurnScript {
    /// The turn's echo.
    pub fn new(uuid: &str) -> Self {
        Self {
            frames: vec![echo_frame(uuid)],
        }
    }

    /// `n` streamed text deltas.
    pub fn deltas(mut self, n: usize) -> Self {
        for i in 0..n {
            self.frames.push(json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": format!("t{i} ")}
                }
            }));
        }
        self
    }

    /// A message start frame (the turn's usage snapshot).
    pub fn usage(mut self, input: u64, output: u64) -> Self {
        self.frames
            .push(json!({"type":"stream_event","parent_tool_use_id":null,
            "event":{"type":"message_start","message":{"model":"claude-sonnet-4-5",
            "usage":{"input_tokens":input,"output_tokens":output,
                "cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}));
        self
    }

    pub fn frames(&self) -> &[Value] {
        &self.frames
    }
}

/// Await a prompt reply the way the agent layer does: wait for the reply's
/// flush barrier and acknowledge it, so every update emitted before the settle
/// reached the client before the response. `Err(String)` is a hang or a dropped
/// reply; the inner result is the turn's own outcome.
pub async fn settle(
    rx: oneshot::Receiver<Result<PromptReply, PromptFailure>>,
    within: Duration,
) -> Result<Result<PromptReply, PromptFailure>, String> {
    let reply = tokio::time::timeout(within, rx)
        .await
        .map_err(|_| "the prompt hung (no settle)".to_string())?
        .map_err(|_| "the actor dropped the prompt reply".to_string())?;
    let mut reply = reply;
    match &mut reply {
        Ok(ok) => {
            if let Some(flush) = ok.flush_rx.take() {
                let _ = flush.await;
            }
            if let Some(ack) = ok.ack.take() {
                let _ = ack.send(());
            }
        }
        Err(failure) => {
            if let Some(ack) = failure.ack.take() {
                let _ = ack.send(());
            }
        }
    }
    Ok(reply)
}
