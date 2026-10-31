//! T8B — TEST-MAP classification for the upstream `usage_update computation`,
//! `prompt response quota metadata` and `assembled assistant text fallback`
//! cases of `src/tests/acp-agent.test.ts` (v0.94.0).
//!
//! The frame-level cases drive `handle_session_frame` through the shared
//! [`tests_t2a::Rig`]; the cases that need a real actor (a model switch seeding
//! the context window, then a turn on the new model) drive `run_loop` with a
//! duplex control channel, like `tests_t5a2`.

use super::tests_t2a::Rig;
use super::*;
use crate::session_config::{
    ConfigOption, ConfigOptionsInput, ModelInfo, ModesState, SessionConfigState,
};
use serde_json::json;
use tokio::time::{timeout, Duration};

// ---------------------------------------------------------------- helpers

fn usage_updates(updates: &[Value]) -> Vec<Value> {
    updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "usage_update")
        .cloned()
        .collect()
}

fn assistant_usage(model: &str, usage: Value) -> Value {
    json!({
        "type": "assistant",
        "parent_tool_use_id": null,
        "uuid": "assistant-uuid",
        "message": {
            "id": "msg_usage",
            "model": model,
            "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "usage": usage
        }
    })
}

fn message_start(model: &str, usage: Value) -> Value {
    json!({
        "type": "stream_event",
        "parent_tool_use_id": null,
        "uuid": "stream-uuid",
        "event": {
            "type": "message_start",
            "message": {"id": "msg_usage", "model": model, "usage": usage}
        }
    })
}

fn message_delta(usage: Value) -> Value {
    json!({
        "type": "stream_event",
        "parent_tool_use_id": null,
        "uuid": "stream-uuid",
        "event": {"type": "message_delta", "usage": usage}
    })
}

fn result_with_model_usage(model: &str, window: u64, cost: f64) -> Value {
    json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": false,
        "result": "",
        "total_cost_usd": cost,
        "usage": {"input_tokens": 1, "output_tokens": 1},
        "modelUsage": {model: {"contextWindow": window}}
    })
}

// ---------------------------------------------------------------- usage frames

/// upstream: "used sums all token types as post-turn context occupancy proxy"
#[test]
fn t8b_usage_used_sums_every_token_type() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(assistant_usage(
        "claude-opus-4-20250514",
        json!({
            "input_tokens": 1000,
            "output_tokens": 500,
            "cache_read_input_tokens": 200,
            "cache_creation_input_tokens": 100
        }),
    ));
    rig.feed(result_with_model_usage(
        "claude-opus-4-20250514",
        1_000_000,
        0.01,
    ));
    let updates = usage_updates(&rig.updates());
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["used"], 1800);
}

/// upstream: "coerces null input/output tokens so wire `used` is never null"
#[test]
fn t8b_usage_null_tokens_never_reach_the_wire() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(assistant_usage(
        "claude-opus-4-20250514",
        json!({
            "input_tokens": null,
            "output_tokens": null,
            "cache_read_input_tokens": 200,
            "cache_creation_input_tokens": 100
        }),
    ));
    rig.feed(result_with_model_usage(
        "claude-opus-4-20250514",
        1_000_000,
        0.01,
    ));
    let updates = usage_updates(&rig.updates());
    assert!(!updates.is_empty());
    for update in &updates {
        let wire = serde_json::to_string(update).expect("serialize");
        assert!(!wire.contains("\"used\":null"), "{wire}");
        assert_eq!(update["used"], 300);
    }
}

/// upstream: "stream_event message_start emits usage_update before result"
#[test]
fn t8b_usage_message_start_emits_before_result() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(message_start(
        "claude-opus-4-20250514",
        json!({
            "input_tokens": 1000,
            "output_tokens": 500,
            "cache_read_input_tokens": 200,
            "cache_creation_input_tokens": 100
        }),
    ));
    rig.feed(result_with_model_usage(
        "claude-opus-4-20250514",
        1_000_000,
        0.01,
    ));
    let updates = usage_updates(&rig.updates());
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert_eq!(updates[0]["used"], 1800);
    assert_eq!(updates[0]["size"], 200_000);
    assert!(updates[0].get("cost").is_none(), "{:?}", updates[0]);
    assert_eq!(updates[1]["used"], 1800);
    assert_eq!(updates[1]["size"], 1_000_000);
    assert!(updates[1].get("cost").is_some(), "{:?}", updates[1]);
}

/// upstream: "includes the effective model id on usage_update notifications"
#[test]
fn t8b_usage_carries_the_model_under_meta() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(message_start(
        "claude-sonnet-4-20250514",
        json!({
            "input_tokens": 1000,
            "output_tokens": 0,
            "cache_read_input_tokens": 200,
            "cache_creation_input_tokens": 100
        }),
    ));
    rig.feed(message_delta(json!({"output_tokens": 500})));
    rig.feed(result_with_model_usage(
        "claude-sonnet-4-20250514",
        200_000,
        0.01,
    ));
    let updates = usage_updates(&rig.updates());
    assert_eq!(updates.len(), 3, "{updates:?}");
    for update in &updates {
        assert!(update.get("model").is_none(), "{update}");
        assert_eq!(
            update["_meta"]["_claude/model"], "claude-sonnet-4-20250514",
            "{update}"
        );
    }
}

/// upstream: "stream_event message_delta patches previous snapshot"
#[test]
fn t8b_usage_message_delta_patches_the_snapshot() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(message_start(
        "claude-opus-4-20250514",
        json!({
            "input_tokens": 1000,
            "output_tokens": 0,
            "cache_read_input_tokens": 200,
            "cache_creation_input_tokens": 100
        }),
    ));
    rig.feed(message_delta(json!({"output_tokens": 500})));
    rig.feed(result_with_model_usage(
        "claude-opus-4-20250514",
        1_000_000,
        0.01,
    ));
    let updates = usage_updates(&rig.updates());
    assert_eq!(updates.len(), 3, "{updates:?}");
    assert_eq!(updates[0]["used"], 1300);
    assert!(updates[0].get("cost").is_none());
    assert_eq!(updates[1]["used"], 1800);
    assert!(updates[1].get("cost").is_none());
    assert_eq!(updates[2]["used"], 1800);
    assert!(updates[2].get("cost").is_some());
}

/// upstream: "mid-stream size is inferred from a 1M model name before the first
/// result"
#[test]
fn t8b_usage_mid_stream_1m_model_name() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(message_start(
        "claude-opus-4-6-1m",
        json!({
            "input_tokens": 2000,
            "output_tokens": 1000,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0
        }),
    ));
    rig.feed(result_with_model_usage(
        "claude-opus-4-6-1m",
        1_000_000,
        0.02,
    ));
    let updates = usage_updates(&rig.updates());
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert_eq!(updates[0]["size"], 1_000_000);
    assert_eq!(updates[1]["size"], 1_000_000);
}

