//! Port of `dist/mcp-command.js` (`src/mcp-command.ts`, upstream 0.94.0).
//!
//! `/mcp` is a Claude Code local command: the CLI runs it and prints its own
//! text (which points at a terminal an ACP client does not have). The adapter
//! replaces that text with a Markdown server list, and runs a reconnect, an
//! enable, or a disable through the SDK control API because Claude Code
//! refuses those actions in SDK mode (S-02, S-14, S-15, S-17, S-18, S-20,
//! A-23, A-24).
//!
//! The parsing / rendering functions are pure (`inv_56_*`); the one async
//! entry point [`run_mcp_command`] talks to the CLI through [`Control`] and is
//! spawned by the session actor (Decision 21), never awaited inline.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::Value;

use crate::control::Control;

/// The time that the status read after a `/mcp` command can take.
const MCP_STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// The time that one reconnect, enable, or disable can take.
const MCP_ACTION_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest segment that can be a wrapper. A longer segment is content.
const MAX_WRAPPER_LENGTH: usize = 60;

/// The longest error text kept for one list item, in code points.
const MAX_ERROR_LENGTH: usize = 160;

/// The known statuses in the order of the groups and of the summary counts.
/// A disabled server is not a group; it is on one line at the end.
const GROUP_ORDER: [&str; 4] = ["failed", "needs-auth", "pending", "connected"];

const KNOWN_STATUSES: [&str; 5] = ["failed", "needs-auth", "pending", "connected", "disabled"];

/// The `/mcp` prompt that gets a replacement for its Claude Code text. `all`
/// is true for `all` and for a reconnect without a server name, as in Claude
/// Code. `server` is `None` for an enable or a disable without a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCommand {
    pub action: McpAction,
    pub all: bool,
    pub server: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAction {
    Status,
    Reconnect,
    Enable,
    Disable,
}

impl McpAction {
    /// The action as it is spelled in `/mcp <action>` and in a failure line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Reconnect => "reconnect",
            Self::Enable => "enable",
            Self::Disable => "disable",
        }
    }
}

/// JS `\s` (the characters `trimStart` and `/\s+/` match). Rust's
/// `char::is_whitespace` omits U+FEFF, which the upstream parse test uses.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// JS `String.prototype.trimStart`.
fn js_trim_start(text: &str) -> &str {
    text.trim_start_matches(is_js_space)
}

/// JS `String.prototype.trimEnd`.
fn js_trim_end(text: &str) -> &str {
    text.trim_end_matches(is_js_space)
}

/// JS `String.prototype.trim`.
fn js_trim(text: &str) -> &str {
    js_trim_end(js_trim_start(text))
}

/// JS `text.trim().split(/\s+/)`. The empty string yields one empty word, as
/// in JS, so a whitespace-only prompt never matches `/mcp`.
fn js_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        if is_js_space(c) {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    if words.is_empty() {
        words.push(String::new());
    }
    words
}

/// JS string length (`segment.length` counts UTF-16 code units).
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Parse a prompt that is exactly `/mcp`, or `/mcp reconnect`, `/mcp enable`,
/// or `/mcp disable` with an optional server name or `all`. Any other `/mcp`
/// argument gives `None`, and the prompt gets no replacement.
#[must_use]
pub fn parse_mcp_command(text: &str) -> Option<McpCommand> {
    // The first word is `/mcp` only when the text starts with it after the
    // leading whitespace. Checking that first keeps a long prompt from being
    // split into words.
    if !js_trim_start(text).starts_with("/mcp") {
        return None;
    }
    let words = js_words(js_trim(text));
    if words.first().map(String::as_str) != Some("/mcp") {
        return None;
    }
    if words.len() == 1 {
        return Some(McpCommand {
            action: McpAction::Status,
            all: false,
            server: None,
        });
    }
    let action = match words.get(1).map(|w| w.to_lowercase()).as_deref() {
        Some("reconnect") => McpAction::Reconnect,
        Some("enable") => McpAction::Enable,
        Some("disable") => McpAction::Disable,
        _ => return None,
    };
    let server = words
        .get(2..)
        .map(|rest| rest.join(" "))
        .unwrap_or_default();
    if server == "all" || (server.is_empty() && action == McpAction::Reconnect) {
        return Some(McpCommand {
            action,
            all: true,
            server: None,
        });
    }
    Some(McpCommand {
        action,
        all: false,
        server: if server.is_empty() {
            None
        } else {
            Some(server)
        },
    })
}

/// Parse the `/mcp` command of a `session/prompt` request. Only a prompt that
/// is a SINGLE text block is considered (`localCommandMarkdown`,
/// `acp-agent.ts:3723-3734`); a second block or a non-text block leaves the
/// prompt alone.
#[must_use]
pub fn prompt_mcp_command(params: &Value) -> Option<McpCommand> {
    let prompt = params.get("prompt")?.as_array()?;
    if prompt.len() != 1 {
        return None;
    }
    let block = prompt.first()?;
    if block.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    parse_mcp_command(block.get("text").and_then(Value::as_str)?)
}

