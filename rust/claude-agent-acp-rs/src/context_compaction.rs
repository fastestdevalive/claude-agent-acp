//! `context-compaction.ts`: Claude's compaction signals as one idempotent ACP
//! lifecycle.
//!
//! The lifecycle is a synchronous state machine, like the native-subagent
//! runtime: every method mutates its state and returns the ordered updates the
//! owner must publish. No lock is held across an await, and the owner sends
//! them in exactly Node's order.
//!
//! Two presentations exist:
//!
//! - [`CompactionPresentation::CompactionUpdate`]: the ACP session-compaction
//!   contract (ID-addressed `compaction_update` / `compaction_summary_chunk`),
//!   used when the v1 client advertises `clientCapabilities.session.compaction`.
//! - [`CompactionPresentation::ToolCall`]: the legacy synthetic "Compact
//!   conversation" tool call, kept for every other client.

use serde_json::{json, Map, Value};

use crate::context_compaction_meta::{
    context_compaction_metadata_from_boundary, create_context_compaction_meta, CompactionFacts,
};
use crate::tool_reporters_compaction::{
    compaction_tool_call_finished, compaction_tool_call_in_progress, compaction_tool_call_started,
};

/// How compaction reaches the client (`CompactionPresentation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompactionPresentation {
    /// The legacy synthetic "Compact conversation" tool call.
    #[default]
    ToolCall,
    /// The ID-addressed ACP compaction updates.
    CompactionUpdate,
}

/// A terminal outcome the runtime reports (`CompactionStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionStatus {
    Completed,
    Failed,
}

impl CompactionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

/// The terminal status of an entity, including the turn-boundary `cancelled`
/// (`TerminalStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalStatus {
    Completed,
    Failed,
    Cancelled,
}

impl TerminalStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl From<CompactionStatus> for TerminalStatus {
    fn from(status: CompactionStatus) -> Self {
        match status {
            CompactionStatus::Completed => Self::Completed,
            CompactionStatus::Failed => Self::Failed,
        }
    }
}

/// One compaction entity (`CompactionState`).
#[derive(Debug, Clone, Default)]
struct CompactionState {
    /// Doubles as the `toolCallId` under the legacy presentation.
    compaction_id: String,
    terminal_status: Option<TerminalStatus>,
    heartbeat_sent: bool,
    /// The summary text that went out as chunks.
    streamed_summary: Option<String>,
    /// The user-displayable summary reported by the PostCompact hook.
    summary: Option<String>,
}

/// `ContextCompactionLifecycle`: the per-session compaction lifecycle.
#[derive(Debug)]
pub struct ContextCompactionLifecycle {
    active: Option<CompactionState>,
    output_delivered: bool,
    duplicate_error_output: Option<String>,
    /// After an interruption, uncorrelated terminal frames and hooks may still
    /// arrive from the abandoned work. Wait for a known new turn before
    /// accepting them again.
    interrupted: bool,
    /// A summary reported by PostCompact before the opening status was
    /// consumed. Deliberately survives [`Self::reset`].
    pending_summary: Option<String>,
    session_id: String,
    presentation: CompactionPresentation,
    air_client: bool,
}

impl Default for ContextCompactionLifecycle {
    fn default() -> Self {
        Self::new("", CompactionPresentation::ToolCall, false)
    }
}