/// upstream: "duplicate stream_event totals do not re-emit usage_update"
#[test]
fn t8b_usage_duplicate_totals_do_not_reemit() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(message_start(
        "claude-opus-4-20250514",
        json!({
            "input_tokens": 1000,
            "output_tokens": 500,
            "cache_read_input_tokens": 200,
            "cache_creation_input_tokens": 100
        }),
    ));
    rig.feed(message_delta(json!({"output_tokens": 500})));
    rig.feed(result_with_model_usage(
        "claude-opus-4-20250514",
        1_000_000,
        0.01,
    ));
    let updates = usage_updates(&rig.updates());
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert_eq!(updates[0]["used"], 1800);
    assert!(updates[0].get("cost").is_none());
    assert_eq!(updates[1]["used"], 1800);
    assert!(updates[1].get("cost").is_some());
}

/// upstream: "mid-stream size uses the session's learned context window"
#[test]
fn t8b_usage_mid_stream_uses_the_learned_window() {
    let mut rig = Rig::new();
    rig.ext.usage.context_window = 1_000_000;
    rig.echo("p1");
    rig.updates();
    rig.feed(message_start(
        "claude-opus-4-20250514",
        json!({
            "input_tokens": 2000,
            "output_tokens": 1000,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0
        }),
    ));
    rig.feed(result_with_model_usage(
        "claude-opus-4-20250514",
        1_000_000,
        0.02,
    ));
    let updates = usage_updates(&rig.updates());
    assert_eq!(updates.len(), 2, "{updates:?}");
    assert_eq!(updates[0]["size"], 1_000_000);
    assert_eq!(updates[1]["size"], 1_000_000);
}

/// upstream: "result with no matching modelUsage preserves the learned window"
#[test]
fn t8b_usage_result_without_a_matching_model_preserves_the_window() {
    let mut rig = Rig::new();
    rig.ext.usage.context_window = 1_000_000;
    rig.ext.usage.authoritative = true;
    rig.echo("p1");
    rig.updates();
    rig.feed(json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": false,
        "result": "",
        "usage": {"input_tokens": 1, "output_tokens": 1},
        "modelUsage": {"claude-haiku-4-5-20251001": {"contextWindow": 200_000}}
    }));
    let updates = usage_updates(&rig.updates());
    assert!(updates.is_empty(), "{updates:?}");
    assert_eq!(rig.ext.usage.context_window, 1_000_000);
}

/// upstream: "non-usage stream events do not re-emit usage_update"
#[test]
fn t8b_usage_non_usage_stream_events_do_not_reemit() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(message_start(
        "claude-opus-4-20250514",
        json!({"input_tokens": 1000, "output_tokens": 0,
               "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}),
    ));
    rig.feed(json!({
        "type": "stream_event",
        "parent_tool_use_id": null,
        "event": {"type": "content_block_delta", "index": 0,
                  "delta": {"type": "text_delta", "text": "hello"}}
    }));
    rig.feed(json!({
        "type": "stream_event",
        "parent_tool_use_id": null,
        "event": {"type": "message_stop"}
    }));
    let updates = usage_updates(&rig.updates());
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["used"], 1000);
}

/// upstream: "size reflects the current model's context window, not min across
/// all"
#[test]
fn t8b_usage_size_follows_the_current_model_not_the_minimum() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(assistant_usage(
        "claude-opus-4-20250514",
        json!({"input_tokens": 10, "output_tokens": 5,
               "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}),
    ));
    rig.feed(json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": false,
        "result": "",
        "total_cost_usd": 0.01,
        "usage": {"input_tokens": 10, "output_tokens": 5},
        "modelUsage": {
            "claude-opus-4-20250514": {"contextWindow": 1_000_000},
            "claude-haiku-3-5": {"contextWindow": 200_000}
        }
    }));
    let updates = usage_updates(&rig.updates());
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["size"], 1_000_000);
}

// ---------------------------------------------------------------- quota meta

fn quota_meta(reply: &PromptReply) -> &Value {
    reply.quota.as_ref().expect("the reply carries _meta.quota")
}

fn quota_rows(reply: &PromptReply) -> &Vec<Value> {
    quota_meta(reply)["quota"]["model_usage"]
        .as_array()
        .expect("model_usage rows")
}

fn token_count(input: i64, output: i64, cached_read: i64, cached_write: i64) -> Value {
    json!({
        "totalTokens": input + output + cached_read + cached_write,
        "inputTokens": input,
        "cachedInputTokens": cached_read,
        "cachedWriteTokens": cached_write,
        "outputTokens": output,
        "reasoningOutputTokens": 0
    })
}

fn model_row(input: i64, output: i64) -> Value {
    json!({
        "inputTokens": input,
        "outputTokens": output,
        "cacheReadInputTokens": 0,
        "cacheCreationInputTokens": 0
    })
}

/// upstream: "breaks a turn's spend down per model"
#[test]
fn t8b_quota_breaks_a_turn_down_per_model() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": false,
        "result": "",
        "usage": {"input_tokens": 10, "output_tokens": 5,
                  "cache_read_input_tokens": 3, "cache_creation_input_tokens": 2},
        "modelUsage": {
            "claude-opus-5[1m]": {"inputTokens": 10, "outputTokens": 5,
                "cacheReadInputTokens": 3, "cacheCreationInputTokens": 2},
            "claude-haiku-4-5": model_row(4, 1)
        }
    }));
    let reply = rig
        .replies
        .remove("p1")
        .expect("echoed")
        .try_recv()
        .expect("settled");
    let reply = reply.expect("success settles");
    assert_eq!(
        quota_meta(&reply)["quota"]["token_count"],
        token_count(10, 5, 3, 2)
    );
    let rows = quota_rows(&reply);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0]["model"], "claude-opus-5[1m]");
    assert_eq!(rows[0]["token_count"], token_count(10, 5, 3, 2));
    assert_eq!(rows[1]["model"], "claude-haiku-4-5");
    assert_eq!(rows[1]["token_count"], token_count(4, 1, 0, 0));
}

