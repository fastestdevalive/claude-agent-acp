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
        // The loser of the race is processed before the barrier: `FlushUpdates`
        // is answered behind everything the actor queued before it, so the
        // settle counter is complete (a quiet window could expire first on a
        // loaded scheduler). Then count the SETTLE barriers only: one turn, one
        // response.
        actor.sync(Duration::from_secs(2)).await?;
        let settles = actor.settles.load(std::sync::atomic::Ordering::Relaxed);
        if settles != 1 {
            return Err(format!(
                "one turn settled {settles} times (stopReason {reason}, {} barriers)",
                actor.flushes.load(std::sync::atomic::Ordering::Relaxed)
            ));
        }
        actor.shutdown().await?;
        Ok(())
    })
    .await;
}

/// FA4-01 / FA4-02 (issue #844) — a held turn whose follow-up cycle is live is
/// left active by `cancel()`; the force-cancel backstop (and a clean stream end)
/// must answer `session/prompt` with the usage and `_meta.quota` the turn's own
/// result recorded (`cancelledOutcome`, `acp-agent.ts:11886-11894`), not the live
/// accumulator. The race: the follow-up's aborted result can land before the
/// grace expires, or the wedged stream lets the grace fire first — both orders
/// must produce Node's exact serialized response.
/// Node's `session/prompt` answer for the held result (10 in / 5 out, no
/// per-model rows): the recorded outcome, not follow-up work.
const HELD_USAGE: &str = r#"{"inputTokens":10,"outputTokens":5,"cachedReadTokens":0,"cachedWriteTokens":0,"totalTokens":15}"#;
const HELD_QUOTA: &str = r#"{"quota":{"token_count":{"totalTokens":15,"inputTokens":10,"cachedInputTokens":0,"cachedWriteTokens":0,"outputTokens":5,"reasoningOutputTokens":0},"model_usage":[]}}"#;

fn held_result() -> Value {
    json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"","num_turns":1,
        "usage":{"input_tokens":10,"output_tokens":5}})
}

fn aborted_followup() -> Value {
    json!({"type":"result","subtype":"error_during_execution","is_error":true,
        "result":"","errors":["aborted"],"num_turns":1,
        "terminal_reason":"aborted_streaming",
        "origin":{"kind":"task-notification"},
        "usage":{"input_tokens":40,"output_tokens":20}})
}

