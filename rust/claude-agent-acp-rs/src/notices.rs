//! Session notices (ROADMAP F2a; `src/session-notices.ts`).
//!
//! A notice is fire-and-forget advisory text for the user: live events rather
//! than conversation history (ACP Session Notices RFD). A client that
//! advertised `clientCapabilities.session.notices` gets a `notice` session
//! update; every other client gets the bold-label transcript line that the
//! adapter used before the capability existed.
//!
//! The shaping helpers are the exact port of `session-notices.ts`: the first
//! line becomes the title when it fits in [`MAX_NOTICE_TITLE_LENGTH`], the
//! rest becomes the description, and light markdown is stripped because notice
//! fields are plain text.

use serde_json::{json, Map, Value};

/// Longest free-form line that may serve as a notice title on its own. Past
/// this the text moves to `description` under a generic title.
pub const MAX_NOTICE_TITLE_LENGTH: usize = 256;

/// Fire-and-forget advisory text for the user (`SessionNotice`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionNotice {
    pub severity: String,
    /// Plain text that stands alone.
    pub title: String,
    /// Plain-text detail or guidance.
    pub description: Option<String>,
}

impl SessionNotice {
    /// Build a notice.
    pub fn new(
        severity: impl Into<String>,
        title: impl Into<String>,
        description: Option<String>,
    ) -> Self {
        Self {
            severity: severity.into(),
            title: title.into(),
            description,
        }
    }
}

/// Whether the client advertised `clientCapabilities.session.notices`.
///
/// Agents MUST NOT send `notice` updates otherwise; the alternative is a
/// bold-label transcript line.
pub fn client_supports_notices(capabilities: Option<&Value>) -> bool {
    capabilities
        .and_then(|caps| caps.get("session"))
        .and_then(|session| session.get("notices"))
        .is_some_and(Value::is_object)
}

/// The `notice` session update.
pub fn notice_update(notice: &SessionNotice) -> Value {
    let mut update = Map::new();
    update.insert("sessionUpdate".into(), json!("notice"));
    update.insert("severity".into(), json!(notice.severity));
    update.insert("title".into(), json!(notice.title));
    if let Some(description) = &notice.description {
        update.insert("description".into(), json!(description));
    }
    Value::Object(update)
}

/// The transcript rendering of a notice for clients without the capability.
pub fn notice_transcript_text(notice: &SessionNotice) -> String {
    match &notice.description {
        Some(description) => format!("**{}:** {description}", notice.title),
        None => format!("**{}**", notice.title),
    }
}

/// The update to send for `notice`: a `notice` when the client can present one,
/// else an agent message carrying `transcript_text` (by default the bold-label
/// rendering of the notice) and `transcript_meta`, so clients without the
/// capability can still tell the line apart from Claude's reply.
pub fn notice_or_transcript_update(
    notice: &SessionNotice,
    supports_notices: bool,
    transcript_text: Option<&str>,
    transcript_meta: Option<&Value>,
) -> Value {
    if supports_notices {
        return notice_update(notice);
    }
    let text = transcript_text
        .map(str::to_string)
        .unwrap_or_else(|| notice_transcript_text(notice));
    let mut update = Map::new();
    update.insert("sessionUpdate".into(), json!("agent_message_chunk"));
    update.insert("content".into(), json!({"type": "text", "text": text}));
    if let Some(meta) = transcript_meta {
        update.insert("_meta".into(), meta.clone());
    }
    Value::Object(update)
}