impl ContextCompactionLifecycle {
    /// A lifecycle for `session_id`; `air_client` gates the compaction facts
    /// (only AIR gets `_meta.jetbrains.air.contextCompaction`).
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        presentation: CompactionPresentation,
        air_client: bool,
    ) -> Self {
        Self {
            active: None,
            output_delivered: false,
            duplicate_error_output: None,
            interrupted: false,
            pending_summary: None,
            session_id: session_id.into(),
            presentation,
            air_client,
        }
    }

    /// `hasDeliveredOutput`.
    #[must_use]
    pub fn has_delivered_output(&self) -> bool {
        self.output_delivered
    }

    /// The presentation this lifecycle publishes.
    #[must_use]
    pub fn presentation(&self) -> CompactionPresentation {
        self.presentation
    }

    /// The session the updates are addressed to.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Close the lifecycle at a turn boundary (result, cancel, idle-abandon).
    ///
    /// A `compaction_update` entity still `in_progress` gets its one terminal
    /// status, `cancelled`. The legacy tool call is deliberately left as it
    /// was: ACP `ToolCallStatus` has no cancelled state.
    pub fn reset(&mut self) -> Vec<Value> {
        let state = self.active.take();
        self.output_delivered = false;
        self.duplicate_error_output = None;
        if self.presentation != CompactionPresentation::CompactionUpdate {
            return Vec::new();
        }
        let Some(state) = state else {
            return Vec::new();
        };
        if state.terminal_status.is_some() {
            return Vec::new();
        }
        let mut update = Map::new();
        update.insert("sessionUpdate".into(), json!("compaction_update"));
        if !state.compaction_id.is_empty() {
            update.insert("compactionId".into(), json!(state.compaction_id));
        }
        update.insert("status".into(), json!("cancelled"));
        vec![Value::Object(update)]
    }

    /// Abandon the current work before settling its prompt or tearing down the
    /// stream. Unlike a normal result boundary, its pending hooks are not safe
    /// to carry into the next compaction.
    pub fn interrupt(&mut self) -> Vec<Value> {
        if self.presentation == CompactionPresentation::CompactionUpdate {
            self.interrupted = true;
            self.pending_summary = None;
        }
        self.reset()
    }

    /// A live command's dispatch/echo proves the interrupted command's tail has
    /// drained. Do not reset normal lifecycle state.
    pub fn resume(&mut self) {
        self.interrupted = false;
    }

    /// Consume the duplicate of a failed manual compaction's error, which Claude
    /// also emits as local-command stdout.
    pub fn consume_duplicate_error_output(&mut self, content: &str) -> bool {
        let Some(duplicate) = &self.duplicate_error_output else {
            return false;
        };
        if content.trim() != duplicate.trim() {
            return false;
        }
        self.duplicate_error_output = None;
        true
    }

    /// `start`: open the entity on the runtime's `compacting` status.
    pub fn start(&mut self, compaction_id: &str) -> Vec<Value> {
        if self.interrupted {
            return Vec::new();
        }
        if self
            .active
            .as_ref()
            .is_some_and(|state| state.terminal_status.is_none())
        {
            return Vec::new();
        }

        self.open(compaction_id, None);
        match self.presentation {
            CompactionPresentation::CompactionUpdate => {
                let mut update = Map::new();
                update.insert("sessionUpdate".into(), json!("compaction_update"));
                if !compaction_id.is_empty() {
                    update.insert("compactionId".into(), json!(compaction_id));
                }
                update.insert("status".into(), json!("in_progress"));
                if self.air_client {
                    update.insert(
                        "_meta".into(),
                        create_context_compaction_meta(&CompactionFacts::default()),
                    );
                }
                vec![Value::Object(update)]
            }
            CompactionPresentation::ToolCall => {
                vec![compaction_tool_call_started(compaction_id, self.air_client)]
            }
        }
    }

    /// Streaming progress from the API's compaction content block.
    ///
    /// Under `compaction_update` the block's text streams as
    /// `compaction_summary_chunk`s, but only into an entity the CLI announced
    /// with its `compacting` status. The legacy tool call opens on first sight
    /// and re-asserts `in_progress` once.
    pub fn heartbeat(&mut self, fallback_id: &str, summary_chunk: Option<&str>) -> Vec<Value> {
        match self.presentation {
            CompactionPresentation::CompactionUpdate => {
                let Some(state) = self.active.as_mut() else {
                    return Vec::new();
                };
                if state.terminal_status.is_some() {
                    return Vec::new();
                }
                let Some(chunk) = summary_chunk.filter(|chunk| !chunk.is_empty()) else {
                    return Vec::new();
                };
                let mut streamed = state.streamed_summary.take().unwrap_or_default();
                streamed.push_str(chunk);
                state.streamed_summary = Some(streamed);
                let mut update = Map::new();
                update.insert("sessionUpdate".into(), json!("compaction_summary_chunk"));
                if !state.compaction_id.is_empty() {
                    update.insert("compactionId".into(), json!(state.compaction_id));
                }
                update.insert("content".into(), json!({"type": "text", "text": chunk}));
                vec![Value::Object(update)]
            }
            CompactionPresentation::ToolCall => {
                let mut outs = Vec::new();
                if self.active.is_none() {
                    outs.extend(self.start(fallback_id));
                }
                let Some(state) = self.active.as_mut() else {
                    return outs;
                };
                if state.terminal_status.is_some() || state.heartbeat_sent {
                    return outs;
                }
                state.heartbeat_sent = true;
                outs.push(compaction_tool_call_in_progress(
                    &state.compaction_id,
                    self.air_client,
                ));
                outs
            }
        }
    }

    /// The PostCompact hook's summary.
    ///
    /// It lands while its compaction is `in_progress` and rides on the terminal
    /// update. Anything else — no entity yet, or an entity of a previous,
    /// already-terminal compaction whose status frame the consumer is still
    /// catching up to — belongs to the compaction whose frames are ahead of the
    /// consumer, and waits for it. Only the `compaction_update` presentation
    /// carries the summary. Returns whether a summary was retained.
    pub fn record_summary(&mut self, raw_summary: Option<&Value>) -> bool {
        if self.interrupted || self.presentation != CompactionPresentation::CompactionUpdate {
            return false;
        }
        let Some(raw_summary) = raw_summary.and_then(Value::as_str) else {
            return false;
        };
        let Some(summary) = compaction_summary_text(raw_summary) else {
            return false;
        };

        match self.active.as_mut() {
            Some(state) if state.terminal_status.is_none() => state.summary = Some(summary),
            _ => self.pending_summary = Some(summary),
        }
        true
    }

    /// `finish`: the runtime's terminal status or the `compact_boundary`.
    ///
    /// `enrich_terminal` marks the boundary: it may re-send a terminal entity
    /// with the boundary's facts. A summary that went out as chunks is not sent
    /// again in full; the summary is left out only when both hold the same
    /// text.
    pub fn finish(
        &mut self,
        compaction_id: &str,
        status: CompactionStatus,
        facts: &CompactionFacts,
        enrich_terminal: bool,
    ) -> Vec<Value> {
        if self.interrupted {
            return Vec::new();
        }
        // The opening status can be missed (replay, or a terminal-only
        // runtime): the first update for the ID is then already terminal.
        let opened = self.active.is_none();
        let first_terminal = self
            .active
            .as_ref()
            .is_none_or(|state| state.terminal_status.is_none());
        if opened {
            self.open(compaction_id, Some(status.into()));
        }
        let Some(state) = self.active.as_mut() else {
            return Vec::new();
        };
        if first_terminal {
            state.terminal_status = Some(status.into());
        } else if !enrich_terminal {
            return Vec::new();
        }
        let terminal_status = state.terminal_status.unwrap_or(status.into());
        let summary_streamed = state.streamed_summary == state.summary;
        let summary = state.summary.clone();
        let compaction_id = state.compaction_id.clone();
        if status == CompactionStatus::Failed {
            if let Some(error) = facts.error.as_ref().filter(|error| !error.is_empty()) {
                self.duplicate_error_output = Some(error.clone());
            }
        }
        let has_metadata = !facts.is_empty();

        if self.presentation == CompactionPresentation::CompactionUpdate {
            let summary = if first_terminal
                && terminal_status == TerminalStatus::Completed
                && !summary_streamed
            {
                summary
            } else {
                None
            };
            let mut update = Map::new();
            update.insert("sessionUpdate".into(), json!("compaction_update"));
            if !compaction_id.is_empty() {
                update.insert("compactionId".into(), json!(compaction_id));
            }
            update.insert("status".into(), json!(terminal_status.as_str()));
            if let Some(summary) = summary {
                update.insert("summary".into(), json!([{"type": "text", "text": summary}]));
            }
            if terminal_status == TerminalStatus::Failed {
                if let Some(error) = facts.error.as_ref().filter(|error| !error.is_empty()) {
                    update.insert("error".into(), json!(error));
                }
            }
            // `_meta` is a replace-patch: seed it with the first terminal, then
            // only re-send it when the boundary adds facts. The standard `error`
            // field carries the error, so `_meta` does not. Only AIR gets it.
            if self.air_client && (first_terminal || has_metadata) {
                update.insert(
                    "_meta".into(),
                    create_context_compaction_meta(&facts.without_error()),
                );
            }
            return vec![Value::Object(update)];
        }

        vec![compaction_tool_call_finished(
            &compaction_id,
            first_terminal.then_some(status.as_str()),
            facts,
            opened,
            self.air_client,
        )]
    }

    /// Make `compaction_id` the active entity, moving a pending hook summary
    /// onto it, and mark compaction output as delivered for the owning turn.
    fn open(&mut self, compaction_id: &str, terminal_status: Option<TerminalStatus>) {
        let summary = self.pending_summary.take();
        self.active = Some(CompactionState {
            compaction_id: compaction_id.to_string(),
            terminal_status,
            heartbeat_sent: false,
            streamed_summary: None,
            summary,
        });
        self.output_delivered = true;
    }
}