/// The held turn: capability latch, echo, live subagent, result (10/5), and a
/// follow-up request that leaves the turn active after `cancel()`. The usage
/// updates are real barriers: waiting for them proves the actor consumed the
/// frames before the cancel command (the command arm is polled first).
async fn hold_turn(actor: &mut Actor) -> Result<(), String> {
    actor.feed(json!({"type":"system","subtype":"init","session_id":"s1",
        "capabilities":["request_marker_lists_v1"]}));
    actor.echo("p1");
    actor.feed(
        json!({"type":"system","subtype":"task_started","task_id":"sub1",
        "tool_use_id":"toolu_sub1","subagent_type":"Explore"}),
    );
    // The assistant message whose usage the result reports.
    actor.feed(json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"message_start","message":{"id":"m0","model":"claude-sonnet-4-5",
            "usage":{"input_tokens":10,"output_tokens":5,
                "cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}));
    actor.feed(held_result());
    actor.feed(json!({"type":"system","subtype":"status","status":"requesting"}));
    // A follow-up request; waiting for its usage update proves the actor
    // consumed the echo, the spawn, the held result and the `requesting` status
    // first (the stream preserves their order).
    actor.feed(json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"message_start","message":{"id":"m1","model":"claude-sonnet-4-5",
            "usage":{"input_tokens":1,"output_tokens":1,
                "cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}));
    wait_for_usage(actor, 2).await
}

/// Wait (bounded) for the `usage_update` with `used`: a real barrier that the
/// actor processed the frames up to it.
async fn wait_for_usage(actor: &mut Actor, used: u64) -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match actor.wire.recv().await {
                Some(WireItem::Update(value)) => {
                    if value["sessionUpdate"] == "usage_update" && value["used"] == used {
                        return Ok(());
                    }
                }
                Some(WireItem::Permission { reply, .. }) => {
                    let _ = reply.send(Ok(json!({"outcome":{"outcome":"cancelled"}})));
                }
                None => return Err("the actor's outbound channel closed".to_string()),
            }
        }
    })
    .await
    .map_err(|_| format!("no usage_update with used={used}"))?
}

/// Assert the prompt answer is Node's serialized `usage` + `_meta.quota`.
fn assert_recorded_answer(reply: &PromptReply) -> Result<(), String> {
    if reply.stop_reason != "cancelled" {
        return Err(format!("expected cancelled, got {}", reply.stop_reason));
    }
    let usage = reply
        .usage
        .as_ref()
        .ok_or_else(|| "the held turn's cancel carries its usage".to_string())?;
    let serialized_usage = serde_json::to_string(&json!({
        "inputTokens": usage.input_tokens,
        "outputTokens": usage.output_tokens,
        "cachedReadTokens": usage.cached_read_tokens,
        "cachedWriteTokens": usage.cached_write_tokens,
        "totalTokens": usage.total_tokens,
    }))
    .unwrap_or_default();
    if serialized_usage != HELD_USAGE {
        return Err(format!(
            "usage diverges from Node: {serialized_usage} != {HELD_USAGE}"
        ));
    }
    let serialized_quota = reply
        .quota
        .as_ref()
        .and_then(|quota| serde_json::to_string(quota).ok())
        .unwrap_or_default();
    if serialized_quota != HELD_QUOTA {
        return Err(format!(
            "quota diverges from Node: {serialized_quota} != {HELD_QUOTA}"
        ));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t7a_race_1b_force_cancel_held_turn_reports_recorded_outcome() {
    const GRACE: Duration = Duration::from_millis(15);
    run_race("force-cancel-held-recorded-outcome", |seed| async move {
        let mut rng = Rng::new(seed);
        let mut actor = Actor::spawn_with_grace(GRACE);
        let reply_rx = actor.prompt("p1");
        hold_turn(&mut actor).await?;
        jitter(&mut rng, 4).await;

        actor.tx.send(Command::Cancel).expect("actor alive");
        // The cancel is processed first (`FlushUpdates` is behind it), so the
        // held turn stays active with its follow-up live.
        actor.sync(Duration::from_secs(2)).await?;
        // Race the follow-up's aborted result against the force-cancel grace.
        if rng.one_in(2) {
            jitter(&mut rng, 4).await;
            actor.feed(aborted_followup());
        } else {
            // Wedged: no frame, so the backstop must settle the turn.
            tokio::time::sleep(GRACE + Duration::from_millis(10)).await;
        }

        let settled = settle(reply_rx, Duration::from_secs(5)).await?;
        let reply = settled
            .map_err(|failure| format!("the turn failed instead of settling: {failure:?}"))?;
        assert_recorded_answer(&reply)?;
        actor.shutdown().await?;
        Ok(())
    })
    .await;
}

/// FA6-01 — the echo hand-off under a pending cancel is a third settle path
/// (after the force-cancel backstop and the clean stream end): it must answer
/// the held turn with the usage and `_meta.quota` its own result recorded
/// (`cancelledOutcome`, `acp-agent.ts:11886-11894`), not the live accumulator.
/// The race: the next prompt's echo can land before or after the interrupted
/// follow-up's aborted result; both orders must produce Node's exact
/// serialized response for p1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t7a_race_1d_echo_handoff_under_cancel_reports_recorded_outcome() {
    run_race("echo-handoff-held-recorded-outcome", |seed| async move {
        let mut rng = Rng::new(seed);
        let mut actor = Actor::spawn();
        let reply_rx = actor.prompt("p1");
        hold_turn(&mut actor).await?;
        actor.tx.send(Command::Cancel).expect("actor alive");
        // The cancel is processed first (`FlushUpdates` is behind it), so the
        // held turn stays active with its follow-up live.
        actor.sync(Duration::from_secs(2)).await?;

        // The user moves on: p2 is queued, and its echo races the interrupted
        // follow-up's aborted result.
        let _p2 = actor.prompt("p2");
        actor.sync(Duration::from_secs(2)).await?;
        if rng.one_in(2) {
            jitter(&mut rng, 4).await;
            actor.feed(aborted_followup());
            jitter(&mut rng, 2).await;
            actor.echo("p2");
        } else {
            actor.echo("p2");
            jitter(&mut rng, 4).await;
            actor.feed(aborted_followup());
        }

        let settled = settle(reply_rx, Duration::from_secs(5)).await?;
        let reply = settled
            .map_err(|failure| format!("the turn failed instead of settling: {failure:?}"))?;
        assert_recorded_answer(&reply)?;
        // Let p2 settle too, so the forwarder never waits out its ack timeout
        // on a dropped reply.
        actor.feed(result_frame());
        let p2 = settle(_p2, Duration::from_secs(5)).await?;
        if let Err(failure) = p2 {
            return Err(format!("p2 failed instead of settling: {failure:?}"));
        }
        actor.shutdown().await?;
        Ok(())
    })
    .await;
}

/// FA4-02 — the clean stream end under a cancel is deterministic: after the
/// cancel was processed (`sync`), the held turn's recorded outcome must answer
/// the prompt, like Node's `cancelledOutcome(session, inFlight)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t7a_race_1c_stream_end_under_cancel_reports_recorded_outcome() -> Result<(), String> {
    let mut actor = Actor::spawn();
    let reply_rx = actor.prompt("p1");
    hold_turn(&mut actor).await?;
    actor.tx.send(Command::Cancel).expect("actor alive");
    actor.sync(Duration::from_secs(2)).await?;
    actor.end_stream(None);

    let settled = settle(reply_rx, Duration::from_secs(5)).await?;
    let reply =
        settled.map_err(|failure| format!("the turn failed instead of settling: {failure:?}"))?;
    assert_recorded_answer(&reply)?;
    actor.shutdown().await?;
    Ok(())
}
