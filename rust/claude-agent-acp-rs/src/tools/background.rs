//! Tool-call marks of a command that went to the background
//! (`src/tool-calls/background.ts`, 0.94.0): the `backgrounded` marker the Bash
//! card carries for an AIR client, and the display terminal that keeps running
//! after the Bash call returns.

use agent_client_protocol::schema::v1::ToolCallUpdate;
use serde_json::{json, Value};

use crate::air::with_air_meta;

/// The AIR capability the `backgrounded` marker travels under.
const ASYNC_TASKS_CAPABILITY: &str = "asyncTasks";

/// Marks the Bash `tool_call_update` whose command detached into the background
/// (`backgroundedBashToolCall`).
///
/// A backgrounded Bash call returns as soon as the command is handed off, so the
/// card reaches `completed` while the command itself runs on. ACP has no
/// tool-call status for "still running elsewhere", so this marker lets a client
/// render the card as backgrounded work. It rides the update the tool result
/// already emits, so it costs no extra notification and cannot arrive out of
/// order. Only a client that got the async-task lifecycle (`asyncTasks`) is
/// marked: to any other the marker would promise a card state it can never
/// resolve.
pub fn mark_backgrounded(update: &mut ToolCallUpdate, backgrounded: bool, async_tasks: bool) {
    if !async_tasks || !backgrounded {
        return;
    }
    let existing = update.meta.take().map(Value::Object);
    if let Some(Value::Object(meta)) = Some(with_air_meta(
        existing.as_ref(),
        ASYNC_TASKS_CAPABILITY,
        json!({ "backgrounded": true }),
    )) {
        update.meta = Some(meta);
    }
}

/// Keeps the display terminal of a command that went to the background running:
/// drops `terminal_exit` from the tool result update (`withRunningTerminal`). The
/// Bash call returns as soon as the command detaches, so that exit would mark
/// the terminal as ended while the command still runs; the session sends the exit
/// when the command's task ends. An `_meta` left empty is removed: an empty one
/// would clear what a v2 client stores for the tool call.
pub fn keep_terminal_running(update: &mut ToolCallUpdate) {
    let Some(meta) = update.meta.as_mut() else {
        return;
    };
    if meta.remove("terminal_exit").is_some() && meta.is_empty() {
        update.meta = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ToolCallUpdateFields;

    fn update_with(meta: Value) -> ToolCallUpdate {
        let mut update = ToolCallUpdate::new("toolu_1", ToolCallUpdateFields::new());
        update.meta = meta.as_object().cloned();
        update
    }

    /// T-34 — the marker joins the existing `_meta` under the AIR namespace, only
    /// for a backgrounded call of a client that got the async-task lifecycle.
    #[test]
    fn t3a_backgrounded_marker_joins_the_air_meta() {
        let mut marked = update_with(json!({"claudeCode": {"toolName": "Bash"}}));
        mark_backgrounded(&mut marked, true, true);
        assert_eq!(
            Value::Object(marked.meta.clone().unwrap()),
            json!({
                "claudeCode": {"toolName": "Bash"},
                "jetbrains": {"air": {"version": 1, "asyncTasks": {"backgrounded": true}}},
            })
        );
        for (backgrounded, async_tasks) in [(false, true), (true, false)] {
            let mut plain = update_with(json!({"claudeCode": {"toolName": "Bash"}}));
            mark_backgrounded(&mut plain, backgrounded, async_tasks);
            assert_eq!(
                Value::Object(plain.meta.unwrap()),
                json!({"claudeCode": {"toolName": "Bash"}})
            );
        }
        let mut fresh = ToolCallUpdate::new("toolu_1", ToolCallUpdateFields::new());
        mark_backgrounded(&mut fresh, true, true);
        assert!(fresh.meta.is_some(), "a fresh `_meta` is built");
    }

    /// T-34 — `terminal_exit` is dropped and nothing else; an emptied `_meta`
    /// disappears.
    #[test]
    fn t3a_running_terminal_drops_only_the_exit() {
        let mut update = update_with(json!({
            "claudeCode": {"toolName": "Bash"},
            "terminal_exit": {"terminal_id": "toolu_1", "exit_code": 0, "signal": null},
        }));
        keep_terminal_running(&mut update);
        assert_eq!(
            Value::Object(update.meta.unwrap()),
            json!({"claudeCode": {"toolName": "Bash"}})
        );
        let mut only_exit = update_with(json!({"terminal_exit": {"terminal_id": "toolu_1"}}));
        keep_terminal_running(&mut only_exit);
        assert!(only_exit.meta.is_none(), "an empty `_meta` is removed");
        let mut without = update_with(json!({"claudeCode": {"toolName": "Bash"}}));
        keep_terminal_running(&mut without);
        assert!(without.meta.is_some());
    }
}