/// `promptToClaude` (S-08): rewrite `/mcp:<server>:<command> [args]` to
/// `/<server>:<command> (MCP) [args]` (`acp-agent.ts:12678-12684`). Every
/// other text is returned unchanged.
#[must_use]
pub fn rewrite_mcp_prompt(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    const PREFIX: [char; 5] = ['/', 'm', 'c', 'p', ':'];
    if chars.len() < PREFIX.len() || chars[..PREFIX.len()] != PREFIX {
        return text.to_string();
    }
    let mut index = PREFIX.len();
    let server_start = index;
    while index < chars.len() && chars[index] != ':' && !is_js_space(chars[index]) {
        index += 1;
    }
    if index == server_start || index >= chars.len() || chars[index] != ':' {
        return text.to_string();
    }
    let server: String = chars[server_start..index].iter().collect();
    index += 1;
    let command_start = index;
    while index < chars.len() && !is_js_space(chars[index]) {
        index += 1;
    }
    if index == command_start {
        return text.to_string();
    }
    let command: String = chars[command_start..index].iter().collect();
    if index == chars.len() {
        return format!("/{server}:{command} (MCP)");
    }
    if !is_js_space(chars[index]) {
        return text.to_string();
    }
    let args: String = chars[index + 1..].iter().collect();
    // JS `.` does not cross a line terminator, so the anchored regex fails
    // when the argument tail contains one.
    if args
        .chars()
        .any(|c| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
    {
        return text.to_string();
    }
    if args.is_empty() {
        format!("/{server}:{command} (MCP)")
    } else {
        format!("/{server}:{command} (MCP) {args}")
    }
}

/// `escapeMarkdown` (`usage-markdown.ts:84-86`): backslash-escape the Markdown
/// syntax characters, then collapse CR/LF runs to one space.
#[must_use]
pub fn escape_markdown(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    let mut out = String::with_capacity(escaped.len());
    let mut in_newline = false;
    for c in escaped.chars() {
        if matches!(c, '\r' | '\n') {
            if !in_newline {
                out.push(' ');
                in_newline = true;
            }
        } else {
            out.push(c);
            in_newline = false;
        }
    }
    out
}

/// The text as a Markdown code span. The fence is longer than the longest
/// backtick run in the text, so the span shows the text unchanged.
fn code_span(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with('`')
        || text.ends_with('`')
        || (text.starts_with(' ') && text.ends_with(' ') && text.chars().any(|c| c != ' '))
    {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// `/^-?\d+$/`: an optional minus followed by one or more ASCII digits.
fn is_error_code(segment: &str) -> bool {
    let body = segment.strip_prefix('-').unwrap_or(segment);
    !body.is_empty() && body.chars().all(|c| c.is_ascii_digit())
}

/// `/(failed|error|exception)(\s+-?\d+)?$/i`: the segment ends with one of the
/// wrapper words, optionally followed by whitespace and an error code.
fn ends_with_wrapper_word(segment: &str) -> bool {
    let lower = segment.to_lowercase();
    if ["failed", "error", "exception"]
        .iter()
        .any(|word| lower.ends_with(word))
    {
        return true;
    }
    let chars: Vec<char> = lower.chars().collect();
    let mut index = chars.len();
    while index > 0 && chars[index - 1].is_ascii_digit() {
        index -= 1;
    }
    if index == chars.len() {
        return false;
    }
    if index > 0 && chars[index - 1] == '-' {
        index -= 1;
    }
    if index == 0 || !is_js_space(chars[index - 1]) {
        return false;
    }
    while index > 0 && is_js_space(chars[index - 1]) {
        index -= 1;
    }
    let head: String = chars[..index].iter().collect();
    ["failed", "error", "exception"]
        .iter()
        .any(|word| head.ends_with(word))
}

/// True for a segment that only wraps the real error, such as
/// `MCP startup failed`, `McpError`, `MCP error -32001`, an error code, or a
/// Rust or TypeScript type path.
fn is_wrapper_segment(segment: &str) -> bool {
    is_error_code(segment)
        || segment.contains("::")
        || (js_len(segment) <= MAX_WRAPPER_LENGTH
            && !segment.chars().any(|c| matches!(c, '.' | '!' | '?'))
            && ends_with_wrapper_word(segment))
}

/// JS `/\s+/g` -> one space (the caller trims).
fn collapse_js_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for c in text.chars() {
        if is_js_space(c) {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// Cut the text to `MAX_ERROR_LENGTH` code points, so a surrogate pair stays
/// whole.
fn truncate(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= MAX_ERROR_LENGTH {
        return text.to_string();
    }
    let cut: String = chars[..MAX_ERROR_LENGTH - 1].iter().collect();
    format!("{}…", js_trim_end(&cut))
}

/// Split after a `[.!?]` that is immediately followed by one literal space
/// (`content.split(/(?<=[.!?]) /)`), consuming that space.
fn split_sentences_after_space(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < chars.len() {
        if matches!(chars[index], '.' | '!' | '?') && chars.get(index + 1) == Some(&' ') {
            out.push(chars[start..=index].iter().collect());
            index += 2;
            start = index;
        } else {
            index += 1;
        }
    }
    out.push(chars[start..].iter().collect());
    out
}

/// `sentences()` (`mcp-command.ts:298-303`): split after `[.!?]` followed by a
/// whitespace run, then drop empty sentences.
#[must_use]
pub fn sentences(text: &str) -> Vec<String> {
    let trimmed = js_trim(text);
    let chars: Vec<char> = trimmed.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < chars.len() {
        if matches!(chars[index], '.' | '!' | '?')
            && chars.get(index + 1).is_some_and(|c| is_js_space(*c))
        {
            out.push(chars[start..=index].iter().collect());
            index += 1;
            while index < chars.len() && is_js_space(chars[index]) {
                index += 1;
            }
            start = index;
        } else {
            index += 1;
        }
    }
    out.push(chars[start..].iter().collect());
    out.into_iter()
        .filter(|sentence| !sentence.is_empty())
        .collect()
}

/// Make an MCP error short and readable for one list item. The result has no
/// leading wrapper segments, at most two sentences, and at most
/// `MAX_ERROR_LENGTH` code points. A wrapper before an error code stays, so an
/// error code is never alone. The result is empty for an error with only
/// whitespace.
#[must_use]
pub fn clean_mcp_error(error: &str) -> String {
    let collapsed = collapse_js_whitespace(error);
    let trimmed = js_trim(&collapsed);
    let segments: Vec<&str> = trimmed.split(": ").collect();
    let mut start = 0;
    while start + 1 < segments.len()
        && is_wrapper_segment(segments[start])
        && !segments[start + 1..].iter().all(|s| is_error_code(s))
    {
        start += 1;
    }
    let content = segments[start..].join(": ");
    let cleaned = split_sentences_after_space(&content);
    truncate(&cleaned.into_iter().take(2).collect::<Vec<_>>().join(" "))
}

/// The server name as inline code. Claude Code keys the servers by name, so
/// two servers have the same name only from different scopes. Then the scope
/// tells them apart.
fn server_name(status: &Value, duplicates: &HashSet<String>) -> String {
    let name = status.get("name").and_then(Value::as_str).unwrap_or("");
    let origin = match status.get("scope") {
        Some(Value::String(scope)) => Some(scope.as_str()),
        _ => status.get("source").and_then(Value::as_str),
    };
    match origin {
        Some(origin) if duplicates.contains(name) && !origin.is_empty() => {
            format!("{} ({})", code_span(name), escape_markdown(origin))
        }
        _ => code_span(name),
    }
}

/// `toolCount`: an absent `tools` list has no detail; an empty one is
/// `0 tools` (an empty array is truthy in JS).
fn tool_count(status: &Value) -> Option<String> {
    let tools = status.get("tools").and_then(Value::as_array)?;
    Some(if tools.len() == 1 {
        "1 tool".to_string()
    } else {
        format!("{} tools", tools.len())
    })
}

fn server_item(status: &Value, duplicates: &HashSet<String>) -> String {
    let error = status
        .get("error")
        .and_then(Value::as_str)
        .filter(|error| !error.is_empty())
        .map(clean_mcp_error)
        .unwrap_or_default();
    let mut details: Vec<String> = Vec::new();
    if !error.is_empty() {
        details.push(escape_markdown(&error));
    }
    if let Some(count) = tool_count(status) {
        details.push(count);
    }
    let name = server_name(status, duplicates);
    if details.is_empty() {
        format!("- {name}")
    } else {
        format!("- {name}: {}", details.join("; "))
    }
}

fn status_state(status: &Value) -> &str {
    status.get("status").and_then(Value::as_str).unwrap_or("")
}

/// The statuses that this adapter does not know, in the SDK order. The SDK can
/// report a new status before the adapter knows it.
fn unknown_statuses(statuses: &[Value]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for status in statuses {
        let state = status_state(status);
        if KNOWN_STATUSES.contains(&state) || !seen.insert(state.to_string()) {
            continue;
        }
        out.push(state.to_string());
    }
    out
}

/// The group title of a status that the adapter does not know. It starts with
/// a capital letter, as a known title does.
fn unknown_title(state: &str) -> String {
    let mut chars = state.chars();
    let title = match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    };
    escape_markdown(&title)
}

fn summary_label(state: &str) -> Option<&'static str> {
    match state {
        "connected" => Some("connected"),
        "failed" => Some("failed"),
        "needs-auth" => Some("need authentication"),
        "pending" => Some("connecting"),
        "disabled" => Some("disabled"),
        _ => None,
    }
}

fn summary_line(statuses: &[Value]) -> String {
    // The counts follow the group order. An unknown status also counts, so the
    // counts add up to the total.
    let mut states: Vec<String> = GROUP_ORDER.iter().map(|s| (*s).to_string()).collect();
    states.extend(unknown_statuses(statuses));
    states.push("disabled".to_string());
    let mut parts = Vec::new();
    for state in states {
        let count = statuses
            .iter()
            .filter(|status| status_state(status) == state)
            .count();
        if count == 0 {
            continue;
        }
        if count == 1 && state == "needs-auth" {
            parts.push("1 needs authentication".to_string());
        } else {
            let label = summary_label(&state).map(str::to_string);
            parts.push(format!(
                "{} {}",
                count,
                label.unwrap_or_else(|| escape_markdown(&state))
            ));
        }
    }
    format!("**MCP servers:** {} ({})", statuses.len(), parts.join(", "))
}

/// True for a server that a `/mcp reconnect` without a name must retry.
fn needs_reconnect(status: &Value) -> bool {
    matches!(status_state(status), "failed" | "pending" | "needs-auth")
}

/// Markdown for `/mcp`: a summary line, one group of servers for each status,
/// and the disabled servers on one line.
#[must_use]
pub fn format_mcp_status(statuses: &[Value]) -> String {
    if statuses.is_empty() {
        return "No MCP servers are configured.".to_string();
    }
    let mut duplicates = HashSet::new();
    let mut seen = HashSet::new();
    for status in statuses {
        let name = status.get("name").and_then(Value::as_str).unwrap_or("");
        if !seen.insert(name) {
            duplicates.insert(name.to_string());
        }
    }

    let mut blocks = vec![summary_line(statuses)];
    let mut groups: Vec<(String, String)> = vec![
        ("failed".to_string(), "Failed".to_string()),
        ("needs-auth".to_string(), "Needs authentication".to_string()),
        ("pending".to_string(), "Connecting".to_string()),
        ("connected".to_string(), "Connected".to_string()),
    ];
    for state in unknown_statuses(statuses) {
        groups.push((state.clone(), unknown_title(&state)));
    }
    for (state, title) in groups {
        let members: Vec<&Value> = statuses
            .iter()
            .filter(|status| status_state(status) == state)
            .collect();
        if members.is_empty() {
            continue;
        }
        let mut block = format!("**{title}**");
        for member in members {
            block.push('\n');
            block.push_str(&server_item(member, &duplicates));
        }
        blocks.push(block);
    }
    let disabled: Vec<String> = statuses
        .iter()
        .filter(|status| status_state(status) == "disabled")
        .map(|status| server_name(status, &duplicates))
        .collect();
    if !disabled.is_empty() {
        blocks.push(format!("**Disabled:** {}", disabled.join(", ")));
    }
    if statuses.iter().any(needs_reconnect) {
        blocks.push(
            "Run `/mcp reconnect <server>` to reconnect one server, or `/mcp reconnect` to reconnect every server that is not connected and not disabled."
                .to_string(),
        );
    }
    blocks.join("\n\n")
}

/// One result line of a `/mcp` action. An unknown server name is only reported
/// when Claude Code did not already say the name is unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpResultLine {
    Static(String),
    UnknownServer(String),
    Done {
        action: McpAction,
        server: String,
    },
    Failed {
        action: McpAction,
        server: String,
        error: String,
    },
}

/// The action part of a `/mcp` run: the result lines and the server list after
/// the action. The Claude Code sentences are only known once its text arrives,
/// so they are applied by [`McpPrepared::render`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpActionOutcome {
    pub results: Vec<McpResultLine>,
    pub after: Option<Vec<Value>>,
}

/// The result of one `/mcp` run. `render` returns `None` when the turn keeps
/// the text of Claude Code (a failed status read).
#[derive(Debug, Clone, Default)]
pub struct McpPrepared {
    pub status_markdown: Option<String>,
    pub action: Option<McpActionOutcome>,
    pub reconnected: Vec<String>,
}

impl McpPrepared {
    /// The replacement for `original_output`, or `None` to keep the CLI text.
    #[must_use]
    pub fn render(&self, original_output: &str) -> Option<String> {
        match &self.action {
            Some(action) => Some(render_action(action, original_output)),
            None => self.status_markdown.clone(),
        }
    }
}

/// The servers that the action changes. A named server is always a target, so
/// the control API reports its own error.
fn action_targets(command: &McpCommand, statuses: &[Value]) -> Vec<String> {
    if !command.all {
        return command.server.iter().cloned().collect();
    }
    let mut seen = HashSet::new();
    let mut targets = Vec::new();
    for status in statuses {
        let include = match command.action {
            McpAction::Reconnect => needs_reconnect(status),
            McpAction::Enable => status_state(status) == "disabled",
            McpAction::Disable => status_state(status) != "disabled",
            McpAction::Status => false,
        };
        if !include {
            continue;
        }
        let name = status.get("name").and_then(Value::as_str).unwrap_or("");
        if seen.insert(name.to_string()) {
            targets.push(name.to_string());
        }
    }
    targets
}

fn nothing_to_do_line(action: McpAction) -> &'static str {
    match action {
        McpAction::Enable => "Every MCP server is already enabled.",
        McpAction::Disable => "Every MCP server is already disabled.",
        McpAction::Status | McpAction::Reconnect => {
            "Every MCP server is connected or disabled. There is nothing to reconnect."
        }
    }
}