/// upstream: "reports each turn's own share of the running per-model total"
#[test]
fn t8b_quota_reports_the_turn_increment() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": false,
        "result": "",
        "usage": {"input_tokens": 10, "output_tokens": 5},
        "modelUsage": {"claude-opus-5": model_row(10, 5)}
    }));
    let first = rig
        .replies
        .remove("p1")
        .expect("echoed")
        .try_recv()
        .expect("settled");
    first.expect("first settles");

    rig.echo("p2");
    rig.updates();
    rig.feed(json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": false,
        "result": "",
        "usage": {"input_tokens": 15, "output_tokens": 7},
        "modelUsage": {"claude-opus-5": model_row(25, 12)}
    }));
    let second = rig
        .replies
        .remove("p2")
        .expect("echoed")
        .try_recv()
        .expect("settled");
    let second = second.expect("second settles");
    assert_eq!(
        quota_meta(&second)["quota"]["token_count"],
        token_count(15, 7, 0, 0)
    );
    let rows = quota_rows(&second);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["model"], "claude-opus-5");
    assert_eq!(rows[0]["token_count"], token_count(15, 7, 0, 0));
}

/// upstream: "keeps an autonomous cycle's spend out of the next turn"
#[test]
fn t8b_quota_autonomous_spend_is_not_charged_to_the_next_turn() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": false,
        "result": "",
        "usage": {"input_tokens": 10, "output_tokens": 5},
        "modelUsage": {"claude-opus-5": model_row(10, 5)}
    }));
    let first = rig
        .replies
        .remove("p1")
        .expect("echoed")
        .try_recv()
        .expect("settled");
    first.expect("first settles");
    rig.updates();

    // The autonomous followup advances the baseline but is not a user turn.
    rig.feed(json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": false,
        "result": "",
        "origin": {"kind": "task-notification"},
        "usage": {"input_tokens": 4, "output_tokens": 1},
        "modelUsage": {"claude-opus-5": model_row(14, 6)}
    }));
    rig.feed(json!({"type": "system", "subtype": "session_state_changed", "state": "idle"}));
    rig.updates();

    rig.echo("p2");
    rig.updates();
    rig.feed(json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": false,
        "result": "",
        "usage": {"input_tokens": 6, "output_tokens": 3},
        "modelUsage": {"claude-opus-5": model_row(20, 9)}
    }));
    let second = rig
        .replies
        .remove("p2")
        .expect("echoed")
        .try_recv()
        .expect("settled");
    let second = second.expect("second settles");
    assert_eq!(
        quota_meta(&second)["quota"]["token_count"],
        token_count(6, 3, 0, 0)
    );
    let rows = quota_rows(&second);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["token_count"], token_count(6, 3, 0, 0));
}

/// upstream: "treats a rewound running total as the turn's own spend"
#[test]
fn t8b_quota_rewound_running_total_is_the_turn_share() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": false,
        "result": "",
        "usage": {"input_tokens": 30, "output_tokens": 9},
        "modelUsage": {"claude-opus-5": model_row(30, 9)}
    }));
    let first = rig
        .replies
        .remove("p1")
        .expect("echoed")
        .try_recv()
        .expect("settled");
    first.expect("first settles");

    rig.echo("p2");
    rig.updates();
    rig.feed(json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": false,
        "result": "",
        "usage": {"input_tokens": 8, "output_tokens": 2},
        "modelUsage": {"claude-opus-5": model_row(8, 2)}
    }));
    let second = rig
        .replies
        .remove("p2")
        .expect("echoed")
        .try_recv()
        .expect("settled");
    let second = second.expect("second settles");
    let rows = quota_rows(&second);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["token_count"], token_count(8, 2, 0, 0));
}

/// upstream: "seeds the per-model baseline from a resumed session's first
/// result"
#[test]
fn t8b_quota_resumed_first_result_seeds_the_baseline() {
    let mut rig = Rig::new();
    rig.machine.mark_resumed();
    rig.echo("p1");
    rig.updates();
    rig.feed(message_start(
        "claude-opus-5",
        json!({"input_tokens": 10, "output_tokens": 5,
               "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}),
    ));
    rig.updates();
    rig.feed(json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": false,
        "result": "",
        "usage": {"input_tokens": 10, "output_tokens": 5},
        "modelUsage": {
            "claude-opus-5[1m]": model_row(510, 205),
            "claude-haiku-4-5": model_row(900, 300)
        }
    }));
    let first = rig
        .replies
        .remove("p1")
        .expect("echoed")
        .try_recv()
        .expect("settled");
    let first = first.expect("first settles");
    let rows = quota_rows(&first);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["model"], "claude-opus-5[1m]");
    assert_eq!(rows[0]["token_count"], token_count(10, 5, 0, 0));

    rig.echo("p2");
    rig.updates();
    rig.feed(json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": false,
        "result": "",
        "usage": {"input_tokens": 15, "output_tokens": 7},
        "modelUsage": {
            "claude-opus-5[1m]": model_row(525, 212),
            "claude-haiku-4-5": model_row(900, 300)
        }
    }));
    let second = rig
        .replies
        .remove("p2")
        .expect("echoed")
        .try_recv()
        .expect("settled");
    let second = second.expect("second settles");
    let rows = quota_rows(&second);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["model"], "claude-opus-5[1m]");
    assert_eq!(rows[0]["token_count"], token_count(15, 7, 0, 0));
}

/// upstream: "lists no per-model rows for a resumed first result with no
/// top-level model"
#[test]
fn t8b_quota_resumed_first_result_without_a_model_lists_no_rows() {
    let mut rig = Rig::new();
    rig.machine.mark_resumed();
    rig.echo("p1");
    rig.updates();
    rig.feed(json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": false,
        "result": "",
        "usage": {"input_tokens": 10, "output_tokens": 5},
        "modelUsage": {"claude-opus-5": model_row(510, 205)}
    }));
    let reply = rig
        .replies
        .remove("p1")
        .expect("echoed")
        .try_recv()
        .expect("settled");
    let reply = reply.expect("settles");
    assert_eq!(
        quota_meta(&reply)["quota"]["token_count"],
        token_count(10, 5, 0, 0)
    );
    assert!(
        quota_rows(&reply).is_empty(),
        "an empty list beats charging the pre-resume history: {:?}",
        quota_rows(&reply)
    );
}

// ------------------------------------------------------ actor: model switch

/// A running actor with its outbound updates and stream, for the cases that
/// need a real `SetConfigOption` / `ModelApplied` switch before the turn.
struct Harness {
    tx: mpsc::UnboundedSender<Command>,
    updates: mpsc::UnboundedReceiver<SessionOutbound>,
    stream: mpsc::UnboundedSender<Value>,
}

