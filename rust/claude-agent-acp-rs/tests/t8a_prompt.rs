//! T8A — `promptToClaude`'s slash-command handling: a built-in command is
//! forwarded verbatim, an MCP command gets its `mcp:` prefix removed.

use claude_agent_acp_rs::agent::prompt_to_claude;
use serde_json::json;

#[test]
fn t8a_builtin_slash_command_is_unchanged() {
    let params = json!({
        "sessionId": "test",
        "prompt": [{"type": "text", "text": "/compact args"}],
    });
    let message = prompt_to_claude(&params, "test", "u");
    assert_eq!(
        message["message"]["content"],
        json!([{"text": "/compact args", "type": "text"}])
    );
}

#[test]
fn t8a_mcp_prefix_is_removed_from_a_slash_command() {
    let params = json!({
        "sessionId": "test",
        "prompt": [{"type": "text", "text": "/mcp:server:name args"}],
    });
    let message = prompt_to_claude(&params, "test", "u");
    assert_eq!(
        message["message"]["content"],
        json!([{"text": "/server:name (MCP) args", "type": "text"}])
    );
}