/// Fragments that read on after a bold label stand alone as a notice
/// description, so start them with a capital.
pub fn sentence_case(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Collapses whitespace so a text shown as a notice can be recognized when the
/// SDK repeats it elsewhere (a hook-blocked turn's result, for one).
pub fn normalize_notice_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Notice fields are plain text. Free-form SDK/hook output may carry light
/// markdown that a transcript would render; drop the inline markers rather
/// than show literal asterisks and backticks in a toast.
pub fn notice_plain_text(text: &str) -> String {
    let headings: String = text
        .split('\n')
        .map(strip_heading)
        .collect::<Vec<_>>()
        .join("\n");
    strip_code(&strip_bold(&headings))
}

/// Shapes free-form text into a notice title plus optional description: the
/// first line stands as the title when it is short enough, otherwise the whole
/// text becomes the description under `fallback_title`.
pub fn split_notice_text(text: &str, fallback_title: &str) -> (String, Option<String>) {
    let trimmed = notice_plain_text(text).trim().to_string();
    if trimmed.is_empty() {
        return (fallback_title.to_string(), None);
    }
    let line_break = find_line_break(&trimmed);
    let (first_line, rest) = match line_break {
        None => (trimmed.as_str(), ""),
        Some(index) => {
            let first_line = trimmed[..index].trim_end();
            let rest = trimmed[index..].trim();
            (first_line, rest)
        }
    };
    // JS `.length` counts UTF-16 code units; use the same measure so an emoji
    // counts as two.
    if first_line.encode_utf16().count() > MAX_NOTICE_TITLE_LENGTH {
        return (fallback_title.to_string(), Some(trimmed));
    }
    if rest.is_empty() {
        (first_line.to_string(), None)
    } else {
        (first_line.to_string(), Some(rest.to_string()))
    }
}

/// The index of the start of the first `\r?\n` (the `\r` when both are
/// present), matching `trimmed.search(/\r?\n/)`.
fn find_line_break(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let newline = bytes.iter().position(|b| *b == b'\n')?;
    if newline > 0 && bytes[newline - 1] == b'\r' {
        Some(newline - 1)
    } else {
        Some(newline)
    }
}

/// `^[ \t]*#{1,6}[ \t]+` (multiline): drop an ATX heading marker.
fn strip_heading(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() && (bytes[index] == b' ' || bytes[index] == b'\t') {
        index += 1;
    }
    let mut hashes = 0;
    while index + hashes < bytes.len() && bytes[index + hashes] == b'#' && hashes < 7 {
        hashes += 1;
    }
    if hashes == 0 || hashes > 6 {
        return line;
    }
    let mut marker_end = index + hashes;
    if marker_end >= bytes.len() || (bytes[marker_end] != b' ' && bytes[marker_end] != b'\t') {
        return line;
    }
    while marker_end < bytes.len() && (bytes[marker_end] == b' ' || bytes[marker_end] == b'\t') {
        marker_end += 1;
    }
    &line[marker_end..]
}

/// `(\*\*|__)(?=\S)([\s\S]*?\S)\1`: drop bold markers while keeping the text.
fn strip_bold(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        let is_star = chars[index] == '*' && index + 1 < chars.len() && chars[index + 1] == '*';
        let is_underscore =
            chars[index] == '_' && index + 1 < chars.len() && chars[index + 1] == '_';
        if !is_star && !is_underscore {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        // `(?=\S)`: the opening delimiter must be followed by non-space.
        if index + 2 >= chars.len() || chars[index + 2].is_whitespace() {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        let delimiter = chars[index];
        let mut closing = None;
        let mut scan = index + 3;
        while scan + 1 < chars.len() {
            if chars[scan] == delimiter
                && chars[scan + 1] == delimiter
                && !chars[scan - 1].is_whitespace()
            {
                closing = Some(scan);
                break;
            }
            scan += 1;
        }
        match closing {
            Some(closing) => {
                for ch in &chars[index + 2..closing] {
                    out.push(*ch);
                }
                index = closing + 2;
            }
            None => {
                out.push(chars[index]);
                index += 1;
            }
        }
    }
    out
}

/// `(?<![\w`])`([^`\n]+)`(?![\w`])`: drop inline-code backticks.
fn strip_code(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '`' {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        let previous_ok = index == 0 || !is_word_like(chars[index - 1]) && chars[index - 1] != '`';
        if !previous_ok {
            out.push('`');
            index += 1;
            continue;
        }
        let mut scan = index + 1;
        let mut closing = None;
        while scan < chars.len() {
            match chars[scan] {
                '\n' => break,
                '`' => {
                    let next_ok = scan + 1 >= chars.len()
                        || (!is_word_like(chars[scan + 1]) && chars[scan + 1] != '`');
                    if scan > index + 1 && next_ok {
                        closing = Some(scan);
                    }
                    break;
                }
                _ => scan += 1,
            }
        }
        match closing {
            Some(closing) => {
                for ch in &chars[index + 1..closing] {
                    out.push(*ch);
                }
                index = closing + 1;
            }
            None => {
                out.push('`');
                index += 1;
            }
        }
    }
    out
}

/// JavaScript's `\w` (ASCII letters, digits, underscore).
fn is_word_like(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(severity: &str, title: &str, description: Option<&str>) -> SessionNotice {
        SessionNotice::new(severity, title, description.map(str::to_string))
    }

    #[test]
    fn notices_require_an_object_under_session_notices() {
        assert!(!client_supports_notices(None));
        assert!(!client_supports_notices(Some(&json!({}))));
        assert!(!client_supports_notices(Some(&json!({"session": {}}))));
        assert!(!client_supports_notices(Some(
            &json!({"session": {"notices": null}})
        )));
        assert!(!client_supports_notices(Some(
            &json!({"session": {"notices": []}})
        )));
        assert!(client_supports_notices(Some(
            &json!({"session": {"notices": {}}})
        )));
    }

    #[test]
    fn renders_the_bold_label_transcript_line_without_the_capability() {
        let with_description = notice("warning", "Fast mode turned off", Some("Why."));
        assert_eq!(
            notice_transcript_text(&with_description),
            "**Fast mode turned off:** Why."
        );
        let without = notice("info", "Done", None);
        assert_eq!(notice_transcript_text(&without), "**Done**");
        assert_eq!(
            notice_or_transcript_update(&with_description, false, None, None),
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "**Fast mode turned off:** Why."}
            })
        );
        assert_eq!(
            notice_or_transcript_update(&with_description, false, Some("custom line"), None),
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "custom line"}
            })
        );
        let meta = json!({"claudeCode": {"kind": "informational", "level": "warning"}});
        assert_eq!(
            notice_or_transcript_update(&with_description, false, Some("custom line"), Some(&meta)),
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "custom line"},
                "_meta": meta
            })
        );
        assert!(notice_or_transcript_update(
            &with_description,
            true,
            Some("custom line"),
            Some(&meta)
        )
        .get("_meta")
        .is_none());
    }

    #[test]
    fn sends_a_notice_update_when_the_client_can_present_one() {
        assert_eq!(
            notice_or_transcript_update(
                &notice("info", "Task stopped by user", None),
                true,
                None,
                None
            ),
            json!({"sessionUpdate": "notice", "severity": "info", "title": "Task stopped by user"})
        );
        assert_eq!(
            notice_or_transcript_update(&notice("error", "T", Some("D")), true, None, None),
            json!({"sessionUpdate": "notice", "severity": "error", "title": "T", "description": "D"})
        );
    }

    /// `async-tasks.test.ts` > "acknowledges a stop as a live notice for a client
    /// on the notice contract": the acknowledgement is a live `notice` with the
    /// task's name; without the capability it is the bold-label transcript line.
    #[test]
    fn t9a_task_stopped_notice_wire_shape() {
        let stopped = notice(
            "info",
            "Task stopped by user",
            Some("Build generated assets."),
        );
        assert_eq!(
            notice_or_transcript_update(&stopped, true, None, None),
            json!({
                "sessionUpdate": "notice",
                "severity": "info",
                "title": "Task stopped by user",
                "description": "Build generated assets."
            })
        );
        assert_eq!(
            notice_or_transcript_update(&stopped, false, None, None),
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text",
                    "text": "**Task stopped by user:** Build generated assets."}
            })
        );
    }

    #[test]
    fn strips_inline_markdown_that_a_plain_text_notice_would_show_literally() {
        assert_eq!(
            notice_plain_text("**Blocked:** run `npm test` first"),
            "Blocked: run npm test first"
        );
        assert_eq!(
            notice_plain_text("## Heading\n__strong__ and a * lone star"),
            "Heading\nstrong and a * lone star"
        );
        assert_eq!(
            split_notice_text("**Blocked:** run `npm test` first", "Fallback"),
            ("Blocked: run npm test first".to_string(), None)
        );
    }

    #[test]
    fn normalizes_whitespace_for_repeat_detection_and_capitalizes_fragments() {
        assert_eq!(normalize_notice_text("  a\r\nb  c\n"), "a b c");
        assert_eq!(
            sentence_case("not available on the free plan"),
            "Not available on the free plan"
        );
        assert_eq!(sentence_case(""), "");
    }

    #[test]
    fn shapes_free_form_text_into_a_standalone_title() {
        assert_eq!(
            split_notice_text("hook says no", "Fallback"),
            ("hook says no".to_string(), None)
        );
        assert_eq!(
            split_notice_text("  hook says no \n", "Fallback"),
            ("hook says no".to_string(), None)
        );
        assert_eq!(
            split_notice_text("First line\r\n\nRest of it\n", "Fallback"),
            ("First line".to_string(), Some("Rest of it".to_string()))
        );
        assert_eq!(
            split_notice_text("", "Fallback"),
            ("Fallback".to_string(), None)
        );
        let long = "x".repeat(MAX_NOTICE_TITLE_LENGTH + 1);
        assert_eq!(
            split_notice_text(&long, "Fallback"),
            ("Fallback".to_string(), Some(long.clone()))
        );
        assert_eq!(
            split_notice_text(&format!("{long}\nmore"), "Fallback"),
            ("Fallback".to_string(), Some(format!("{long}\nmore")))
        );
    }
}
