//! T6A: ACP v2 `session/prompt` (M-113, M-114, U-81, U-82, U-86, Q-86).
//!
//! Ported from `src/tests/acp-v2.test.ts` ("ACP v2 prompts", the content-block
//! and audience cases, and the session-update cases). The intake answer is
//! exercised through [`super::prompt::intake_error`] and
//! [`super::prompt::v1_prompt_request`], the two halves the agent composes
//! (`v2/agent.rs`, `agent.rs`'s prompt branch).

use serde_json::json;

use super::prompt::*;
use crate::turn::StopReason;
use crate::turn_events::{
    ForegroundEnd, ForegroundEvent, ForegroundState, ForegroundTracker, JsonRpcError, TurnOutcome,
    TurnUsage,
};

fn text(value: &str) -> serde_json::Value {
    json!([{ "type": "text", "text": value }])
}

fn outcome(stop: StopReason) -> TurnOutcome {
    TurnOutcome {
        stop_reason: stop,
        usage: Some(TurnUsage {
            total_tokens: 11,
            input_tokens: 7,
            output_tokens: 4,
            thought_tokens: None,
            cached_read_tokens: Some(0),
            cached_write_tokens: Some(0),
        }),
        meta: Some(json!({
            "quota": {
                "token_count": {
                    "totalTokens": 11,
                    "inputTokens": 7,
                    "cachedInputTokens": 0,
                    "cachedWriteTokens": 0,
                    "outputTokens": 4,
                    "reasoningOutputTokens": 0,
                },
                "model_usage": [],
            }
        })),
    }
}

// ---------------------------------------------------------------- M-113

/// acp-v2.test.ts: "answers a prompt once Claude Code takes it in, then reports
/// the turn as state" — the answer carries the message id, not a stop reason.
#[test]
fn t6a_answers_a_prompt_with_the_message_id_at_intake() {
    // The request the agent serves: the v2 prompt mapped to v1. The answer's
    // `{ "messageId" }` shape is written by the prompt task (`agent.rs`) and
    // asserted end to end by `t6g_v2_prompt_wire_order`.
    let params = json!({ "sessionId": "s", "prompt": text("hi") });
    let request = v1_prompt_request(&params).expect("a v1 request");
    assert_eq!(request["sessionId"], "s");
    assert_eq!(request["prompt"], text("hi"));
}

/// acp-v2.test.ts: "cancels the running turn after its last output, and a
/// queued prompt with -32800" — a queued prompt a cancel ends before intake.
#[test]
fn t6a_a_prompt_cancelled_before_intake_answers_32800() {
    let error = intake_error(&crate::turn_events::V2IntakeError::Cancelled);
    assert_eq!(i32::from(error.code), -32800);
    assert_eq!(error.message, "Request cancelled");
}

/// acp-v2.test.ts: "answers a queued prompt with the error of a turn that
/// failed before it was taken in" — an ended-before-intake prompt is an
/// internal error naming the stop reason.
#[test]
fn t6a_a_turn_that_ended_before_intake_answers_internal_error() {
    let error = intake_error(&crate::turn_events::V2IntakeError::Ended(
        StopReason::EndTurn,
    ));
    assert_eq!(i32::from(error.code), -32603);
    assert_eq!(
        error.message,
        "Internal error: The turn ended before Claude Code took the prompt in"
    );
    assert_eq!(error.data, Some(json!({ "stopReason": "end_turn" })));
}

/// acp-v2.test.ts: "keeps the auth_required code of a turn that fails for a
/// missing login" and "reports a turn that fails after it was taken in as idle
/// with the error stop reason".
#[test]
fn t6a_a_failure_before_intake_keeps_the_v1_error() {
    let auth = JsonRpcError {
        code: -32000,
        message: "Authentication required".to_string(),
        data: None,
    };
    let error = intake_error(&crate::turn_events::V2IntakeError::Failed(auth));
    assert_eq!(i32::from(error.code), -32000);
    assert_eq!(error.message, "Authentication required");
}