/// `clientSupportsCompactionUpdates`: the v1 `session.compaction` object.
#[must_use]
pub fn client_supports_compaction_updates(capabilities: Option<&Value>) -> bool {
    capabilities
        .and_then(|caps| caps.get("session"))
        .and_then(|session| session.get("compaction"))
        .is_some_and(Value::is_object)
}

/// `isCompactSummaryMessage`: the post-compaction user message Claude Code
/// persists with the summary of the dropped history.
#[must_use]
pub fn is_compact_summary_message(message: &Value) -> bool {
    message.get("type").and_then(Value::as_str) == Some("user")
        && message.get("isCompactSummary").and_then(Value::as_bool) == Some(true)
}

// ----- compactionSummaryText -------------------------------------------------

/// Whether `c` is whitespace for JavaScript's `\s`.
fn is_js_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

fn skip_js_space(text: &str, mut i: usize) -> usize {
    for c in text[i..].chars() {
        if !is_js_space(c) {
            break;
        }
        i += c.len_utf8();
    }
    i
}

fn hspace_end(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    i
}

fn ci_starts_with(b: &[u8], i: usize, literal: &str) -> bool {
    let literal = literal.as_bytes();
    i + literal.len() <= b.len() && b[i..i + literal.len()].eq_ignore_ascii_case(literal)
}