impl Harness {
    fn new(config: SessionConfigState) -> Self {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (control_in_tx, control_in_rx) = mpsc::unbounded_channel::<Value>();
        let (tx, commands) = mpsc::unbounded_channel::<Command>();
        let control = Control::spawn(a, control_in_rx, ControlOptions { on_request: None });
        let (update_tx, updates) = mpsc::unbounded_channel::<SessionOutbound>();
        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(b);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        tokio::spawn(run_loop(
            commands,
            tx.clone(),
            stream_rx,
            control,
            control_in_tx,
            update_tx,
            Duration::from_secs(30),
            "s".into(),
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
        let _ = tx.send(Command::InitConfig {
            config: Box::new(config),
        });
        Self {
            tx,
            updates,
            stream: stream_tx,
        }
    }

    async fn feed(&mut self, frame: Value) -> Vec<Value> {
        let _ = self.stream.send(frame);
        self.collect().await
    }

    async fn collect(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        loop {
            match timeout(Duration::from_secs(2), self.updates.recv()).await {
                Ok(Some(SessionOutbound::Flush(ack))) => {
                    let _ = ack.send(());
                }
                Ok(Some(SessionOutbound::Update(notif))) => {
                    out.push(serde_json::to_value(&notif).expect("serialize"));
                }
                Ok(Some(SessionOutbound::RawUpdate(value))) => {
                    out.push(value.get("update").cloned().unwrap_or(value));
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        out
    }

    async fn apply_model(&mut self, model: &str) {
        let (reply, reply_rx) = oneshot::channel();
        let _ = self.tx.send(Command::ModelApplied {
            model_val: model.into(),
            result: Ok(()),
            reply,
            notify_client: false,
            inline: false,
        });
        let mut reply_rx = reply_rx;
        loop {
            tokio::select! {
                reply = &mut reply_rx => {
                    reply
                        .expect("the model switch replies")
                        .expect("the switch succeeds");
                    return;
                }
                item = self.updates.recv() => {
                    match item {
                        Some(SessionOutbound::Flush(ack)) => {
                            let _ = ack.send(());
                        }
                        Some(SessionOutbound::Update(_)) | Some(SessionOutbound::RawUpdate(_)) => {}
                        Some(_) => {}
                        None => panic!("the actor ended during the switch"),
                    }
                }
            }
        }
    }
}

fn model(value: &str, description: &str) -> ModelInfo {
    ModelInfo {
        value: value.into(),
        display_name: value.into(),
        description: description.into(),
        resolved_model: None,
        supports_auto_mode: Some(true),
        supports_effort: Some(false),
        supported_effort_levels: None,
        supports_fast_mode: None,
    }
}

fn config_with(models: Vec<ModelInfo>, current: &str) -> SessionConfigState {
    let selection = crate::session_model::get_available_models(
        &models, &models, None, None, false, None, false,
    );
    let modes = ModesState {
        current_mode_id: "default".into(),
        available_modes: Vec::new(),
    };
    let mut config = SessionConfigState {
        modes,
        available_models: selection.available_models,
        current_model_id: current.into(),
        model_infos: models,
        config_options: Vec::new(),
        current_effort: None,
        effort_pinned: None,
        applied_effort: None,
        allow_bypass: false,
        is_air: false,
        ultracode: false,
        pre_plan_mode: None,
        agents: Vec::new(),
        current_agent: "default".into(),
        fast_mode_enabled: false,
        fast_mode_disabled_reason: None,
        pending_auto_fallback_warning: false,
        provider_cache_key: String::new(),
        effort_settings_override: None,
    };
    let configs: Vec<ConfigOption> =
        crate::session_config::build_config_options_with(&ConfigOptionsInput {
            modes: &config.modes,
            available_models: &config.available_models,
            current_model_id: &config.current_model_id,
            model_infos: &config.model_infos,
            current_effort: None,
            agents: &config.agents,
            current_agent: &config.current_agent,
            fast_mode: None,
            use_recommended_value: false,
        });
    config.config_options = configs;
    config
}

/// upstream: "switching to a 1M model seeds the context window from the
/// heuristic"
#[tokio::test]
async fn t8b_usage_model_switch_seeds_a_1m_window() {
    let models = vec![
        model("claude-sonnet-4-6", "Fast model"),
        model("claude-opus-4-6-1m", "Extended context model"),
    ];
    let mut harness = Harness::new(config_with(models, "claude-sonnet-4-6"));
    harness.collect().await;
    harness.apply_model("claude-opus-4-6-1m").await;
    harness.collect().await;

    let updates = harness
        .feed(message_start(
            "claude-opus-4-6-1m",
            json!({"input_tokens": 2000, "output_tokens": 1000,
                   "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}),
        ))
        .await;
    let updates = usage_updates(&updates);
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["size"], 1_000_000);
}

/// upstream: "infers the 1M window from a model's description when the ID lacks
/// a 1m token (issue #596)"
#[tokio::test]
async fn t8b_usage_model_switch_reads_the_description_for_1m() {
    let models = vec![
        model("claude-sonnet-4-6", "Fast model"),
        model("default", "Opus 4.7 with 1M context"),
    ];
    let mut harness = Harness::new(config_with(models, "claude-sonnet-4-6"));
    harness.collect().await;
    harness.apply_model("default").await;
    harness.collect().await;

    let updates = harness
        .feed(message_start(
            "default",
            json!({"input_tokens": 10, "output_tokens": 5,
                   "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}),
        ))
        .await;
    let updates = usage_updates(&updates);
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["size"], 1_000_000);
}

/// upstream: "after model switch, size updates to the new model's window" and
/// "after switching back to original model, size returns to original window"
#[tokio::test]
async fn t8b_usage_model_switch_updates_and_restores_the_window() {
    let models = vec![
        model("claude-sonnet-4-6", "Fast model"),
        model("claude-opus-4-6-1m", "Extended context model"),
    ];
    let mut harness = Harness::new(config_with(models, "claude-sonnet-4-6"));
    harness.collect().await;

    // The first model's window (200k default).
    let first = harness
        .feed(message_start(
            "claude-sonnet-4-6",
            json!({"input_tokens": 1, "output_tokens": 1,
                   "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}),
        ))
        .await;
    assert_eq!(usage_updates(&first)[0]["size"], 200_000);
    harness.collect().await;

    harness.apply_model("claude-opus-4-6-1m").await;
    harness.collect().await;
    let switched = harness
        .feed(message_start(
            "claude-opus-4-6-1m",
            json!({"input_tokens": 2, "output_tokens": 2,
                   "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}),
        ))
        .await;
    assert_eq!(usage_updates(&switched)[0]["size"], 1_000_000);

    harness.apply_model("claude-sonnet-4-6").await;
    harness.collect().await;
    let back = harness
        .feed(message_start(
            "claude-sonnet-4-6",
            json!({"input_tokens": 3, "output_tokens": 3,
                   "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}),
        ))
        .await;
    assert_eq!(usage_updates(&back)[0]["size"], 200_000);
}

/// upstream: "prefix-matches when assistant model has date suffix but modelUsage
/// key does not" and "prefix-matches when modelUsage key has date suffix but
/// assistant model does not"
#[test]
fn t8b_usage_prefix_matches_model_date_suffixes() {
    for (assistant, usage_key) in [
        ("claude-opus-4-6-20250514", "claude-opus-4-6"),
        ("claude-opus-4-6", "claude-opus-4-6-20250514"),
    ] {
        let mut rig = Rig::new();
        rig.echo("p1");
        rig.updates();
        rig.feed(assistant_usage(
            assistant,
            json!({"input_tokens": 100, "output_tokens": 50,
                   "cache_read_input_tokens": 20, "cache_creation_input_tokens": 10}),
        ));
        rig.feed(json!({
            "type": "result",
            "subtype": "success",
            "stop_reason": "end_turn",
            "is_error": false,
            "result": "",
            "total_cost_usd": 0.01,
            "usage": {"input_tokens": 100, "output_tokens": 50},
            "modelUsage": {usage_key: {"contextWindow": 1_000_000}}
        }));
        let updates = usage_updates(&rig.updates());
        assert_eq!(updates.len(), 1, "{assistant}: {updates:?}");
        assert_eq!(updates[0]["size"], 1_000_000, "{assistant}");
    }
}

/// upstream: "synthetic assistant messages do not override lastAssistantModel"
#[test]
fn t8b_usage_synthetic_assistant_keeps_the_model() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(assistant_usage(
        "claude-opus-4-20250514",
        json!({"input_tokens": 100, "output_tokens": 50,
               "cache_read_input_tokens": 20, "cache_creation_input_tokens": 10}),
    ));
    rig.feed(json!({
        "type": "assistant",
        "parent_tool_use_id": null,
        "uuid": "synthetic-uuid",
        "message": {
            "model": "<synthetic>",
            "content": [{"type": "text", "text": "compacted"}],
            "usage": {"input_tokens": 0, "output_tokens": 0,
                      "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
        }
    }));
    rig.feed(json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": false,
        "result": "",
        "total_cost_usd": 0.01,
        "usage": {"input_tokens": 100, "output_tokens": 50},
        "modelUsage": {"claude-opus-4-20250514": {"contextWindow": 1_000_000}}
    }));
    let updates = usage_updates(&rig.updates());
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["size"], 1_000_000);
}

/// upstream: "compact_boundary uses post_tokens without waiting for
/// getContextUsage"
#[test]
fn t8b_usage_compact_boundary_keeps_the_learned_window() {
    let mut rig = Rig::new();
    rig.ext.usage.context_window = 1_000_000;
    rig.ext.usage.authoritative = true;
    rig.ext.air = crate::air::AirClient::from_client_capabilities(&json!({
        "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": []}}}
    }));
    rig.ext.t2a.compaction = crate::context_compaction::ContextCompactionLifecycle::new(
        "s",
        crate::context_compaction::CompactionPresentation::ToolCall,
        true,
    );
    rig.echo("p1");
    rig.updates();
    rig.feed(json!({
        "type": "system",
        "subtype": "compact_boundary",
        "uuid": "compact-boundary",
        "compact_metadata": {
            "trigger": "auto",
            "pre_tokens": 180000,
            "post_tokens": 12345,
            "duration_ms": 2500
        }
    }));
    let updates = rig.updates();
    let usage = usage_updates(&updates);
    assert_eq!(usage.len(), 1, "{updates:?}");
    assert_eq!(usage[0]["used"], 12345);
    assert_eq!(usage[0]["size"], 1_000_000);
    assert_eq!(rig.ext.usage.context_window, 1_000_000);
    let tool_call = updates
        .iter()
        .find(|u| u["sessionUpdate"] == "tool_call")
        .unwrap_or_else(|| panic!("the compaction tool call: {updates:?}"));
    assert_eq!(tool_call["toolCallId"], "compact-boundary");
    assert_eq!(tool_call["title"], "Compact conversation");
    assert_eq!(tool_call["kind"], "think");
    assert_eq!(tool_call["status"], "completed");
    assert_eq!(
        tool_call["_meta"]["jetbrains"]["air"]["contextCompaction"],
        json!({"version": 1, "trigger": "automatic", "preTokens": 180000,
               "postTokens": 12345, "durationMs": 2500})
    );
}

/// upstream: "compact_boundary falls back to used:0 when post_tokens is
/// unavailable"
#[test]
fn t8b_usage_compact_boundary_without_post_tokens_uses_zero() {
    let mut rig = Rig::new();
    rig.ext.usage.context_window = 200_000;
    rig.echo("p1");
    rig.updates();
    rig.feed(json!({
        "type": "system",
        "subtype": "compact_boundary",
        "uuid": "compact-boundary"
    }));
    let updates = usage_updates(&rig.updates());
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["used"], 0);
    assert_eq!(updates[0]["size"], 200_000);
}

/// upstream: "caches the turn's authoritative window under the resolved id and
/// serves it on a later switch, with no getContextUsage IPC"
#[tokio::test]
async fn t8b_usage_model_switch_reads_the_cached_window() {
    const PROVIDER: &str = "t8b-cache-probe";
    const RESOLVED: &str = "claude-cachehit-probe-9[1m]";
    crate::session_model::cache_context_window(
        &crate::session_model::context_window_cache_key(PROVIDER, RESOLVED),
        777_000,
    );
    let alias = ModelInfo {
        value: "probe-alias".into(),
        display_name: "Probe".into(),
        description: "probe model".into(),
        resolved_model: Some(RESOLVED.into()),
        supports_auto_mode: Some(true),
        supports_effort: Some(false),
        supported_effort_levels: None,
        supports_fast_mode: None,
    };
    let mut config = config_with(
        vec![model("claude-sonnet-4-6", "Fast model"), alias],
        "claude-sonnet-4-6",
    );
    config.provider_cache_key = PROVIDER.into();
    let mut harness = Harness::new(config);
    harness.collect().await;
    harness.apply_model("probe-alias").await;
    harness.collect().await;

    let updates = harness
        .feed(message_start(
            "claude-cachehit-probe-9",
            json!({"input_tokens": 1, "output_tokens": 1,
                   "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}),
        ))
        .await;
    let updates = usage_updates(&updates);
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(
        updates[0]["size"], 777_000,
        "the switch seeds from the cross-session cache: {updates:?}"
    );
}

/// upstream: "scopes the window cache per provider: a switch on a different
/// provider does not read another provider's window"
#[test]
fn t8b_usage_window_cache_is_scoped_per_provider() {
    const RESOLVED: &str = "claude-provkey-probe[1m]";
    let info = ModelInfo {
        value: "probe-alias".into(),
        display_name: "Probe".into(),
        description: "probe model".into(),
        resolved_model: Some(RESOLVED.into()),
        ..Default::default()
    };
    crate::session_model::cache_context_window(
        &crate::session_model::context_window_cache_key("t8b-prov-a", RESOLVED),
        500_000,
    );
    let a =
        crate::session_model::immediate_context_window("t8b-prov-a", "probe-alias", Some(&info));
    assert_eq!(a.size, 500_000);
    assert!(a.authoritative);
    let b =
        crate::session_model::immediate_context_window("t8b-prov-b", "probe-alias", Some(&info));
    assert_eq!(
        b.size, 1_000_000,
        "provider B falls back to the 1m heuristic"
    );
    assert!(!b.authoritative);
}

/// upstream: "does not let the message_start heuristic clobber an authoritative
/// 200k window"
#[test]
fn t8b_usage_authoritative_window_survives_message_start() {
    let mut rig = Rig::new();
    rig.ext.usage.context_window = 200_000;
    rig.ext.usage.authoritative = true;
    rig.echo("p1");
    rig.updates();
    rig.feed(message_start(
        "claude-opus-4-6-1m",
        json!({"input_tokens": 10, "output_tokens": 5,
               "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}),
    ));
    let updates = usage_updates(&rig.updates());
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["size"], 200_000);
    assert_eq!(rig.ext.usage.context_window, 200_000);
}

// ------------------------------------------------------ assembled text fallback

fn chunk_texts(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|u| u["content"]["text"].as_str().map(str::to_string))
        .collect()
}

fn thought_texts(updates: &[Value]) -> Vec<String> {
    updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_thought_chunk")
        .filter_map(|u| u["content"]["text"].as_str().map(str::to_string))
        .collect()
}

fn reply_chunks(rig: &mut Rig, uuid: &str) -> Vec<String> {
    let mut reply = rig.replies.remove(uuid).expect("prompt was echoed");
    match reply.try_recv().ok() {
        Some(Ok(reply)) => reply
            .updates
            .iter()
            .filter_map(|n| match &n.update {
                SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn text_delta(text: &str) -> Value {
    json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"content_block_delta","index":0,
        "delta":{"type":"text_delta","text":text}}})
}

fn thinking_delta(text: &str) -> Value {
    json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"content_block_delta","index":0,
        "delta":{"type":"thinking_delta","thinking":text}}})
}

fn stream_start(id: &str) -> Value {
    json!({"type":"stream_event","parent_tool_use_id":null,
        "event":{"type":"message_start","message":{"id":id,"model":"claude-sonnet-4-5"}}})
}

fn consolidated(id: &str, content: Value) -> Value {
    json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":id,"model":"claude-sonnet-4-5","role":"assistant","content":content}})
}

/// upstream: "dedupes streamed text even when the stream arrives before the
/// user echo"
#[test]
fn t8b_assembled_text_stream_before_echo_dedupes() {
    let mut rig = Rig::new();
    rig.feed(stream_start("msg-streamed"));
    rig.feed(text_delta("hello "));
    rig.feed(text_delta("world"));
    rig.echo("p1");
    rig.feed(consolidated(
        "msg-streamed",
        json!([{"type":"text","text":"hello world"}]),
    ));
    let updates = rig.updates();
    assert_eq!(chunk_texts(&updates), vec!["hello ", "world"]);
}

/// upstream: "dedupes streamed text when the user echo activates the turn
/// mid-message, between a thinking and a text block"
#[test]
fn t8b_assembled_text_echo_mid_message_dedupes() {
    let mut rig = Rig::new();
    rig.feed(stream_start("msg-mixed"));
    rig.feed(thinking_delta("private reasoning"));
    rig.echo("p1");
    rig.feed(text_delta("Starting now."));
    rig.feed(consolidated(
        "msg-mixed",
        json!([
            {"type":"thinking","thinking":"private reasoning"},
            {"type":"text","text":"Starting now."}
        ]),
    ));
    let updates = rig.updates();
    assert_eq!(chunk_texts(&updates), vec!["Starting now."]);
    assert_eq!(thought_texts(&updates), vec!["private reasoning"]);
}

/// upstream: "does not re-emit the next turn's text after a turn is cancelled
/// mid-stream"
#[test]
fn t8b_assembled_text_cancelled_turn_does_not_leak() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.feed(stream_start("msg-1"));
    rig.feed(text_delta("Hello "));
    rig.feed(text_delta("there"));
    rig.machine.cancel();
    // The cancelled turn's consolidated message is dropped whole.
    rig.feed(consolidated(
        "msg-1",
        json!([{"type":"text","text":"Hello there"}]),
    ));
    rig.feed(json!({"type":"system","subtype":"session_state_changed","state":"idle"}));
    rig.updates();

    rig.echo("p2");
    rig.feed(stream_start("msg-2"));
    rig.feed(text_delta("Second answer"));
    rig.feed(consolidated(
        "msg-2",
        json!([{"type":"text","text":"Second answer"}]),
    ));
    let updates = rig.updates();
    let second: Vec<String> = chunk_texts(&updates)
        .into_iter()
        .filter(|text| text == "Second answer")
        .collect();
    assert_eq!(second, vec!["Second answer"]);
}

/// upstream: "drops the streamed text record of a finished subagent"
#[test]
fn t8b_assembled_text_finished_subagent_forgets_its_record() {
    let mut rig = Rig::new();
    rig.map_state.set_forward_subagent_text(true);
    rig.echo("p1");
    rig.feed(
        json!({"type":"stream_event","parent_tool_use_id":"tool_use_1",
        "event":{"type":"content_block_delta","index":0,
        "delta":{"type":"text_delta","text":"nested "}}}),
    );
    rig.feed(
        json!({"type":"system","subtype":"task_notification","task_id":"agent-1",
        "tool_use_id":"tool_use_1","status":"completed","summary":"","output_file":""}),
    );
    rig.feed(json!({"type":"assistant","parent_tool_use_id":"tool_use_1",
        "message":{"id":"msg-subagent","model":"claude-sonnet-4-5","role":"assistant",
        "content":[{"type":"text","text":"nested report"}]}}));
    let updates = rig.updates();
    assert_eq!(chunk_texts(&updates), vec!["nested ", "nested report"]);
}

/// upstream: "does not re-emit the result text when the echo lands mid-message"
#[test]
fn t8b_assembled_text_result_not_reemitted_mid_message() {
    let mut rig = Rig::new();
    rig.machine.enqueue(Turn::new("p1".into(), false));
    rig.feed(stream_start("msg-streamed"));
    rig.feed(text_delta("**3**"));
    rig.echo("p1");
    rig.feed(consolidated(
        "msg-streamed",
        json!([{"type":"text","text":"**3**"}]),
    ));
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"**3**","usage":{"input_tokens":1,"output_tokens":0}}));
    let updates = rig.updates();
    assert_eq!(chunk_texts(&updates), vec!["**3**"]);
    assert_eq!(reply_chunks(&mut rig, "p1"), Vec::<String>::new());
}

/// upstream: "leaves the result text alone when the turn generated output
/// tokens"
#[test]
fn t8b_assembled_text_result_with_output_tokens_is_ignored() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"**3**","usage":{"input_tokens":1,"output_tokens":5}}));
    let updates = rig.updates();
    assert!(chunk_texts(&updates).is_empty(), "{updates:?}");
    assert_eq!(reply_chunks(&mut rig, "p1"), Vec::<String>::new());
}

