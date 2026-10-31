//! The ACP `systemPrompt` session option and AIR `customInstructions`
//! (rust-air-gaps Decision 10), a port of `dist/client-system-prompt.js` plus
//! `airCustomInstructions`.
//!
//! Pure functions only: validation happens before any process is spawned (Node
//! order: after `validate_cwd`), and the result is turned into the two
//! `initialize` control_request fields the SDK would send.

use agent_client_protocol::Error;
use serde_json::{json, Value};

/// How a client `systemPrompt` combines with the built-in prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    /// Append to the built-in `claude_code` prompt.
    Append,
    /// Replace the built-in prompt.
    Override,
}

/// A validated client `systemPrompt`: mode plus the text blocks joined by a
/// blank line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPrompt {
    pub mode: PromptMode,
    pub text: String,
}

/// JS-style rendering of a JSON value inside a template literal.
fn js_display(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// `RequestError.invalidParams(data, msg)`: code -32602, message
/// `Invalid params: <msg>`, plus `data` (Decision 9).
fn invalid_params(message: &str, data: Value) -> Error {
    Error::new(-32602, format!("Invalid params: {message}")).data(data)
}

/// A thrown JS `TypeError`: `-32603` with `data:{details}`.
fn type_error(details: &str) -> Error {
    Error::internal_error().data(json!({ "details": details }))
}

/// Port of `clientSystemPrompt`: `None` for an absent / null `systemPrompt`,
/// else the validated mode and joined text. Errors are `-32602`:
/// an unsupported mode, or a non-text content block.
pub fn client_system_prompt(system_prompt: Option<&Value>) -> Result<Option<ClientPrompt>, Error> {
    let Some(prompt) = system_prompt.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let mode_value = prompt.get("mode");
    let mode = match mode_value.and_then(Value::as_str) {
        Some("append") => PromptMode::Append,
        Some("override") => PromptMode::Override,
        _ => {
            let shown = js_display(mode_value);
            let data = match mode_value {
                Some(mode) => json!({ "systemPrompt": { "mode": mode } }),
                None => json!({ "systemPrompt": {} }),
            };
            return Err(invalid_params(
                &format!("Unsupported systemPrompt mode: {shown}"),
                data,
            ));
        }
    };
    // Node iterates `content` with `for…of` and reads `block.type`: a missing /
    // non-iterable `content` and a null block throw a `TypeError` (-32603),
    // not an `invalid_params` (a string iterates as one-character blocks).
    let blocks: Vec<Value> = match prompt.get("content") {
        Some(Value::Array(blocks)) => blocks.clone(),
        Some(Value::String(s)) => s.chars().map(|c| Value::String(c.to_string())).collect(),
        _ => return Err(type_error("content is not iterable")),
    };
    let mut parts: Vec<String> = Vec::new();
    for block in &blocks {
        if block.is_null() {
            return Err(type_error(
                "Cannot read properties of null (reading 'type')",
            ));
        }
        let block_type = block.get("type");
        if block_type.and_then(Value::as_str) != Some("text") {
            let shown = js_display(block_type);
            let data = match block_type {
                Some(t) => json!({ "systemPrompt": { "content": { "type": t } } }),
                None => json!({ "systemPrompt": { "content": {} } }),
            };
            return Err(invalid_params(
                &format!("systemPrompt content must be text, but received a {shown} block"),
                data,
            ));
        }
        parts.push(
            block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        );
    }
    Ok(Some(ClientPrompt {
        mode,
        text: parts.join("\n\n"),
    }))
}

/// Port of `airCustomInstructions`: `_meta.jetbrains.air.customInstructions`
/// when it is a string. Ungated: any client may send it.
pub fn air_custom_instructions(meta: Option<&Value>) -> Option<String> {
    meta?
        .get("jetbrains")?
        .get("air")?
        .get("customInstructions")?
        .as_str()
        .map(String::from)
}

/// The SDK's translation of the options into the `initialize` control_request
/// fields `(systemPrompt, appendSystemPrompt)`:
///
/// - no client prompt: `customInstructions` alone becomes `appendSystemPrompt`
///   (an empty string still counts as present);
/// - `override`: `systemPrompt: [text]` where text joins the client text and
///   the custom instructions (empty parts dropped);
/// - `append`: the same joined text as `appendSystemPrompt`, omitted when empty.
pub fn sdk_initialize_fields(
    prompt: Option<&ClientPrompt>,
    custom_instructions: Option<&str>,
) -> (Option<Value>, Option<String>) {
    let Some(prompt) = prompt else {
        return (None, custom_instructions.map(String::from));
    };
    let text = [prompt.text.as_str(), custom_instructions.unwrap_or("")]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    match prompt.mode {
        PromptMode::Override => (Some(json!([text])), None),
        PromptMode::Append => (None, (!text.is_empty()).then_some(text)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_block(text: &str) -> Value {
        json!({"type": "text", "text": text})
    }

    #[test]
    fn inv_39_system_prompt_validation() {
        // Absent / null -> no prompt.
        assert_eq!(client_system_prompt(None).unwrap(), None);
        assert_eq!(client_system_prompt(Some(&Value::Null)).unwrap(), None);

        // Valid modes; blocks join with a blank line.
        let ok = client_system_prompt(Some(&json!({
            "mode": "append",
            "content": [text_block("a"), text_block("b")]
        })))
        .unwrap()
        .unwrap();
        assert_eq!(
            ok,
            ClientPrompt {
                mode: PromptMode::Append,
                text: "a\n\nb".into()
            }
        );
        let over = client_system_prompt(Some(&json!({"mode": "override", "content": []})))
            .unwrap()
            .unwrap();
        assert_eq!((over.mode, over.text.as_str()), (PromptMode::Override, ""));

        // Unsupported mode: -32602 + data {systemPrompt:{mode}}.
        let bad = client_system_prompt(Some(&json!({"mode": "weird", "content": []}))).unwrap_err();
        assert_eq!(i32::from(bad.code), -32602);
        assert_eq!(
            bad.message,
            "Invalid params: Unsupported systemPrompt mode: weird"
        );
        assert_eq!(bad.data, Some(json!({"systemPrompt": {"mode": "weird"}})));

        // Missing mode renders like JS `undefined`.
        let missing = client_system_prompt(Some(&json!({"content": []}))).unwrap_err();
        assert_eq!(
            missing.message,
            "Invalid params: Unsupported systemPrompt mode: undefined"
        );

        // Non-text block.
        let image = client_system_prompt(Some(&json!({
            "mode": "append",
            "content": [text_block("ok"), {"type": "image", "data": "x"}]
        })))
        .unwrap_err();
        assert_eq!(i32::from(image.code), -32602);
        assert_eq!(
            image.message,
            "Invalid params: systemPrompt content must be text, but received a image block"
        );
        assert_eq!(
            image.data,
            Some(json!({"systemPrompt": {"content": {"type": "image"}}}))
        );
    }

    #[test]
    fn inv_39_sdk_fields() {
        let append = ClientPrompt {
            mode: PromptMode::Append,
            text: "client".into(),
        };
        let over = ClientPrompt {
            mode: PromptMode::Override,
            text: "client".into(),
        };
        let empty = ClientPrompt {
            mode: PromptMode::Append,
            text: String::new(),
        };
        // No client prompt.
        assert_eq!(sdk_initialize_fields(None, None), (None, None));
        assert_eq!(
            sdk_initialize_fields(None, Some("custom")),
            (None, Some("custom".into()))
        );
        assert_eq!(
            sdk_initialize_fields(None, Some("")),
            (None, Some(String::new()))
        );
        // Append: joined with a blank line; empty text omitted.
        assert_eq!(
            sdk_initialize_fields(Some(&append), None),
            (None, Some("client".into()))
        );
        assert_eq!(
            sdk_initialize_fields(Some(&append), Some("custom")),
            (None, Some("client\n\ncustom".into()))
        );
        assert_eq!(sdk_initialize_fields(Some(&empty), None), (None, None));
        assert_eq!(
            sdk_initialize_fields(Some(&empty), Some("custom")),
            (None, Some("custom".into()))
        );
        // Override: `systemPrompt: [text]`, custom instructions follow.
        assert_eq!(
            sdk_initialize_fields(Some(&over), None),
            (Some(json!(["client"])), None)
        );
        assert_eq!(
            sdk_initialize_fields(Some(&over), Some("custom")),
            (Some(json!(["client\n\ncustom"])), None)
        );
    }

    /// Node's `for (const block of content)` / `block.type` throw a `TypeError`
    /// (-32603 `data:{details}`) for a missing / non-iterable `content` and a
    /// null block — not an `invalid_params`. The messages are V8's.
    #[test]
    fn inv_39_type_errors_mirror_node() {
        for content in [None, Some(json!(null)), Some(json!(5)), Some(json!({}))] {
            let mut prompt = json!({"mode": "append"});
            if let Some(c) = content {
                prompt["content"] = c;
            }
            let err = client_system_prompt(Some(&prompt)).unwrap_err();
            assert_eq!(i32::from(err.code), -32603, "{prompt}");
            assert_eq!(
                err.data,
                Some(json!({"details": "content is not iterable"}))
            );
        }
        let null_block = json!({"mode": "append", "content": [null]});
        let err = client_system_prompt(Some(&null_block)).unwrap_err();
        assert_eq!(i32::from(err.code), -32603);
        assert_eq!(
            err.data,
            Some(json!({"details": "Cannot read properties of null (reading 'type')"}))
        );
        // A string iterates as one-character blocks, each of type `undefined`.
        let string_content = json!({"mode": "append", "content": "ab"});
        let err = client_system_prompt(Some(&string_content)).unwrap_err();
        assert_eq!(i32::from(err.code), -32602);
        assert_eq!(
            err.message,
            "Invalid params: systemPrompt content must be text, but received a undefined block"
        );
    }

    #[test]
    fn inv_39_custom_instructions_reads_only_the_air_string() {
        let meta = json!({"jetbrains": {"air": {"customInstructions": "be terse"}}});
        assert_eq!(
            air_custom_instructions(Some(&meta)).as_deref(),
            Some("be terse")
        );
        assert_eq!(air_custom_instructions(None), None);
        let not_string = json!({"jetbrains": {"air": {"customInstructions": 3}}});
        assert_eq!(air_custom_instructions(Some(&not_string)), None);
        let other = json!({"customInstructions": "x"});
        assert_eq!(air_custom_instructions(Some(&other)), None);
    }
}