fn action_done(action: McpAction) -> &'static str {
    match action {
        McpAction::Reconnect => "Reconnected",
        McpAction::Enable => "Enabled",
        McpAction::Disable => "Disabled",
        McpAction::Status => "",
    }
}

/// `UNAVAILABLE_SENTENCE`: Claude Code's refusal of an action in SDK mode.
fn is_unavailable_sentence(sentence: &str) -> bool {
    sentence
        .to_lowercase()
        .contains("aren't available in this session")
}

/// A Claude Code sentence that points to a terminal, which an ACP client does
/// not have.
fn is_terminal_sentence(sentence: &str) -> bool {
    sentence.to_lowercase().contains("in the terminal")
        || sentence.contains("`/mcp reconnect all` here")
}

fn render_action(action: &McpActionOutcome, original_output: &str) -> String {
    let cli_sentences: Vec<String> = sentences(original_output)
        .into_iter()
        .filter(|sentence| !is_terminal_sentence(sentence))
        .collect();
    let cli_names_unknown = cli_sentences
        .iter()
        .any(|sentence| sentence.to_lowercase().contains("no mcp server named"));
    let kept: Vec<&str> = cli_sentences
        .iter()
        .filter(|sentence| !is_unavailable_sentence(sentence))
        .map(String::as_str)
        .collect();

    let mut results: Vec<String> = Vec::new();
    for line in &action.results {
        match line {
            McpResultLine::Static(text) => results.push(text.clone()),
            McpResultLine::UnknownServer(server) => {
                if !cli_names_unknown {
                    results.push(format!(
                        "There's no MCP server named {}.",
                        code_span(server)
                    ));
                }
            }
            McpResultLine::Done { action, server } => {
                results.push(format!("- {} {}", action_done(*action), code_span(server)));
            }
            McpResultLine::Failed {
                action,
                server,
                error,
            } => {
                let failure = format!("- Couldn't {} {}", action.as_str(), code_span(server));
                results.push(if error.is_empty() {
                    failure
                } else {
                    format!("{failure}: {}", escape_markdown(error))
                });
            }
        }
    }

    let mut blocks: Vec<String> = Vec::new();
    if !kept.is_empty() {
        blocks.push(kept.join(" "));
    }
    if !results.is_empty() {
        blocks.push(results.join("\n"));
    }
    if let Some(after) = &action.after {
        blocks.push(format_mcp_status(after));
    }
    blocks.join("\n\n")
}