fn find_ci(b: &[u8], from: usize, needle: &str) -> Option<usize> {
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return Some(from);
    }
    if from + needle.len() > b.len() {
        return None;
    }
    (from..=b.len() - needle.len()).find(|&i| b[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

fn rfind_ci(b: &[u8], needle: &str) -> Option<usize> {
    let needle = needle.as_bytes();
    if needle.len() > b.len() {
        return None;
    }
    (0..=b.len() - needle.len())
        .rev()
        .find(|&i| b[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

/// Whether `i` is a JavaScript multiline `$`: before any line terminator.
fn is_line_end(b: &[u8], i: usize) -> bool {
    i >= b.len() || b[i] == b'\n' || b[i] == b'\r'
}

/// The next line start at or after `from` (only `\n` separates lines; the tags
/// are ASCII and the tests' CRLF input still has the `\n`).
fn next_line_start(text: &str, from: usize) -> Option<usize> {
    text[from..].find('\n').map(|rel| from + rel + 1)
}

/// `^[ \t]*<analysis>[ \t]*\r?\n`: the own-line opening of an analysis block.
fn find_analysis_open(text: &str, from: usize) -> Option<(usize, usize)> {
    let b = text.as_bytes();
    let mut line_start = from;
    loop {
        if line_start == 0 || b[line_start - 1] == b'\n' {
            let tag = hspace_end(b, line_start);
            if ci_starts_with(b, tag, "<analysis>") {
                let after = hspace_end(b, tag + "<analysis>".len());
                if after < b.len() && b[after] == b'\n' {
                    return Some((line_start, after + 1));
                }
                if after + 1 < b.len() && b[after] == b'\r' && b[after + 1] == b'\n' {
                    return Some((line_start, after + 2));
                }
            }
        }
        line_start = next_line_start(text, line_start)?;
    }
}

/// `\n[ \t]*<\/analysis>[ \t]*$`: the own-line close of an analysis block. The
/// returned index is the match's end, before the line terminator.
fn find_analysis_close(text: &str, from: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut newline = from;
    loop {
        let rel = text[newline..].find('\n')?;
        let line_start = newline + rel + 1;
        let tag = hspace_end(b, line_start);
        if ci_starts_with(b, tag, "</analysis>") {
            let after = hspace_end(b, tag + "</analysis>".len());
            if is_line_end(b, after) {
                return Some(after);
            }
        }
        newline = line_start;
    }
}

/// Replace every own-line `<analysis>…</analysis>` block (`ANALYSIS_BLOCKS[0]`).
fn strip_analysis_blocks(text: &str) -> String {
    let mut out = String::new();
    let mut cursor = 0;
    let mut search = 0;
    while let Some((open_start, open_end)) = find_analysis_open(text, search) {
        // The close's `\n` must come AFTER the open's own newline: JS consumed
        // that newline in the open match, so a non-leading `<analysis>\n
        // </analysis>` has no newline left for the close and is kept.
        if let Some(close_end) = find_analysis_close(text, open_end) {
            out.push_str(&text[cursor..open_start]);
            cursor = close_end;
            search = close_end;
        } else {
            search = open_end;
        }
    }
    out.push_str(&text[cursor..]);
    out
}

/// Replace a leading inline analysis block (`ANALYSIS_BLOCKS[1]`).
fn strip_leading_inline_analysis(text: &str) -> String {
    let b = text.as_bytes();
    let start = skip_js_space(text, 0);
    if !ci_starts_with(b, start, "<analysis>") {
        return text.to_string();
    }
    let after_open = start + "<analysis>".len();
    let Some(close) = find_ci(b, after_open, "</analysis>") else {
        return text.to_string();
    };
    text[close + "</analysis>".len()..].to_string()
}

/// The last own-line `</summary>` match, at its line start (`SUMMARY_CLOSE_LINE`).
fn last_summary_close_line(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    let mut last = None;
    let mut line_start = 0;
    loop {
        let tag = hspace_end(b, line_start);
        if ci_starts_with(b, tag, "</summary>") {
            let after = hspace_end(b, tag + "</summary>".len());
            if is_line_end(b, after) {
                last = Some(line_start);
            }
        }
        match next_line_start(text, line_start) {
            Some(next) => line_start = next,
            None => return last,
        }
    }
}

/// The last own-line `<summary>\r?\n` match before the end (`SUMMARY_OPEN_LINE`).
fn last_summary_open_line(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    let mut last = None;
    let mut line_start = 0;
    loop {
        let tag = hspace_end(b, line_start);
        if ci_starts_with(b, tag, "<summary>") {
            let after = hspace_end(b, tag + "<summary>".len());
            if after < b.len() && b[after] == b'\n' {
                last = Some(after + 1);
            } else if after + 1 < b.len() && b[after] == b'\r' && b[after + 1] == b'\n' {
                last = Some(after + 2);
            }
        }
        match next_line_start(text, line_start) {
            Some(next) => line_start = next,
            None => return last,
        }
    }
}

/// `taggedSummary`: the `<summary>` body, own-line tags first, then the widest
/// inline span.
fn tagged_summary(text: &str) -> Option<String> {
    if let Some(close_start) = last_summary_close_line(text) {
        if let Some(open_end) = last_summary_open_line(&text[..close_start]) {
            return Some(text[open_end..close_start].to_string());
        }
    }
    // Inline tags: the widest span, so a quoted `</summary>` inside the body
    // does not end it early.
    let b = text.as_bytes();
    let open = find_ci(b, 0, "<summary>")?;
    let close = rfind_ci(b, "</summary>")?;
    if close > open {
        return Some(text[open + "<summary>".len()..close].to_string());
    }
    None
}

/// `^\s*This session is being continued from a previous conversation`
/// (`CONTINUATION_PREAMBLE`).
fn has_continuation_preamble(text: &str) -> bool {
    let b = text.as_bytes();
    let start = skip_js_space(text, 0);
    ci_starts_with(
        b,
        start,
        "This session is being continued from a previous conversation",
    )
}

/// The end of the persisted continuation framing's preamble
/// (`PERSISTED_PREAMBLE`), or `None` when the framing is not recognized.
fn persisted_preamble_end(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    let literal = "This session is being continued from a previous conversation";
    let start = skip_js_space(text, 0);
    if !text[start..].starts_with(literal) {
        return None;
    }
    let after_literal = start + literal.len();
    let newline = text[after_literal..].find('\n')? + after_literal;
    let mut after_newlines = newline;
    while after_newlines < b.len() && b[after_newlines] == b'\n' {
        after_newlines += 1;
    }
    if !text[after_newlines..].starts_with("Summary:") {
        return None;
    }
    let mut after_summary = hspace_end(b, after_newlines + "Summary:".len());
    if after_summary < b.len() && b[after_summary] == b'\n' {
        return Some(after_summary + 1);
    }
    if after_summary + 1 < b.len() && b[after_summary] == b'\r' && b[after_summary + 1] == b'\n' {
        after_summary += 2;
        return Some(after_summary);
    }
    None
}

/// The start of a persisted trailer (`PERSISTED_TRAILERS`), which runs to the
/// end of the input.
fn persisted_trailer_start(text: &str, continue_trailer: bool) -> Option<usize> {
    let mut i = 0;
    while i < text.len() {
        if text.as_bytes()[i] == b'\n' {
            let mut after = i;
            while after < text.len() && text.as_bytes()[after] == b'\n' {
                after += 1;
            }
            let rest = &text[after..];
            let matches = if continue_trailer {
                let rest = rest.strip_prefix("Please ").unwrap_or(rest);
                let rest = rest
                    .strip_prefix("Continue")
                    .or_else(|| rest.strip_prefix("continue"));
                rest.and_then(|rest| rest.strip_prefix(" the conversation from where it left "))
                    .is_some_and(|rest| rest.strip_prefix("it ").unwrap_or(rest).starts_with("off"))
            } else {
                rest.starts_with("If you need specific details from before compaction")
            };
            if matches {
                return Some(i);
            }
            i = after;
        } else {
            i += 1;
        }
    }
    None
}

/// `compactionSummaryText`: reduce Claude's compaction output to the
/// user-displayable summary.
#[must_use]
pub fn compaction_summary_text(raw: &str) -> Option<String> {
    let mut text = strip_analysis_blocks(raw);
    text = strip_leading_inline_analysis(&text);
    if let Some(tagged) = tagged_summary(&text) {
        text = tagged;
    } else if has_continuation_preamble(&text) {
        let Some(end) = persisted_preamble_end(&text) else {
            // A continuation prompt whose framing is not recognized yields no
            // summary rather than model-facing instructions.
            return None;
        };
        text = text[end..].to_string();
        if let Some(start) = persisted_trailer_start(&text, false) {
            text.truncate(start);
        }
        if let Some(start) = persisted_trailer_start(&text, true) {
            text.truncate(start);
        }
    }
    let trimmed = text.trim_matches(is_js_space);
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// `contextCompactionMetadataFromBoundary`, re-exported for the session's
/// `compact_boundary` arm.
#[must_use]
pub fn metadata_from_boundary(compact_metadata: &Value) -> CompactionFacts {
    context_compaction_metadata_from_boundary(compact_metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERSISTED_SUMMARY: &str = "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\nSummary:\n1. Primary Request and Intent:\n   Count upward.\n\n2. Optional Next Step:\n   None.\n\nIf you need specific details from before compaction (like exact code snippets, error messages, or content you generated), read the full transcript at: /tmp/session.jsonl\nContinue the conversation from where it left off without asking the user any further questions. Resume directly — do not acknowledge the summary, do not recap what was happening.";

    struct Harness {
        sent: Vec<Value>,
        compaction: ContextCompactionLifecycle,
    }

    impl Harness {
        fn new(presentation: CompactionPresentation, air_client: bool) -> Self {
            Self {
                sent: Vec::new(),
                compaction: ContextCompactionLifecycle::new("s", presentation, air_client),
            }
        }

        fn states(&self) -> Vec<&str> {
            self.sent
                .iter()
                .map(|u| u["sessionUpdate"].as_str().unwrap_or(""))
                .collect()
        }

        fn start(&mut self, id: &str) {
            let updates = self.compaction.start(id);
            self.sent.extend(updates);
        }

        fn finish(
            &mut self,
            id: &str,
            status: CompactionStatus,
            facts: &CompactionFacts,
            enrich: bool,
        ) {
            let updates = self.compaction.finish(id, status, facts, enrich);
            self.sent.extend(updates);
        }

        fn heartbeat(&mut self, id: &str, chunk: Option<&str>) {
            let updates = self.compaction.heartbeat(id, chunk);
            self.sent.extend(updates);
        }

        fn reset(&mut self) {
            let updates = self.compaction.reset();
            self.sent.extend(updates);
        }

        fn interrupt(&mut self) {
            let updates = self.compaction.interrupt();
            self.sent.extend(updates);
        }
    }

    fn compaction_update(air_client: bool) -> Harness {
        Harness::new(CompactionPresentation::CompactionUpdate, air_client)
    }

    fn tool_call(air_client: bool) -> Harness {
        Harness::new(CompactionPresentation::ToolCall, air_client)
    }

    fn air_meta(facts: Value) -> Value {
        json!({"jetbrains": {"air": {"version": 1, "contextCompaction": facts}}})
    }

    #[test]
    fn non_air_tool_call_sends_upstream_fields_and_no_air_key() {
        let mut h = tool_call(false);
        h.start("c");
        h.finish(
            "c",
            CompactionStatus::Completed,
            &CompactionFacts {
                trigger: Some("manual".into()),
                pre_tokens: Some(json!(10)),
                ..CompactionFacts::default()
            },
            false,
        );
        assert_eq!(
            h.sent,
            vec![
                json!({
                    "sessionUpdate": "tool_call", "toolCallId": "c",
                    "title": "Compact conversation", "kind": "think",
                    "status": "in_progress",
                    "_meta": {"claudeCode": {"toolName": "compact"}}
                }),
                json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": "c",
                    "status": "completed",
                    "rawOutput": {"trigger": "manual", "preTokens": 10},
                    "_meta": {"claudeCode": {"toolName": "compact"}}
                }),
            ]
        );
    }

    #[test]
    fn compaction_updates_without_meta_and_a_summary_that_differs_from_the_chunks() {
        let mut h = compaction_update(false);
        h.start("c");
        h.heartbeat("c", Some("Part"));
        assert!(h
            .compaction
            .record_summary(Some(&json!("<summary>\nThe whole summary\n</summary>"))));
        h.finish(
            "c",
            CompactionStatus::Completed,
            &CompactionFacts {
                trigger: Some("manual".into()),
                ..CompactionFacts::default()
            },
            false,
        );
        assert_eq!(
            h.sent,
            vec![
                json!({"sessionUpdate": "compaction_update", "compactionId": "c", "status": "in_progress"}),
                json!({"sessionUpdate": "compaction_summary_chunk", "compactionId": "c",
                    "content": {"type": "text", "text": "Part"}}),
                json!({"sessionUpdate": "compaction_update", "compactionId": "c", "status": "completed",
                    "summary": [{"type": "text", "text": "The whole summary"}]}),
            ]
        );
    }

    #[test]
    fn client_supports_compaction_updates_requires_the_v1_object() {
        assert!(client_supports_compaction_updates(Some(
            &json!({"session": {"compaction": {}}})
        )));
        assert!(client_supports_compaction_updates(Some(
            &json!({"session": {"compaction": {"extra": true}}})
        )));
        for caps in [
            json!({"session": {"compaction": null}}),
            json!({"session": null}),
            json!({"session": {}}),
            json!({}),
        ] {
            assert!(!client_supports_compaction_updates(Some(&caps)), "{caps}");
        }
        assert!(!client_supports_compaction_updates(None));
    }

    #[test]
    fn summary_text_keeps_only_the_tagged_body() {
        assert_eq!(
            compaction_summary_text(
                "<analysis>\nThinking about it.\n</analysis>\n\n<summary>\n## Retained\n\nThe user counted.\n</summary>"
            )
            .as_deref(),
            Some("## Retained\n\nThe user counted.")
        );
    }

    #[test]
    fn summary_text_is_not_fooled_by_quoted_tag_names() {
        assert_eq!(
            compaction_summary_text(
                "<analysis>\nThe user asked for an <analysis> block followed by a <summary> block.\n</analysis>\n\n\
                 <summary>\n1. Intent:\n   Produce an `<analysis>` and a `<summary>` block, then </summary> nothing.\n</summary>\n"
            )
            .as_deref(),
            Some("1. Intent:\n   Produce an `<analysis>` and a `<summary>` block, then </summary> nothing.")
        );
    }

    #[test]
    fn summary_text_takes_the_widest_inline_span_and_stays_linear() {
        assert_eq!(
            compaction_summary_text("<summary>Inline </summary> quoted.</summary>").as_deref(),
            Some("Inline </summary> quoted.")
        );
        let unterminated = "<summary> mention ".repeat(5000);
        let started = std::time::Instant::now();
        assert_eq!(
            compaction_summary_text(&unterminated).as_deref(),
            Some(unterminated.trim())
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
    }

    #[test]
    fn summary_text_strips_the_persisted_continuation_framing() {
        assert_eq!(
            compaction_summary_text(PERSISTED_SUMMARY).as_deref(),
            Some("1. Primary Request and Intent:\n   Count upward.\n\n2. Optional Next Step:\n   None.")
        );
    }

    #[test]
    fn summary_text_strips_the_older_tagged_transcript_framing() {
        assert_eq!(
            compaction_summary_text(
                "This session is being continued from a previous conversation that ran out of context. The conversation is summarized below:\n<summary>\nOld style body.\n</summary>\nPlease continue the conversation from where it left it off without asking the user any further questions."
            )
            .as_deref(),
            Some("Old style body.")
        );
    }

    #[test]
    fn summary_text_fails_closed_on_unrecognized_continuation_framing() {
        assert_eq!(
            compaction_summary_text(
                "This session is being continued from a previous conversation that ran out of context. Here is what happened:\n\nThe user counted.\n\nResume directly."
            ),
            None
        );
    }

    #[test]
    fn summary_text_strips_a_leading_analysis_block_without_summary_tags() {
        assert_eq!(
            compaction_summary_text(
                "<analysis>\nPrivate reasoning about <summary> tags.\n</analysis>\n\n1. Primary Request:\n   Count."
            )
            .as_deref(),
            Some("1. Primary Request:\n   Count.")
        );
        assert_eq!(
            compaction_summary_text("<analysis>inline</analysis> Body.").as_deref(),
            Some("Body.")
        );
        assert_eq!(
            compaction_summary_text("Body mentions <analysis> and </analysis> inline.").as_deref(),
            Some("Body mentions <analysis> and </analysis> inline.")
        );
    }

    #[test]
    fn summary_text_does_not_anchor_on_a_summary_mention_inside_inline_analysis() {
        assert_eq!(
            compaction_summary_text(
                "<analysis>Wants a <summary> block.</analysis> <summary>Real body.</summary>"
            )
            .as_deref(),
            Some("Real body.")
        );
    }

    #[test]
    fn summary_text_passes_plain_text_and_drops_empty_summaries() {
        assert_eq!(
            compaction_summary_text("  Just a summary.  ").as_deref(),
            Some("Just a summary.")
        );
        assert_eq!(compaction_summary_text(""), None);
        assert_eq!(compaction_summary_text("<summary>   </summary>"), None);
    }

    #[test]
    fn compact_summary_message_matches_only_flagged_user_messages() {
        assert!(is_compact_summary_message(
            &json!({"type": "user", "isCompactSummary": true})
        ));
        assert!(!is_compact_summary_message(&json!({"type": "user"})));
        assert!(!is_compact_summary_message(
            &json!({"type": "assistant", "isCompactSummary": true})
        ));
        assert!(!is_compact_summary_message(&Value::Null));
    }

    #[test]
    fn opens_in_progress_carries_the_summary_then_enriches_from_the_boundary() {
        let mut h = compaction_update(true);
        h.start("cmp-1");
        assert!(h.compaction.record_summary(Some(&json!(
            "<analysis>x</analysis><summary>\nRetained.\n</summary>"
        ))));
        h.finish(
            "cmp-status",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        h.finish(
            "cmp-boundary",
            CompactionStatus::Completed,
            &CompactionFacts {
                trigger: Some("manual".into()),
                pre_tokens: Some(json!(100)),
                post_tokens: Some(json!(10)),
                duration_ms: Some(json!(5)),
                error: None,
            },
            true,
        );
        // A duplicated terminal status neither re-sends nor clears the metadata.
        h.finish(
            "cmp-status-dup",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        assert_eq!(
            h.sent,
            vec![
                json!({"sessionUpdate": "compaction_update", "compactionId": "cmp-1",
                    "status": "in_progress", "_meta": air_meta(json!({"version": 1}))}),
                json!({"sessionUpdate": "compaction_update", "compactionId": "cmp-1",
                    "status": "completed", "summary": [{"type": "text", "text": "Retained."}],
                    "_meta": air_meta(json!({"version": 1}))}),
                json!({"sessionUpdate": "compaction_update", "compactionId": "cmp-1",
                    "status": "completed",
                    "_meta": air_meta(json!({"version": 1, "trigger": "manual",
                        "preTokens": 100, "postTokens": 10, "durationMs": 5}))}),
            ]
        );
        assert!(h.compaction.has_delivered_output());
    }

    #[test]
    fn holds_a_summary_recorded_before_the_opening_status() {
        let mut h = compaction_update(true);
        assert!(h
            .compaction
            .record_summary(Some(&json!("<summary>Early.</summary>"))));
        assert!(h.sent.is_empty());
        h.start("cmp-1");
        h.finish(
            "cmp-1",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        h.finish(
            "cmp-b",
            CompactionStatus::Completed,
            &CompactionFacts {
                trigger: Some("automatic".into()),
                pre_tokens: Some(json!(1)),
                ..CompactionFacts::default()
            },
            true,
        );
        let summaries: Vec<Option<&Value>> = h.sent.iter().map(|u| u.get("summary")).collect();
        assert_eq!(
            summaries,
            vec![
                None,
                Some(&json!([{"type": "text", "text": "Early."}])),
                None,
            ]
        );
    }

    #[test]
    fn keeps_a_pending_summary_across_a_previous_turns_reset() {
        let mut h = compaction_update(true);
        assert!(h
            .compaction
            .record_summary(Some(&json!("<summary>Next turn's.</summary>"))));
        h.reset();
        h.start("cmp-2");
        h.finish(
            "cmp-2",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        assert_eq!(
            h.sent.last().unwrap()["summary"],
            json!([{"type": "text", "text": "Next turn's."}])
        );
    }

    #[test]
    fn holds_a_summary_that_arrives_after_a_terminal_entity_for_the_next_compaction() {
        let mut h = compaction_update(true);
        h.start("cmp-a");
        h.finish(
            "cmp-a",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        assert!(h
            .compaction
            .record_summary(Some(&json!("<summary>B's summary.</summary>"))));
        assert_eq!(h.sent.len(), 2);
        h.start("cmp-b");
        h.finish(
            "cmp-b",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        assert_eq!(
            h.sent.last().unwrap()["summary"],
            json!([{"type": "text", "text": "B's summary."}])
        );
    }

    #[test]
    fn ignores_a_malformed_or_empty_hook_payload() {
        let mut h = compaction_update(true);
        assert!(!h.compaction.record_summary(None));
        assert!(!h.compaction.record_summary(Some(&Value::Null)));
        assert!(!h.compaction.record_summary(Some(&json!({"text": "x"}))));
        assert!(!h
            .compaction
            .record_summary(Some(&json!("<summary>  </summary>"))));
        assert!(h
            .compaction
            .record_summary(Some(&json!("<summary>Kept.</summary>"))));
    }

    #[test]
    fn materializes_a_terminal_only_compaction_as_one_completed_update() {
        let mut h = compaction_update(true);
        assert!(h
            .compaction
            .record_summary(Some(&json!("<summary>Only terminal.</summary>"))));
        h.finish(
            "cmp-boundary",
            CompactionStatus::Completed,
            &CompactionFacts {
                trigger: Some("automatic".into()),
                pre_tokens: Some(json!(50)),
                ..CompactionFacts::default()
            },
            false,
        );
        assert_eq!(
            h.sent,
            vec![
                json!({"sessionUpdate": "compaction_update", "compactionId": "cmp-boundary",
                "status": "completed", "summary": [{"type": "text", "text": "Only terminal."}],
                "_meta": air_meta(json!({"version": 1, "trigger": "automatic", "preTokens": 50}))})
            ]
        );
    }

    #[test]
    fn seeds_meta_on_a_boundary_first_terminal_without_metadata() {
        let mut h = compaction_update(true);
        h.finish(
            "cmp-boundary",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            true,
        );
        assert_eq!(
            h.sent,
            vec![
                json!({"sessionUpdate": "compaction_update", "compactionId": "cmp-boundary",
                "status": "completed", "_meta": air_meta(json!({"version": 1}))})
            ]
        );
    }

    #[test]
    fn failures_carry_the_error_and_consume_the_duplicated_stdout_once() {
        let mut h = compaction_update(true);
        h.start("cmp-1");
        assert!(h
            .compaction
            .record_summary(Some(&json!("<summary>Never shown.</summary>"))));
        h.finish(
            "cmp-1",
            CompactionStatus::Failed,
            &CompactionFacts {
                error: Some("summary rejected".into()),
                ..CompactionFacts::default()
            },
            false,
        );
        assert_eq!(
            h.sent[1],
            json!({"sessionUpdate": "compaction_update", "compactionId": "cmp-1",
                "status": "failed", "error": "summary rejected",
                "_meta": air_meta(json!({"version": 1}))})
        );
        assert!(h
            .compaction
            .consume_duplicate_error_output("summary rejected\n"));
        assert!(!h
            .compaction
            .consume_duplicate_error_output("summary rejected"));
        // A summary never patches a failed entity.
        assert!(h
            .compaction
            .record_summary(Some(&json!("<summary>Still never.</summary>"))));
        assert_eq!(h.sent.len(), 2);
    }

    #[test]
    fn streams_api_text_as_summary_chunks_only_into_an_announced_entity() {
        let mut h = compaction_update(true);
        h.heartbeat("cmp-stream", None);
        h.heartbeat("cmp-stream", Some("orphan text"));
        assert!(h.sent.is_empty());
        assert!(!h.compaction.has_delivered_output());
        h.start("cmp-1");
        h.heartbeat("cmp-stream", None);
        h.heartbeat("cmp-stream", Some("## Retained\n"));
        h.heartbeat("cmp-stream", Some("The user counted."));
        h.finish(
            "cmp-1",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        h.heartbeat("cmp-stream", Some("ignored after terminal"));
        assert_eq!(
            h.sent,
            vec![
                json!({"sessionUpdate": "compaction_update", "compactionId": "cmp-1",
                    "status": "in_progress", "_meta": air_meta(json!({"version": 1}))}),
                json!({"sessionUpdate": "compaction_summary_chunk", "compactionId": "cmp-1",
                    "content": {"type": "text", "text": "## Retained\n"}}),
                json!({"sessionUpdate": "compaction_summary_chunk", "compactionId": "cmp-1",
                    "content": {"type": "text", "text": "The user counted."}}),
                json!({"sessionUpdate": "compaction_update", "compactionId": "cmp-1",
                    "status": "completed", "_meta": air_meta(json!({"version": 1}))}),
            ]
        );
    }

    #[test]
    fn closes_an_in_progress_entity_as_cancelled_on_reset_and_starts_fresh() {
        let mut h = compaction_update(true);
        h.start("cmp-1");
        h.reset();
        assert_eq!(
            h.sent.last().unwrap(),
            &json!({"sessionUpdate": "compaction_update", "compactionId": "cmp-1",
                "status": "cancelled"})
        );
        assert!(!h.compaction.has_delivered_output());
        h.start("cmp-2");
        h.finish(
            "cmp-2",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        let before = h.sent.len();
        h.reset();
        assert_eq!(h.sent.len(), before);
    }

    #[test]
    fn ignores_interrupted_works_late_frames_and_hooks_until_a_new_turn() {
        let mut h = compaction_update(true);
        h.start("cmp-1");
        assert!(h
            .compaction
            .record_summary(Some(&json!("<summary>Abandoned.</summary>"))));
        h.interrupt();
        h.interrupt();
        h.reset();
        h.start("cmp-1");
        h.start("unseen-late-opening");
        h.heartbeat("late-stream", Some("late chunk"));
        h.finish(
            "late-status",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        h.finish(
            "late-boundary",
            CompactionStatus::Completed,
            &CompactionFacts {
                pre_tokens: Some(json!(100)),
                ..CompactionFacts::default()
            },
            true,
        );
        assert!(!h
            .compaction
            .record_summary(Some(&json!("<summary>Late hook.</summary>"))));
        assert_eq!(h.sent.len(), 2);
        assert_eq!(
            h.sent[1],
            json!({"sessionUpdate": "compaction_update", "compactionId": "cmp-1",
                "status": "cancelled"})
        );
        h.compaction.resume();
        h.start("cmp-2");
        h.finish(
            "cmp-2",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        assert_eq!(
            h.sent.last().unwrap(),
            &json!({"sessionUpdate": "compaction_update", "compactionId": "cmp-2",
                "status": "completed", "_meta": air_meta(json!({"version": 1}))})
        );
    }

    #[test]
    fn discards_pending_summaries_on_interruption_and_accepts_terminal_only_output() {
        let mut h = compaction_update(true);
        assert!(h.compaction.record_summary(Some(&json!(
            "<summary>Pending abandoned summary.</summary>"
        ))));
        h.interrupt();
        assert!(!h
            .compaction
            .record_summary(Some(&json!("<summary>Late abandoned summary.</summary>"))));
        h.finish(
            "abandoned-terminal",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        assert!(h.sent.is_empty());
        h.compaction.resume();
        h.finish(
            "next-terminal",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        assert_eq!(
            h.sent,
            vec![
                json!({"sessionUpdate": "compaction_update", "compactionId": "next-terminal",
                "status": "completed", "_meta": air_meta(json!({"version": 1}))})
            ]
        );
    }

    #[test]
    fn leaves_completed_entities_terminal_when_their_work_is_interrupted() {
        let mut h = compaction_update(true);
        h.start("cmp-1");
        h.finish(
            "cmp-1",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        assert!(h
            .compaction
            .record_summary(Some(&json!("<summary>Pending hook.</summary>"))));
        h.interrupt();
        assert_eq!(h.sent.len(), 2);
        h.compaction.resume();
        h.start("cmp-2");
        assert!(h
            .compaction
            .record_summary(Some(&json!("<summary>Fresh.</summary>"))));
        h.finish(
            "cmp-2",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        assert_eq!(
            h.sent.last().unwrap()["summary"],
            json!([{"type": "text", "text": "Fresh."}])
        );
    }

    #[test]
    fn reset_and_interrupt_are_idempotent_and_never_panic() {
        let mut h = compaction_update(true);
        h.reset();
        h.interrupt();
        h.reset();
        h.interrupt();
        assert!(h.sent.is_empty());
    }

    #[test]
    fn starts_a_new_entity_for_a_compaction_that_follows_a_terminal_one() {
        let mut h = compaction_update(true);
        h.start("cmp-1");
        h.finish(
            "cmp-1",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        h.start("cmp-2");
        assert_eq!(h.sent.last().unwrap()["compactionId"], "cmp-2");
        assert_eq!(h.sent.last().unwrap()["status"], "in_progress");
    }

    #[test]
    fn does_not_repeat_a_streamed_summary_in_the_terminal_update() {
        let mut h = compaction_update(true);
        h.start("cmp-1");
        h.heartbeat("cmp-1", Some("Retained "));
        h.heartbeat("cmp-1", Some("context."));
        assert!(h
            .compaction
            .record_summary(Some(&json!("<summary>\nRetained context.\n</summary>"))));
        h.finish(
            "cmp-1",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        assert_eq!(
            h.states(),
            vec![
                "compaction_update",
                "compaction_summary_chunk",
                "compaction_summary_chunk",
                "compaction_update",
            ]
        );
        assert!(h.sent[3].get("summary").is_none());
    }

    #[test]
    fn sends_the_summary_when_it_differs_from_the_streamed_chunks() {
        let mut h = compaction_update(true);
        h.start("cmp-1");
        h.heartbeat("cmp-1", Some("<analysis>raw</analysis><summary>Retained"));
        assert!(h
            .compaction
            .record_summary(Some(&json!("<summary>\nRetained context.\n</summary>"))));
        h.finish(
            "cmp-1",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        assert_eq!(
            h.sent.last().unwrap()["summary"],
            json!([{"type": "text", "text": "Retained context."}])
        );
    }

    #[test]
    fn legacy_tool_call_never_exposes_the_summary() {
        let mut h = tool_call(true);
        h.start("compact-start");
        h.heartbeat("compact-start", Some("streamed summary text"));
        h.heartbeat("compact-start", Some("more"));
        assert!(!h
            .compaction
            .record_summary(Some(&json!("<summary>Hidden.</summary>"))));
        h.finish(
            "compact-start",
            CompactionStatus::Completed,
            &CompactionFacts::default(),
            false,
        );
        h.reset();
        assert_eq!(
            h.sent,
            vec![
                json!({"sessionUpdate": "tool_call", "toolCallId": "compact-start",
                    "title": "Compact conversation", "kind": "think",
                    "status": "in_progress", "_meta": air_meta(json!({"version": 1}))}),
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "compact-start",
                    "status": "in_progress", "_meta": air_meta(json!({"version": 1}))}),
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "compact-start",
                    "status": "completed", "_meta": air_meta(json!({"version": 1}))}),
            ]
        );
    }

    #[test]
    fn materializes_a_missed_opening_as_a_terminal_tool_call_with_failure_content() {
        let mut h = tool_call(true);
        h.finish(
            "compact-failed",
            CompactionStatus::Failed,
            &CompactionFacts {
                error: Some("summary rejected".into()),
                ..CompactionFacts::default()
            },
            false,
        );
        h.finish(
            "compact-failed",
            CompactionStatus::Failed,
            &CompactionFacts {
                error: Some("summary rejected".into()),
                ..CompactionFacts::default()
            },
            false,
        );
        h.finish(
            "compact-boundary",
            CompactionStatus::Completed,
            &CompactionFacts {
                trigger: Some("manual".into()),
                pre_tokens: Some(json!(3)),
                ..CompactionFacts::default()
            },
            true,
        );
        assert_eq!(
            h.sent,
            vec![
                json!({"sessionUpdate": "tool_call", "toolCallId": "compact-failed",
                    "title": "Compact conversation", "kind": "think", "status": "failed",
                    "content": [{"type": "content", "content": {"type": "text",
                        "text": "Compaction failed: summary rejected"}}],
                    "_meta": air_meta(json!({"version": 1, "error": "summary rejected"}))}),
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "compact-failed",
                    "_meta": air_meta(json!({"version": 1, "trigger": "manual", "preTokens": 3}))}),
            ]
        );
    }
}
