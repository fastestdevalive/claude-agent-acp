//! CONC-2 scenario 4: the client's permission answer racing cancel, turn end
//! and a second request.
//!
//! Every `can_use_tool` request gets exactly one `control_response` on the
//! child's stdin — the allow the client chose, or the deny/abort the actor
//! writes when the cancel withdrew the request. A racing answer never produces
//! a second response, and two requests opened together are each answered once
//! (no leaked entry).

use super::tests_t7a_race_common::*;
use super::*;

fn can_use_tool(tool_use_id: &str) -> Value {
    json!({"type":"control_request","request_id":format!("req-{tool_use_id}"),
        "request":{"subtype":"can_use_tool","tool_name":"Bash",
        "input":{"command":"ls"},"tool_use_id":tool_use_id,
        "permission_suggestions":[]}})
}

fn tool_result(tool_use_id: &str) -> Value {
    json!({"type":"user","parent_tool_use_id":null,
        "message":{"role":"user","content":[{"type":"tool_result",
            "tool_use_id":tool_use_id,"content":"ok"}]}})
}

fn allow() -> Value {
    json!({"outcome":{"outcome":"selected","optionId":"allow-once"}})
}

fn result_frame() -> Value {
    json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"ok",
        "usage":{"input_tokens":3,"output_tokens":2}})
}

fn idle_frame() -> Value {
    json!({"type":"system","subtype":"session_state_changed","state":"idle"})
}

async fn next_permission(
    actor: &mut Actor,
) -> Result<oneshot::Sender<Result<Value, String>>, String> {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match actor.wire.recv().await {
                Some(WireItem::Permission { reply, .. }) => return Ok(reply),
                Some(WireItem::Update(_)) => continue,
                None => return Err("the actor's outbound channel closed".to_string()),
            }
        }
    })
    .await
    .map_err(|_| "no permission request reached the client".to_string())?
}

/// Count the control responses for `ids`, waiting until each has `want` or the
/// window closes. Lines for other requests are counted too, so a response that
/// overtakes another is not lost while scanning for one id.
async fn count_responses(
    actor: &mut Actor,
    ids: &[&str],
    want: usize,
    within: Duration,
) -> std::collections::HashMap<String, usize> {
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for id in ids {
        counts.insert((*id).to_string(), 0);
    }
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if ids
            .iter()
            .all(|id| counts.get(*id).copied().unwrap_or(0) >= want)
        {
            return counts;
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return counts;
        }
        match tokio::time::timeout(left, actor.control_lines.recv()).await {
            Ok(Some(line)) => {
                if line["type"] == "control_response" {
                    if let Some(id) = line.pointer("/response/request_id").and_then(Value::as_str) {
                        *counts.entry(id.to_string()).or_insert(0) += 1;
                    }
                }
            }
            _ => return counts,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t7a_race_4_permission_reply_timing() {
    run_race("permission-reply-timing", |seed| async move {
        let mut rng = Rng::new(seed);
        let mut actor = Actor::spawn();
        let reply_rx = actor.prompt("p1");
        actor.echo("p1");
        actor
            .control_in_tx
            .send(can_use_tool("toolu_1"))
            .expect("control alive");
        let first = next_permission(&mut actor).await?;
        let mode = rng.below(4);

        if mode == 3 {
            // A second request is opened while the first is still open, and
            // both answers race each other.
            actor
                .control_in_tx
                .send(can_use_tool("toolu_2"))
                .expect("control alive");
            let second = next_permission(&mut actor).await?;
            if rng.one_in(2) {
                let _ = first.send(Ok(allow()));
                jitter(&mut rng, 4).await;
                let _ = second.send(Ok(allow()));
            } else {
                let _ = second.send(Ok(allow()));
                jitter(&mut rng, 4).await;
                let _ = first.send(Ok(allow()));
            }
            actor.feed(tool_result("toolu_1"));
            actor.feed(tool_result("toolu_2"));
            actor.feed(result_frame());
        } else if mode == 0 {
            // The client answers, then a cancel races the answered request.
            let _ = first.send(Ok(allow()));
            jitter(&mut rng, 4).await;
            actor.tx.send(Command::Cancel).expect("actor alive");
            actor.feed(idle_frame());
        } else if mode == 1 {
            // The cancel withdraws the request; the late answer is ignored.
            actor.tx.send(Command::Cancel).expect("actor alive");
            jitter(&mut rng, 4).await;
            let _ = first.send(Ok(allow()));
            actor.feed(idle_frame());
        } else {
            // The turn ends; the request stays open and is answered after it.
            actor.feed(tool_result("toolu_1"));
            actor.feed(result_frame());
            jitter(&mut rng, 4).await;
            let _ = first.send(Ok(allow()));
        }

        let ids: &[&str] = if mode == 3 {
            &["req-toolu_1", "req-toolu_2"]
        } else {
            &["req-toolu_1"]
        };
        let counts = count_responses(&mut actor, ids, 1, Duration::from_secs(2)).await;
        for id in ids {
            let count = counts.get(*id).copied().unwrap_or(0);
            if count != 1 {
                return Err(format!(
                    "{id} got {count} control responses (exactly one expected)"
                ));
            }
        }
        // A short extra window: no request may be answered twice.
        let extra = count_responses(&mut actor, ids, 1, Duration::from_millis(20)).await;
        for id in ids {
            if extra.get(*id).copied().unwrap_or(0) != 0 {
                return Err(format!("{id} was answered twice"));
            }
        }
        // M5 (`review-T7`): the prompt must settle (cancelled or end_turn),
        // never hang, and never fail: swallowing `Err`/`Ok(Err)` here would let
        // a dropped or failed reply pass as "still settles".
        match settle(reply_rx, Duration::from_secs(5)).await {
            Ok(Ok(reply)) => {
                if reply.stop_reason != "cancelled" && reply.stop_reason != "end_turn" {
                    return Err(format!(
                        "unexpected prompt stopReason {}",
                        reply.stop_reason
                    ));
                }
            }
            Ok(Err(failure)) => {
                return Err(format!(
                    "the prompt failed instead of settling: {failure:?}"
                ));
            }
            Err(error) => return Err(format!("the prompt did not settle: {error}")),
        }
        actor.shutdown().await;
        Ok(())
    })
    .await;
}