/// Read the MCP server status for the list after a `/mcp` command. `None` when
/// the read fails or takes too long; then the turn keeps the CLI text.
async fn read_status(control: &Control) -> Option<Vec<Value>> {
    let response = tokio::time::timeout(MCP_STATUS_TIMEOUT, control.mcp_status())
        .await
        .ok()?
        .ok()?;
    // `Control::send_request` returns the full `control_response`; the SDK's
    // `mcpServerStatus` reads `response.response.mcpServers`.
    response
        .pointer("/response/response/mcpServers")
        .and_then(Value::as_array)
        .cloned()
}

/// Wait for one control call. The wait stops after [`MCP_ACTION_TIMEOUT`].
async fn await_action<F>(call: F) -> Result<(), String>
where
    F: std::future::Future<Output = Result<(), crate::control::ControlError>>,
{
    match tokio::time::timeout(MCP_ACTION_TIMEOUT, call).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("The request timed out.".to_string()),
    }
}

/// Run a `/mcp` command through the SDK control API. Claude Code refuses a
/// reconnect, an enable, and a disable in SDK mode, so the adapter runs them
/// itself. An error of one server does not stop the others. A failed first
/// status read leaves `render` returning `None`, which keeps the CLI text.
pub async fn run_mcp_command(control: &Control, command: &McpCommand) -> McpPrepared {
    let mut prepared = McpPrepared::default();
    if command.action == McpAction::Status {
        prepared.status_markdown = read_status(control).await.map(|s| format_mcp_status(&s));
        return prepared;
    }
    let Some(before) = read_status(control).await else {
        return prepared;
    };
    let mut results: Vec<McpResultLine> = Vec::new();
    if !command.all && command.server.is_none() {
        results.push(McpResultLine::Static(format!(
            "Name a server or `all`, for example `/mcp {} <server>`.",
            command.action.as_str()
        )));
    }
    let known: HashSet<String> = before
        .iter()
        .filter_map(|status| status.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    let targets = action_targets(command, &before);
    if command.all && targets.is_empty() {
        results.push(McpResultLine::Static(
            nothing_to_do_line(command.action).to_string(),
        ));
    }
    for server in targets {
        if !known.contains(&server) {
            // Claude Code checks the name first. Its sentence then stays.
            results.push(McpResultLine::UnknownServer(server));
            continue;
        }
        if command.action == McpAction::Reconnect {
            prepared.reconnected.push(server.clone());
        }
        let outcome = match command.action {
            McpAction::Reconnect => await_action(control.mcp_reconnect(&server)).await,
            McpAction::Enable => await_action(control.mcp_toggle(&server, true)).await,
            McpAction::Disable => await_action(control.mcp_toggle(&server, false)).await,
            McpAction::Status => Ok(()),
        };
        match outcome {
            Ok(()) => results.push(McpResultLine::Done {
                action: command.action,
                server,
            }),
            Err(error) => results.push(McpResultLine::Failed {
                action: command.action,
                server,
                error: clean_mcp_error(&error),
            }),
        }
    }
    prepared.action = Some(McpActionOutcome {
        results,
        after: read_status(control).await,
    });
    prepared
}

/// Per-turn `/mcp` state owned by the session actor (Decision 7/21).
#[derive(Debug, Default)]
pub struct McpTurns {
    turns: HashMap<String, McpTurn>,
}

/// One `/mcp` turn: the command, the CLI text and message id of the first
/// output shape that carried it, and the produced replacement.
#[derive(Debug)]
pub struct McpTurn {
    pub command: McpCommand,
    /// The CLI text of the first output shape (the `originalOutput` of
    /// `takeLocalCommandMarkdown`).
    pub original_output: Option<String>,
    /// `messageIdForGrouping` of that shape, stamped on the replacement chunk.
    pub message_id: Option<String>,
    /// The produced replacement, once the spawned control run finishes.
    pub prepared: Option<McpPrepared>,
    /// Whether the replacement (or the kept CLI text) has been published.
    pub delivered: bool,
}

impl McpTurns {
    pub fn insert(&mut self, uuid: &str, command: McpCommand) {
        self.turns.insert(
            uuid.to_string(),
            McpTurn {
                command,
                original_output: None,
                message_id: None,
                prepared: None,
                delivered: false,
            },
        );
    }

    pub fn get(&self, uuid: &str) -> Option<&McpTurn> {
        self.turns.get(uuid)
    }

    pub fn get_mut(&mut self, uuid: &str) -> Option<&mut McpTurn> {
        self.turns.get_mut(uuid)
    }

    pub fn remove(&mut self, uuid: &str) -> Option<McpTurn> {
        self.turns.remove(uuid)
    }

    pub fn clear(&mut self) {
        self.turns.clear();
    }
}

/// `messageIdForGrouping` (`acp-agent.ts:12766-12782`): an assistant frame uses
/// its inner API message id when non-empty, else its frame uuid; every other
/// frame uses its uuid.
#[must_use]
pub fn message_id_of(line: &Value) -> Option<String> {
    if line.get("type").and_then(Value::as_str) == Some("assistant") {
        if let Some(api_id) = line.pointer("/message/id").and_then(Value::as_str) {
            if !api_id.is_empty() {
                return Some(api_id.to_string());
            }
        }
    }
    line.get("uuid")
        .and_then(Value::as_str)
        .filter(|uuid| !uuid.is_empty())
        .map(str::to_string)
}

/// The text of an assistant message's text blocks, joined with no separator
/// (`assistantMessageText`).
#[must_use]
pub fn assistant_text(message: Option<&Value>) -> Option<String> {
    let message = message?;
    if let Some(content) = message.get("content").and_then(Value::as_str) {
        return if content.is_empty() {
            None
        } else {
            Some(content.to_string())
        };
    }
    let blocks = message.get("content").and_then(Value::as_array)?;
    let mut text = String::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(part) = block.get("text").and_then(Value::as_str) {
                text.push_str(part);
            }
        }
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn servers() -> Vec<Value> {
        vec![
            json!({"name":"github","status":"connected","scope":"user",
                   "tools":[{"name":"search"},{"name":"issue"}]}),
            json!({"name":"linear","status":"needs-auth","scope":"project","source":"plugin"}),
            json!({"name":"db","status":"failed","error":"spawn db-mcp ENOENT","scope":"local"}),
            json!({"name":"docs","status":"pending","scope":"user"}),
            json!({"name":"old","status":"disabled","scope":"user"}),
        ]
    }

    fn status(action: McpAction) -> McpCommand {
        McpCommand {
            action,
            all: false,
            server: None,
        }
    }

    fn enable(server: Option<&str>) -> McpCommand {
        McpCommand {
            action: McpAction::Enable,
            all: false,
            server: server.map(str::to_string),
        }
    }

    fn reconnect(server: &str) -> McpCommand {
        McpCommand {
            action: McpAction::Reconnect,
            all: false,
            server: Some(server.to_string()),
        }
    }

    /// rust-air-gaps G12 (S-15) — the upstream parse table.
    #[test]
    fn inv_56_parse_table() {
        assert_eq!(parse_mcp_command("/mcp"), Some(status(McpAction::Status)));
        assert_eq!(
            parse_mcp_command("/mcp reconnect"),
            Some(McpCommand {
                action: McpAction::Reconnect,
                all: true,
                server: None
            })
        );
        assert_eq!(
            parse_mcp_command("/mcp reconnect all"),
            Some(McpCommand {
                action: McpAction::Reconnect,
                all: true,
                server: None
            })
        );
        assert_eq!(
            parse_mcp_command("/mcp Reconnect my server"),
            Some(reconnect("my server"))
        );
        assert_eq!(
            parse_mcp_command("/mcp disable all"),
            Some(McpCommand {
                action: McpAction::Disable,
                all: true,
                server: None
            })
        );
        assert_eq!(parse_mcp_command("/mcp enable"), Some(enable(None)));
        assert_eq!(parse_mcp_command(" /mcp "), Some(status(McpAction::Status)));
    }

    /// rust-air-gaps G12 (S-15) — every other prompt is left alone.
    #[test]
    fn inv_56_parse_rejects_other_prompts() {
        for text in [
            "/mcp:github:prompt",
            "/mcp help",
            "/mcp list",
            "/mcpx",
            "please run /mcp",
        ] {
            assert_eq!(parse_mcp_command(text), None, "{text}");
        }
        // A long prompt is not split into words (the prefix check runs first).
        let long = format!("hello {}/mcp", "word ".repeat(100_000));
        assert_eq!(parse_mcp_command(&long), None);
    }

    /// rust-air-gaps G12 (S-15) — JS `\s` before the prefix, including U+FEFF.
    #[test]
    fn inv_56_parse_leading_whitespace() {
        assert_eq!(
            parse_mcp_command("\n\t\u{00a0}\u{feff} /mcp enable  my\nserver "),
            Some(enable(Some("my server")))
        );
        assert_eq!(
            parse_mcp_command("\u{2028}/mcp"),
            Some(status(McpAction::Status))
        );
    }

    /// rust-air-gaps G12 (S-17) — no configured server.
    #[test]
    fn inv_56_format_empty() {
        assert_eq!(format_mcp_status(&[]), "No MCP servers are configured.");
    }

    /// rust-air-gaps G12 (S-17) — the upstream golden list.
    #[test]
    fn inv_56_format_summary_groups_disabled_hint() {
        assert_eq!(
            format_mcp_status(&servers()),
            [
                "**MCP servers:** 5 (1 failed, 1 needs authentication, 1 connecting, 1 connected, 1 disabled)",
                "",
                "**Failed**",
                "- `db`: spawn db-mcp ENOENT",
                "",
                "**Needs authentication**",
                "- `linear`",
                "",
                "**Connecting**",
                "- `docs`",
                "",
                "**Connected**",
                "- `github`: 2 tools",
                "",
                "**Disabled:** `old`",
                "",
                "Run `/mcp reconnect <server>` to reconnect one server, or `/mcp reconnect` to reconnect every server that is not connected and not disabled.",
            ]
            .join("\n")
        );
    }

    /// rust-air-gaps G12 (S-17) — SDK order inside a group, empty groups
    /// omitted.
    #[test]
    fn inv_56_format_keeps_order_and_omits_empty_groups() {
        let markdown = format_mcp_status(&[
            json!({"name":"b","status":"failed","error":"boom"}),
            json!({"name":"c","status":"connected"}),
            json!({"name":"a","status":"failed","error":"bang"}),
            json!({"name":"x","status":"disabled"}),
            json!({"name":"y","status":"disabled"}),
        ]);
        assert!(markdown.contains("**Failed**\n- `b`: boom\n- `a`: bang"));
        assert!(markdown.contains("**Connected**\n- `c`\n"));
        assert!(markdown.contains("**Disabled:** `x`, `y`"));
        assert!(!markdown.contains("**Connecting**"));
        assert!(!markdown.contains("**Needs authentication**"));
        assert!(markdown.contains("2 failed"));
    }

    /// rust-air-gaps G12 (S-17) — tool count singular/plural, empty list.
    #[test]
    fn inv_56_format_tool_count() {
        let markdown = format_mcp_status(&[
            json!({"name":"one","status":"connected","tools":[{"name":"t"}]}),
            json!({"name":"none","status":"connected","tools":[]}),
            json!({"name":"many","status":"connected","tools":[{"name":"a"},{"name":"b"}]}),
        ]);
        assert!(markdown.contains("- `one`: 1 tool\n- `none`: 0 tools\n- `many`: 2 tools"));
        assert!(markdown.contains("**MCP servers:** 3 (3 connected)"));
    }

    /// rust-air-gaps G12 (S-17) — plural needs-authentication summary.
    #[test]
    fn inv_56_format_needs_auth_plural() {
        let markdown = format_mcp_status(&[
            json!({"name":"a","status":"needs-auth"}),
            json!({"name":"b","status":"needs-auth"}),
        ]);
        assert!(markdown.contains("**MCP servers:** 2 (2 need authentication)"));
    }

    /// rust-air-gaps G12 (S-17) — the hint only when a server can reconnect.
    #[test]
    fn inv_56_format_hint_only_when_reconnectable() {
        let hint = "Run `/mcp reconnect <server>`";
        assert!(!format_mcp_status(&[
            json!({"name":"a","status":"connected"}),
            json!({"name":"b","status":"disabled"}),
        ])
        .contains(hint));
        for state in ["failed", "needs-auth", "pending"] {
            assert!(
                format_mcp_status(&[json!({"name":"a","status":state})]).contains(hint),
                "{state}"
            );
        }
    }

    /// rust-air-gaps G12 (S-17) — scope/source only disambiguates duplicates.
    #[test]
    fn inv_56_format_scope_tags() {
        let markdown = format_mcp_status(&servers());
        assert!(!markdown.contains("Scope"));
        assert!(!markdown.contains("plugin"));
        let markdown = format_mcp_status(&[
            json!({"name":"ctx","status":"connected","scope":"user"}),
            json!({"name":"ctx","status":"failed","scope":"project","error":"boom"}),
            json!({"name":"solo","status":"connected","scope":"user"}),
        ]);
        assert!(markdown.contains("- `ctx` (project): boom"));
        assert!(markdown.contains("- `ctx` (user)\n- `solo`"));
    }

    /// rust-air-gaps G12 (S-17/S-18) — a long error is shortened to two
    /// sentences.
    #[test]
    fn inv_56_format_shortens_long_error() {
        let long = "MCP startup failed: handshaking with MCP server failed: JSON-RPC error: -32603: No IDE found. Install the \"MCP Server\" plugin and ensure it is enabled. Probed ports: 64342: JSON-RPC error: -32603: No IDE found. Install the \"MCP Server\" plugin and ensure it is enabled.";
        let markdown = format_mcp_status(&[json!({"name":"ide","status":"failed","error":long})]);
        assert!(markdown.contains(
            "- `ide`: No IDE found. Install the \"MCP Server\" plugin and ensure it is enabled.\n"
        ));
    }

    /// rust-air-gaps G12 (S-17) — Markdown escaping keeps one line.
    #[test]
    fn inv_56_format_escapes_error() {
        let markdown = format_mcp_status(&[
            json!({"name":"my_server","status":"failed","error":"<html> *bad*\n`x` | y"}),
        ]);
        assert!(markdown.contains("- `my_server`: \\<html\\> \\*bad\\* \\`x\\` \\| y\n"));
    }

    /// rust-air-gaps G12 (S-17) — unknown statuses count and group.
    #[test]
    fn inv_56_format_unknown_status() {
        let markdown = format_mcp_status(&[
            json!({"name":"a","status":"connected"}),
            json!({"name":"b","status":"sleeping"}),
        ]);
        assert!(markdown.contains("**MCP servers:** 2 (1 connected, 1 sleeping)"));
        assert!(markdown.contains("**Connected**\n- `a`\n\n**Sleeping**\n- `b`"));
        let markdown = format_mcp_status(&[
            json!({"name":"a","status":"connected"}),
            json!({"name":"b","status":"disabled"}),
            json!({"name":"c","status":"sleeping"}),
            json!({"name":"d","status":"pending"}),
            json!({"name":"e","status":"failed"}),
        ]);
        assert!(markdown.contains(
            "**MCP servers:** 5 (1 failed, 1 connecting, 1 connected, 1 sleeping, 1 disabled)"
        ));
        assert!(markdown.contains("**Sleeping**"));
    }

    /// rust-air-gaps G12 (S-17) — a whitespace-only error has no detail.
    #[test]
    fn inv_56_format_whitespace_error() {
        let markdown = format_mcp_status(&[
            json!({"name":"x","status":"failed","error":"  \n ","tools":[{"name":"a"},{"name":"b"}]}),
            json!({"name":"y","status":"failed","error":"\t"}),
        ]);
        assert!(markdown.contains("- `x`: 2 tools\n- `y`\n"));
    }

    /// rust-air-gaps G12 (S-17) — a name with a backtick gets a longer fence.
    #[test]
    fn inv_56_format_backtick_name() {
        let markdown = format_mcp_status(&[
            json!({"name":"a`b","status":"connected"}),
            json!({"name":"`edge","status":"disabled"}),
        ]);
        assert!(markdown.contains("- ``a`b``\n"));
        assert!(markdown.contains("**Disabled:** `` `edge ``"));
    }

    /// rust-air-gaps G12 (S-18) — the upstream cleanMcpError table.
    #[test]
    fn inv_56_clean_wrapped_error() {
        let long = "MCP startup failed: handshaking with MCP server failed: JSON-RPC error: -32603: No IDE found. Install the \"MCP Server\" plugin and ensure it is enabled. Probed ports: 64342: JSON-RPC error: -32603: No IDE found. Install the \"MCP Server\" plugin and ensure it is enabled.";
        assert_eq!(
            clean_mcp_error(long),
            "No IDE found. Install the \"MCP Server\" plugin and ensure it is enabled."
        );
    }

    /// rust-air-gaps G12 (S-18) — a short error stays as it is.
    #[test]
    fn inv_56_clean_short_error() {
        assert_eq!(
            clean_mcp_error("spawn db-mcp ENOENT"),
            "spawn db-mcp ENOENT"
        );
        assert_eq!(
            clean_mcp_error("Failed to connect to the server: timeout"),
            "Failed to connect to the server: timeout"
        );
    }

    /// rust-air-gaps G12 (S-18) — a type path and a long generic are wrappers.
    #[test]
    fn inv_56_clean_type_paths() {
        assert_eq!(
            clean_mcp_error(
                "rmcp::service::ServiceError: Transport closed: connection reset by peer"
            ),
            "Transport closed: connection reset by peer"
        );
        assert_eq!(
            clean_mcp_error(
                "Result<ServerInfo, TransportError<std::io::Error>>: the server exited with code 1"
            ),
            "the server exited with code 1"
        );
    }

    /// rust-air-gaps G12 (S-18) — at most two sentences.
    #[test]
    fn inv_56_clean_two_sentences() {
        assert_eq!(clean_mcp_error("One. Two! Three? Four."), "One. Two!");
    }

    /// rust-air-gaps G12 (S-18) — 160 code points with an ellipsis.
    #[test]
    fn inv_56_clean_truncates() {
        let cleaned = clean_mcp_error(&format!("{}end", "word ".repeat(60)));
        assert!(cleaned.chars().count() <= 160);
        assert!(cleaned.ends_with("word…"));
    }

    /// rust-air-gaps G12 (S-18) — an error code is never alone.
    #[test]
    fn inv_56_clean_error_codes() {
        assert_eq!(clean_mcp_error("error: 401"), "error: 401");
        assert_eq!(clean_mcp_error("Error: 12345"), "Error: 12345");
        assert_eq!(clean_mcp_error("Error 401"), "Error 401");
        assert_eq!(
            clean_mcp_error("MCP startup failed: JSON-RPC error: -32603"),
            "JSON-RPC error: -32603"
        );
    }

    /// rust-air-gaps G12 (S-18) — the MCP SDK wrapper.
    #[test]
    fn inv_56_clean_mcp_wrapper() {
        assert_eq!(
            clean_mcp_error("McpError: MCP error -32001: Request timed out"),
            "Request timed out"
        );
    }

    /// rust-air-gaps G12 (S-18) — truncation at a code-point boundary.
    #[test]
    fn inv_56_clean_code_point_boundary() {
        let cleaned = clean_mcp_error(&"😀".repeat(200));
        assert_eq!(clean_mcp_error(&"😀".repeat(100)), "😀".repeat(100));
        assert!(cleaned.chars().count() <= 160);
        assert!(cleaned.chars().all(|c| c == '😀' || c == '…'));
        assert!(cleaned.ends_with("😀…"));
    }

    /// rust-air-gaps G12 (S-18) — whitespace-only error is empty.
    #[test]
    fn inv_56_clean_whitespace_only() {
        assert_eq!(clean_mcp_error(" \n\t "), "");
    }

    /// rust-air-gaps G12 (S-20) — the Claude Code terminal and unavailable
    /// sentences are dropped; the unknown-name sentence is kept.
    #[test]
    fn inv_56_render_action_sentences() {
        let outcome = McpActionOutcome {
            results: vec![McpResultLine::Done {
                action: McpAction::Reconnect,
                server: "db".to_string(),
            }],
            after: Some(servers()),
        };
        let rendered = render_action(
            &outcome,
            "Reconnect, enable, and disable aren't available in this session.",
        );
        assert_eq!(
            rendered,
            format!("- Reconnected `db`\n\n{}", format_mcp_status(&servers()))
        );
        assert!(!rendered.contains("aren't available"));

        let unknown = McpActionOutcome {
            results: vec![McpResultLine::UnknownServer("nosuch".to_string())],
            after: Some(servers()),
        };
        assert_eq!(
            render_action(
                &unknown,
                "There's no MCP server named \"nosuch\". Run `/mcp` in the terminal to see configured servers."
            ),
            format!("There's no MCP server named \"nosuch\".\n\n{}", format_mcp_status(&servers()))
        );
        assert_eq!(
            render_action(
                &unknown,
                "Reconnect, enable, and disable aren't available in this session."
            ),
            format!(
                "There's no MCP server named `nosuch`.\n\n{}",
                format_mcp_status(&servers())
            )
        );
    }

    /// rust-air-gaps G12 (S-02) — the action targets follow the upstream
    /// filters.
    #[test]
    fn inv_56_action_targets() {
        let reconnect_all = McpCommand {
            action: McpAction::Reconnect,
            all: true,
            server: None,
        };
        assert_eq!(
            action_targets(&reconnect_all, &servers()),
            vec!["linear", "db", "docs"]
        );
        let disable_all = McpCommand {
            action: McpAction::Disable,
            all: true,
            server: None,
        };
        assert_eq!(
            action_targets(&disable_all, &servers()),
            vec!["github", "linear", "db", "docs"]
        );
        let enable_all = McpCommand {
            action: McpAction::Enable,
            all: true,
            server: None,
        };
        assert_eq!(action_targets(&enable_all, &servers()), vec!["old"]);
    }

    /// rust-air-gaps G12 (S-02) — the action assembly for the upstream cases.
    #[test]
    fn inv_56_render_action_cases() {
        let after = |changes: &[(&str, &str)]| {
            let mut statuses = servers();
            for status in &mut statuses {
                let name = status.get("name").and_then(Value::as_str).unwrap_or("");
                if let Some((_, state)) = changes.iter().find(|(n, _)| *n == name) {
                    status["status"] = json!(state);
                }
            }
            statuses
        };
        // "says when a reconnect of all has nothing to reconnect"
        let nothing = McpActionOutcome {
            results: vec![McpResultLine::Static(
                nothing_to_do_line(McpAction::Reconnect).to_string(),
            )],
            after: Some(vec![json!({"name":"github","status":"connected"})]),
        };
        assert_eq!(
            render_action(&nothing, "Every server is fine."),
            format!(
                "Every server is fine.\n\nEvery MCP server is connected or disabled. There is nothing to reconnect.\n\n{}",
                format_mcp_status(&[json!({"name":"github","status":"connected"})])
            )
        );
        // "asks for a server name for an enable without a name"
        let named = McpActionOutcome {
            results: vec![McpResultLine::Static(
                "Name a server or `all`, for example `/mcp disable <server>`.".to_string(),
            )],
            after: Some(servers()),
        };
        assert_eq!(
            render_action(
                &named,
                "Reconnect, enable, and disable aren't available in this session."
            ),
            format!(
                "Name a server or `all`, for example `/mcp disable <server>`.\n\n{}",
                format_mcp_status(&servers())
            )
        );
        // "isolates a control API error to its server"
        let isolated = McpActionOutcome {
            results: vec![
                McpResultLine::Done {
                    action: McpAction::Reconnect,
                    server: "linear".to_string(),
                },
                McpResultLine::Failed {
                    action: McpAction::Reconnect,
                    server: "db".to_string(),
                    error: "spawn db-mcp ENOENT".to_string(),
                },
                McpResultLine::Done {
                    action: McpAction::Reconnect,
                    server: "docs".to_string(),
                },
            ],
            after: Some(after(&[("db", "failed"), ("docs", "pending")])),
        };
        assert!(render_action(&isolated, "Reconnect, enable, and disable aren't available in this session.")
            .starts_with("- Reconnected `linear`\n- Couldn't reconnect `db`: spawn db-mcp ENOENT\n- Reconnected `docs`"));
    }

    /// rust-air-gaps G12 (S-08) — the upstream MCP-prefix rewrite.
    #[test]
    fn inv_56_rewrite_mcp_prompt() {
        assert_eq!(
            rewrite_mcp_prompt("/mcp:server:name args"),
            "/server:name (MCP) args"
        );
        assert_eq!(rewrite_mcp_prompt("/mcp:server:name"), "/server:name (MCP)");
        assert_eq!(rewrite_mcp_prompt("/mcp"), "/mcp");
        assert_eq!(rewrite_mcp_prompt("/mcp:server"), "/mcp:server");
        assert_eq!(rewrite_mcp_prompt("/mcp::name"), "/mcp::name");
        assert_eq!(
            rewrite_mcp_prompt("/mcp:server:name a\nb"),
            "/mcp:server:name a\nb"
        );
        assert_eq!(rewrite_mcp_prompt("hello /mcp:a:b"), "hello /mcp:a:b");
        assert_eq!(rewrite_mcp_prompt("/mcp:a:b "), "/a:b (MCP)");
    }

    /// rust-air-gaps G12 — only a single text block is a `/mcp` prompt.
    #[test]
    fn inv_56_prompt_mcp_command_single_text_block() {
        assert_eq!(
            prompt_mcp_command(&json!({"prompt":[{"type":"text","text":"/mcp"}]})),
            Some(status(McpAction::Status))
        );
        assert_eq!(
            prompt_mcp_command(
                &json!({"prompt":[{"type":"text","text":"/mcp"},{"type":"text","text":"more"}]})
            ),
            None
        );
        assert_eq!(
            prompt_mcp_command(
                &json!({"prompt":[{"type":"image","data":"","mimeType":"image/png"}]})
            ),
            None
        );
        assert_eq!(
            prompt_mcp_command(&json!({"prompt":[{"type":"text","text":"/mcp:github:prompt"}]})),
            None
        );
    }

    /// rust-air-gaps G12 (S-02) — a failed status read keeps the CLI text.
    #[test]
    fn inv_56_render_status_failure_keeps_cli_text() {
        assert_eq!(McpPrepared::default().render("cli text"), None);
        let prepared = McpPrepared {
            status_markdown: Some("list".to_string()),
            ..McpPrepared::default()
        };
        assert_eq!(prepared.render("cli text"), Some("list".to_string()));
    }

    /// rust-air-gaps G12 — `messageIdForGrouping` for the two output shapes.
    #[test]
    fn inv_56_message_id_of() {
        assert_eq!(
            message_id_of(&json!({"type":"assistant","uuid":"frame","message":{"id":"api"}})),
            Some("api".to_string())
        );
        assert_eq!(
            message_id_of(&json!({"type":"assistant","uuid":"frame","message":{"id":""}})),
            Some("frame".to_string())
        );
        assert_eq!(
            message_id_of(&json!({"type":"system","uuid":"local"})),
            Some("local".to_string())
        );
        assert_eq!(message_id_of(&json!({"type":"result"})), None);
    }

    /// rust-air-gaps G12 — `assistantMessageText` joins text blocks.
    #[test]
    fn inv_56_assistant_text() {
        assert_eq!(
            assistant_text(Some(
                &json!({"content":[{"type":"text","text":"a"},{"type":"tool_use"},{"type":"text","text":"b"}]})
            )),
            Some("ab".to_string())
        );
        assert_eq!(
            assistant_text(Some(&json!({"content":"str"}))),
            Some("str".to_string())
        );
        assert_eq!(assistant_text(Some(&json!({"content":""}))), None);
        assert_eq!(assistant_text(None), None);
    }
}
