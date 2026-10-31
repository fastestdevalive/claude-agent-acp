//! The first prompt of a transcript, extracted the way the SDK does for a
//! session title (upstream `session-index/first-prompt.ts`; the SDK's `rf`,
//! `ti` and `jP`).
//!
//! - Only main-chain user records count: no tool result, meta or compact
//!   summary record.
//! - Each text (the string content, or every text block) is expanded from its
//!   `<pasted_content>` blocks and flattened to one line.
//! - A slash command (`<command-name>/init</command-name>`) is only a fallback
//!   while a real prompt may follow; `<bash-input>x</bash-input>` is `! x`; a
//!   text that starts with another tag or with an interrupt marker is skipped.
//! - A prompt longer than 200 characters is cut and gets an ellipsis.
//! - Without any text, an image or a document prompt is `Image` or `Document`.

use serde_json::Value;

use super::js_text::{cut_utf16, is_js_ws, js_trim, utf16_len};

const MAX_PROMPT_LENGTH: usize = 200;
const PASTED_OPEN: &str = "<pasted_content id=\"";

fn is_pasted_id(value: &str) -> bool {
    value.len() == 4
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `text` with each `<pasted_content id="xxxx">` block replaced by its body.
fn expand_pasted_content(text: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let mut done = 0usize;
    let mut from = 0usize;
    let mut expanded = false;
    let bytes = text.as_bytes();
    while let Some(rel) = text.get(from..).and_then(|t| t.find(PASTED_OPEN)) {
        let open = from + rel;
        let id_start = open + PASTED_OPEN.len();
        let id = text.get(id_start..id_start + 4).unwrap_or("");
        if !is_pasted_id(id) || !text[id_start + 4..].starts_with("\">\n") {
            from = id_start;
            continue;
        }
        let body_start = id_start + 4 + 3;
        let close = format!("</pasted_content id=\"{id}\">");
        // `text.indexOf("\n" + close, bodyStart - 1) + 1`
        let Some(found) = text
            .get(body_start - 1..)
            .and_then(|t| t.find(&format!("\n{close}")))
        else {
            break;
        };
        let close_at = body_start - 1 + found + 1;
        let mut start = open;
        for _ in 0..2 {
            if start > done && bytes[start - 1] == b'\n' {
                start -= 1;
            }
        }
        if start > done {
            parts.push(&text[done..start]);
        }
        done = close_at + close.len();
        for _ in 0..2 {
            if bytes.get(done) == Some(&b'\n') {
                done += 1;
            }
        }
        parts.push(&text[body_start..close_at - 1]);
        expanded = true;
        from = done;
    }
    if !expanded {
        return text.to_string();
    }
    if done < text.len() {
        parts.push(&text[done..]);
    }
    parts.concat()
}

/// `<command-name>(.*?)</command-name>`: the first command name in `text`
/// (`text` holds no newline by then).
fn command_name(text: &str) -> Option<&str> {
    let open = "<command-name>";
    let start = text.find(open)? + open.len();
    let end = text[start..].find("</command-name>")?;
    Some(&text[start..start + end])
}

/// `<bash-input>([\s\S]*?)</bash-input>`.
fn bash_input(text: &str) -> Option<&str> {
    let open = "<bash-input>";
    let start = text.find(open)? + open.len();
    let end = text[start..].find("</bash-input>")?;
    Some(&text[start..start + end])
}

/// `^(?:\s*<[a-z][\w-]*[\s>]|\[Request interrupted by user[^\]]*\])`.
fn is_skipped_text(text: &str) -> bool {
    let trimmed = text.trim_start_matches(is_js_ws);
    if let Some(after) = trimmed.strip_prefix('<') {
        let mut chars = after.chars();
        if chars.next().is_some_and(|c| c.is_ascii_lowercase()) {
            for c in chars {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    continue;
                }
                if c == '>' || is_js_ws(c) {
                    return true;
                }
                break;
            }
        }
    }
    if let Some(after) = text.strip_prefix("[Request interrupted by user") {
        return after.contains(']');
    }
    false
}

