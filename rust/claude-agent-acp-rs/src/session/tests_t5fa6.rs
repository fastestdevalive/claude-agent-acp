//! T5FA6: a second refusal replayed after EOF keeps its update ahead of the
//! next frame.
//!
//! `eof_seen` is cleared only when a fresh child is adopted, so a switch
//! started by a replayed `model_refusal_fallback` (after the first switch
//! settled `Closed` at EOF) still holds the frames behind it: its
//! `config_option_update` is published before the frame queued after it, as
//! Node's awaited inline sync does (`acp-agent.ts:6702`).
//!
//! The rig is the real actor loop from `tests_t5fa5`.

use super::tests_t5fa5::pin_clamp_actor;
use super::*;

fn rv_text(text: &str) -> Value {
    json!({"type": "assistant", "parent_tool_use_id": null,
        "message": {"id": format!("m-{text}"), "role": "assistant",
            "content": [{"type": "text", "text": text}]}})
}

/// Second refusal fallback replayed after EOF: S2's `config_option_update`
/// must precede the frame queued behind it (as Node's inline await does).
#[tokio::test]
async fn t5fa6_second_refusal_after_eof_keeps_its_update_ahead_of_the_next_frame() {
    for i in 0..10 {
        let mut actor = pin_clamp_actor();
        actor.start_external_switch("claude-sonnet-4-6", true);
        let request = actor.next_request().await;
        assert_eq!(request["request"]["subtype"], "set_permission_mode");
        actor.rv_send(json!({"type":"system","subtype":"model_refusal_fallback",
            "original_model":"claude-sonnet-4-6","fallback_model":"claude-haiku-4-5",
            "direction":"retry"}));
        actor.rv_send(rv_text("after"));
        let _ = actor.drain_for(Duration::from_millis(100)).await;
        actor.close_stream();
        let after = actor.drain_for(Duration::from_secs(2)).await;
        let cfg: Vec<usize> = after
            .iter()
            .enumerate()
            .filter(|(_, u)| u["update"]["sessionUpdate"] == "config_option_update")
            .map(|(index, _)| index)
            .collect();
        let ch = after.iter().position(|u| {
            u["update"]["sessionUpdate"] == "agent_message_chunk"
                && u["update"]["content"]["text"] == "after"
        });
        assert_eq!(cfg.len(), 2, "iter {i}: two switch updates: {after:?}");
        assert!(ch.is_some(), "iter {i}: the frame runs: {after:?}");
        assert!(
            cfg[1] < ch.unwrap(),
            "iter {i}: S2 update precedes 'after': cfg={cfg:?} ch={ch:?} {after:?}"
        );
    }
}
