//! CONC-2 scenario 2: a steer racing the end of the turn.
//!
//! Upstream decides the steer's outcome from whether a turn is in flight: it is
//! `injected` into the running turn, else it starts a new turn. Racing a result
//! frame may land it on either side, but it is never lost (the reply names one
//! outcome) and never runs twice (one settle per turn).

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
        let steer_rx = actor.steer("s1");
        if rng.one_in(2) {
            jitter(&mut rng, 4).await;
            actor.feed(result_frame());
        } else {
            actor.feed(result_frame());
            jitter(&mut rng, 4).await;
        }
        let outcome = tokio::time::timeout(Duration::from_secs(5), steer_rx)
            .await
            .map_err(|_| "the steer hung".to_string())?
            .map_err(|_| "the actor dropped the steer reply".to_string())?;
        let kind = outcome
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match kind.as_str() {
            "injected" => {
                // The CLI echoes the injected message; the steered turn settles
                // at the next idle (the only signal spanning the interrupted and
                // steered cycles).
                actor.echo("s1");
                // The steered cycle's own result, then the idle that settles
                // the turn (the only signal spanning both cycles).
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
                tokio::time::sleep(Duration::from_millis(2)).await;
                let flushes = actor.flushes.load(std::sync::atomic::Ordering::Relaxed);
                if flushes != 1 {
                    return Err(format!(
                        "an injected steer ran {flushes} turns for one prompt"
                    ));
                }
            }
            "startedNewTurn" => {
                // The prompt settled at its result before the steer started.
                match tokio::time::timeout(Duration::from_millis(300), reply_rx).await {
                    Err(_) => {
                        return Err(format!(
                            "DIAG prompt reply never arrived; flushes={}",
                            actor.flushes.load(std::sync::atomic::Ordering::Relaxed)
                        ))
                    }
                    Ok(Err(_)) => return Err("the actor dropped the prompt reply".into()),
                    Ok(Ok(Err(failure))) => {
                        return Err(format!("the prompt failed: {failure:?}"))
                    }
                    Ok(Ok(Ok(mut reply))) => {
                        if let Some(flush) = reply.flush_rx.take() {
                            let _ = tokio::time::timeout(Duration::from_millis(300), flush).await;
                        }
                        if let Some(ack) = reply.ack.take() {
                            let _ = ack.send(());
                        }
                        if reply.stop_reason != "end_turn" && reply.stop_reason != "cancelled" {
                            return Err(format!(
                                "unexpected prompt stopReason {}",
                                reply.stop_reason
                            ));
                        }
                    }
                }
                // The steer's turn runs on its own: its echo and result settle
                // it, and the session reports a second settle.
                actor.echo("s1");
                actor.feed(result_frame());
                let mut flushes = 0;
                for _ in 0..200 {
                    tokio::task::yield_now().await;
                    flushes = actor.flushes.load(std::sync::atomic::Ordering::Relaxed);
                    if flushes >= 2 {
                        break;
                    }
                }
                if flushes != 2 {
                    return Err(format!(
                        "the steer's new turn settled {flushes} times (expected 2: the prompt, then the steer)"
                    ));
                }
            }
            other => return Err(format!("unexpected steer outcome {other}")),
        }
        actor.shutdown().await;
        Ok(())
    })
    .await;
}