/// The prompt of one user record, or `None` when it has none. A slash command is
/// recorded in `command_fallback` instead.
pub fn prompt_of(entry: &Value, command_fallback: &mut String) -> Option<String> {
    if entry.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    if entry.get("isMeta") == Some(&Value::Bool(true))
        || entry.get("isCompactSummary") == Some(&Value::Bool(true))
    {
        return None;
    }
    let message = entry.get("message").filter(|m| m.is_object())?;
    let mut texts: Vec<&str> = Vec::new();
    match message.get("content") {
        Some(Value::String(text)) => texts.push(text),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                if !block.is_object() {
                    continue;
                }
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_result") => return None,
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            texts.push(text);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    for raw in texts {
        let flat = expand_pasted_content(raw).replace('\n', " ");
        let text = js_trim(&flat);
        if text.is_empty() {
            continue;
        }
        if let Some(command) = command_name(text) {
            if command_fallback.is_empty() {
                *command_fallback = command.to_string();
            }
            continue;
        }
        if let Some(bash) = bash_input(text) {
            return Some(format!("! {}", js_trim(bash)));
        }
        if is_skipped_text(text) {
            continue;
        }
        if utf16_len(text) > MAX_PROMPT_LENGTH {
            return Some(format!("{}…", js_trim(cut_utf16(text, MAX_PROMPT_LENGTH))));
        }
        return Some(text.to_string());
    }
    None
}

/// Whether a user record is a prompt the user sent: one with prompt text
/// ([`prompt_of`]), or one with an image or a document, which the SDK titles
/// `Image` or `Document` when it has no prompt text. A tool result, a meta or
/// compact summary record, an interrupt and a slash command are none.
pub fn is_user_prompt(entry: &Value) -> bool {
    if prompt_of(entry, &mut String::new()).is_some() {
        return true;
    }
    if entry.get("type").and_then(Value::as_str) != Some("user")
        || entry.get("isMeta") == Some(&Value::Bool(true))
        || entry.get("isCompactSummary") == Some(&Value::Bool(true))
    {
        return false;
    }
    let Some(content) = entry.pointer("/message/content").and_then(Value::as_array) else {
        return false;
    };
    let mut media = false;
    for block in content {
        if !block.is_object() {
            continue;
        }
        match block.get("type").and_then(Value::as_str) {
            Some("tool_result") => return false,
            Some("image" | "document") => media = true,
            _ => {}
        }
    }
    media
}

fn is_user_line(line: &str) -> bool {
    line.contains("\"type\":\"user\"") || line.contains("\"type\": \"user\"")
}

/// The first prompt of the head, else the first slash command, else "".
pub fn first_prompt(head: &str) -> String {
    let mut fallback = String::new();
    for line in head.split('\n') {
        if !is_user_line(line) || line.contains("\"tool_result\"") {
            continue;
        }
        if line.contains("\"isMeta\":true") || line.contains("\"isMeta\": true") {
            continue;
        }
        if line.contains("\"isCompactSummary\":true") || line.contains("\"isCompactSummary\": true")
        {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if !entry.is_object() {
            continue;
        }
        if let Some(prompt) = prompt_of(&entry, &mut fallback) {
            return prompt;
        }
    }
    fallback
}

/// `Image` or `Document` for a first user record that holds one, else "".
pub fn media_prompt(head: &str) -> &'static str {
    for line in head.split('\n') {
        if !is_user_line(line) || line.contains("\"tool_result\"") {
            continue;
        }
        if line.contains("\"isMeta\":true") || line.contains("\"isMeta\": true") {
            continue;
        }
        if line.contains("\"type\":\"image\"") || line.contains("\"type\": \"image\"") {
            return "Image";
        }
        if line.contains("\"type\":\"document\"") || line.contains("\"type\": \"document\"") {
            return "Document";
        }
    }
    ""
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(content: Value) -> String {
        json!({"type": "user", "message": {"role": "user", "content": content}}).to_string()
    }

    /// first prompt > keeps a slash command only as a fallback for a real prompt
    #[test]
    fn a_slash_command_is_only_a_fallback() {
        let command = user(json!("<command-name>/init</command-name>"));
        assert_eq!(first_prompt(&command), "/init");
        let both = format!("{command}\n{}", user(json!("real prompt")));
        assert_eq!(first_prompt(&both), "real prompt");
    }

    /// first prompt > skips tags, interrupts, meta, compact summaries and tool results
    #[test]
    fn skips_tags_interrupts_meta_summaries_and_tool_results() {
        let lines = [
            user(json!("<local-command-stdout>x</local-command-stdout>")),
            user(json!("[Request interrupted by user for tool use]")),
            json!({"type": "user", "isMeta": true, "message": {"content": "meta"}}).to_string(),
            json!({"type": "user", "isCompactSummary": true, "message": {"content": "sum"}})
                .to_string(),
            user(json!([{"type": "tool_result", "content": "r"}])),
            user(json!("the prompt")),
        ];
        assert_eq!(first_prompt(&lines.join("\n")), "the prompt");
        // A text block in a record with a tool result is no prompt.
        assert_eq!(
            first_prompt(&user(
                json!([{"type": "text", "text": "a"}, {"type": "tool_result"}])
            )),
            ""
        );
    }

    /// first prompt > renders bash input, expands pasted content, and cuts at 200 characters
    #[test]
    fn renders_bash_input_expands_pasted_content_and_cuts() {
        assert_eq!(
            first_prompt(&user(json!("<bash-input> ls -la </bash-input>"))),
            "! ls -la"
        );
        let pasted = "before\n\n<pasted_content id=\"ab12\">\nline1\nline2\n</pasted_content id=\"ab12\">\n\nafter";
        assert_eq!(first_prompt(&user(json!(pasted))), "beforeline1 line2after");
        let long = "x".repeat(250);
        let prompt = first_prompt(&user(json!(long)));
        assert_eq!(utf16_len(&prompt), 201);
        assert!(prompt.ends_with('…'));
        // A surrogate pair is not split at the cut.
        let emoji = format!("{}\u{1f600}tail", "x".repeat(199));
        assert_eq!(
            first_prompt(&user(json!(emoji))),
            format!("{}…", "x".repeat(199))
        );
    }

    /// first prompt > titles an image-only first prompt
    #[test]
    fn titles_an_image_only_first_prompt() {
        let image = user(json!([{"type": "image", "source": {}}]));
        assert_eq!(first_prompt(&image), "");
        assert_eq!(media_prompt(&image), "Image");
        let document = user(json!([{"type": "document", "source": {}}]));
        assert_eq!(media_prompt(&document), "Document");
        assert!(is_user_prompt(
            &serde_json::from_str(&image).unwrap_or_default()
        ));
    }
}
