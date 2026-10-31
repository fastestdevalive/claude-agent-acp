//! CONC-2 scenario 2: a steer racing the end of the turn.
//!
//! Upstream decides the steer's outcome from whether a turn is in flight: it is
//! `injected` into the running turn, else it starts a new turn. Both sides are
//! reachable: the seeded side decides whether the steer is queued before the
//! result frame (injected, the actor's biased select serves the command first)
//! or only after the turn settled at its result (startedNewTurn). A steer is
//! never lost (the reply names one outcome) and never runs twice (one settle per
//! turn).

use super::tests_t7a_race_common::*;
use super::*;

fn result_frame() -> Value {
    json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"ok",
        "usage":{"input_tokens":3,"output_tokens":2}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t7a_race_2_steer_vs_end() {
    run_race("steer-vs-end", |seed| async move {
        let mut rng = Rng::new(seed);
        let mut actor = Actor::spawn();
        let reply_rx = actor.prompt("p1");
        actor.echo("p1");
        actor.feed(json!({"type":"stream_event","parent_tool_use_id":null,
            "event":{"type":"message_start","message":{"model":"claude-sonnet-4-5",
            "usage":{"input_tokens":10,"output_tokens":0}}}}));
        let inject = rng.one_in(2);
        if inject {
            // The steer is queued before the result: the biased select serves the
            // command first, against the live turn.
            let steer_rx = actor.steer("s1");
            jitter(&mut rng, 4).await;
            actor.feed(result_frame());
            let outcome = tokio::time::timeout(Duration::from_secs(5), steer_rx)
                .await
                .map_err(|_| "the steer hung".to_string())?
                .map_err(|_| "the actor dropped the steer reply".to_string())?;
            if outcome.get("outcome").and_then(Value::as_str) != Some("injected") {
                return Err(format!("a steer queued before the result was not injected: {outcome}"));
            }
            // The CLI echoes the injected message; the steered turn settles at the
            // idle after the steered cycle's own result.
            actor.echo("s1");
            actor.feed(result_frame());
            actor.feed(json!({"type":"system","subtype":"session_state_changed",
                "state":"idle"}));
            let prompt = settle(reply_rx, Duration::from_secs(5)).await?;
            let reason = prompt
                .map_err(|failure| format!("the steered turn failed: {failure:?}"))?
                .stop_reason;
            if reason != "end_turn" && reason != "cancelled" {
                return Err(format!("unexpected steered stopReason {reason}"));
            }
            // The injected turn settled once; the barrier proves the forwarder
            // counted every barrier the actor emitted before it.
            actor.sync(Duration::from_secs(2)).await?;
            let settles = actor.settles.load(std::sync::atomic::Ordering::Relaxed);
            if settles != 1 {
                return Err(format!(
                    "an injected steer ran {settles} turns for one prompt"
                ));
            }
        } else {
            // The result settles the turn first; the steer that follows finds no
            // turn and starts its own.
            actor.feed(result_frame());
            let prompt = settle(reply_rx, Duration::from_secs(5)).await?;
            let reason = prompt
                .map_err(|failure| format!("the prompt failed: {failure:?}"))?
                .stop_reason;
            if reason != "end_turn" && reason != "cancelled" {
                return Err(format!("unexpected prompt stopReason {reason}"));
            }
            let steer_rx = actor.steer("s1");
            let outcome = tokio::time::timeout(Duration::from_secs(5), steer_rx)
                .await
                .map_err(|_| "the steer hung".to_string())?
                .map_err(|_| "the actor dropped the steer reply".to_string())?;
            if outcome.get("outcome").and_then(Value::as_str) != Some("startedNewTurn") {
                return Err(format!(
                    "a steer after the turn ended did not start a new turn: {outcome}"
                ));
            }
            // The steer's own turn: its echo and result settle it.
            actor.echo("s1");
            actor.feed(result_frame());
            let settles = actor.wait_settles(2, Duration::from_secs(5)).await;
            if settles != 2 {
                return Err(format!(
                    "the steer's new turn settled {settles} times (expected 2: the prompt, then the steer)"
                ));
            }
        }
        actor.shutdown().await?;
        Ok(())
    })
    .await;
}
