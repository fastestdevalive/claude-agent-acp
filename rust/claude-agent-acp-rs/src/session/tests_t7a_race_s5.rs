//! CONC-2 scenario 5: notification order across turn boundaries.
//!
//! The updates a session emits reach the client in emission order, also when a
//! long turn interleaves text, a tool call and usage, and the next turn's
//! frames are already queued behind it.

use super::tests_t7a_race_common::*;
use super::*;

fn delta(prefix: &str, i: usize) -> Value {
    json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,
        "delta":{"type":"text_delta","text":format!("{prefix}{i} ")}}})
}

fn usage_start() -> Value {
    json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"message_start","message":{"model":"claude-sonnet-4-5",
        "usage":{"input_tokens":10,"output_tokens":0}}}})
}

fn tool_use_frame() -> Value {
    json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_t","model":"claude-sonnet-4-5","role":"assistant",
        "content":[{"type":"tool_use","id":"toolu_A","name":"Bash",
            "input":{"command":"ls"}}]}})
}

fn result_frame() -> Value {
    json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"ok",
        "usage":{"input_tokens":3,"output_tokens":2}})
}

/// One marker per update the order check cares about.
fn marker(update: &Value) -> Option<String> {
    match update["sessionUpdate"].as_str()? {
        "agent_message_chunk" => update
            .pointer("/content/text")
            .and_then(Value::as_str)
            .map(|text| text.trim().to_string()),
        "tool_call" => update
            .get("toolCallId")
            .and_then(Value::as_str)
            .map(|id| format!("tool:{id}")),
        "usage_update" => Some("usage".into()),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t7a_race_5_notification_order() {
    run_race("notification-order", |seed| async move {
        let mut rng = Rng::new(seed);
        let mut actor = Actor::spawn();
        let p1 = actor.prompt("p1");
        actor.echo("p1");
        actor.feed(usage_start());
        let a_count = 5 + rng.below(20);
        for i in 0..a_count {
            actor.feed(delta("a", i));
            jitter(&mut rng, 2).await;
        }
        actor.feed(tool_use_frame());
        let b_count = 5 + rng.below(20);
        for i in 0..b_count {
            actor.feed(delta("b", i));
        }
        actor.feed(json!({"type":"user","parent_tool_use_id":null,
            "message":{"role":"user","content":[{"type":"tool_result",
                "tool_use_id":"toolu_A","content":"ok"}]}}));
        actor.feed(result_frame());
        // The next turn's frames are queued right behind the result.
        let p2 = actor.prompt("p2");
        actor.echo("p2");
        for i in 0..5 {
            actor.feed(delta("c", i));
        }
        actor.feed(result_frame());

        for reply in [p1, p2] {
            let settled = settle(reply, Duration::from_secs(5)).await?;
            let reply = settled.map_err(|failure| format!("a turn failed: {failure:?}"))?;
            if reply.stop_reason != "end_turn" {
                return Err(format!("unexpected stopReason {}", reply.stop_reason));
            }
        }
        let updates = actor.drain_updates();
        let kinds: Vec<String> = updates
            .iter()
            .map(|u| u["sessionUpdate"].as_str().unwrap_or("?").to_string())
            .collect();
        let marks: Vec<String> = updates.iter().filter_map(marker).collect();
        let pos = |needle: &str| marks.iter().position(|mark| mark == needle);
        // The message-start frame's usage update comes first, then the turn's
        // deltas, its tool call, the rest of the deltas, the result's usage and
        // the next turn's deltas.
        if pos("usage") != Some(0) {
            return Err(format!("the message-start usage is not first: {marks:?}"));
        }
        for i in 0..a_count {
            if pos(&format!("a{i}")) != Some(1 + i) {
                return Err(format!(
                    "a-delta {i} out of order (a_count {a_count}, {} updates {:?}): {marks:?}",
                    updates.len(),
                    kinds
                ));
            }
        }
        let tool = pos("tool:toolu_A").ok_or_else(|| format!("no tool call: {marks:?}"))?;
        if tool != 1 + a_count {
            return Err(format!(
                "the tool call is not after the a-deltas: {marks:?}"
            ));
        }
        for i in 0..b_count {
            if pos(&format!("b{i}")) != Some(2 + a_count + i) {
                return Err(format!("b-delta {i} out of order: {marks:?}"));
            }
        }
        let usage = 2 + a_count + b_count;
        if marks.get(usage).map(String::as_str) != Some("usage") {
            return Err(format!(
                "the result's usage is not after the deltas: {marks:?}"
            ));
        }
        for i in 0..5 {
            if pos(&format!("c{i}")) != Some(usage + 1 + i) {
                return Err(format!(
                    "the next turn's delta {i} is out of order: {marks:?}"
                ));
            }
        }
        actor.shutdown().await?;
        Ok(())
    })
    .await;
}
