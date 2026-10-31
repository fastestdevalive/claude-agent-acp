//! CONC-2 scenario 3: `session/close` racing a running prompt.
//!
//! The close cancels exactly as `session/cancel` does and ends the actor: the
//! prompt gets its terminal state, the close answers behind its flush barrier,
//! and nothing is left running behind it (the in-process equivalent of the
//! child being gone).

use super::tests_t7a_race_common::*;
use super::*;

fn result_frame() -> Value {
    json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"ok",
        "usage":{"input_tokens":3,"output_tokens":2}})
}

fn delta(i: usize) -> Value {
    json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,
        "delta":{"type":"text_delta","text":format!("t{i} ")}}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t7a_race_3_close_during_prompt() {
    run_race("close-during-prompt", |seed| async move {
        let mut rng = Rng::new(seed);
        let actor = Actor::spawn();
        let reply_rx = actor.prompt("p1");
        actor.echo("p1");
        for i in 0..rng.below(40) {
            actor.feed(delta(i));
        }
        let (close_tx, close_rx) = oneshot::channel();
        if rng.one_in(2) {
            actor
                .tx
                .send(Command::Close { reply: close_tx })
                .expect("actor alive");
            jitter(&mut rng, 4).await;
            actor.feed(result_frame());
        } else {
            actor.feed(result_frame());
            jitter(&mut rng, 4).await;
            actor
                .tx
                .send(Command::Close { reply: close_tx })
                .expect("actor alive");
        }
        let settled = settle(reply_rx, Duration::from_secs(5)).await?;
        let reason = settled
            .map_err(|failure| format!("the prompt failed instead of settling: {failure:?}"))?
            .stop_reason;
        if reason != "cancelled" && reason != "end_turn" {
            return Err(format!("unexpected prompt stopReason {reason}"));
        }
        tokio::time::timeout(Duration::from_secs(5), close_rx)
            .await
            .map_err(|_| "the close hung".to_string())?
            .map_err(|_| "the close reply was dropped".to_string())?;
        // The actor ended: a later command finds it gone.
        for _ in 0..100 {
            tokio::task::yield_now().await;
            let (late, _late_rx) = oneshot::channel();
            if actor.tx.send(Command::Close { reply: late }).is_err() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        Err("the actor still accepts commands after close".into())
    })
    .await;
}
