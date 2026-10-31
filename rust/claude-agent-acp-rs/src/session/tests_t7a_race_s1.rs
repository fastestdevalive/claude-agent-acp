//! CONC-2 scenario 1: `session/cancel` racing the turn's result frame.
//!
//! Upstream settles a turn once: a cancel and a result that race either settle
//! it `cancelled` (the cancel won) or `end_turn` (the result won); the loser
//! must not settle it again. The reply resolves once and exactly one flush
//! barrier (one response) goes out.

use super::tests_t7a_race_common::*;
use super::*;

fn result_frame() -> Value {
    json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"ok",
        "usage":{"input_tokens":3,"output_tokens":2}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t7a_race_1_cancel_vs_result() {
    run_race("cancel-vs-result", |seed| async move {
        let mut rng = Rng::new(seed);
        let mut actor = Actor::spawn();
        let reply_rx = actor.prompt("p1");
        // The turn's opening frames (echo + usage snapshot) from a script.
        for frame in TurnScript::new("p1").usage(10, 0).frames() {
            actor.feed(frame.clone());
        }
        if rng.one_in(2) {
            actor.tx.send(Command::Cancel).expect("actor alive");
            jitter(&mut rng, 4).await;
            actor.feed(result_frame());
        } else {
            actor.feed(result_frame());
            jitter(&mut rng, 4).await;
            actor.tx.send(Command::Cancel).expect("actor alive");
        }
        let settled = settle(reply_rx, Duration::from_secs(5)).await?;
        let reason = settled
            .map_err(|failure| format!("the turn failed instead of settling: {failure:?}"))?
            .stop_reason;
        if reason != "cancelled" && reason != "end_turn" {
            return Err(format!("unexpected stopReason {reason}"));
        }
        // Let the loser of the race be processed, then count the settle
        // barriers: one turn, one response.
        tokio::time::sleep(Duration::from_millis(2)).await;
        let _ = actor.updates_until_idle(Duration::from_millis(20)).await;
        let flushes = actor.flushes.load(std::sync::atomic::Ordering::Relaxed);
        if flushes != 1 {
            return Err(format!(
                "one turn settled {flushes} times (stopReason {reason})"
            ));
        }
        actor.shutdown().await;
        Ok(())
    })
    .await;
}
