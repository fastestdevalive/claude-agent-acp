//! CONC-2 scenario 6: duplicate and late replies.
//!
//! A client that answers a permission request twice, after the turn ended, or
//! after the session closed must not panic the actor, write a second
//! `control_response`, or corrupt the state a later command sees.

use super::tests_t7a_race_common::*;
use super::*;
use std::sync::Arc;

fn can_use_tool(tool_use_id: &str) -> Value {
    json!({"type":"control_request","request_id":format!("req-{tool_use_id}"),
        "request":{"subtype":"can_use_tool","tool_name":"Bash",
        "input":{"command":"ls"},"tool_use_id":tool_use_id,
        "permission_suggestions":[]}})
}

fn allow() -> Value {
    json!({"outcome":{"outcome":"selected","optionId":"allow-once"}})
}

fn result_frame() -> Value {
    json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"ok",
        "usage":{"input_tokens":3,"output_tokens":2}})
}

fn ctx() -> Arc<crate::permission_options::OptionsContext> {
    Arc::new(crate::permission_options::OptionsContext {
        tool_name: "Bash".into(),
        display_name: None,
        input: json!({"command":"ls"}),
        cwd: std::env::temp_dir(),
        change_set: None,
        no_persistent_rule: false,
        default_to_no: false,
        available_modes: Vec::new(),
        auto_unavailable: false,
        pre_plan_mode: None,
        context_used_percent: None,
        home_dir: None,
    })
}

async fn responses_for(
    actor: &mut Actor,
    request_id: &str,
    want: usize,
    within: Duration,
) -> usize {
    let deadline = tokio::time::Instant::now() + within;
    let mut count = 0;
    while count < want {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return count;
        }
        match tokio::time::timeout(left, actor.control_lines.recv()).await {
            Ok(Some(line)) => {
                if line["type"] == "control_response"
                    && line.pointer("/response/request_id").and_then(Value::as_str)
                        == Some(request_id)
                {
                    count += 1;
                }
            }
            _ => return count,
        }
    }
    count
}

/// Extra responses for `request_id` that arrive in the next `within`.
async fn extra_responses(actor: &mut Actor, request_id: &str, within: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + within;
    let mut count = 0;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return count;
        }
        match tokio::time::timeout(left, actor.control_lines.recv()).await {
            Ok(Some(line)) => {
                if line["type"] == "control_response"
                    && line.pointer("/response/request_id").and_then(Value::as_str)
                        == Some(request_id)
                {
                    count += 1;
                }
            }
            _ => return count,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t7a_race_6_duplicate_and_late_replies() {
    run_race("duplicate-and-late-replies", |seed| async move {
        let mut rng = Rng::new(seed);
        let mut actor = Actor::spawn();
        let reply_rx = actor.prompt("p1");
        actor.echo("p1");
        actor
            .control_in_tx
            .send(can_use_tool("toolu_1"))
            .expect("control alive");
        let client_side = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match actor.wire.recv().await {
                    Some(WireItem::Permission { reply, .. }) => return Ok(reply),
                    Some(WireItem::Update(_)) => continue,
                    None => return Err("the actor's outbound channel closed".to_string()),
                }
            }
        })
        .await
        .map_err(|_| "no permission request reached the client".to_string())??;
        let _ = client_side.send(Ok(allow()));
        jitter(&mut rng, 4).await;
        if responses_for(&mut actor, "req-toolu_1", 1, Duration::from_secs(2)).await != 1 {
            return Err("the first answer did not produce exactly one control response".into());
        }
        // The turn ends after the tool call's result.
        actor.feed(json!({"type":"user","parent_tool_use_id":null,
            "message":{"role":"user","content":[{"type":"tool_result",
                "tool_use_id":"toolu_1","content":"ok"}]}}));
        actor.feed(result_frame());
        let settled = settle(reply_rx, Duration::from_secs(5)).await?;
        if settled
            .map_err(|f| format!("the turn failed: {f:?}"))?
            .stop_reason
            != "end_turn"
        {
            return Err("the turn did not end at its result".into());
        }
        // A duplicate answer after the turn ended, and one for a request that
        // never existed: both are ignored without a second control response.
        for request_id in ["req-toolu_1", "req-never"] {
            let (reply, reply_rx) = oneshot::channel();
            actor
                .tx
                .send(Command::PermissionAnswered {
                    value: allow(),
                    ctx: ctx(),
                    request_id: request_id.into(),
                    tool_use_id: "toolu_1".into(),
                    tool_input: json!({"command":"ls"}),
                    reply,
                })
                .expect("actor alive");
            let _ = tokio::time::timeout(Duration::from_secs(2), reply_rx).await;
        }
        jitter(&mut rng, 4).await;
        if extra_responses(&mut actor, "req-toolu_1", Duration::from_millis(30)).await != 0 {
            return Err("a duplicate answer wrote a second control response".into());
        }
        // The actor survived both: it still answers a command.
        let (fg_tx, fg_rx) = oneshot::channel();
        actor
            .tx
            .send(Command::ForegroundState { reply: fg_tx })
            .expect("actor alive after duplicate answers");
        tokio::time::timeout(Duration::from_secs(2), fg_rx)
            .await
            .map_err(|_| "the actor stopped answering after the duplicate answers".to_string())?
            .map_err(|_| "the actor dropped the foreground reply".to_string())?;
        // After close, a late answer finds the actor gone.
        actor.shutdown().await;
        let (reply, _reply_rx) = oneshot::channel();
        if actor
            .tx
            .send(Command::PermissionAnswered {
                value: allow(),
                ctx: ctx(),
                request_id: "req-toolu_1".into(),
                tool_use_id: "toolu_1".into(),
                tool_input: json!({"command":"ls"}),
                reply,
            })
            .is_ok()
        {
            return Err("a closed session still accepts a permission answer".into());
        }
        Ok(())
    })
    .await;
}