// ---------------------------------------------------------------- M-114

/// acp-v2.test.ts: "rejects a prompt with a content block that v1 cannot
/// express".
#[test]
fn t6a_rejects_a_content_block_that_v1_cannot_express() {
    let params = json!({
        "sessionId": "s",
        "prompt": [{ "type": "_acme/widget", "spec": 1 }],
    });
    let error = v1_prompt_request(&params).expect_err("the block is not expressible");
    assert_eq!(i32::from(error.code), -32602);
    assert_eq!(
        error.message,
        "Invalid params: Content blocks of type _acme/widget are not supported"
    );
    assert_eq!(error.data, Some(json!({ "type": "_acme/widget" })));
}

/// acp-v2.test.ts: "keeps the audience roles of a prompt block that v1 knows".
#[test]
fn t6a_keeps_the_audience_roles_of_a_prompt_block_that_v1_knows() {
    let audience = |roles: serde_json::Value| {
        let params = json!({
            "sessionId": "s",
            "prompt": [{
                "type": "text",
                "text": "hi",
                "annotations": { "audience": roles, "priority": 1 },
            }],
        });
        v1_prompt_request(&params)
            .expect("v1 can express the block")
            .prompt_annotations(0)
    };
    assert_eq!(
        audience(json!(["user", "_robot"])),
        Some(json!({ "audience": ["user"], "priority": 1 }))
    );
    // Only unknown roles: no audience, rather than an audience of nobody.
    assert_eq!(audience(json!(["_robot"])), Some(json!({ "priority": 1 })));
    assert_eq!(
        audience(json!([])),
        Some(json!({ "audience": [], "priority": 1 }))
    );
}

/// acp-v2.test.ts: "passes the audience of a block that has none".
#[test]
fn t6a_a_block_without_annotations_passes_through() {
    let block = json!({ "type": "text", "text": "hi" });
    assert_eq!(v1_content_block(&block).expect("known kind"), block);
}

/// acp-v2.test.ts: "reports a prompt whose block kinds v1 can express" — image,
/// audio, resource link and resource all pass.
#[test]
fn t6a_accepts_every_v1_block_kind() {
    for block in [
        json!({"type":"image","data":"AA==","mimeType":"image/png"}),
        json!({"type":"audio","data":"AA==","mimeType":"audio/wav"}),
        json!({"type":"resource_link","uri":"file:///a","name":"a"}),
        json!({"type":"resource","resource":{"uri":"file:///a","text":"x"}}),
    ] {
        assert_eq!(v1_content_block(&block).expect("known kind"), block);
    }
}

// ---------------------------------------------------------------- U-81

/// acp-v2.test.ts: "reports a cycle after the turn ended as running, then idle
/// with the cycle's usage".
#[test]
fn t6a_reports_running_then_idle_with_usage_and_quota() {
    assert_eq!(
        state_update(&ForegroundEvent::Running),
        json!({ "sessionUpdate": "state_update", "state": "running" })
    );
    assert_eq!(
        state_update(&ForegroundEvent::AwaitingUser),
        json!({ "sessionUpdate": "state_update", "state": "requires_action" })
    );
    assert_eq!(
        state_update(&ForegroundEvent::Ended(outcome(StopReason::EndTurn))),
        json!({
            "sessionUpdate": "state_update",
            "state": "idle",
            "stopReason": "end_turn",
            "usage": {
                "totalTokens": 11,
                "inputTokens": 7,
                "outputTokens": 4,
                "cachedReadTokens": 0,
                "cachedWriteTokens": 0,
            },
            "_meta": {
                "quota": {
                    "token_count": {
                        "totalTokens": 11,
                        "inputTokens": 7,
                        "cachedInputTokens": 0,
                        "cachedWriteTokens": 0,
                        "outputTokens": 4,
                        "reasoningOutputTokens": 0,
                    },
                    "model_usage": [],
                }
            },
        })
    );
}