/// upstream: "does not forward the result text of a task-notification followup"
#[test]
fn t8b_assembled_text_task_notification_result_is_ignored() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"background output",
        "origin":{"kind":"task-notification"},
        "usage":{"input_tokens":1,"output_tokens":0}}));
    rig.updates();
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"","usage":{"input_tokens":1,"output_tokens":0}}));
    let updates = rig.updates();
    assert!(chunk_texts(&updates).is_empty(), "{updates:?}");
    assert_eq!(reply_chunks(&mut rig, "p1"), Vec::<String>::new());
}

/// upstream: "forwards the result text when the backend omits output_tokens"
#[test]
fn t8b_assembled_text_missing_output_tokens_still_falls_back() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"**3**","usage":{"input_tokens":1}}));
    assert_eq!(reply_chunks(&mut rig, "p1"), vec!["**3**"]);
}

/// upstream: "forwards the result text when only subagent content preceded it"
#[test]
fn t8b_assembled_text_subagent_content_does_not_suppress_the_result() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.feed(json!({"type":"assistant","parent_tool_use_id":"tool-1",
        "message":{"id":"msg-sub","model":"claude-sonnet-4-5","role":"assistant",
        "content":[{"type":"image","source":{"type":"base64","data":"aGk=",
            "media_type":"image/png"}}]}}));
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"**3**","usage":{"input_tokens":1,"output_tokens":0}}));
    assert_eq!(reply_chunks(&mut rig, "p1"), vec!["**3**"]);
}

