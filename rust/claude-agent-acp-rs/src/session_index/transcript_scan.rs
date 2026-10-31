//! What the session list needs from a transcript beyond the SDK metadata
//! (upstream `session-index/transcript-scan.ts`).
//!
//! One open reads the first and the last [`CHUNK_SIZE`] bytes, the same window
//! the SDK reads. The tail gives the time of the last message and the end of
//! the last turn; the head and the tail give the `cwd` candidates. A last
//! message longer than the tail window leaves no message in it: the tail window
//! then grows, up to [`MAX_TAIL_SIZE`], until it holds one.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use super::archive_title::SdkTitles;
use super::first_prompt::{first_prompt, is_user_prompt, media_prompt, prompt_of};
use super::js_text::{is_js_ws, parse_iso_ms};

pub const CHUNK_SIZE: u64 = 64 * 1024;
/// The largest tail window read to find the last message.
const MAX_TAIL_SIZE: u64 = 4 * 1024 * 1024;
const TAIL_GROWTH: u64 = 4;
const FULL_SEARCH_CACHE_SIZE: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnState {
    Finished,
    Unfinished,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TranscriptFacts {
    /// Time of the last user or assistant message in the tail, epoch ms.
    pub last_message_at: Option<i64>,
    /// Whether the last turn in the tail ended. `None` when the tail shows no turn.
    pub turn_state: Option<TurnState>,
    /// Time the last ended turn ended, epoch ms.
    pub last_turn_ended_at: Option<i64>,
    /// The last turn ended with an API error (an `isApiErrorMessage` assistant
    /// record), not a user interrupt, and nothing came after it.
    pub last_turn_error: bool,
    /// The first `cwd` of the head.
    pub head_cwd: Option<String>,
    /// The last `cwd` of the tail.
    pub tail_cwd: Option<String>,
    /// Time of the last real user prompt in the tail, epoch ms.
    pub last_prompt_at: Option<i64>,
    /// Whether `last_prompt_at` is final.
    pub prompt_searched: bool,
    /// The model of the last assistant message in the tail.
    pub model: Option<String>,
    /// The session this one was forked from.
    pub forked_from: Option<String>,
    /// What the last `worktree-state` record says (see [`agent_worktree_of`]).
    pub agent_worktree: Option<bool>,
    /// Whether the head or the tail holds a user or an assistant message.
    pub has_messages: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeadTail {
    pub head: String,
    /// The last 64 KB from their first whole line on; the whole file when it
    /// fits in the head.
    pub tail: String,
    /// The last 64 KB as the SDK searches them, with the cut line before `tail`;
    /// absent when that is `tail`.
    pub sdk_tail: Option<String>,
}

/// What an earlier scan of the same file found, at `size`.
#[derive(Clone, Debug, Default)]
pub struct PreviousScan {
    pub size: u64,
    pub last_prompt_at: Option<i64>,
    pub prompt_searched: bool,
    pub model: Option<String>,
    pub tail_cwd: Option<String>,
    pub last_turn_ended_at: Option<i64>,
    pub agent_worktree: Option<bool>,
}

impl PreviousScan {
    pub fn of(facts: &TranscriptFacts, size: u64) -> Self {
        Self {
            size,
            last_prompt_at: facts.last_prompt_at,
            prompt_searched: facts.prompt_searched,
            model: facts.model.clone(),
            tail_cwd: facts.tail_cwd.clone(),
            last_turn_ended_at: facts.last_turn_ended_at,
            agent_worktree: facts.agent_worktree,
        }
    }
}

/// By `path\0size\0identity`, the facts of the transcripts whose last
/// [`MAX_TAIL_SIZE`] bytes lack the last message or prompt: a file of the same
/// size is not searched again.
#[derive(Default)]
pub struct FullSearchCache {
    entries: HashMap<String, TranscriptFacts>,
    order: VecDeque<String>,
}

impl FullSearchCache {
    fn get(&self, key: &str) -> Option<&TranscriptFacts> {
        self.entries.get(key)
    }

    fn insert(&mut self, key: String, facts: TranscriptFacts) {
        if self.entries.insert(key.clone(), facts).is_none() {
            self.order.push_back(key);
        }
        if self.entries.len() > FULL_SEARCH_CACHE_SIZE {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn read_at(file: &mut std::fs::File, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = Vec::new();
    file.take(length).read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// The last `window` bytes of a file of `size` bytes, without the first, cut
/// line. The whole file when it fits.
fn read_tail(
    file: &mut std::fs::File,
    size: u64,
    window: u64,
) -> std::io::Result<(String, String)> {
    let length = window.min(size);
    let raw = lossy(&read_at(file, size - length, length)?);
    if length == size {
        return Ok((raw.clone(), raw));
    }
    let tail = match raw.find('\n') {
        Some(newline) => raw[newline + 1..].to_string(),
        None => String::new(),
    };
    Ok((raw, tail))
}

/// The first and the last 64 KB of the first `size` bytes of `path`: what was
/// appended after the file was stat'ed is not read, so the result matches the
/// size it is cached by. The tail is the head for a file that fits in one chunk.
pub fn read_head_tail(path: &Path, size: u64) -> std::io::Result<HeadTail> {
    let mut file = std::fs::File::open(path)?;
    read_head_tail_of(&mut file, size)
}

/// [`read_head_tail`] of a file that is open already.
pub fn read_head_tail_of(file: &mut std::fs::File, size: u64) -> std::io::Result<HeadTail> {
    let head = lossy(&read_at(file, 0, CHUNK_SIZE.min(size))?);
    if size <= CHUNK_SIZE {
        return Ok(HeadTail {
            tail: head.clone(),
            head,
            sdk_tail: None,
        });
    }
    let (raw, tail) = read_tail(file, size, CHUNK_SIZE)?;
    let sdk_tail = (raw != tail).then_some(raw);
    Ok(HeadTail {
        head,
        tail,
        sdk_tail,
    })
}

/// The facts of `path`, from its head and tail. When the transcript has messages
/// but the tail window lacks the last message or the last user prompt (a long
/// answer or tool output follows it), the window grows, reading only the new
/// range each step, until it holds both or reaches [`MAX_TAIL_SIZE`]. The
/// result of a full search is kept per `(path, size)`.
///
/// `previous` is what an earlier scan of the same file found: when the file only
/// grew by less than the tail window, and the tail holds no prompt, the appended
/// bytes hold none either, so the earlier `last_prompt_at` stands.
pub fn scan_transcript_file(
    path: &Path,
    size: u64,
    head_tail: Option<HeadTail>,
    previous: Option<&PreviousScan>,
    identity: Option<u64>,
    cache: &mut FullSearchCache,
) -> std::io::Result<TranscriptFacts> {
    let read = match head_tail {
        Some(read) => read,
        None => read_head_tail(path, size)?,
    };
    let mut facts = scan_transcript(&read, ScanOptions::default());
    // The file only grew by bytes that the tail covers, and the tail holds no
    // prompt: the appended bytes hold none either, so the earlier search stands.
    let grown_within_tail = previous
        .is_some_and(|p| size >= p.size && size.saturating_sub(read.tail.len() as u64) <= p.size);
    if let (true, Some(previous)) = (grown_within_tail, previous) {
        if facts.model.is_none() {
            facts.model = previous.model.clone();
        }
        if facts.tail_cwd.is_none() {
            facts.tail_cwd = previous.tail_cwd.clone();
        }
        if facts.last_turn_ended_at.is_none() {
            facts.last_turn_ended_at = previous.last_turn_ended_at;
        }
        if facts.agent_worktree.is_none() {
            facts.agent_worktree = previous.agent_worktree;
        }
    }
    // No worktree record in the tail (or before it, by an earlier scan): the one
    // `claude --worktree` writes at the start may still be in the head.
    if facts.agent_worktree.is_none() {
        facts.agent_worktree = agent_worktree_of(&read.head);
    }
    if facts.last_prompt_at.is_some() || size <= CHUNK_SIZE {
        facts.prompt_searched = true;
    } else if let (true, Some(previous)) = (grown_within_tail, previous) {
        if previous.last_prompt_at.is_some() || previous.prompt_searched {
            facts.last_prompt_at = previous.last_prompt_at;
            facts.prompt_searched = true;
        }
    }
    let complete =
        |found: &TranscriptFacts| found.last_message_at.is_some() && found.prompt_searched;
    if !facts.has_messages || complete(&facts) || size <= CHUNK_SIZE {
        return Ok(facts);
    }
    let key = format!(
        "{}\0{size}\0{}",
        path.to_string_lossy(),
        identity.map(|i| i.to_string()).unwrap_or_default()
    );
    if let Some(searched) = cache.get(&key) {
        return Ok(searched.clone());
    }
    let mut result = facts.clone();
    let mut file = std::fs::File::open(path)?;
    // The bytes from `start` to the end, grown by reading the new range only.
    let mut start = size.saturating_sub(CHUNK_SIZE);
    let mut bytes = read_at(&mut file, start, size - start)?;
    let mut window = CHUNK_SIZE * TAIL_GROWTH;
    while start > 0 {
        let next_start = size - window.min(MAX_TAIL_SIZE).min(size);
        if next_start >= start {
            break;
        }
        let mut range = read_at(&mut file, next_start, start - next_start)?;
        range.extend_from_slice(&bytes);
        bytes = range;
        start = next_start;
        let mut tail = lossy(&bytes);
        if start > 0 {
            // The first line of the window is cut; drop it.
            tail = match tail.find('\n') {
                Some(newline) => tail[newline + 1..].to_string(),
                None => String::new(),
            };
        }
        // A wider tail ends with the same records: what the narrow one found
        // stays, and the wider one adds what lay before it.
        let wider = scan_transcript(
            &HeadTail {
                head: read.head.clone(),
                tail,
                sdk_tail: None,
            },
            ScanOptions {
                wide: true,
                known_model: facts.model.is_some(),
                known_tail_cwd: facts.tail_cwd.is_some(),
            },
        );
        let last_prompt_at = wider.last_prompt_at.or(facts.last_prompt_at);
        result = TranscriptFacts {
            model: facts.model.clone().or(wider.model.clone()),
            tail_cwd: facts.tail_cwd.clone().or(wider.tail_cwd.clone()),
            // The wider tail holds the narrow one, so a record it has is the
            // latest; else the narrow scan's stands.
            agent_worktree: wider.agent_worktree.or(facts.agent_worktree),
            // A turn end the wider tail lacks may lie before it: the narrow scan
            // inherited it from an earlier scan of this file.
            last_turn_ended_at: wider.last_turn_ended_at.or(facts.last_turn_ended_at),
            has_messages: facts.has_messages,
            last_prompt_at,
            // Searched to the start of the file, or found.
            prompt_searched: facts.prompt_searched || last_prompt_at.is_some() || start == 0,
            ..wider
        };
        if complete(&result) {
            return Ok(result);
        }
        window *= TAIL_GROWTH;
    }
    // The whole window is searched: no prompt in it either.
    result.prompt_searched = true;
    cache.insert(key, result.clone());
    Ok(result)
}

const RELOCATED_MARKER: &str = "\"relocated\"";

/// The cwd of the last `relocated` record of the tail: where the session was
/// moved, whatever cwd its earlier messages carry.
pub fn relocated_cwd(tail: &str) -> Option<String> {
    for line in tail.split('\n').rev() {
        if !line.contains(RELOCATED_MARKER) {
            continue;
        }
        let Some(entry) = parse_line(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) == Some("relocated") {
            if let Some(cwd) = entry.get("relocatedCwd").and_then(Value::as_str) {
                return Some(cwd.to_string());
            }
        }
    }
    None
}

const WORKTREE_STATE_MARKER: &str = "\"worktree-state\"";

/// Whether the last `worktree-state` record of `text` says the session is in a
/// worktree that Claude Code made for it, as AIR's native Claude provider reads
/// it: a `worktreeSession` that is an object without `enteredExisting: true`.
/// The last line that parses as a `worktree-state` record decides; any other
/// `worktreeSession` (`null`, missing, or no object) means false. Only lines
/// that are not JSON are skipped. `None` when `text` holds no such record.
pub fn agent_worktree_of(text: &str) -> Option<bool> {
    if !text.contains(WORKTREE_STATE_MARKER) {
        return None;
    }
    for line in text.split('\n').rev() {
        if !line.contains(WORKTREE_STATE_MARKER) {
            continue;
        }
        let Some(entry) = parse_line(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("worktree-state") {
            continue;
        }
        let Some(session) = entry.get("worktreeSession").filter(|s| s.is_object()) else {
            return Some(false);
        };
        return Some(session.get("enteredExisting") != Some(&Value::Bool(true)));
    }
    None
}

/// The cwd a transcript belongs to, as the SDK reads it: the last `relocated`
/// record of the tail, else the first cwd of the head.
pub fn transcript_project_cwd(head_tail: &HeadTail) -> Option<String> {
    if let Some(relocated) = relocated_cwd(&head_tail.tail) {
        return Some(relocated);
    }
    first_string_field_raw(&head_tail.head, "cwd").and_then(|raw| decode_json_string(&raw))
}

const MESSAGE_MARKERS: [&str; 4] = [
    "\"type\":\"user\"",
    "\"type\":\"assistant\"",
    "\"type\": \"user\"",
    "\"type\": \"assistant\"",
];

/// System records that the CLI writes when a turn ends.
const TURN_END_SYSTEM_SUBTYPES: [&str; 2] = ["stop_hook_summary", "turn_duration"];

const INTERRUPT_PREFIX: &str = "[Request interrupted by user";

fn parse_line(line: &str) -> Option<Value> {
    match serde_json::from_str::<Value>(line) {
        Ok(value) if value.is_object() => Some(value),
        _ => None,
    }
}

fn timestamp_of(entry: &Value) -> Option<i64> {
    parse_iso_ms(entry.get("timestamp")?.as_str()?)
}

fn first_text(entry: &Value) -> Option<&str> {
    match entry.pointer("/message/content")? {
        Value::String(text) => Some(text),
        Value::Array(blocks) => blocks.iter().find_map(|block| {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                block.get("text").and_then(Value::as_str)
            } else {
                None
            }
        }),
        _ => None,
    }
}

/// How a main-chain record bears on the end of the turn: `Finished` for a record
/// that ends a turn (an assistant `end_turn`, an API error, a user interrupt, the
/// CLI's turn-end system records), `Unfinished` for a record of a turn in
/// progress, and `None` for a record that says nothing.
pub fn turn_effect(entry: &Value) -> Option<TurnState> {
    if entry.get("isSidechain") == Some(&Value::Bool(true)) {
        return None;
    }
    match entry.get("type").and_then(Value::as_str)? {
        "assistant" => {
            if entry.get("isApiErrorMessage") == Some(&Value::Bool(true)) {
                return Some(TurnState::Finished);
            }
            let stop = entry
                .pointer("/message/stop_reason")
                .and_then(Value::as_str);
            Some(if stop == Some("end_turn") {
                TurnState::Finished
            } else {
                TurnState::Unfinished
            })
        }
        "user" => {
            if entry.get("isMeta") == Some(&Value::Bool(true)) {
                return None;
            }
            Some(
                if first_text(entry).is_some_and(|t| t.starts_with(INTERRUPT_PREFIX)) {
                    TurnState::Finished
                } else {
                    TurnState::Unfinished
                },
            )
        }
        "system" => entry
            .get("subtype")
            .and_then(Value::as_str)
            .filter(|s| TURN_END_SYSTEM_SUBTYPES.contains(s))
            .map(|_| TurnState::Finished),
        _ => None,
    }
}

fn decode_json_string(raw: &str) -> Option<String> {
    serde_json::from_str::<String>(&format!("\"{raw}\"")).ok()
}

/// The matches of `"key":\s?"((?:[^"\\]|\\.)*)"` in `text`: byte range of the
/// raw body, in order, non-overlapping.
fn string_field_matches(text: &str, key: &str) -> Vec<(usize, usize)> {
    let marker = format!("\"{key}\":");
    let bytes = text.as_bytes();
    let mut matches = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = text.get(from..).and_then(|t| t.find(&marker)) {
        let found = from + rel;
        let mut at = found + marker.len();
        // `\s?`: one optional white space character.
        if let Some(c) = text.get(at..).and_then(|t| t.chars().next()) {
            if is_js_ws(c) {
                at += c.len_utf8();
            }
        }
        let mut matched = None;
        if bytes.get(at) == Some(&b'"') {
            let body = at + 1;
            let mut i = body;
            while i < bytes.len() {
                match bytes[i] {
                    b'"' => {
                        matched = Some((body, i));
                        break;
                    }
                    b'\\' => match text.get(i + 1..).and_then(|t| t.chars().next()) {
                        Some(c) if !matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}') => {
                            i += 1 + c.len_utf8();
                        }
                        _ => break,
                    },
                    _ => i += 1,
                }
            }
        }
        match matched {
            Some((body, end)) => {
                matches.push((body, end));
                from = end + 1;
            }
            None => from = found + 1,
        }
    }
    matches
}

fn first_string_field_raw(text: &str, key: &str) -> Option<String> {
    let (start, end) = *string_field_matches(text, key).first()?;
    Some(text[start..end].to_string())
}

/// Every `"key":"value"` string of `text`, in order, decoded.
fn string_fields(text: &str, key: &str) -> Vec<String> {
    string_field_matches(text, key)
        .into_iter()
        .filter_map(|(start, end)| decode_json_string(&text[start..end]))
        .collect()
}

/// The last value of `key`, or `None` when absent or empty.
pub fn last_field(text: &str, key: &str) -> Option<String> {
    string_fields(text, key).pop().filter(|v| !v.is_empty())
}

fn forked_from_of(head: &str) -> Option<String> {
    let marker = "\"forkedFrom\":";
    let mut from = 0usize;
    while let Some(rel) = head.get(from..).and_then(|t| t.find(marker)) {
        let found = from + rel;
        from = found + 1;
        let mut rest = &head[found + marker.len()..];
        if rest.chars().next().is_some_and(is_js_ws) {
            rest = &rest[rest.chars().next().map_or(0, char::len_utf8)..];
        }
        let Some(rest) = rest.strip_prefix("{\"sessionId\":") else {
            continue;
        };
        let mut rest = rest;
        if rest.chars().next().is_some_and(is_js_ws) {
            rest = &rest[rest.chars().next().map_or(0, char::len_utf8)..];
        }
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        let candidate = rest.get(..36).unwrap_or("");
        if is_uuid(candidate) && rest[36..].starts_with('"') {
            return Some(candidate.to_string());
        }
    }
    None
}

fn is_uuid(value: &str) -> bool {
    crate::session_store::is_session_id(value)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanOptions {
    /// A wider tail (see [`scan_transcript_file`]).
    pub wide: bool,
    /// The narrow scan already found the model.
    pub known_model: bool,
    /// The narrow scan already found a tail cwd.
    pub known_tail_cwd: bool,
}

fn has_type(line: &str, kind: &str) -> bool {
    line.contains(&format!("\"type\":\"{kind}\""))
        || line.contains(&format!("\"type\": \"{kind}\""))
}

/// Whether a line may be a user prompt: a user record that is no tool result,
/// or one that carries an image or a document.
fn may_be_prompt(line: &str) -> bool {
    if !has_type(line, "user") {
        return false;
    }
    !has_type(line, "tool_result") || has_type(line, "image") || has_type(line, "document")
}

/// Whether a line may end a turn (see [`turn_effect`]).
fn may_end_turn(line: &str) -> bool {
    line.contains("\"end_turn\"")
        || line.contains("\"isApiErrorMessage\"")
        || line.contains(INTERRUPT_PREFIX)
        || TURN_END_SYSTEM_SUBTYPES
            .iter()
            .any(|subtype| line.contains(&format!("\"{subtype}\"")))
}

/// The facts of a transcript, from its head and its tail.
pub fn scan_transcript(head_tail: &HeadTail, options: ScanOptions) -> TranscriptFacts {
    let HeadTail { head, tail, .. } = head_tail;
    let mut facts = TranscriptFacts {
        has_messages: MESSAGE_MARKERS
            .iter()
            .any(|marker| head.contains(marker) || tail.contains(marker)),
        ..TranscriptFacts::default()
    };
    if let Some(raw) = first_string_field_raw(head, "cwd") {
        facts.head_cwd = decode_json_string(&raw);
    }
    facts.forked_from = forked_from_of(head);
    // The tail only: scan_transcript_file falls back to the head after it
    // inherits what an earlier scan found before the tail.
    facts.agent_worktree = agent_worktree_of(tail);

    let mut turn_end_found = false;
    let mut message_found = false;
    let mut prompt_found = false;
    // The last turn ended: is its end an API error?
    let mut checking_turn_error = false;
    // A wide tail is searched for what the narrow one lacked: the last message,
    // prompt and turn end, and the model and the cwd only when the narrow tail
    // had none, so a long answer is not parsed line by line.
    let wide = options.wide;
    let mut model_found = wide && options.known_model;
    let search_cwd = !wide || !options.known_tail_cwd;
    let lines: Vec<&str> = tail.split('\n').collect();
    for line in lines.into_iter().rev() {
        if line.is_empty() {
            continue;
        }
        let is_turn_record = line.contains("\"user\"")
            || line.contains("\"assistant\"")
            || line.contains("\"system\"");
        let has_cwd = search_cwd && facts.tail_cwd.is_none() && line.contains("\"cwd\"");
        if !is_turn_record && !has_cwd {
            continue;
        }
        // Everything has been found: stop parsing.
        if turn_end_found
            && message_found
            && prompt_found
            && model_found
            && !checking_turn_error
            && !has_cwd
        {
            break;
        }
        // Once the last message and the turn state are known, only a line that
        // can still tell something is parsed.
        if message_found
            && facts.turn_state.is_some()
            && !has_cwd
            && !(!prompt_found && may_be_prompt(line))
            && !(!model_found && has_type(line, "assistant"))
            && !(!turn_end_found && may_end_turn(line))
            && !(checking_turn_error && is_turn_record)
        {
            continue;
        }
        let Some(entry) = parse_line(line) else {
            continue;
        };
        if has_cwd {
            if let Some(cwd) = entry
                .get("cwd")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
            {
                facts.tail_cwd = Some(cwd.to_string());
            }
        }
        if entry.get("isSidechain") == Some(&Value::Bool(true)) {
            continue;
        }
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if !message_found && (kind == "user" || kind == "assistant") {
            message_found = true;
            facts.last_message_at = timestamp_of(&entry);
        }
        if !prompt_found && kind == "user" && is_user_prompt(&entry) {
            prompt_found = true;
            facts.last_prompt_at = timestamp_of(&entry);
        }
        if !model_found && kind == "assistant" {
            // The CLI marks the messages it makes up (API errors) `<synthetic>`.
            if let Some(model) = entry
                .pointer("/message/model")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty() && *m != "<synthetic>")
            {
                model_found = true;
                facts.model = Some(model.to_string());
            }
        }
        let Some(effect) = turn_effect(&entry) else {
            continue;
        };
        if facts.turn_state.is_none() {
            facts.turn_state = Some(effect);
            checking_turn_error = effect == TurnState::Finished;
        }
        if checking_turn_error {
            // The record that ended the last turn, behind the CLI's turn-end
            // system records: an API error is an error.
            if kind == "assistant" && entry.get("isApiErrorMessage") == Some(&Value::Bool(true)) {
                facts.last_turn_error = true;
                checking_turn_error = false;
            } else if kind != "system" {
                checking_turn_error = false;
            }
        }
        if effect == TurnState::Finished && !turn_end_found {
            turn_end_found = true;
            facts.last_turn_ended_at = timestamp_of(&entry);
        }
    }
    facts
}

/// Whether the first record of the transcript is a sidechain one, which the SDK
/// never lists.
pub fn is_sidechain_transcript(head: &str) -> bool {
    let first = head.split('\n').next().unwrap_or("");
    first.contains("\"isSidechain\":true") || first.contains("\"isSidechain\": true")
}

/// The last `customTitle` of the tail, as the SDK reads it.
pub fn tail_custom_title(head_tail: &HeadTail) -> Option<String> {
    let tail = head_tail.sdk_tail.as_ref().unwrap_or(&head_tail.tail);
    last_field(tail, "customTitle")
}

/// The last non-blank top-level `agentName` of the records of `text`, the name
/// `/rename`, a `rename_session` and AIR write with the custom title.
fn last_agent_name(text: &str) -> Option<String> {
    if !text.contains("\"agentName\"") {
        return None;
    }
    for line in text.split('\n').rev() {
        if !line.contains("\"agentName\"") {
            continue;
        }
        let value = parse_line(line).and_then(|e| e.get("agentName").cloned());
        if let Some(Value::String(name)) = value {
            if !super::js_text::js_trim(&name).is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// The last agent name of a transcript, as AIR's native Claude provider reads
/// it: the top-level `agentName` of any record, from the tail, else from the head.
pub fn transcript_agent_name(head_tail: &HeadTail) -> Option<String> {
    last_agent_name(&head_tail.tail).or_else(|| last_agent_name(&head_tail.head))
}

/// The titles the SDK gives a transcript, from its own head and tail, in the
/// SDK's order: custom title (tail, the sidecar, head), AI title, then
/// [`generated_title`]. The sidecar title counts only when the tail has no
/// custom title.
pub fn sdk_titles(head_tail: &HeadTail, sidecar_title: Option<&str>) -> SdkTitles {
    let tail = head_tail.sdk_tail.as_ref().unwrap_or(&head_tail.tail);
    let head = &head_tail.head;
    let custom_title = last_field(tail, "customTitle")
        .or_else(|| sidecar_title.map(str::to_string))
        .or_else(|| last_field(head, "customTitle"))
        .or_else(|| last_field(tail, "aiTitle"))
        .or_else(|| last_field(head, "aiTitle"));
    let summary = custom_title.clone().or_else(|| generated_title(head, tail));
    SdkTitles {
        custom_title,
        summary,
    }
}

/// The title of a transcript without a custom title, in the SDK's order: AI
/// title, last prompt, summary, first prompt.
pub fn generated_title(head: &str, tail: &str) -> Option<String> {
    last_field(tail, "aiTitle")
        .or_else(|| last_field(head, "aiTitle"))
        .or_else(|| last_field(tail, "lastPrompt"))
        .or_else(|| last_field(tail, "summary"))
        .or_else(|| {
            let prompt = first_prompt(head);
            if !prompt.is_empty() {
                return Some(prompt);
            }
            let media = media_prompt(head);
            (!media.is_empty()).then(|| media.to_string())
        })
}

/// The time of the last `"timestamp"` in the last 64 KB of a file, cut line
/// included, epoch ms: a record longer than that still ends with it.
pub fn last_timestamp(path: &Path, size: u64) -> std::io::Result<Option<i64>> {
    let mut file = std::fs::File::open(path)?;
    let length = size.min(CHUNK_SIZE);
    let text = lossy(&read_at(&mut file, size - length, length)?);
    Ok(string_fields(&text, "timestamp")
        .iter()
        .rev()
        .find_map(|raw| parse_iso_ms(raw)))
}

const CONTINUED_IN_MARKER: &str = "\"type\":\"continued-in\"";

/// The session this transcript was continued in, as the SDK list reads it: the
/// last `continued-in` record of the tail, unless a completed turn follows it
/// (the session was resumed here again).
pub fn continued_in_session_id(tail: &str) -> Option<String> {
    if !tail.contains(CONTINUED_IN_MARKER) {
        return None;
    }
    for line in tail.split('\n').rev() {
        let is_continuation = line.contains(CONTINUED_IN_MARKER);
        let is_message =
            line.contains("\"type\":\"user\"") || line.contains("\"type\":\"assistant\"");
        if !is_continuation && !is_message {
            continue;
        }
        let Some(entry) = parse_line(line) else {
            continue;
        };
        let kind = entry.get("type").and_then(Value::as_str);
        if is_continuation && kind == Some("continued-in") {
            return entry
                .get("continuedInSessionId")
                .and_then(Value::as_str)
                .filter(|id| is_uuid(id))
                .map(str::to_string);
        }
        if kind == Some("assistant") {
            let stop = entry
                .pointer("/message/stop_reason")
                .and_then(Value::as_str);
            if entry.get("isApiErrorMessage") != Some(&Value::Bool(true)) && stop.is_some() {
                return None;
            }
        } else if prompt_of(&entry, &mut String::new()).is_some() {
            return None;
        }
    }
    None
}

const HISTORY_MARKER: &[u8] = b"\"parentUuid\":";
const HISTORY_SCAN_LIMIT: u64 = 16 * 1024 * 1024;
const HISTORY_SCAN_CHUNK: usize = 1024 * 1024;

/// Whether the transcript at `path` holds conversation history (a record with a
/// `parentUuid`), as the SDK checks a continuation's successor.
pub fn has_history(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let Ok(size) = file.metadata().map(|m| m.len()) else {
        return false;
    };
    let mut carry: Vec<u8> = Vec::new();
    let mut offset = 0u64;
    let mut buffer = vec![0u8; HISTORY_SCAN_CHUNK];
    while offset < size.min(HISTORY_SCAN_LIMIT) {
        let Ok(read) = file.read(&mut buffer) else {
            return false;
        };
        if read == 0 {
            return false;
        }
        let mut window = carry.clone();
        window.extend_from_slice(&buffer[..read]);
        if window
            .windows(HISTORY_MARKER.len())
            .any(|w| w == HISTORY_MARKER)
        {
            return true;
        }
        let keep = HISTORY_MARKER.len().min(window.len());
        carry = window[window.len() - keep..].to_vec();
        offset += read as u64;
    }
    // Past the scan limit the SDK assumes history.
    size > HISTORY_SCAN_LIMIT
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(ts: &str, text: &str) -> String {
        json!({"type": "user", "timestamp": ts, "cwd": "/w", "message": {"role": "user", "content": text}}).to_string()
    }

    fn assistant(ts: &str, stop: &str) -> String {
        json!({"type": "assistant", "timestamp": ts, "message": {"role": "assistant", "model": "claude-x", "stop_reason": stop, "content": []}}).to_string()
    }

    fn scan(lines: &[String]) -> TranscriptFacts {
        let text = lines.join("\n") + "\n";
        scan_transcript(
            &HeadTail {
                head: text.clone(),
                tail: text,
                sdk_tail: None,
            },
            ScanOptions::default(),
        )
    }

    fn temp_file(tag: &str, content: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("acp-t2b-ts-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("t.jsonl");
        std::fs::write(&path, content).expect("write");
        path
    }

    /// transcript scan > sees a finished turn at an assistant end_turn
    #[test]
    fn sees_a_finished_turn_at_an_assistant_end_turn() {
        let facts = scan(&[
            user("2026-01-01T00:00:00.000Z", "hi"),
            assistant("2026-01-01T00:00:05.000Z", "end_turn"),
        ]);
        assert_eq!(facts.turn_state, Some(TurnState::Finished));
        assert_eq!(facts.last_turn_ended_at, Some(1_767_225_605_000));
        assert_eq!(facts.last_message_at, Some(1_767_225_605_000));
        assert_eq!(facts.last_prompt_at, Some(1_767_225_600_000));
        assert_eq!(facts.model.as_deref(), Some("claude-x"));
        assert!(!facts.last_turn_error);
    }

    /// transcript scan > sees a turn end at an interrupt, an API error, and the turn-end system records
    #[test]
    fn sees_turn_ends_at_interrupts_errors_and_system_records() {
        let interrupt = scan(&[
            user("2026-01-01T00:00:00.000Z", "hi"),
            user("2026-01-01T00:00:01.000Z", "[Request interrupted by user]"),
        ]);
        assert_eq!(interrupt.turn_state, Some(TurnState::Finished));
        assert!(!interrupt.last_turn_error);
        let api_error = json!({"type": "assistant", "isApiErrorMessage": true, "timestamp": "2026-01-01T00:00:02.000Z", "message": {"model": "<synthetic>", "content": []}}).to_string();
        let failed = scan(&[user("2026-01-01T00:00:00.000Z", "hi"), api_error.clone()]);
        assert_eq!(failed.turn_state, Some(TurnState::Finished));
        assert!(failed.last_turn_error);
        assert_eq!(failed.model, None, "a synthetic model is no model");
        let system = json!({"type": "system", "subtype": "turn_duration", "timestamp": "2026-01-01T00:00:03.000Z"}).to_string();
        // The turn-end system record follows the API error: still an error.
        let behind = scan(&[user("2026-01-01T00:00:00.000Z", "hi"), api_error, system]);
        assert!(behind.last_turn_error);
    }

    /// transcript scan > sees an unfinished turn and keeps the previous turn end
    #[test]
    fn sees_an_unfinished_turn_and_keeps_the_previous_turn_end() {
        let facts = scan(&[
            user("2026-01-01T00:00:00.000Z", "one"),
            assistant("2026-01-01T00:00:01.000Z", "end_turn"),
            user("2026-01-01T00:00:02.000Z", "two"),
            assistant("2026-01-01T00:00:03.000Z", "tool_use"),
        ]);
        assert_eq!(facts.turn_state, Some(TurnState::Unfinished));
        assert_eq!(facts.last_turn_ended_at, Some(1_767_225_601_000));
        assert_eq!(facts.last_prompt_at, Some(1_767_225_602_000));
    }

    /// transcript scan > ignores sidechain and meta records
    #[test]
    fn ignores_sidechain_and_meta_records() {
        let side = json!({"type": "assistant", "isSidechain": true, "timestamp": "2026-01-01T00:00:09.000Z", "message": {"stop_reason": "end_turn"}}).to_string();
        let meta = json!({"type": "user", "isMeta": true, "timestamp": "2026-01-01T00:00:08.000Z", "message": {"content": "meta"}}).to_string();
        let facts = scan(&[
            user("2026-01-01T00:00:00.000Z", "hi"),
            assistant("2026-01-01T00:00:01.000Z", "tool_use"),
            side,
            meta,
        ]);
        assert_eq!(facts.turn_state, Some(TurnState::Unfinished));
        assert_eq!(facts.last_prompt_at, Some(1_767_225_600_000));
    }

    /// transcript scan > reports the head cwd and the last tail cwd, and a stub without messages
    #[test]
    fn reports_cwds_and_a_stub_without_messages() {
        let stub = scan(&[json!({"type": "custom-title", "customTitle": "t"}).to_string()]);
        assert!(!stub.has_messages);
        let head = user("2026-01-01T00:00:00.000Z", "hi");
        let moved = json!({"type": "assistant", "cwd": "/moved", "timestamp": "2026-01-01T00:00:01.000Z", "message": {"stop_reason": "end_turn"}}).to_string();
        let facts = scan(&[head, moved]);
        assert_eq!(facts.head_cwd.as_deref(), Some("/w"));
        assert_eq!(facts.tail_cwd.as_deref(), Some("/moved"));
    }

    /// worktree-state records > reads the last record: an object without enteredExisting marks
    #[test]
    fn the_last_worktree_state_record_decides() {
        let rec = |session: Value| {
            json!({"type": "worktree-state", "worktreeSession": session}).to_string()
        };
        let text = |lines: &[String]| lines.join("\n");
        assert_eq!(
            agent_worktree_of(&text(&[rec(json!({"name": "w"}))])),
            Some(true)
        );
        assert_eq!(
            agent_worktree_of(&text(&[rec(json!({"enteredExisting": true}))])),
            Some(false)
        );
        assert_eq!(
            agent_worktree_of(&text(&[rec(json!({"name": "w"})), rec(Value::Null)])),
            Some(false)
        );
        assert_eq!(agent_worktree_of("{}"), None);
        // A line that is no JSON is skipped; a malformed record still decides.
        assert_eq!(
            agent_worktree_of(&text(&[
                rec(json!({"name": "w"})),
                "\"worktree-state\" not json".to_string()
            ])),
            Some(true)
        );
        assert_eq!(
            agent_worktree_of(&text(&[rec(json!({"name": "w"})), rec(json!("str"))])),
            Some(false)
        );
    }

    fn wt_record(session: Value) -> String {
        json!({"type": "worktree-state", "worktreeSession": session}).to_string()
    }

    /// An assistant message of ~10 KB (`big` in `session-index.test.ts`).
    fn big_assistant(ts: &str) -> String {
        json!({"type": "assistant", "timestamp": ts, "cwd": "/w",
            "message": {"role": "assistant", "model": "claude-x",
                "content": [{"type": "text", "text": "z".repeat(10_000)}]}})
        .to_string()
    }

    /// `session-index.test.ts`: "reads the worktree record of the tail, else of
    /// an earlier scan, else of the head" — the tail record wins, a null record
    /// appended to the tail wins over the head, and the record an earlier scan
    /// found stands when the grown tail lost it (not the head's older record).
    #[test]
    fn reads_the_worktree_record_of_the_tail_then_an_earlier_scan_then_the_head() {
        let messages: Vec<String> = (0..20)
            .map(|i| big_assistant(&format!("2026-07-01T00:00:{i:02}.000Z")))
            .collect();
        // `claude --worktree` writes the record at the start; 200 KB pushes it
        // out of the 64 KB tail.
        let head = format!(
            "{}\n{}\n",
            wt_record(json!({"path": "/wt"})),
            messages.join("\n")
        );
        let path = temp_file("wt-precedence", head.as_bytes());
        let mut cache = FullSearchCache::default();
        let facts = scan_transcript_file(&path, head.len() as u64, None, None, None, &mut cache)
            .expect("scanned");
        assert_eq!(facts.agent_worktree, Some(true), "the head record");

        // A later record in the tail wins over the head.
        let null = format!("{}\n", wt_record(Value::Null));
        let with_null = format!("{head}{null}");
        std::fs::write(&path, &with_null).expect("write");
        let facts =
            scan_transcript_file(&path, with_null.len() as u64, None, None, None, &mut cache)
                .expect("scanned");
        assert_eq!(facts.agent_worktree, Some(false));

        // A file with the null record before the tail and a prompt at its end:
        // the earlier scan found `false`, the grown tail lost it, the head has
        // `true`. The earlier scan's record stands.
        let before = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            wt_record(json!({"path": "/wt"})),
            messages.join("\n"),
            wt_record(Value::Null),
            big_assistant("2026-07-01T00:01:00.000Z"),
            user("2026-07-01T00:01:01.000Z", "hi"),
        );
        let grown = temp_file("wt-grown", before.as_bytes());
        let first = scan_transcript_file(&grown, before.len() as u64, None, None, None, &mut cache)
            .expect("scanned");
        assert_eq!(first.agent_worktree, Some(false));
        let appended: String = (0..6)
            .map(|i| big_assistant(&format!("2026-07-01T00:02:{i:02}.000Z")))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&grown, format!("{before}{appended}")).expect("write");
        let previous = PreviousScan::of(&first, before.len() as u64);
        let second = scan_transcript_file(
            &grown,
            (before.len() + appended.len()) as u64,
            None,
            Some(&previous),
            None,
            &mut cache,
        )
        .expect("scanned");
        assert_eq!(
            second.agent_worktree,
            Some(false),
            "the earlier scan's record stands, not the head's"
        );
    }

    /// `session-index.test.ts`: "takes the worktree record of a wider tail over
    /// the head's" — the null record lies beyond the 64 KB tail and the 64 KB
    /// head but inside the wider tail the scan grows.
    #[test]
    fn takes_the_worktree_record_of_a_wider_tail_over_the_heads() {
        let messages: Vec<String> = (0..10)
            .map(|i| big_assistant(&format!("2026-07-01T00:00:{i:02}.000Z")))
            .collect();
        let later: Vec<String> = (0..10)
            .map(|i| big_assistant(&format!("2026-07-01T00:01:{i:02}.000Z")))
            .collect();
        let content = format!(
            "{}\n{}\n{}\n{}\n",
            wt_record(json!({"path": "/wt"})),
            messages.join("\n"),
            wt_record(Value::Null),
            later.join("\n"),
        );
        let path = temp_file("wt-wider", content.as_bytes());
        let mut cache = FullSearchCache::default();
        let facts = scan_transcript_file(&path, content.len() as u64, None, None, None, &mut cache)
            .expect("scanned");
        assert_eq!(facts.agent_worktree, Some(false));
    }

    /// row facts of the tail and head > knows forkedFrom only from a fork's records
    #[test]
    fn forked_from_comes_from_a_forks_records_only() {
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let fork = json!({"type": "user", "forkedFrom": {"sessionId": id, "messageUuid": "m"}, "message": {"content": "x"}}).to_string();
        assert_eq!(scan(&[fork]).forked_from.as_deref(), Some(id));
        assert_eq!(
            scan(&[user("2026-01-01T00:00:00.000Z", "a")]).forked_from,
            None
        );
    }

    /// continued-in > counts only a real prompt or a finished answer after it as a resume
    #[test]
    fn continued_in_ends_at_a_prompt_or_a_finished_answer() {
        let id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
        let cont = json!({"type": "continued-in", "continuedInSessionId": id}).to_string();
        let lines = |extra: Vec<String>| {
            let mut all = vec![user("2026-01-01T00:00:00.000Z", "hi"), cont.clone()];
            all.extend(extra);
            all.join("\n")
        };
        assert_eq!(continued_in_session_id(&lines(vec![])).as_deref(), Some(id));
        assert_eq!(
            continued_in_session_id(&lines(vec![assistant(
                "2026-01-01T00:00:03.000Z",
                "tool_use"
            )]))
            .as_deref(),
            None,
            "a stop reason (a completed answer) resumes it"
        );
        assert_eq!(
            continued_in_session_id(&lines(vec![user("2026-01-01T00:00:04.000Z", "again")])),
            None
        );
        let bad = json!({"type": "continued-in", "continuedInSessionId": "nope"}).to_string();
        assert_eq!(continued_in_session_id(&bad), None);
    }

    /// transcript scan > reads a small transcript only up to the size it was stat'ed at
    #[test]
    fn reads_only_up_to_the_stat_size() {
        let first = user("2026-01-01T00:00:00.000Z", "hi") + "\n";
        let path = temp_file(
            "size",
            format!("{first}{}\n", user("2026-01-01T00:00:09.000Z", "later")).as_bytes(),
        );
        let read = read_head_tail(&path, first.len() as u64).expect("read");
        assert_eq!(read.head, first);
        assert_eq!(read.tail, first);
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    /// transcript scan > finds the model and cwd before a tail that is one large tool result
    #[test]
    fn grows_the_tail_to_find_the_last_message_and_prompt() {
        let mut text = String::new();
        text.push_str(&user("2026-01-01T00:00:00.000Z", "hi"));
        text.push('\n');
        text.push_str(&assistant("2026-01-01T00:00:01.000Z", "tool_use"));
        text.push('\n');
        // One huge record that no 64 KB window holds whole.
        text.push_str(&json!({"type": "user", "timestamp": "2026-01-01T00:00:02.000Z", "message": {"content": [{"type": "tool_result", "content": "y".repeat(200_000)}]}}).to_string());
        text.push('\n');
        let path = temp_file("grow", text.as_bytes());
        let mut cache = FullSearchCache::default();
        let facts = scan_transcript_file(&path, text.len() as u64, None, None, None, &mut cache)
            .expect("scan");
        assert_eq!(facts.last_prompt_at, Some(1_767_225_600_000));
        assert_eq!(facts.model.as_deref(), Some("claude-x"));
        assert!(facts.prompt_searched);
        assert!(facts.last_message_at.is_some());
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn titles_follow_the_sdk_order() {
        let tail = format!(
            "{}\n{}\n",
            json!({"type": "ai-title", "aiTitle": "ai"}),
            json!({"type": "last-prompt", "lastPrompt": "lp"})
        );
        let head_tail = HeadTail {
            head: user("2026-01-01T00:00:00.000Z", "first"),
            tail,
            sdk_tail: None,
        };
        let titles = sdk_titles(&head_tail, None);
        assert_eq!(titles.custom_title.as_deref(), Some("ai"));
        let with_sidecar = sdk_titles(&head_tail, Some("sidecar"));
        assert_eq!(with_sidecar.custom_title.as_deref(), Some("sidecar"));
        let plain = HeadTail {
            head: user("2026-01-01T00:00:00.000Z", "first"),
            tail: String::new(),
            sdk_tail: None,
        };
        assert_eq!(sdk_titles(&plain, None).summary.as_deref(), Some("first"));
        assert_eq!(sdk_titles(&plain, None).custom_title, None);
    }

    fn lines(records: &[Value]) -> String {
        records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// archive titles > reads the last top-level agent name of the tail, else of the head
    #[test]
    fn the_last_agent_name_is_the_last_non_blank_one() {
        let head = lines(&[json!({"type": "agent-name", "agentName": "Head"})]);
        let tail = lines(&[
            json!({"type": "agent-name", "agentName": "First"}),
            json!({"type": "agent-name", "agentName": "Last"}),
            json!({"type": "agent-name", "agentName": "  "}),
            json!({"type": "user", "toolUseResult": {"agentName": "Nested"}}),
            json!({"type": "custom-title", "customTitle": "Custom"}),
        ]);
        let with = |head: &str, tail: &str| HeadTail {
            head: head.to_string(),
            tail: tail.to_string(),
            sdk_tail: None,
        };
        assert_eq!(
            transcript_agent_name(&with(&head, &tail)).as_deref(),
            Some("Last")
        );
        let only_title = lines(&[json!({"type": "custom-title", "customTitle": "C"})]);
        assert_eq!(
            transcript_agent_name(&with(&head, &only_title)).as_deref(),
            Some("Head")
        );
        assert_eq!(transcript_agent_name(&with("", "")), None);
    }

    /// archive titles > takes the custom title the SDK reports, never an agent name
    #[test]
    fn the_sdk_reports_the_custom_title_never_an_agent_name() {
        let text = lines(&[
            json!({"type": "custom-title", "customTitle": "Custom"}),
            json!({"type": "agent-name", "agentName": "[archived] Agent"}),
        ]);
        let both = HeadTail {
            head: text.clone(),
            tail: text,
            sdk_tail: None,
        };
        let titles = sdk_titles(&both, None);
        assert_eq!(titles.custom_title.as_deref(), Some("Custom"));
        assert_eq!(titles.summary.as_deref(), Some("Custom"));
        // The tail wins over the sidecar, the sidecar over the head.
        let head = lines(&[json!({"type": "custom-title", "customTitle": "Head"})]);
        let tail = lines(&[json!({"type": "custom-title", "customTitle": "Tail"})]);
        let ht = |head: &str, tail: &str| HeadTail {
            head: head.to_string(),
            tail: tail.to_string(),
            sdk_tail: None,
        };
        assert_eq!(
            sdk_titles(&ht(&head, &tail), Some("Sidecar"))
                .custom_title
                .as_deref(),
            Some("Tail")
        );
        assert_eq!(
            sdk_titles(&ht(&head, ""), Some("Sidecar"))
                .custom_title
                .as_deref(),
            Some("Sidecar")
        );
        assert_eq!(
            sdk_titles(&ht(&head, ""), None).custom_title.as_deref(),
            Some("Head")
        );
        // A generated title is no custom title.
        let prompt = lines(
            &[json!({"type": "user", "timestamp": "2026-01-01T00:00:00Z",
            "message": {"role": "user", "content": "[archived] Looks archived"}})],
        );
        let generated = sdk_titles(&ht(&prompt, &prompt), None);
        assert_eq!(generated.custom_title, None);
        assert_eq!(
            generated.summary.as_deref(),
            Some("[archived] Looks archived")
        );
    }

    /// transcript scan > takes a prompt whose text is "tool_result" for a prompt
    #[test]
    fn a_prompt_whose_text_is_tool_result_is_a_prompt() {
        let facts = scan(&[
            user("2026-01-01T00:00:01.000Z", "tool_result"),
            assistant("2026-01-01T00:00:02.000Z", "end_turn"),
        ]);
        assert_eq!(facts.last_prompt_at, Some(1_767_225_601_000));
    }

    /// row facts of the tail and head > takes lastPromptAt from the last real prompt only
    #[test]
    fn last_prompt_at_is_the_last_real_prompt_only() {
        let facts = scan(&[
            user("2026-01-01T00:00:00Z", "Fix it"),
            assistant("2026-01-01T00:00:01Z", "tool_use"),
            json!({"type": "user", "timestamp": "2026-01-01T00:00:02Z",
                "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t"}]}})
            .to_string(),
            user("2026-01-01T00:00:03Z", "<command-name>/status</command-name>"),
            json!({"type": "user", "isMeta": true, "timestamp": "2026-01-01T00:00:04Z",
                "message": {"role": "user", "content": "note"}})
            .to_string(),
            user("2026-01-01T00:00:05Z", "[Request interrupted by user]"),
            assistant("2026-01-01T00:00:06Z", "end_turn"),
        ]);
        assert_eq!(facts.last_prompt_at, Some(1_767_225_600_000));
    }

    /// row facts of the tail and head > takes the model of the last real assistant message
    #[test]
    fn the_model_is_that_of_the_last_real_assistant_message() {
        let facts = scan(&[
            json!({"type": "assistant", "timestamp": "2026-01-01T00:00:01Z",
                "message": {"role": "assistant", "model": "claude-a", "content": [], "stop_reason": "end_turn"}})
            .to_string(),
            json!({"type": "assistant", "isApiErrorMessage": true, "timestamp": "2026-01-01T00:00:02Z",
                "message": {"role": "assistant", "model": "<synthetic>", "content": [], "stop_reason": "end_turn"}})
            .to_string(),
        ]);
        assert_eq!(facts.model.as_deref(), Some("claude-a"));
        assert_eq!(scan(&[user("2026-01-01T00:00:00Z", "x")]).model, None);
    }

    fn media(kind: &str, ts: &str, extra: Value) -> String {
        let mut record = json!({"type": "user", "timestamp": ts, "message": {"role": "user",
            "content": [{"type": kind, "source": {"type": "base64", "media_type": "x", "data": "x"}}]}});
        if let (Some(map), Some(extra)) = (record.as_object_mut(), extra.as_object()) {
            map.extend(extra.clone());
        }
        record.to_string()
    }

    /// lastPromptAt of a media prompt > counts an image or a document as a prompt
    #[test]
    fn an_image_or_a_document_counts_as_a_prompt() {
        for kind in ["image", "document"] {
            let facts = scan(&[
                user("2026-01-01T00:00:00Z", "Fix it"),
                assistant("2026-01-01T00:00:01Z", "end_turn"),
                media(kind, "2026-01-01T00:00:02Z", json!({})),
                assistant("2026-01-01T00:00:03Z", "end_turn"),
            ]);
            assert_eq!(facts.last_prompt_at, Some(1_767_225_602_000), "{kind}");
        }
    }

    /// lastPromptAt of a media prompt > does not count media in a tool result or a meta record
    #[test]
    fn media_in_a_tool_result_or_a_meta_record_is_no_prompt() {
        let facts = scan(&[
            user("2026-01-01T00:00:00Z", "Fix it"),
            json!({"type": "user", "timestamp": "2026-01-01T00:00:02Z", "message": {"role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "t",
                    "content": [{"type": "image", "source": {}}]}]}})
            .to_string(),
            media("image", "2026-01-01T00:00:03Z", json!({"isMeta": true})),
        ]);
        assert_eq!(facts.last_prompt_at, Some(1_767_225_600_000));
    }

    #[test]
    fn history_is_a_record_with_a_parent_uuid() {
        let with = temp_file("hist-a", b"{\"parentUuid\":null,\"type\":\"user\"}\n");
        let without = temp_file("hist-b", b"{\"type\":\"user\"}\n");
        assert!(has_history(&with));
        assert!(!has_history(&without));
        assert!(!has_history(Path::new("/nonexistent/x.jsonl")));
        let _ = std::fs::remove_dir_all(with.parent().expect("dir"));
        let _ = std::fs::remove_dir_all(without.parent().expect("dir"));
    }

    /// titles by the SDK's rule > finds a custom title whose record starts before
    /// the tail window: the SDK searches the raw 64 KB tail, including the cut
    /// line.
    #[test]
    fn t8d_finds_a_custom_title_across_the_tail_boundary() {
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let mut text = String::new();
        // About 70 KB before the title, past the head window.
        for _ in 0..40 {
            text.push_str(&assistant("2026-01-01T00:00:00.000Z", "end_turn"));
            text.push('\n');
            text.push_str(&json!({"type": "progress", "data": "x".repeat(4_000)}).to_string());
            text.push('\n');
        }
        // The title record starts before the last 64 KB; its `customTitle` is
        // inside it.
        let title = json!({
            "type": "custom-title",
            "padding": "p".repeat(10_000),
            "customTitle": "Across the boundary",
            "sessionId": id,
        })
        .to_string();
        text.push_str(&title);
        text.push('\n');
        for _ in 0..20 {
            text.push_str(&assistant("2026-01-01T00:00:00.000Z", "end_turn"));
            text.push('\n');
            text.push_str(&json!({"type": "progress", "data": "y".repeat(3_000)}).to_string());
            text.push('\n');
        }
        let path = temp_file("boundary", text.as_bytes());
        let size = std::fs::metadata(&path).expect("meta").len();
        assert!(size > 128 * 1024, "the transcript spans several windows");
        let head_tail = read_head_tail(&path, size).expect("read");
        assert_eq!(
            sdk_titles(&head_tail, None).custom_title.as_deref(),
            Some("Across the boundary")
        );
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }
}