/// acp-v2.test.ts: "reports a turn that fails after it was taken in as idle
/// with the error stop reason".
#[test]
fn t6a_reports_a_failed_turn_as_idle_error() {
    let error = JsonRpcError {
        code: -32603,
        message: "API Error: 529 Overloaded".to_string(),
        data: None,
    };
    assert_eq!(
        state_update(&ForegroundEvent::Failed(error.clone())),
        json!({
            "sessionUpdate": "state_update",
            "state": "idle",
            "stopReason": "error",
            "error": { "code": -32603, "message": "API Error: 529 Overloaded" },
        })
    );
}

/// acp-v2.test.ts: "reports the running turn of a live session it resumes,
/// before the response" — the bare foreground state.
#[test]
fn t6a_foreground_state_updates() {
    assert_eq!(
        foreground_state_update(ForegroundState::Idle),
        json!({ "sessionUpdate": "state_update", "state": "idle" })
    );
    assert_eq!(
        foreground_state_update(ForegroundState::Running),
        json!({ "sessionUpdate": "state_update", "state": "running" })
    );
    assert_eq!(
        foreground_state_update(ForegroundState::RequiresAction),
        json!({ "sessionUpdate": "state_update", "state": "requires_action" })
    );
}

/// acp-v2.test.ts: "reports idle cancelled" — the idle carries the cancelled
/// stop reason.
#[test]
fn t6a_idle_cancelled_has_no_usage() {
    let outcome = TurnOutcome {
        stop_reason: StopReason::Cancelled,
        usage: None,
        meta: None,
    };
    assert_eq!(
        idle_update(&outcome),
        json!({
            "sessionUpdate": "state_update",
            "state": "idle",
            "stopReason": "cancelled",
        })
    );
}

/// acp-v2.test.ts: "ends a cancelled turn ... " — the tracker reports the end
/// as `idle cancelled`.
#[test]
fn t6a_a_cancelled_turn_ends_the_foreground_work() {
    let mut tracker = ForegroundTracker::new();
    tracker.start(crate::turn_events::ForegroundOwner::Turn("p".into()), false);
    let events = tracker.end(
        &crate::turn_events::ForegroundOwner::Turn("p".into()),
        ForegroundEnd::Ended(TurnOutcome {
            stop_reason: StopReason::Cancelled,
            usage: None,
            meta: None,
        }),
    );
    assert_eq!(
        state_update(&events[0]),
        json!({ "sessionUpdate": "state_update", "state": "idle", "stopReason": "cancelled" })
    );
}

// ---------------------------------------------------------------- U-82

/// acp-v2.test.ts: "fails on a message chunk without a messageId, which v2
/// requires".
#[test]
fn t6a_requires_a_message_id_on_a_chunk() {
    let update = json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": "hi" },
    });
    // The check is the session-update translation's (`v2/session_update.rs`).
    let error = super::session_update::v2_session_update(&update).expect_err("the chunk has no id");
    assert_eq!(i32::from(error.code), -32603);
    assert_eq!(
        error.message,
        "An ACP v2 agent_message_chunk needs a messageId"
    );
    let update = json!({
        "sessionUpdate": "agent_message_chunk",
        "messageId": "msg_1",
        "content": { "type": "text", "text": "hi" },
    });
    let translated = super::session_update::v2_session_update(&update)
        .expect("a chunk with an id")
        .expect("it translates");
    assert_eq!(translated["messageId"], "msg_1");
}

// ---------------------------------------------------------------- U-86