/// upstream: "emits one compaction tool lifecycle and ignores duplicate
/// terminal status"
#[test]
fn t8b_assembled_text_one_compaction_lifecycle() {
    let mut rig = Rig::new();
    rig.ext.air = crate::air::AirClient::from_client_capabilities(&json!({
        "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": []}}}
    }));
    rig.ext.t2a.compaction = crate::context_compaction::ContextCompactionLifecycle::new(
        "s",
        crate::context_compaction::CompactionPresentation::ToolCall,
        true,
    );
    rig.echo("p1");
    rig.feed(
        json!({"type":"system","subtype":"status","status":"compacting",
        "uuid":"compact-start"}),
    );
    for uuid in ["compact-failed-1", "compact-failed-2"] {
        rig.feed(json!({"type":"system","subtype":"status","status":null,
            "compact_result":"failed","compact_error":"summary rejected","uuid":uuid}));
    }
    rig.feed(json!({"type":"system","subtype":"local_command_output",
        "content":"summary rejected","uuid":"compact-local-output"}));
    rig.feed(json!({"type":"system","subtype":"local_command_output",
        "content":"additional diagnostic","uuid":"compact-distinct-local-output"}));
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"summary rejected",
        "usage":{"input_tokens":1,"output_tokens":0}}));
    let updates = rig.updates();
    let tools: Vec<&Value> = updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "tool_call" || u["sessionUpdate"] == "tool_call_update")
        .collect();
    assert_eq!(tools.len(), 2, "{updates:?}");
    assert_eq!(tools[1]["status"], "failed");
    assert_eq!(
        tools[1]["content"],
        json!([{"type":"content","content":{"type":"text",
            "text":"Compaction failed: summary rejected"}}])
    );
    assert_eq!(chunk_texts(&updates), vec!["additional diagnostic"]);
}

/// upstream: "does not repeat a failed compaction error delivered as an
/// assistant message"
#[test]
fn t8b_assembled_text_failed_compaction_assistant_repeat_is_swallowed() {
    let mut rig = Rig::new();
    rig.ext.air = crate::air::AirClient::from_client_capabilities(&json!({
        "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": []}}}
    }));
    rig.ext.t2a.compaction = crate::context_compaction::ContextCompactionLifecycle::new(
        "s",
        crate::context_compaction::CompactionPresentation::ToolCall,
        true,
    );
    rig.echo("p1");
    rig.feed(
        json!({"type":"system","subtype":"status","status":"compacting",
        "uuid":"compact-start"}),
    );
    rig.feed(json!({"type":"system","subtype":"status","status":null,
        "compact_result":"failed","compact_error":"Not enough messages to compact.",
        "uuid":"compact-failed"}));
    rig.feed(consolidated(
        "compact-error",
        json!([{"type":"text","text":"Not enough messages to compact."}]),
    ));
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"Not enough messages to compact.",
        "usage":{"input_tokens":1,"output_tokens":0}}));
    let updates = rig.updates();
    assert!(chunk_texts(&updates).is_empty(), "{updates:?}");
    assert!(updates
        .iter()
        .any(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "failed"));
}

/// upstream: "does not replay stale local-command output after a successful
/// compaction"
#[test]
fn t8b_assembled_text_stale_local_command_after_compaction() {
    let mut rig = Rig::new();
    rig.ext.air = crate::air::AirClient::from_client_capabilities(&json!({
        "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": []}}}
    }));
    rig.ext.t2a.compaction = crate::context_compaction::ContextCompactionLifecycle::new(
        "s",
        crate::context_compaction::CompactionPresentation::ToolCall,
        true,
    );
    rig.echo("p1");
    rig.feed(
        json!({"type":"system","subtype":"status","status":"compacting",
        "uuid":"compact-start"}),
    );
    rig.feed(json!({"type":"system","subtype":"status","status":null,
        "compact_result":"success","uuid":"compact-completed"}));
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"stale-command-output","model":"<synthetic>","role":"assistant",
        "content":[{"type":"text","text":"Not enough messages to compact."}]}}));
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"Not enough messages to compact.",
        "usage":{"input_tokens":1,"output_tokens":0}}));
    let updates = rig.updates();
    assert!(chunk_texts(&updates).is_empty(), "{updates:?}");
    assert!(updates
        .iter()
        .any(|u| u["sessionUpdate"] == "tool_call_update" && u["status"] == "completed"));
}