/// acp-v2.test.ts: "fails on a subagent state that the subagents RFD does not
/// define".
#[test]
fn t6a_rejects_a_subagent_state_the_rfd_does_not_define() {
    let error = v2_subagent_state(&json!({ "state": "_paused" })).expect_err("unknown state");
    assert_eq!(
        error.message,
        "The ACP v2 surface does not translate the subagent state _paused"
    );
    let error = v2_subagent_state(&json!({ "state": "idle", "stopReason": "_timeout" }))
        .expect_err("unknown stop reason");
    assert_eq!(
        error.message,
        "The ACP v2 surface does not translate the subagent state idle with the stop reason _timeout"
    );
    assert!(v2_subagent_state(&json!({ "state": "running" })).is_ok());
    assert!(v2_subagent_state(&json!({ "state": "requires_action" })).is_ok());
    assert!(v2_subagent_state(&json!({ "state": "unknown" })).is_ok());
    assert!(v2_subagent_state(&json!({ "state": "idle", "stopReason": "cancelled" })).is_ok());
    assert!(v2_subagent_state(&json!({ "state": "idle", "stopReason": null })).is_ok());
    assert!(v2_subagent_state(&json!({ "state": "idle" })).is_ok());
}

/// acp-v2.test.ts: "reports a subagent's work state on the child's stream as
/// well".
#[test]
fn t6a_reports_a_subagent_state_on_the_child_stream() {
    let failed = json!({
        "state": "idle",
        "stopReason": "error",
        "error": { "code": -32603, "message": "Overloaded" },
    });
    let notification = json!({
        "sessionUpdate": "subagent_update",
        "sessionId": "agent-1",
        "state": failed,
    });
    assert_eq!(
        v2_child_state_update(&notification).expect("valid state"),
        Some(json!({
            "sessionId": "agent-1",
            "update": {
                "sessionUpdate": "state_update",
                "state": "idle",
                "stopReason": "error",
                "error": { "code": -32603, "message": "Overloaded" },
            }
        }))
    );
    // An update of the association that reports no state has no copy.
    assert_eq!(
        v2_child_state_update(&json!({
            "sessionUpdate": "subagent_update",
            "sessionId": "agent-1",
            "title": "X",
        }))
        .expect("no state"),
        None
    );
    assert_eq!(
        v2_child_state_update(&json!({
            "sessionUpdate": "agent_message_chunk",
            "messageId": "m",
            "content": { "type": "text", "text": "hi" },
        }))
        .expect("not a subagent update"),
        None
    );
}

// ---------------------------------------------------------------- B-112

/// acp-v2.test.ts: "replays the history before it answers session/resume,
/// clearing each message first" — the clearing transform lives in
/// [`crate::turn_events::ReplayClearer`]; this pins the v2 prompt path's share
/// of the replay contract (the wiring hook is in lane-A-wiring.patch).
#[test]
fn t6a_replay_clearing_is_per_message_and_per_kind() {
    let mut clearer = crate::turn_events::ReplayClearer::new();
    let user = json!({
        "sessionUpdate": "user_message_chunk",
        "messageId": "uuid-1",
        "content": { "type": "text", "text": "hi" },
    });
    assert_eq!(
        clearer.apply(user.clone()),
        vec![
            json!({"sessionUpdate": "user_message", "messageId": "uuid-1", "content": []}),
            user.clone(),
        ]
    );
    assert_eq!(clearer.apply(user.clone()), vec![user]);
}

/// The `v2` prompt request of a v2 `session/prompt` keeps its other fields
/// (`v1PromptRequest`'s `{...request}`).
#[test]
fn t6a_the_v1_request_keeps_the_other_fields() {
    let params = json!({
        "sessionId": "s",
        "prompt": text("hi"),
        "_meta": { "claudeCode": { "options": {} } },
    });
    let request = v1_prompt_request(&params).expect("a v1 request");
    assert_eq!(request["_meta"]["claudeCode"]["options"], json!({}));
    assert_eq!(request["sessionId"], "s");
}

trait PromptAnnotations {
    fn prompt_annotations(&self, index: usize) -> Option<serde_json::Value>;
}

impl PromptAnnotations for serde_json::Value {
    fn prompt_annotations(&self, index: usize) -> Option<serde_json::Value> {
        self.get("prompt")?
            .as_array()?
            .get(index)?
            .get("annotations")
            .cloned()
    }
}