/// upstream: "preserves the model response after multiple compactions in one
/// turn"
#[test]
fn t8b_assembled_text_multiple_compactions_preserve_the_answer() {
    let mut rig = Rig::new();
    rig.echo("p1");
    for (start, done) in [
        ("compact-start-1", "compact-completed-1"),
        ("compact-start-2", "compact-completed-2"),
    ] {
        rig.feed(
            json!({"type":"system","subtype":"status","status":"compacting",
            "uuid":start}),
        );
        rig.feed(json!({"type":"system","subtype":"status","status":null,
            "compact_result":"success","uuid":done}));
    }
    rig.feed(consolidated(
        "answer-after-compactions",
        json!([{"type":"text","text":"final answer"}]),
    ));
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"","usage":{"input_tokens":1,"output_tokens":1}}));
    let updates = rig.updates();
    assert_eq!(chunk_texts(&updates), vec!["final answer"]);
}

/// upstream: "the SDK session id of a fresh Claude context > sends every update
/// to the ACP session"
#[test]
fn t8b_updates_carry_the_acp_session_id_not_the_sdk_one() {
    let mut rig = Rig::new();
    rig.echo("p1");
    let sdk = "sdk-session-after-clear";
    for frame in [
        json!({"type":"tool_progress","tool_use_id":"toolu_slow","tool_name":"Bash",
            "parent_tool_use_id":null,"elapsed_time_seconds":3,"uuid":"u1","session_id":sdk}),
        json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning",
            "resetsAt":1700000000,"utilization":0.9},"uuid":"u2","session_id":sdk}),
        json!({"type":"system","subtype":"compact_boundary",
            "compact_metadata":{"trigger":"auto","pre_tokens":1000,"post_tokens":100},
            "uuid":"u3","session_id":sdk}),
        json!({"type":"system","subtype":"local_command_output","content":"Local output",
            "uuid":"u4","session_id":sdk}),
        json!({"type":"result","subtype":"success","stop_reason":"end_turn",
            "is_error":false,"result":"","usage":{"input_tokens":1,"output_tokens":1},
            "uuid":"u5","session_id":sdk}),
    ] {
        rig.feed(frame);
    }
    let mut seen = Vec::new();
    while let Ok(item) = rig.rx.try_recv() {
        let value = match item {
            SessionOutbound::Update(notif) => serde_json::to_value(&notif).expect("serialize"),
            SessionOutbound::RawUpdate(value) => value,
            _ => continue,
        };
        if let Some(id) = value.get("sessionId").and_then(Value::as_str) {
            seen.push(id.to_string());
        }
    }
    assert!(!seen.is_empty(), "the frames produced updates");
    assert!(
        seen.iter().all(|id| id == "s"),
        "every update is addressed to the ACP session id: {seen:?}"
    );
}

/// upstream: "records the notice against the queued turn when the hook blocks
/// before any echo"
#[test]
fn t8b_notice_is_recorded_on_a_queued_turn_before_any_echo() {
    let mut rig = Rig::new();
    rig.ext.caps.notices = true;
    let (reply_tx, reply_rx) = oneshot::channel();
    rig.pending.insert("p1".to_string(), reply_tx);
    rig.replies.insert("p1".to_string(), reply_rx);
    rig.machine.enqueue(Turn::new("p1".into(), false));
    let blocked =
        "UserPromptSubmit operation blocked by hook:\nhook says no\n\nOriginal prompt: 1+2";
    rig.feed(
        json!({"type":"system","subtype":"informational","content":blocked,
        "level":"warning"}),
    );
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":blocked,"usage":{"input_tokens":1,"output_tokens":0}}));
    rig.feed(json!({"type":"system","subtype":"session_state_changed","state":"idle"}));
    let updates = rig.updates();
    assert!(chunk_texts(&updates).is_empty(), "{updates:?}");
    let notices: Vec<&Value> = updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "notice")
        .collect();
    assert_eq!(notices.len(), 1, "{updates:?}");
    assert_eq!(notices[0]["severity"], "warning");
    assert_eq!(
        notices[0]["title"],
        "UserPromptSubmit operation blocked by hook:"
    );
    assert_eq!(
        notices[0]["description"],
        "hook says no\n\nOriginal prompt: 1+2"
    );
}

// ---------------------------------------------------------------- G4 / F11

/// `session-config-options.test.ts` > "seeds the window from text inference on
/// model switch": the new model's `resolvedModel` carries the `[1m]` hint, so
/// the switch seeds 1,000,000 before any IPC.
#[tokio::test]
async fn g4_f11_model_switch_seeds_the_window_from_text_inference() {
    let target = ModelInfo {
        value: "claude-sonnet-4-6".into(),
        display_name: "Sonnet".into(),
        description: String::new(),
        resolved_model: Some("claude-sonnet-5[1m]".into()),
        supports_auto_mode: Some(true),
        supports_effort: Some(false),
        supported_effort_levels: None,
        supports_fast_mode: None,
    };
    let models = vec![model("claude-opus-4-5", "Most capable"), target];
    let mut harness = Harness::new(config_with(models, "claude-opus-4-5"));
    harness.collect().await;
    harness.apply_model("claude-sonnet-4-6").await;
    harness.collect().await;

    let updates = harness
        .feed(message_start(
            "claude-sonnet-4-6",
            json!({"input_tokens": 10, "output_tokens": 5,
                   "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}),
        ))
        .await;
    let updates = usage_updates(&updates);
    assert_eq!(updates.len(), 1, "{updates:?}");
    assert_eq!(updates[0]["size"], 1_000_000);
}

/// `session-config-options.test.ts` > "falls back to the default window when
/// inference misses, then refines it in the background": the switch seeds the
/// default 200k non-authoritative window, and the background `getContextUsage`
/// answer refines it and marks it authoritative.
#[test]
fn g4_f11_default_window_is_refined_in_the_background() {
    let info = ModelInfo {
        value: "claude-sonnet-4-6".into(),
        display_name: "Sonnet".into(),
        description: String::new(),
        resolved_model: None,
        supports_auto_mode: Some(true),
        supports_effort: Some(false),
        supported_effort_levels: None,
        supports_fast_mode: None,
    };
    let seed = crate::session_model::immediate_context_window(
        "g4-f11-refine-provider",
        "claude-sonnet-4-6",
        Some(&info),
    );
    assert_eq!(seed.size, crate::usage::DEFAULT_CONTEXT_WINDOW);
    assert!(!seed.authoritative);

    let mut usage = crate::usage::UsageState {
        context_window: seed.size,
        authoritative: seed.authoritative,
        ..Default::default()
    };
    assert!(apply_context_window_answer(
        &mut usage,
        "claude-sonnet-4-6",
        967_000,
        "claude-sonnet-4-6",
        0,
        0
    ));
    assert_eq!(usage.context_window, 967_000);
    assert!(usage.authoritative);
}
