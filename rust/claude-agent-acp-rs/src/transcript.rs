//! `session/load` history replay (rust-parity-fixes Phase 3, item 3.5 /
//! Decision 14, P10).
//!
//! Wire truth is the Node adapter at the pinned version
//! (`porting/UPSTREAM_VERSION`, currently 0.94.0): [`session_messages`] ports
//! the SDK's `getSessionMessages` chain-from-leaf walk over the on-disk
//! `projects/<slug>/<sessionId>.jsonl` transcript store, and
//! [`replay_notifications`] maps each returned message into the ACP `user` /
//! `assistant` chunks Node sends before answering a `session/load`
//! (`replaySessionHistory` → `toAcpNotifications(…, {replay:true,
//! registerHooks:false})`, `acp-agent.js:6732-7105`).
//!
//! PC-2 (parity-closeout): the SDK's `Iwe` meta filter, the terminal-node leaf
//! walk (`ed`), the >5 MiB precompact skip with a streaming read and the
//! `isCompletedLocalCommand` marker are ported here. Replay also rebuilds the
//! Task* state and publishes the `plan` update (`toAcpNotifications` with the
//! session's `taskState`), so a status-only TaskUpdate after a load applies.
//!
//! Not ported (PARITY `pending`): the SDK's queue-command re-linking,
//! `wwe` assistant-message regrouping, the `{dir}`-option quirk and
//! native-subagent (AIR) replay. The linear parentUuid chain, compaction
//! re-linking, `isSidechain` filtering, marker stripping and the subagent-text
//! drop that the staged corpus exercises are ported here.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use crate::env_snapshot::EnvSnapshot;
use crate::map::{MapState, MsgRole};
use crate::session_store;

/// Origin kinds whose meta user messages replay must hide
/// (`REPLAY_HIDDEN_META_ORIGIN_KINDS`, `acp-agent.js:925-931`).
const HIDDEN_META_ORIGIN_KINDS: [&str; 5] = [
    "peer",
    "channel",
    "observer",
    "observer-activity",
    "slack-ping",
];

/// The XML-like marker pairs the CLI wraps local slash-command output and
/// injected context in (`LOCAL_COMMAND_MARKERS` + `INJECTED_CONTEXT_MARKERS`,
/// `acp-agent.js:727-748`).
const TRANSCRIPT_MARKERS: [(&str, &str); 7] = [
    ("<command-name>", "</command-name>"),
    ("<command-message>", "</command-message>"),
    ("<command-args>", "</command-args>"),
    ("<local-command-stdout>", "</local-command-stdout>"),
    ("<local-command-stderr>", "</local-command-stderr>"),
    ("<system-reminder>", "</system-reminder>"),
    ("<task-notification>", "</task-notification>"),
];

/// `isReplayHiddenMetaMessage` (`acp-agent.js:942-950`).
pub fn is_hidden_meta_message(message: &Value) -> bool {
    message.get("type").and_then(Value::as_str) == Some("user")
        && message.get("is_meta").and_then(Value::as_bool) == Some(true)
        && message
            .pointer("/origin/kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| HIDDEN_META_ORIGIN_KINDS.contains(&kind))
}

/// `isSyntheticLoginMessage` (`acp-agent.js:909-922`).
fn is_synthetic_login_message(message: &Value) -> bool {
    if message.get("type").and_then(Value::as_str) != Some("assistant") {
        return false;
    }
    let m = message.get("message").unwrap_or(&Value::Null);
    if m.get("model").and_then(Value::as_str) != Some("<synthetic>") {
        return false;
    }
    let Some(blocks) = m.get("content").and_then(Value::as_array) else {
        return false;
    };
    blocks.len() == 1
        && blocks[0].get("type").and_then(Value::as_str) == Some("text")
        && blocks[0]
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|t| t.contains("Please run /login"))
}

/// `stripSubagentTextAndThinking` (`acp-agent.js:585-592`).
fn strip_subagent_text_and_thinking(content: &Value) -> Value {
    let Some(blocks) = content.as_array() else {
        return content.clone();
    };
    Value::Array(
        blocks
            .iter()
            .filter(|item| {
                let kind = item.get("type").and_then(Value::as_str);
                kind != Some("text") && kind != Some("thinking")
            })
            .cloned()
            .collect(),
    )
}

/// The value of the first `<tag>…</tag>` body, or `None`.
fn marker_tag_text(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)?;
    let from = start + open.len();
    text[from..]
        .find(&close)
        .map(|rel| text[from..from + rel].to_string())
}

fn has_marker(text: &str, tag: &str) -> bool {
    marker_tag_text(text, tag).is_some()
}

/// Single-pass marker-tag scanner (`stripMarkerTags`, `acp-agent.js:751-775`).
fn strip_marker_tags(text: &str) -> String {
    let mut dead: HashSet<&'static str> = HashSet::new();
    let mut result = String::new();
    let mut copied_up_to = 0usize;
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == '<' {
            let mut matched: Option<&'static (&str, &str)> = None;
            for marker in TRANSCRIPT_MARKERS.iter() {
                if dead.contains(marker.0) {
                    continue;
                }
                let open: Vec<char> = marker.0.chars().collect();
                if bytes[i..].starts_with(&open[..]) {
                    matched = Some(marker);
                    break;
                }
            }
            if let Some(marker) = matched {
                let close: Vec<char> = marker.1.chars().collect();
                let mut end = None;
                for (k, _) in bytes.iter().enumerate().skip(i + marker.0.chars().count()) {
                    if bytes[k..].starts_with(&close[..]) {
                        end = Some(k);
                        break;
                    }
                }
                if let Some(end) = end {
                    result.push_str(&bytes[copied_up_to..i].iter().collect::<String>());
                    let pos = end + marker.1.chars().count();
                    i = pos;
                    copied_up_to = pos;
                    continue;
                }
                dead.insert(marker.0);
            }
        }
        i += 1;
    }
    result.push_str(&bytes[copied_up_to..].iter().collect::<String>());
    result
}

/// `stripLocalCommandMetadataText` (`acp-agent.js:802-807`): strip markers; a
/// marker-only message rewrites to its (non-hidden) command form, else `None`.
fn strip_local_command_metadata_text(text: &str) -> Option<String> {
    let stripped = strip_marker_tags(text);
    if !stripped.trim().is_empty() {
        return Some(stripped);
    }
    if has_marker(text, "local-command-stdout") || has_marker(text, "local-command-stderr") {
        return None;
    }
    let name = marker_tag_text(text, "command-name")?.trim().to_string();
    if !name.starts_with('/') {
        return None;
    }
    let head = name.split(' ').next().unwrap_or("");
    if [
        "/context",
        "/heapdump",
        "/extra-usage",
        "/compact",
        "/model",
        "/mcp",
        "/status",
        "/usage",
    ]
    .contains(&head)
    {
        return None;
    }
    // A missing `<command-args>` tag is not a reason to drop the command
    // (`commandInvocationFromMarkerOnlyText`, `acp-agent.js:786-795`).
    let args = marker_tag_text(text, "command-args")
        .map(|args| args.trim().to_string())
        .unwrap_or_default();
    Some(if args.is_empty() {
        name
    } else {
        format!("{name} {args}")
    })
}

/// `stripLocalCommandMetadata` (`acp-agent.js:815-841`), returning `None` when
/// nothing meaningful survives.
pub(crate) fn strip_local_command_metadata(content: Value) -> Option<Value> {
    match content {
        Value::String(s) => strip_local_command_metadata_text(&s).map(Value::String),
        Value::Array(blocks) => {
            let mut kept: Vec<Value> = Vec::new();
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text")
                    && block.get("text").is_some_and(Value::is_string)
                {
                    if let Some(stripped) =
                        strip_local_command_metadata_text(block["text"].as_str().unwrap_or(""))
                    {
                        let mut b = block.clone();
                        b["text"] = Value::String(stripped);
                        kept.push(b);
                    }
                } else {
                    kept.push(block.clone());
                }
            }
            if kept.is_empty() {
                None
            } else {
                Some(Value::Array(kept))
            }
        }
        _ => Some(content.clone()),
    }
}

/// `parentToolUseIdOf` (`acp-agent.js:580-584`): only a string field; `null` and
/// a missing field both read as no parent.
fn parent_tool_use_id_of(message: &Value) -> Option<&str> {
    match message.get("parent_tool_use_id") {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

/// `isTaskNotificationRecord` (`acp-agent.js:889-892`).
fn is_task_notification_record(message: &Value) -> bool {
    message.get("type").and_then(Value::as_str) == Some("user")
        && message.pointer("/origin/kind").and_then(Value::as_str) == Some("task-notification")
        && message
            .pointer("/origin/subkind")
            .is_none_or(Value::is_null)
}

/// The marker prefixes the SDK's `p0` classifies (`_Oe`, `sdk.mjs`).
const PROMPT_SOURCE_MARKERS: [(&str, &str); 4] = [
    ("<command-name>", "record"),
    ("<local-command-stdout>", "output"),
    ("<local-command-stderr>", "output"),
    ("<local-command-caveat>", "caveat"),
];

/// The SDK's `p0` prompt source: an explicit `promptSource` wins (and then
/// yields no source), else the last text block's marker prefix.
fn prompt_source(entry: &Value) -> Option<&'static str> {
    if entry.get("promptSource").is_some() {
        return None;
    }
    let content = entry.pointer("/message/content")?;
    let text = match content {
        Value::String(text) => text.as_str(),
        Value::Array(blocks) => blocks
            .iter()
            .rev()
            .find(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|block| block.get("text").and_then(Value::as_str))?,
        _ => return None,
    };
    PROMPT_SOURCE_MARKERS
        .iter()
        .find(|(marker, _)| text.starts_with(marker))
        .map(|(_, source)| *source)
}

/// The indices of the output records of a completed local command (`vwe`,
/// `sdk.mjs`): after a meta caveat and its command record, the following
/// non-meta output records are marked `isCompletedLocalCommand`.
fn completed_local_command_indices(records: &[Record]) -> HashSet<usize> {
    let mut marked = HashSet::new();
    for (index, record) in records.iter().enumerate() {
        if record.entry.get("type").and_then(Value::as_str) != Some("user")
            || record.entry.get("isMeta").and_then(Value::as_bool) != Some(true)
            || prompt_source(&record.entry) != Some("caveat")
        {
            continue;
        }
        let mut seen_record = false;
        for (next_index, next) in records.iter().enumerate().skip(index + 1) {
            let kind = next.entry.get("type").and_then(Value::as_str).unwrap_or("");
            if kind == "assistant" {
                break;
            }
            if kind != "user" || next.entry.get("isMeta").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            match prompt_source(&next.entry) {
                Some("record") if !seen_record => seen_record = true,
                Some("output") if seen_record => {
                    marked.insert(next_index);
                }
                _ => break,
            }
        }
    }
    marked
}

/// A parsed transcript record that participates in the chain.
#[derive(Debug, Clone)]
struct Record {
    uuid: String,
    parent_uuid: Option<String>,
    entry: Value,
}

/// The SDK's `$L` precompact-skip threshold (`sdk.mjs`): a transcript larger
/// than 5 MiB is read only from its last `compact_boundary` record, because the
/// older context is already compacted away.
const LOAD_PRECOMPACT_SKIP_BYTES: u64 = 5 * 1024 * 1024;

/// The queue-operation delivery ledger (`G0`/`V0`/`q0`, `sdk.mjs`): the
/// number of times each queued prompt was delivered, keyed the way
/// `nR`/`kwe` look it up (`delivery:<id>`, `uuid:<id>`, `entry:<uuid>`).
#[derive(Default)]
struct QueueLedger {
    counts: HashMap<String, usize>,
    copies: HashMap<String, String>,
    mixed: HashSet<String>,
    delivery_copies: HashMap<String, Vec<String>>,
    delivery_copy_uuids: HashSet<String>,
}

impl QueueLedger {
    /// `V0`: fold one transcript record into the ledger.
    fn observe(&mut self, entry: &Value) {
        if entry.get("type").and_then(Value::as_str) == Some("queue-operation")
            && entry.get("operation").and_then(Value::as_str) == Some("remove")
            && entry.get("reason").and_then(Value::as_str) == Some("absorbed_mid_turn")
        {
            let key = entry
                .get("deliveryId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(|id| format!("delivery:{id}"))
                .or_else(|| {
                    entry
                        .get("commandUuid")
                        .and_then(Value::as_str)
                        .map(|id| format!("uuid:{id}"))
                });
            if let Some(key) = key {
                *self.counts.entry(key).or_insert(0) += 1;
            }
            return;
        }
        let Some(attachment) = entry
            .get("attachment")
            .filter(|a| a.get("type").and_then(Value::as_str) == Some("queued_command"))
        else {
            return;
        };
        let field = |name: &str| {
            attachment
                .get(name)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        };
        if let Some(delivery) = field("delivery_id") {
            if let Some(uuid) = entry.get("uuid").and_then(Value::as_str) {
                if self.delivery_copy_uuids.insert(uuid.to_string()) {
                    self.delivery_copies
                        .entry(format!("delivery:{delivery}"))
                        .or_default()
                        .push(uuid.to_string());
                }
            }
            return;
        }
        if let Some(source) = field("source_uuid") {
            let serialized = serde_json::to_string(attachment).unwrap_or_default();
            if let Some(existing) = self.copies.get(source) {
                if existing != &serialized {
                    self.mixed.insert(source.to_string());
                }
            } else {
                self.copies.insert(source.to_string(), serialized);
            }
        }
    }

    /// `q0`: the final delivered counts.
    fn finish(&self) -> HashMap<String, usize> {
        let mut counts = self.counts.clone();
        for source in &self.mixed {
            counts.remove(&format!("uuid:{source}"));
        }
        for (key, uuids) in &self.delivery_copies {
            let delivered = counts.get(key).copied().unwrap_or(0);
            let start = uuids.len().saturating_sub(delivered);
            for uuid in &uuids[start..] {
                counts.insert(format!("entry:{uuid}"), 1);
            }
        }
        counts
    }
}

/// Stream the transcript line by line (never `read_to_string`), and — when the
/// file exceeds `cap` — keep only the records from the last `compact_boundary`
/// on (the SDK's precompact skip, `Twe`/`HL`). The boundary record itself is
/// kept; its preserved-message anchors are gone, so the compaction re-linking
/// below skips it. Also returns the queue-delivery ledger of the same bytes.
fn read_records_capped(
    root: &std::path::Path,
    session_id: &str,
    cap: u64,
) -> std::io::Result<(Vec<Record>, HashMap<String, usize>)> {
    let Some(path) = session_store::find_transcripts(root, session_id)
        .into_iter()
        .next()
    else {
        return Ok((Vec::new(), HashMap::new()));
    };
    read_records_from_file(&path, cap)
}

/// [`read_records_capped`] for a transcript file the caller already located
/// (the session's own, or a subagent's `agent-<id>.jsonl`).
fn read_records_from_file(
    path: &std::path::Path,
    cap: u64,
) -> std::io::Result<(Vec<Record>, HashMap<String, usize>)> {
    let mut out = Vec::new();
    let mut ledger = QueueLedger::default();
    let skip_precompact = std::fs::metadata(path)
        .map(|metadata| metadata.len() > cap)
        .unwrap_or(false);
    let file = std::fs::File::open(path)?;
    for line in std::io::BufRead::lines(std::io::BufReader::new(file)) {
        // A malformed byte stream is an unreadable history, not an empty one
        // (the load path reports it; see `session_messages_async`).
        let line = line?;
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if skip_precompact
            && kind == "system"
            && entry.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        {
            out.clear();
            ledger = QueueLedger::default();
        }
        ledger.observe(&entry);
        if !matches!(
            kind,
            "user" | "assistant" | "progress" | "system" | "attachment"
        ) {
            continue;
        }
        let Some(uuid) = entry.get("uuid").and_then(Value::as_str) else {
            continue;
        };
        if uuid.is_empty() {
            continue;
        }
        out.push(Record {
            uuid: uuid.to_string(),
            parent_uuid: entry
                .get("parentUuid")
                .and_then(Value::as_str)
                .map(String::from),
            entry,
        });
    }
    Ok((out, ledger.finish()))
}

/// Apply the compaction re-linking from every `compact_boundary` system record
/// (the rewiring loop of the SDK's `ed`): the preserved subtree is linearized
/// onto the anchor and everything else pointing at the anchor reroutes to the
/// tail.
fn apply_compaction_records(records: &mut HashMap<String, Record>) {
    let bounds: Vec<Record> = records
        .values()
        .filter(|r| {
            r.entry.get("type").and_then(Value::as_str) == Some("system")
                && r.entry.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        })
        .cloned()
        .collect();
    for boundary in bounds {
        if let Some(preserved) = boundary
            .entry
            .pointer("/compactMetadata/preservedMessages")
            .cloned()
        {
            let Some(anchor) = preserved.get("anchorUuid").and_then(Value::as_str) else {
                continue;
            };
            let Some(uuids) = preserved.get("uuids").and_then(Value::as_array) else {
                continue;
            };
            let uuids: Vec<&str> = uuids.iter().filter_map(|u| u.as_str()).collect();
            if uuids.is_empty() || uuids.iter().any(|u| !records.contains_key(*u)) {
                continue;
            }
            let mut parent = anchor.to_string();
            for u in &uuids {
                if let Some(rec) = records.get_mut(*u) {
                    rec.parent_uuid = Some(parent.clone());
                }
                parent = (*u).to_string();
            }
            let first = uuids[0].to_string();
            let tail = uuids[uuids.len() - 1].to_string();
            let others: Vec<Record> = records
                .values()
                .filter(|rec| rec.uuid != first && rec.parent_uuid.as_deref() == Some(anchor))
                .cloned()
                .collect();
            for mut rec in others {
                rec.parent_uuid = Some(tail.clone());
                records.insert(rec.uuid.clone(), rec);
            }
        } else if let Some(segment) = boundary
            .entry
            .pointer("/compactMetadata/preservedSegment")
            .cloned()
        {
            let (Some(head), Some(anchor), Some(tail)) = (
                segment.get("headUuid").and_then(Value::as_str),
                segment.get("anchorUuid").and_then(Value::as_str),
                segment.get("tailUuid").and_then(Value::as_str),
            ) else {
                continue;
            };
            if let Some(rec) = records.get_mut(head) {
                rec.parent_uuid = Some(anchor.to_string());
            }
            let others: Vec<Record> = records
                .values()
                .filter(|rec| rec.uuid != head && rec.parent_uuid.as_deref() == Some(anchor))
                .cloned()
                .collect();
            for mut rec in others {
                rec.parent_uuid = Some(tail.to_string());
                records.insert(rec.uuid.clone(), rec);
            }
        }
    }
}

/// The SDK's terminal-node predicate (`a`, `ed`): a record that can end a
/// branch — not a sidechain, not a team record, not progress, and not a
/// fork-briefing attachment.
fn is_terminal_candidate(record: &Record) -> bool {
    let entry = &record.entry;
    if entry.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    if entry.get("teamName").is_some() {
        return false;
    }
    let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
    if kind == "progress" {
        return false;
    }
    !(kind == "attachment"
        && entry.pointer("/attachment/type").and_then(Value::as_str) == Some("fork_briefing"))
}

/// The SDK's leaf selection (`ed`): walk from each branch tip (latest first)
/// up to the nearest user/assistant, not from the latest user/assistant in
/// file order. A tip that is a system/attachment record can therefore select
/// an earlier user/assistant than the newest record does.
fn select_leaf(records: &HashMap<String, Record>, order: &HashMap<&str, usize>) -> Option<Record> {
    // `l`: every parent of a terminal candidate; those records are not tips.
    let mut parents: HashSet<&str> = HashSet::new();
    for record in records.values() {
        if is_terminal_candidate(record) {
            if let Some(parent) = record.parent_uuid.as_deref() {
                parents.insert(parent);
            }
        }
    }
    let mut tips: Vec<&Record> = records
        .values()
        .filter(|record| is_terminal_candidate(record) && !parents.contains(record.uuid.as_str()))
        .collect();
    tips.sort_by_key(|record| {
        std::cmp::Reverse(order.get(record.uuid.as_str()).copied().unwrap_or(0))
    });

    let mut visited: HashSet<String> = HashSet::new();
    let mut leaf: Option<Record> = None;
    for tip in tips {
        let mut walked: Vec<String> = Vec::new();
        let mut cursor = Some(tip);
        let mut found: Option<&Record> = None;
        while let Some(record) = cursor {
            if visited.contains(&record.uuid) {
                break;
            }
            let kind = record
                .entry
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("");
            if kind == "user" || kind == "assistant" {
                found = Some(record);
                break;
            }
            walked.push(record.uuid.clone());
            cursor = record
                .parent_uuid
                .as_deref()
                .and_then(|parent| records.get(parent));
        }
        if let Some(record) = found {
            leaf = Some(record.clone());
            break;
        }
        for uuid in walked {
            visited.insert(uuid);
        }
    }
    if leaf.is_some() {
        return leaf;
    }

    // Fallback (`ed`): the records no record points at, walked up to the
    // nearest user/assistant. The latest non-sidechain, non-teamName, non-meta
    // one wins; else the latest at all.
    let mut all_parents: HashSet<&str> = HashSet::new();
    for record in records.values() {
        if let Some(parent) = record.parent_uuid.as_deref() {
            all_parents.insert(parent);
        }
    }
    let mut found: Vec<&Record> = Vec::new();
    for record in records.values() {
        if all_parents.contains(record.uuid.as_str()) {
            continue;
        }
        let mut seen: HashSet<&str> = HashSet::new();
        let mut cursor = Some(record);
        while let Some(current) = cursor {
            if !seen.insert(current.uuid.as_str()) {
                break;
            }
            let kind = current
                .entry
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("");
            if kind == "user" || kind == "assistant" {
                found.push(current);
                break;
            }
            cursor = current
                .parent_uuid
                .as_deref()
                .and_then(|parent| records.get(parent));
        }
    }
    if found.is_empty() {
        return None;
    }
    let latest = |items: &[&Record]| {
        items
            .iter()
            .copied()
            .max_by_key(|record| order.get(record.uuid.as_str()).copied().unwrap_or(0))
            .cloned()
    };
    let preferred: Vec<&Record> = found
        .iter()
        .copied()
        .filter(|record| {
            record.entry.get("isSidechain").and_then(Value::as_bool) != Some(true)
                && record.entry.get("teamName").is_none()
                && record.entry.get("isMeta").and_then(Value::as_bool) != Some(true)
        })
        .collect();
    if preferred.is_empty() {
        latest(&found)
    } else {
        latest(&preferred)
    }
}

/// Port of the SDK `getSessionMessages` core: build the ordered messages
/// (oldest first) for `session_id`'s transcript, dropping sidechain / teamName
/// records and the `Iwe`-filtered meta records, and mapping each to the SDK
/// output shape with `parent_tool_use_id`/`parent_agent_id` always `null`.
pub fn session_messages(env: &EnvSnapshot, session_id: &str) -> Vec<Value> {
    session_messages_capped(env, session_id, LOAD_PRECOMPACT_SKIP_BYTES)
}

/// The full imported transcript (`importSessionToStore`): every persisted
/// user/assistant/attachment/system/progress record, inactive branches and
/// sidechains included, in file order. Fork resolution reads it because the
/// active `getSessionMessages` chain cannot reference an inactive branch
/// (`loadFullSessionHistory`, `fork-session.ts:72-87`).
pub fn full_history(env: &EnvSnapshot, session_id: &str) -> Vec<Value> {
    if !session_store::is_session_id(session_id) {
        return Vec::new();
    }
    let root = session_store::projects_root_with_env(env);
    let Some(path) = session_store::find_transcripts(&root, session_id)
        .into_iter()
        .next()
    else {
        return Vec::new();
    };
    let Ok(file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in std::io::BufRead::lines(std::io::BufReader::new(file)) {
        let Ok(line) = line else {
            continue;
        };
        let Ok(entry) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(
            kind,
            "user" | "assistant" | "progress" | "system" | "attachment"
        ) {
            continue;
        }
        if entry.get("uuid").and_then(Value::as_str).is_none() {
            continue;
        }
        out.push(entry);
    }
    out
}

/// Async wrapper so a `session/load` replay never blocks the runtime on the
/// transcript read. The error travels: a load whose history cannot be read
/// rejects instead of replaying an empty one (M-132).
pub async fn session_messages_async(
    env: &EnvSnapshot,
    session_id: &str,
) -> Result<Vec<Value>, String> {
    let env = env.clone();
    let id = session_id.to_string();
    let session_id = session_id.to_string();
    match tokio::task::spawn_blocking(move || session_messages_checked(&env, &id)).await {
        Ok(result) => result,
        Err(error) => Err(format!(
            "failed to read the history of session {session_id}: {error}"
        )),
    }
}

/// The SDK's `getSubagentMessages` core for a subagent's own transcript file
/// (`agent-<id>.jsonl`): the active chain from its leaf (the same `select_leaf`
/// / compaction re-linking the session reader uses), keeping only the
/// `user`/`assistant` records, in the SDK output shape. A rewound branch stays
/// out, as it does for the session's history.
#[must_use]
pub fn messages_of_transcript(path: &std::path::Path, session_id: &str) -> Vec<Value> {
    let (raw, _delivered) =
        read_records_from_file(path, LOAD_PRECOMPACT_SKIP_BYTES).unwrap_or_default();
    let mut records: HashMap<String, Record> =
        raw.iter().map(|r| (r.uuid.clone(), r.clone())).collect();
    let order: HashMap<&str, usize> = raw
        .iter()
        .enumerate()
        .map(|(i, r)| (r.uuid.as_str(), i))
        .collect();
    apply_compaction_records(&mut records);
    let mut chain: Vec<Record> = Vec::new();
    if let Some(selected) = select_leaf(&records, &order) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut cursor = Some(selected);
        while let Some(rec) = cursor {
            if !seen.insert(rec.uuid.clone()) {
                break;
            }
            cursor = rec
                .parent_uuid
                .as_deref()
                .and_then(|p| records.get(p))
                .cloned();
            chain.push(rec);
        }
        chain.reverse();
    }
    let mut out = Vec::new();
    for record in chain {
        let kind = record
            .entry
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("");
        if kind != "user" && kind != "assistant" {
            continue;
        }
        let mut message = to_sdk_message(&record.entry, None, None);
        if let Some(object) = message.as_object_mut() {
            object.insert("session_id".into(), json!(session_id));
        }
        out.push(message);
    }
    out
}

fn session_messages_capped(env: &EnvSnapshot, session_id: &str, cap: u64) -> Vec<Value> {
    session_messages_capped_result(env, session_id, cap).unwrap_or_default()
}

/// The history read that keeps its I/O error: a `session/load` replay fails when
/// the transcript cannot be read instead of replaying nothing (M-132,
/// `getSessionMessages` rejecting).
pub fn session_messages_checked(env: &EnvSnapshot, session_id: &str) -> Result<Vec<Value>, String> {
    session_messages_capped_result(env, session_id, LOAD_PRECOMPACT_SKIP_BYTES)
        .map_err(|error| format!("failed to read the history of session {session_id}: {error}"))
}

fn session_messages_capped_result(
    env: &EnvSnapshot,
    session_id: &str,
    cap: u64,
) -> std::io::Result<Vec<Value>> {
    if !session_store::is_session_id(session_id) {
        return Ok(Vec::new());
    }
    let root = session_store::projects_root_with_env(env);
    let (raw, delivered) = read_records_capped(&root, session_id, cap)?;
    let mut records: HashMap<String, Record> =
        raw.iter().map(|r| (r.uuid.clone(), r.clone())).collect();
    let order: HashMap<&str, usize> = raw
        .iter()
        .enumerate()
        .map(|(i, r)| (r.uuid.as_str(), i))
        .collect();
    apply_compaction_records(&mut records);

    // Walk from the SDK's leaf up the parentUuid chain to the root, then
    // reverse (oldest first).
    let mut chain: Vec<Record> = Vec::new();
    let mut leaf: Option<Record> = None;
    if let Some(selected) = select_leaf(&records, &order) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut cursor = Some(selected.clone());
        while let Some(rec) = cursor {
            if !seen.insert(rec.uuid.clone()) {
                break;
            }
            chain.push(rec.clone());
            cursor = rec
                .parent_uuid
                .as_deref()
                .and_then(|p| records.get(p))
                .cloned();
        }
        chain.reverse();
        leaf = Some(selected);
    }

    // `Dwe`: queued-command attachments on branches that branch off the leaf
    // chain but are not ancestors of another turn (`branch-turn walk`).
    let trailing = leaf
        .as_ref()
        .map(|leaf| trailing_queued_commands(&records, &chain, &leaf.uuid))
        .unwrap_or_default();
    let mut combined = chain;
    let trailing_ids: HashSet<String> = trailing.iter().map(|r| r.uuid.clone()).collect();
    combined.extend(trailing);

    let completed_local_commands = completed_local_command_indices(&combined);
    let reply = reply_flags(&combined, &completed_local_commands);
    let queued_delivered = delivered_flags(&combined, &delivered);
    let mut existing: HashSet<String> = combined.iter().map(|r| r.uuid.clone()).collect();
    let mut out = Vec::new();
    for (index, record) in combined.iter().enumerate() {
        let entry = &record.entry;
        // `nR`: a completed local command is marked; a delivered (or trailing,
        // or reply) queued command becomes a user prompt.
        let mapped = if completed_local_commands.contains(&index) {
            let mut value = entry.clone();
            if let Some(map) = value.as_object_mut() {
                map.insert("isCompletedLocalCommand".into(), json!(true));
            }
            value
        } else {
            let convert = queued_delivered.get(&index).copied().unwrap_or(false)
                || reply[index]
                || trailing_ids.contains(&record.uuid);
            queued_command_to_user(entry, convert, &mut existing)
        };
        if !replay_visible(&mapped) {
            continue;
        }
        out.push(to_sdk_message(&mapped, None, None));
    }
    Ok(out)
}

/// `isQueuedCommandAttachment` (`Lwe`): an attachment of type `queued_command`.
fn is_queued_command_attachment(record: &Record) -> bool {
    record.entry.get("type").and_then(Value::as_str) == Some("attachment")
        && record
            .entry
            .pointer("/attachment/type")
            .and_then(Value::as_str)
            == Some("queued_command")
}

/// `Dwe`: collect queued-command attachments on off-chain branches that are
/// not ancestors of a turn (`branch-turn walk`, `sdk.mjs`).
fn trailing_queued_commands(
    records: &HashMap<String, Record>,
    chain: &[Record],
    leaf_uuid: &str,
) -> Vec<Record> {
    // `r`: non-user/assistant records by parent uuid.
    let mut children: HashMap<String, Vec<&Record>> = HashMap::new();
    for record in records.values() {
        let kind = record
            .entry
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(parent) = record.parent_uuid.as_deref() {
            if kind != "user" && kind != "assistant" {
                children.entry(parent.to_string()).or_default().push(record);
            }
        }
    }
    let by_uuid: HashMap<&str, &Record> = records.values().map(|r| (r.uuid.as_str(), r)).collect();
    let mut seen: HashSet<String> = chain.iter().map(|r| r.uuid.clone()).collect();
    // `a`: ancestors of a user/assistant record outside the chain.
    let mut turn_ancestors: HashSet<String> = HashSet::new();
    for record in records.values() {
        let kind = record
            .entry
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("");
        if (kind == "user" || kind == "assistant")
            && record.entry.get("isSidechain").and_then(Value::as_bool) != Some(true)
            && record.entry.get("teamName").is_none()
            && !seen.contains(&record.uuid)
        {
            let mut cursor = record
                .parent_uuid
                .as_deref()
                .and_then(|parent| by_uuid.get(parent).copied());
            while let Some(current) = cursor {
                if seen.contains(&current.uuid) || !turn_ancestors.insert(current.uuid.clone()) {
                    break;
                }
                cursor = current
                    .parent_uuid
                    .as_deref()
                    .and_then(|parent| by_uuid.get(parent).copied());
            }
        }
    }
    let timestamp = |record: &&Record| {
        record
            .entry
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let mut stack: Vec<&Record> = vec![by_uuid.get(leaf_uuid).copied().unwrap_or(&chain[0])];
    let mut trailing = Vec::new();
    while let Some(current) = stack.pop() {
        if current.uuid != leaf_uuid {
            if seen.contains(&current.uuid) {
                continue;
            }
            seen.insert(current.uuid.clone());
            if is_queued_command_attachment(current) && !turn_ancestors.contains(&current.uuid) {
                trailing.push(current.clone());
            }
        }
        let mut kids: Vec<&Record> = children.get(&current.uuid).cloned().unwrap_or_default();
        if kids.len() > 1 {
            kids.sort_by_key(|a| timestamp(a));
        }
        for child in kids.into_iter().rev() {
            if !seen.contains(&child.uuid) {
                stack.push(child);
            }
        }
    }
    trailing
}

/// `xwe`: whether each record is a reply (true) or a prompt (false), scanning
/// backwards.
fn reply_flags(records: &[Record], completed: &HashSet<usize>) -> Vec<bool> {
    let mut flags = vec![false; records.len()];
    let mut state: Option<&str> = None;
    for index in (0..records.len()).rev() {
        flags[index] = state == Some("reply");
        let entry = &records[index].entry;
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "assistant" || is_tool_result_message(entry) || is_stream_closed_message(entry) {
            state = Some("reply");
        } else if kind == "user"
            && entry.get("isMeta").and_then(Value::as_bool) != Some(true)
            && entry.get("isCompactSummary").and_then(Value::as_bool) != Some(true)
            && !completed.contains(&index)
        {
            state = Some("prompt");
        }
    }
    flags
}

/// `Z0`: a user record whose content has a `tool_result` block.
fn is_tool_result_message(entry: &Value) -> bool {
    if entry.get("type").and_then(Value::as_str) != Some("user")
        || !entry
            .get("parentUuid")
            .and_then(Value::as_str)
            .is_some_and(|parent| !parent.is_empty())
    {
        return false;
    }
    entry
        .pointer("/message/content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
}

/// The SDK's interruption prefixes (`KL`): a user message that starts with one
/// is a reply, not a prompt.
const STREAM_CLOSED_PREFIXES: [&str; 4] = [
    "[Request interrupted by user]",
    "[Request interrupted by user for tool use]",
    "[Tool call did not complete: the turn was ended to deliver the message that follows. Nothing refused it; re-run it if still needed.]",
    "[Tool call interrupted: the session ended before this call's result was recorded, so its outcome is unknown. Check whether it took effect before relying on it or running it again.",
];

/// `GL`: a user message whose text (or every text/tool-result-error block)
/// starts with an interruption prefix.
fn is_stream_closed_message(entry: &Value) -> bool {
    if entry.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(content) = entry.pointer("/message/content") else {
        return false;
    };
    let starts = |text: &str| STREAM_CLOSED_PREFIXES.iter().any(|p| text.starts_with(p));
    match content {
        Value::String(text) => starts(text),
        Value::Array(blocks) => {
            !blocks.is_empty()
                && blocks.iter().all(|block| {
                    let text = match block.get("type").and_then(Value::as_str) {
                        Some("text") => block.get("text").and_then(Value::as_str),
                        Some("tool_result")
                            if block.get("is_error").and_then(Value::as_bool) == Some(true) =>
                        {
                            block.get("content").and_then(Value::as_str)
                        }
                        _ => None,
                    };
                    text.is_some_and(starts)
                })
        }
        _ => false,
    }
}

/// The `kwe` delivered flags: which queued-command records were delivered.
fn delivered_flags(records: &[Record], delivered: &HashMap<String, usize>) -> HashMap<usize, bool> {
    let mut flags = HashMap::new();
    if delivered.is_empty() {
        return flags;
    }
    let key_of = |entry: &Value| {
        entry
            .pointer("/attachment/delivery_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(|value| format!("delivery:{value}"))
            .or_else(|| {
                entry
                    .pointer("/attachment/source_uuid")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(|value| format!("uuid:{value}"))
            })
    };
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        if let Some(key) = key_of(&record.entry) {
            if delivered.contains_key(&key) {
                groups.entry(key).or_default().push(index);
            }
        }
    }
    for (key, indices) in groups {
        if key.starts_with("delivery:") {
            for index in indices {
                flags.insert(
                    index,
                    delivered.contains_key(&format!("entry:{}", records[index].uuid)),
                );
            }
        } else {
            let count = delivered.get(&key).copied().unwrap_or(0);
            let len = indices.len();
            for (position, index) in indices.iter().enumerate() {
                flags.insert(*index, position >= len.saturating_sub(count));
            }
        }
    }
    flags
}

/// `ZA`/`hOe` with `udsInboxEnabled: true`: the meta origins the SDK keeps.
fn visible_origin(origin: Option<&Value>) -> bool {
    let Some(kind) = origin.and_then(|o| o.get("kind")).and_then(Value::as_str) else {
        return false;
    };
    matches!(
        kind,
        "channel" | "observer" | "observer-activity" | "slack-ping" | "peer"
    )
}

/// `mOe(e) !== undefined`: a queued command with a valid `forwardedIntent`
/// stays an attachment (it is a forwarded prompt, not a queued one).
fn has_forwarded_intent(attachment: &Value) -> bool {
    let Some(intent) = attachment.get("forwardedIntent") else {
        return false;
    };
    let Some(object) = intent.as_object() else {
        return false;
    };
    let lineage = object
        .get("lineage")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    let source = object
        .get("source")
        .and_then(Value::as_str)
        .is_some_and(|value| matches!(value, "typed" | "relay" | "owner"));
    lineage && source
}

/// `WE`: normalize a `task-notification` origin (a runId that is a safe token
/// survives; other keys pass through).
fn normalize_origin(origin: Option<&Value>) -> Option<Value> {
    let origin = origin?;
    if origin.get("kind").and_then(Value::as_str) != Some("task-notification") {
        return Some(origin.clone());
    }
    let mut out = serde_json::Map::new();
    out.insert("kind".into(), json!("task-notification"));
    for key in ["subkind", "fireReason", "producer"] {
        if let Some(value) = origin.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    if let Some(run_id) = origin.get("runId").and_then(Value::as_str).filter(|value| {
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
    }) {
        out.insert("runId".into(), json!(run_id));
    }
    if origin.get("taskKind").and_then(Value::as_str) == Some("mcp-task") {
        out.insert("taskKind".into(), json!("mcp-task"));
    }
    Some(Value::Object(out))
}

/// `fL(origin, commandMode, runId)` for a queued command.
fn queued_origin(attachment: &Value) -> Option<Value> {
    let mut origin = normalize_origin(attachment.get("origin")).or_else(|| {
        (attachment.get("commandMode").and_then(Value::as_str) == Some("task-notification"))
            .then(|| json!({"kind": "task-notification"}))
    });
    let run_id = attachment
        .get("runId")
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 128
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
        });
    if let (Some(origin), Some(run_id)) = (origin.as_mut(), run_id) {
        if origin.get("kind").and_then(Value::as_str) == Some("task-notification")
            && origin.get("runId").is_none()
        {
            if let Some(map) = origin.as_object_mut() {
                map.insert("runId".into(), json!(run_id));
            }
        }
    }
    origin
}

/// `Pwe`: rewrite a queued-command attachment into the user prompt the SDK
/// returns for it, when `convert` is set.
fn queued_command_to_user(entry: &Value, convert: bool, existing: &mut HashSet<String>) -> Value {
    if !convert || entry.get("type").and_then(Value::as_str) != Some("attachment") {
        return entry.clone();
    }
    let Some(attachment) = entry.get("attachment").and_then(Value::as_object) else {
        return entry.clone();
    };
    if attachment.get("type").and_then(Value::as_str) != Some("queued_command") {
        return entry.clone();
    }
    let prompt = attachment.get("prompt");
    if !prompt.is_some_and(|p| p.is_string() || p.is_array()) {
        return entry.clone();
    }
    let origin = queued_origin(entry.get("attachment").unwrap_or(&Value::Null));
    if attachment.get("isMeta").is_some_and(js_truthy) && !visible_origin(origin.as_ref()) {
        return entry.clone();
    }
    if has_forwarded_intent(entry.get("attachment").unwrap_or(&Value::Null)) {
        return entry.clone();
    }
    let uuid = attachment
        .get("source_uuid")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(String::from)
        .or_else(|| entry.get("uuid").and_then(Value::as_str).map(String::from));
    let Some(uuid) = uuid else {
        return entry.clone();
    };
    if entry.get("uuid").and_then(Value::as_str) != Some(uuid.as_str()) && existing.contains(&uuid)
    {
        return entry.clone();
    }
    existing.insert(uuid.clone());
    let mut out = serde_json::Map::new();
    out.insert("type".into(), json!("user"));
    out.insert("uuid".into(), json!(uuid));
    out.insert(
        "parentUuid".into(),
        entry.get("parentUuid").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "sessionId".into(),
        entry.get("sessionId").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "timestamp".into(),
        entry.get("timestamp").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "message".into(),
        json!({"role": "user", "content": prompt.cloned().unwrap_or(Value::Null)}),
    );
    out.insert(
        "isMeta".into(),
        json!(attachment.get("isMeta").is_some_and(js_truthy)),
    );
    if let Some(origin) = origin {
        out.insert("origin".into(), origin);
    }
    out.insert("isQueuedCommand".into(), json!(true));
    out.insert(
        "isSidechain".into(),
        entry.get("isSidechain").cloned().unwrap_or(Value::Null),
    );
    if let Some(team) = entry.get("teamName") {
        out.insert("teamName".into(), team.clone());
    }
    Value::Object(out)
}

/// JS truthiness for a JSON value (`Boolean(value)`).
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `Iwe`: whether the SDK returns this record (`includeSystemMessages` false).
fn replay_visible(entry: &Value) -> bool {
    let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
    if kind != "user" && kind != "assistant" {
        return false;
    }
    if entry.get("isMeta").and_then(Value::as_bool) == Some(true)
        && !visible_origin(entry.get("origin"))
    {
        return false;
    }
    if entry.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    if entry.get("teamName").is_some() {
        return false;
    }
    true
}

/// `rR`: the SDK's output shape for one transcript record.
pub(crate) fn to_sdk_message(
    entry: &Value,
    parent_tool_use_id: Option<&str>,
    parent_agent_id: Option<&str>,
) -> Value {
    let mut out = serde_json::Map::new();
    out.insert(
        "type".into(),
        entry.get("type").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "uuid".into(),
        entry.get("uuid").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "session_id".into(),
        entry.get("sessionId").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "message".into(),
        entry.get("message").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "parent_tool_use_id".into(),
        match parent_tool_use_id {
            Some(id) => json!(id),
            None => Value::Null,
        },
    );
    out.insert(
        "parent_agent_id".into(),
        match parent_agent_id {
            Some(id) => json!(id),
            None => Value::Null,
        },
    );
    if entry.get("interruptedByShutdown").and_then(Value::as_bool) == Some(true) {
        out.insert("interruptedByShutdown".into(), json!(true));
    }
    if entry.get("toolDenialUnanswered").and_then(Value::as_str) == Some("stream-closed") {
        out.insert("toolDenialUnanswered".into(), json!("stream-closed"));
    }
    let compact = entry.get("isCompactSummary").and_then(Value::as_bool) == Some(true);
    if compact {
        out.insert("isCompactSummary".into(), json!(true));
    }
    if entry.get("isMeta").and_then(Value::as_bool) == Some(true)
        || compact
        || entry
            .get("isVisibleInTranscriptOnly")
            .and_then(Value::as_bool)
            == Some(true)
    {
        out.insert("is_meta".into(), json!(true));
    }
    if entry.get("isQueuedCommand").and_then(Value::as_bool) == Some(true) {
        out.insert("isQueuedCommand".into(), json!(true));
    }
    if entry
        .get("isCompletedLocalCommand")
        .and_then(Value::as_bool)
        == Some(true)
    {
        out.insert("isCompletedLocalCommand".into(), json!(true));
    }
    if let Some(timestamp) = entry.get("timestamp") {
        out.insert("timestamp".into(), timestamp.clone());
    }
    if let Some(origin) = normalize_origin(entry.get("origin")) {
        out.insert("origin".into(), origin);
    }
    Value::Object(out)
}

/// Build the ACP `session/update` notification JSON a `session/load` replay
/// sends (oldest first), as `{"sessionId", "update"}` values ready for
/// `session/update`. Mirrors `replaySessionHistory` → `toAcpNotifications` for
/// a plain client: hidden meta, synthetic login, task-notification and empty
/// marker-only user messages are skipped; a nested assistant's subagent
/// text/thinking is stripped; local-command metadata is stripped from user
/// content; chunks are grouped under the API message id.
pub fn replay_notifications(messages: &[Value], session_id: &str) -> Vec<Value> {
    replay_with_task_state(messages, session_id, false).0
}

/// The user-displayable text of a persisted compact summary's content
/// (`compactionSummaryText`), or `None` when it carries none.
fn compact_summary_text_of(content: &Value) -> Option<String> {
    let texts: Vec<&str> = match content {
        Value::String(text) => vec![text.as_str()],
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    };
    crate::context_compaction::compaction_summary_text(&texts.join(""))
}

/// The tool_result content-block kinds (`toAcpNotifications`'s result arm).
fn is_tool_result_block(block: &Value) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some("tool_result")
            | Some("tool_search_tool_result")
            | Some("web_fetch_tool_result")
            | Some("web_search_tool_result")
            | Some("code_execution_tool_result")
            | Some("bash_code_execution_tool_result")
            | Some("text_editor_code_execution_tool_result")
            | Some("mcp_tool_result")
    )
}

/// Replay `content` for one message. A Task* tool result rebuilds the task
/// state and publishes the `plan` update in the block's position (Node
/// `toAcpNotifications` with `options.taskState`); every other block maps as
/// before. `air` reproduces `changedTaskPlanEntries`' dedupe for AIR clients.
/// What a replay knows about its client and the message it is mapping.
#[derive(Clone, Copy)]
struct ReplayFlags {
    /// An AIR client: the plan dedupe applies.
    air: bool,
    /// The client takes plan operations (`clientCapabilities.plan`).
    plan_ops: bool,
    /// The message belongs to the main conversation (no parent tool call).
    top_level: bool,
}

fn replay_content(
    content: &Value,
    role: MsgRole,
    state: &mut MapState,
    task_state: &mut Vec<(String, crate::hooks::TaskStateEntry)>,
    flags: ReplayFlags,
    last_published: &mut Option<String>,
    parent: Option<&str>,
) -> Vec<Value> {
    let air = flags.air;
    let Value::Array(blocks) = content else {
        return crate::map::map_consolidated_with_parent(content, role, state, parent)
            .into_iter()
            .filter_map(|update| serde_json::to_value(update).ok())
            .map(|mut val| {
                crate::map::materialize_tool_call_defaults(&mut val);
                crate::map::materialize_diff_old_text(&mut val);
                val
            })
            .collect();
    };
    let mut out = Vec::new();
    for block in blocks {
        if role == MsgRole::User && is_tool_result_block(block) {
            let id = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let tool_use = state.tool_use_from_cache(id).cloned();
            if let Some(tool_use) = tool_use {
                let name = tool_use.get("name").and_then(Value::as_str).unwrap_or("");
                if crate::tools::is_task_tool(name) {
                    let input = tool_use.get("input").cloned().unwrap_or(Value::Null);
                    // Let the shared mapper do its Task* bookkeeping (drop the
                    // cache entry, finish the tracker); it emits no update.
                    let _ = crate::map::map_consolidated_with_parent(
                        &Value::Array(vec![block.clone()]),
                        role,
                        state,
                        parent,
                    );
                    let is_error = block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    // Node passes no `toolUseResult` on replay: the SDK output
                    // has no message-level result, so only `content` parses.
                    if !is_error
                        && crate::hooks::apply_task_tool_result(
                            task_state,
                            name,
                            &input,
                            &Value::Null,
                            &block["content"],
                        )
                    {
                        let entries = crate::hooks::task_state_to_plan_entries(task_state);
                        let publish = if air {
                            let serialized = serde_json::to_string(&entries).unwrap_or_default();
                            if last_published.as_deref() == Some(&serialized) {
                                false
                            } else {
                                *last_published = Some(serialized);
                                true
                            }
                        } else {
                            true
                        };
                        if publish {
                            out.push(json!({
                                "sessionUpdate": "plan",
                                "entries": entries,
                            }));
                        }
                    }
                    continue;
                }
            }
        }
        // U-24: an approved ExitPlanMode plan of the main conversation becomes
        // the session's markdown plan. Replay has only the tool input: the SDK's
        // `getSessionMessages` drops the structured output the transcript keeps.
        let approved_plan = (flags.plan_ops && flags.top_level && is_tool_result_block(block))
            .then(|| block.get("tool_use_id").and_then(Value::as_str))
            .flatten()
            .and_then(|id| state.tool_use_from_cache(id))
            .filter(|tool_use| tool_use.get("name").and_then(Value::as_str) == Some("ExitPlanMode"))
            .filter(|_| block.get("is_error").and_then(Value::as_bool) != Some(true))
            .and_then(|tool_use| {
                crate::session::exit_plan::approved_plan(
                    tool_use.get("input").unwrap_or(&Value::Null),
                    &Value::Null,
                )
            });
        for update in crate::map::map_consolidated_with_parent(
            &Value::Array(vec![block.clone()]),
            role,
            state,
            parent,
        ) {
            if let Ok(mut value) = serde_json::to_value(update) {
                crate::map::materialize_tool_call_defaults(&mut value);
                crate::map::materialize_diff_old_text(&mut value);
                out.push(value);
            }
        }
        if let Some(plan) = approved_plan {
            out.push(crate::session::exit_plan::plan_mode_update(&plan));
        }
    }
    out
}

/// Replay history and rebuild the Task* state it carries (PC-2.3). Returns the
/// `session/update` values (oldest first), the final task state to seed into
/// the live session, and — for an AIR client — the last published plan JSON
/// so the live dedupe continues where replay left off.
pub fn replay_with_task_state(
    messages: &[Value],
    session_id: &str,
    air: bool,
) -> (
    Vec<Value>,
    Vec<(String, crate::hooks::TaskStateEntry)>,
    Option<String>,
) {
    replay_with_failures(messages, session_id, air, false, None)
}

/// [`replay_with_task_state`] for a client that reads AIR session failures:
/// `failures` is the session's failure book, and every synthetic usage-limit
/// message of the history replays as a typed failure at its transcript
/// position instead of as chat text (N-53 / V-15, `acp-agent.ts:9307-9343`).
/// The record is named after the prompt it answered, so it recreates the live
/// incident identity; only the latest limit not followed by a real model
/// answer stays active in the book.
/// Replay sinks a caller can supply: the failure book, the AIR async-task
/// runtime, and the ACP messageId -> SDK uuid map (`Session.messageIdToUuid`).
#[derive(Default)]
pub struct ReplaySinks<'a> {
    pub failures: Option<&'a mut crate::session::failures::SessionFailures>,
    pub async_tasks: Option<&'a mut crate::async_tasks::AsyncTaskRuntime>,
    pub message_ids: Option<&'a mut crate::message_ids::MessageIdMap>,
    /// The client takes ID-addressed compaction updates (`session.compaction`),
    /// so a persisted compact summary replays as one completed
    /// `compaction_update` instead of chat text (B-50).
    pub compaction: bool,
    /// The client is on AIR's native subagent draft (`subagentForm === "air"`):
    /// the replay announces every Agent/Task launch and replays inline
    /// sidechain messages into synthetic child sessions
    /// (`acp-agent.ts:9083-9210`). Off for RFD clients, which replay each
    /// child from its own transcript instead.
    pub native_air_replay: bool,
    /// The client gets a subagent's text on the root (`forwardSubagentText`,
    /// `acp-agent.ts:9068-9073`): the legacy `subagent-transcript` opt-in or
    /// the `forwardSubagentText` session option, for a client without native
    /// subagent sessions.
    pub forward_subagent_text: bool,
}

/// The one `session_info_update` after a complete history replay
/// (`acp-agent.ts:9529-9544`): no `title` and no `updatedAt` (a `null` would
/// clear them), only `_meta.jetbrains.air.loadReplayEnd: true`.
pub fn load_replay_end_marker(session_id: &str) -> Value {
    json!({
        "sessionId": session_id,
        "update": {
            "sessionUpdate": "session_info_update",
            "_meta": {
                "jetbrains": {
                    "air": {"version": 1, "loadReplayEnd": true}
                }
            }
        }
    })
}

/// One persisted `<task-notification>`, in the shape of the live SDK frame
/// (`PersistedTaskNotification`, `acp-agent.ts:2237-2248`).
#[derive(Debug, Clone)]
struct PersistedTaskNotification {
    task_id: String,
    status: String,
    tool_use_id: Option<String>,
    summary: Option<String>,
    output_file: Option<String>,
}

/// `taskNotificationsOf` (`acp-agent.ts:2250-2275`): each
/// `<task-notification>` in user-message content. The `<result>` body is model
/// context the live frame does not carry, so the scan stops at it.
fn task_notifications_of(content: &Value) -> Vec<PersistedTaskNotification> {
    let texts: Vec<&str> = match content {
        Value::String(text) => vec![text.as_str()],
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    };
    let open = "<task-notification>";
    let close = "</task-notification>";
    let mut notifications = Vec::new();
    for text in texts {
        let mut at = text.find(open);
        while let Some(start) = at {
            let Some(end_rel) = text[start + open.len()..].find(close) else {
                break;
            };
            let end = start + open.len() + end_rel;
            let body = text[start + open.len()..end]
                .split("<result>")
                .next()
                .unwrap_or("");
            let field = |tag: &str| {
                marker_tag_text(body, tag)
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            };
            let (Some(task_id), Some(status)) = (field("task-id"), field("status")) else {
                at = text[end + close.len()..]
                    .find(open)
                    .map(|next| end + close.len() + next);
                continue;
            };
            notifications.push(PersistedTaskNotification {
                task_id,
                status,
                tool_use_id: field("tool-use-id"),
                summary: field("summary"),
                output_file: field("output-file"),
            });
            at = text[end + close.len()..]
                .find(open)
                .map(|next| end + close.len() + next);
        }
    }
    notifications
}

/// What replay knows of a tool call it has seen (`replayToolUses`).
#[derive(Debug, Clone)]
struct ReplayToolUse {
    name: String,
    input: Value,
    /// The ACP session the call belongs to (`replayToolUses.set(..., {sessionId})`):
    /// the replay target of its message, so a later task notification routes
    /// there (`AsyncTaskRuntime`'s `routeOf`, `acp-agent.ts:9246-9252`).
    session_id: String,
}

/// One child the AIR replay shows (`replayChildren`, `acp-agent.ts:9083-9135`).
#[derive(Debug, Clone)]
struct AirReplayChild {
    /// `<session>:replay-subagent:<launch id>` (AIR's draft), or the SDK agent id
    /// when the child was read from its own persisted transcript.
    session_id: String,
    /// The Agent/Task call of the child that launched this one.
    parent_tool_use_id: Option<String>,
    name: String,
    task: String,
    /// False for a launch without persisted metadata, or one whose lineage is
    /// malformed: its end is `disconnected`.
    reconstructable: bool,
    announced: bool,
    /// The state of the launch's tool_result (`replayTerminalStates`).
    terminal_state: Option<crate::acp_subagents::SubagentState>,
    /// The state of the child's last persisted task notification, which comes
    /// after the launch result and wins.
    notified_state: Option<crate::acp_subagents::SubagentState>,
}

/// AIR's native-subagent replay state (`acp-agent.ts:9083-9135`): the launches
/// of the history and the children synthesized from inline sidechain messages.
#[derive(Debug, Default)]
struct AirReplay {
    /// The children in registration order (`replayChildren` insertion order).
    children: Vec<(String, AirReplayChild)>,
    /// The terminal state of every launch tool_result, by tool-use id.
    terminal_states: HashMap<String, crate::acp_subagents::SubagentState>,
}

impl AirReplay {
    fn child(&self, launch_id: &str) -> Option<&AirReplayChild> {
        self.children
            .iter()
            .find(|(id, _)| id == launch_id)
            .map(|(_, child)| child)
    }

    fn child_mut(&mut self, launch_id: &str) -> Option<&mut AirReplayChild> {
        self.children
            .iter_mut()
            .find(|(id, _)| id == launch_id)
            .map(|(_, child)| child)
    }

    /// `registerLaunches`: the Agent/Task launches of `messages`, with the
    /// terminal state of each launch's tool_result and the launching message's
    /// `parent_tool_use_id`.
    fn register(&mut self, messages: &[Value], root_session_id: &str) {
        for message in messages {
            let Some(content) = message
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            else {
                continue;
            };
            let owner = parent_tool_use_id_of(message).map(str::to_string);
            for block in content {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_result") | Some("mcp_tool_result") => {
                        if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                            self.terminal_states.insert(
                                id.to_string(),
                                crate::subagent_replay::replay_subagent_terminal_state(block),
                            );
                        }
                    }
                    Some("tool_use") | Some("server_tool_use") | Some("mcp_tool_use") => {
                        let (Some(id), Some(name)) = (
                            block.get("id").and_then(Value::as_str),
                            block.get("name").and_then(Value::as_str),
                        ) else {
                            continue;
                        };
                        if !crate::native_subagents::is_native_subagent_control_tool(Some(name)) {
                            continue;
                        }
                        let input = block.get("input").cloned().unwrap_or(Value::Null);
                        let task = ["prompt", "description"]
                            .iter()
                            .find_map(|key| {
                                input
                                    .get(*key)
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|value| !value.is_empty())
                            })
                            .unwrap_or("Delegated task restored from session history")
                            .to_string();
                        let name = ["name", "description", "subagent_type"]
                            .iter()
                            .find_map(|key| {
                                input
                                    .get(*key)
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|value| !value.is_empty())
                            })
                            .unwrap_or("Restored agent")
                            .to_string();
                        let terminal_state = self.terminal_states.get(id).copied();
                        if self.child(id).is_some() {
                            continue;
                        }
                        self.children.push((
                            id.to_string(),
                            AirReplayChild {
                                session_id: format!("{root_session_id}:replay-subagent:{id}"),
                                parent_tool_use_id: owner.clone(),
                                name,
                                task,
                                reconstructable: true,
                                announced: false,
                                terminal_state,
                                notified_state: None,
                            },
                        ));
                    }
                    _ => {}
                }
            }
        }
        // A result that came after its launch in the same pass still wins.
        let terminal_states: Vec<(String, crate::acp_subagents::SubagentState)> = self
            .terminal_states
            .iter()
            .map(|(id, state)| (id.clone(), *state))
            .collect();
        for (id, state) in terminal_states {
            if let Some(child) = self.child_mut(&id) {
                child.terminal_state = Some(state);
            }
        }
    }
}

/// One child transcript the RFD replay is walking (B-52). The stack keeps the
/// upstream recursion: `acp-agent.ts:9479-9493` runs `replayMessage` on each
/// child message, so a nested launch is announced and replayed in full before
/// the child's next message, not after the whole child history.
struct ChildTask {
    launch_id: String,
    session_id: String,
    depth: usize,
    /// Set when the child is announced and its transcript is read and scanned.
    started: bool,
    messages: Vec<Value>,
    index: usize,
    state: MapState,
    task_state: Vec<(String, crate::hooks::TaskStateEntry)>,
    last_published: Option<String>,
}

/// The deepest nested launch a replay follows. The replay cannot loop (a launch
/// id is registered once), but a pathological store stays bounded.
const MAX_SUBAGENT_REPLAY_DEPTH: usize = 64;

/// One replay pass: the ordered updates it produced plus the state the caller
/// seeds into the live session.
struct ReplayRun<'a> {
    session_id: &'a str,
    air: bool,
    plan_ops: bool,
    /// A persisted compact summary replays as a completed `compaction_update`
    /// (B-50, `acp-agent.ts:9403-9422`).
    compaction: bool,
    failures: Option<&'a mut crate::session::failures::SessionFailures>,
    async_tasks: Option<&'a mut crate::async_tasks::AsyncTaskRuntime>,
    message_ids: Option<&'a mut crate::message_ids::MessageIdMap>,
    active_limit_uuid: Option<String>,
    replay_turn_id: Option<String>,
    out: Vec<Value>,
    state: MapState,
    task_state: Vec<(String, crate::hooks::TaskStateEntry)>,
    last_published: Option<String>,
    message_id: String,
    replay_tool_uses: HashMap<String, ReplayToolUse>,
    /// The RFD subagent replay of a load/resume (B-52,
    /// `acp-agent.ts:9479-9498`); `None` for a client without native subagent
    /// sessions, or a history without a launch.
    subagents: Option<SubagentReplayState>,
    /// The children whose history is being replayed, depth-first (see
    /// [`ChildTask`]).
    child_tasks: Vec<ChildTask>,
    /// AIR's native-subagent replay (`acp-agent.ts:9083-9135`): launches are
    /// announced and inline sidechain messages replay into synthetic children.
    /// `None` for every other client.
    air_replay: Option<AirReplay>,
    /// The client gets a subagent's text on the root (`forwardSubagentText`).
    forward_subagent_text: bool,
}

/// The RFD subagent replay and the history it reads (B-52).
struct SubagentReplayState {
    replay: crate::subagent_replay::RfdSubagentReplay,
    context: crate::subagent_replay::SubagentReplayContext,
}

impl<'a> ReplayRun<'a> {
    fn new(
        messages: &[Value],
        session_id: &'a str,
        air: bool,
        plan_ops: bool,
        sinks: ReplaySinks<'a>,
    ) -> Self {
        let active_limit_uuid = sinks
            .failures
            .as_ref()
            .and_then(|_| crate::session_failure::active_usage_limit_message(messages))
            .map(|(uuid, _)| uuid);
        let mut air_replay = sinks.native_air_replay.then(AirReplay::default);
        if let Some(replay) = air_replay.as_mut() {
            replay.register(messages, session_id);
        }
        Self {
            session_id,
            air,
            plan_ops,
            compaction: sinks.compaction,
            failures: sinks.failures,
            async_tasks: sinks.async_tasks,
            message_ids: sinks.message_ids,
            active_limit_uuid,
            replay_turn_id: None,
            out: Vec::new(),
            state: MapState::default(),
            task_state: Vec::new(),
            last_published: None,
            message_id: String::new(),
            replay_tool_uses: HashMap::new(),
            subagents: None,
            child_tasks: Vec::new(),
            air_replay,
            forward_subagent_text: sinks.forward_subagent_text,
        }
    }

    fn push(&mut self, update: Value) {
        self.push_to(self.session_id, update);
    }

    fn push_to(&mut self, session_id: &str, update: Value) {
        self.out
            .push(json!({ "sessionId": session_id, "update": update }));
    }

    /// `announceReplayChild` (`acp-agent.ts:9137-9185`): announce the child of
    /// `parent_tool_use_id` on its parent (the root, or the child that launched
    /// it), after announcing that ancestor first. A launch the history does not
    /// describe, or one whose lineage is malformed, becomes a deterministic
    /// `Disconnected agent`. Returns the child's session id.
    fn announce_air_child(&mut self, parent_tool_use_id: &str) -> String {
        self.announce_air_child_with(parent_tool_use_id, &mut Vec::new())
    }

    fn announce_air_child_with(
        &mut self,
        parent_tool_use_id: &str,
        ancestors: &mut Vec<String>,
    ) -> String {
        let Some(replay) = self.air_replay.as_mut() else {
            return self.session_id.to_string();
        };
        if replay.child(parent_tool_use_id).is_none() {
            replay.children.push((
                parent_tool_use_id.to_string(),
                AirReplayChild {
                    session_id: format!("{}:replay-subagent:{parent_tool_use_id}", self.session_id),
                    parent_tool_use_id: None,
                    name: "Disconnected agent".to_string(),
                    task: "Subagent restored without persisted launch metadata".to_string(),
                    reconstructable: false,
                    announced: false,
                    terminal_state: None,
                    notified_state: None,
                },
            ));
        }
        // A lineage cycle: the child cannot be placed under itself, so its end
        // is `disconnected`.
        if ancestors.iter().any(|id| id == parent_tool_use_id) {
            if let Some(child) = replay.child_mut(parent_tool_use_id) {
                child.reconstructable = false;
                child.terminal_state = None;
                child.parent_tool_use_id = None;
            }
        }
        let parent_of_child = replay
            .child(parent_tool_use_id)
            .and_then(|child| child.parent_tool_use_id.clone());
        if let Some(ancestor) = parent_of_child {
            ancestors.push(parent_tool_use_id.to_string());
            self.announce_air_child_with(&ancestor, ancestors);
            ancestors.pop();
        }
        let Some(replay) = self.air_replay.as_mut() else {
            return self.session_id.to_string();
        };
        let Some(child) = replay.child(parent_tool_use_id) else {
            return self.session_id.to_string();
        };
        let session_id = child.session_id.clone();
        if child.announced {
            return session_id;
        }
        let parent_session_id = child
            .parent_tool_use_id
            .as_deref()
            .and_then(|parent| replay.child(parent))
            .map(|parent| parent.session_id.clone())
            .unwrap_or_else(|| self.session_id.to_string());
        let (name, task) = (child.name.clone(), child.task.clone());
        if let Some(child) = replay.child_mut(parent_tool_use_id) {
            child.announced = true;
        }
        let spawned = crate::acp_subagents::subagent_spawned(
            &parent_session_id,
            &session_id,
            &name,
            &task,
            None,
        );
        self.out.push(spawned);
        session_id
    }

    /// The session a replayed message's updates go to
    /// (`replayTargetSessionId`, `acp-agent.ts:9352-9356`): the child of its
    /// `parent_tool_use_id` on AIR, else the root.
    fn air_target_session(&mut self, message: &Value) -> String {
        match (
            self.air_replay.is_some(),
            parent_tool_use_id_of(message).map(str::to_string),
        ) {
            (true, Some(parent)) => self.announce_air_child(&parent),
            _ => self.session_id.to_string(),
        }
    }

    /// `restoreTaskNotification` (`acp-agent.ts:9258-9284`): a persisted
    /// notification of a background task restores its card. A notification of
    /// a native subagent launch is the child's state, not an async task; a
    /// task that no tool call started stays unknown.
    fn restore_task_notification(&mut self, notification: &PersistedTaskNotification) {
        // `acp-agent.ts:9261`: the RFD replay takes a notification whose task id
        // is a child's agent id first, so a background child ends with the
        // persisted state (`completed`/`failed`) instead of `idle`.
        if let Some(state) = self.subagents.as_mut() {
            if state
                .replay
                .task_notification(&notification.task_id, &notification.status)
            {
                return;
            }
        }
        let Some(tool_use_id) = notification.tool_use_id.clone() else {
            return;
        };
        // AIR (`acp-agent.ts:9263-9271`): a notification of a child launch is
        // the child's persisted state, which wins over its launch result.
        if self.air_replay.is_some() {
            let state = crate::native_subagents::native_subagent_state(&notification.status);
            if let Some(replay) = self.air_replay.as_mut() {
                if let Some(child) = replay.child_mut(&tool_use_id) {
                    if let Some(state) = state {
                        child.notified_state = Some(state);
                    }
                    return;
                }
            }
        }
        let Some(tool_use) = self.replay_tool_uses.get(&tool_use_id).cloned() else {
            return;
        };
        if crate::native_subagents::is_native_subagent_control_tool(Some(&tool_use.name)) {
            return;
        }
        let mut backgrounded = json!({
            "task_id": notification.task_id,
            "is_backgrounded": true,
            "tool_use_id": tool_use_id,
        });
        if let Some(output_file) = &notification.output_file {
            backgrounded["output_file"] = json!(output_file);
        }
        if tool_use.name == "Bash" {
            backgrounded["task_type"] = json!("local_bash");
            if let Some(command) = tool_use.input.get("command").and_then(Value::as_str) {
                backgrounded["description"] = json!(command);
            }
        }
        let mut frame = json!({
            "task_id": notification.task_id,
            "status": notification.status,
        });
        if let Some(tool_use_id) = &notification.tool_use_id {
            frame["tool_use_id"] = json!(tool_use_id);
        }
        if let Some(summary) = &notification.summary {
            frame["summary"] = json!(summary);
        }
        if let Some(output_file) = &notification.output_file {
            frame["output_file"] = json!(output_file);
        }
        let updates = {
            let Some(runtime) = self.async_tasks.as_deref_mut() else {
                return;
            };
            let mut updates = runtime.task_backgrounded(&backgrounded);
            updates.extend(runtime.task_notification(&frame));
            updates
        };
        // `AsyncTaskRuntime.send` routes through `routeOf(toolCallId)`: the
        // session recorded for the launching tool use (`acp-agent.ts:9246-9252`).
        let target = tool_use.session_id;
        for update in updates {
            if target == self.session_id {
                self.push(update);
            } else {
                self.push_to(&target, update);
            }
        }
    }

    fn process_message(&mut self, message: &Value) {
        if is_hidden_meta_message(message) {
            return;
        }
        // In the RFD form a child's history comes from its own transcript, so a
        // subagent message that the session transcript stores inline is skipped
        // (`acp-agent.ts:9297`).
        if self.subagents.is_some() && parent_tool_use_id_of(message).is_some() {
            return;
        }
        let role = message.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = message.get("message").cloned().unwrap_or(Value::Null);
        let top_level = message.get("parent_tool_use_id").is_none_or(Value::is_null);
        if role == "user" && top_level {
            if let Some(uuid) = message
                .get("uuid")
                .and_then(Value::as_str)
                .filter(|uuid| !uuid.is_empty())
            {
                self.replay_turn_id = Some(uuid.to_string());
            }
        }
        // B-54 (`acp-agent.ts:9306-9314`): backfill the ACP messageId -> SDK
        // uuid mapping for messages not observed live (resumed/loaded
        // sessions), so rewind/fork can translate a client-supplied id.
        let replay_message_id = crate::mcp_command::message_id_of(message);
        if let (Some(map), Some(id), Some(uuid)) = (
            self.message_ids.as_deref_mut(),
            replay_message_id.as_deref(),
            message
                .get("uuid")
                .and_then(Value::as_str)
                .filter(|uuid| !uuid.is_empty()),
        ) {
            map.set(id, uuid);
        }

        // B-50 (`acp-agent.ts:9403-9422`): a persisted compact summary replays
        // as one completed compaction entity at its position, not as chat text.
        // Only a client that takes ID-addressed compaction updates gets it.
        if self.compaction && crate::context_compaction::is_compact_summary_message(message) {
            let content = payload.get("content").cloned().unwrap_or(Value::Null);
            if let Some(summary) = compact_summary_text_of(&content) {
                let compaction_id = message
                    .get("uuid")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.push(json!({
                    "sessionUpdate": "compaction_update",
                    "compactionId": compaction_id,
                    "status": "completed",
                    "summary": [{ "type": "text", "text": summary }],
                }));
            }
            return;
        }

        if role == "assistant" && is_synthetic_login_message(message) {
            return;
        }
        if let Some(book) = self.failures.as_deref_mut() {
            if role == "assistant"
                && top_level
                && crate::session_failure::is_synthetic_usage_limit_message(&payload)
            {
                if let Some(title) = crate::session_failure::assistant_message_text(&payload) {
                    let uuid = message.get("uuid").and_then(Value::as_str).unwrap_or("");
                    let id = match &self.replay_turn_id {
                        Some(turn) => format!("{turn}:error"),
                        None => format!("{}:history-error:{uuid}", self.session_id),
                    };
                    let update = book.restore(
                        self.session_id,
                        &id,
                        crate::session_failure::ClaudeFailureKind::QuotaExhausted,
                        &title,
                        self.active_limit_uuid.as_deref() == Some(uuid),
                    );
                    // `restore` already returns the whole notification.
                    self.out.push(update);
                }
                return;
            }
        }
        let mut content = payload.get("content").cloned().unwrap_or(Value::Null);
        // `replayTargetSessionId` (`acp-agent.ts:9352-9356`): AIR announces the
        // child of an inline sidechain message before mapping it, and every
        // update of the message goes to that child's session.
        let air_target = self.air_target_session(message);
        let nested_parent = parent_tool_use_id_of(message).map(str::to_string);
        if role == "user" {
            // B-45: like live, a tool result gives the output path of a known
            // task, and a persisted task notification restores the task card
            // (`acp-agent.ts:9363-9375`).
            let task_updates = self
                .async_tasks
                .as_deref_mut()
                .map(|runtime| runtime.tool_results(&content))
                .unwrap_or_default();
            for update in task_updates {
                // `AsyncTaskRuntime.send` routes through `routeOf`: the session
                // recorded for the task's tool call.
                let target = update
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .and_then(|id| self.replay_tool_uses.get(id))
                    .map(|tool_use| tool_use.session_id.clone())
                    .unwrap_or_else(|| air_target.clone());
                self.push_to(&target, update);
            }
            let notifications = task_notifications_of(&content);
            for notification in &notifications {
                self.restore_task_notification(notification);
            }
            // Live, the prompt loop skips this record and the SDK frame reports
            // the stop; replay restores the task instead of showing it.
            if is_task_notification_record(message) {
                return;
            }
            let Some(stripped) = strip_local_command_metadata(content) else {
                return;
            };
            content = stripped;
        } else if let Value::Array(blocks) = &content {
            for block in blocks {
                if ["tool_use", "server_tool_use", "mcp_tool_use"]
                    .contains(&block.get("type").and_then(Value::as_str).unwrap_or(""))
                {
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        self.replay_tool_uses.insert(
                            id.to_string(),
                            ReplayToolUse {
                                name: block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                input: block.get("input").cloned().unwrap_or(Value::Null),
                                session_id: air_target.clone(),
                            },
                        );
                    }
                }
            }
        }
        if role == "assistant"
            && nested_parent.is_some()
            && self.air_replay.is_none()
            && !self.forward_subagent_text
        {
            // Plain client without native subagent sessions: the nested
            // assistant's text/thinking is dropped (not forwarded to the root),
            // but its tool blocks still replay (`stripSubagentTextAndThinking`,
            // `acp-agent.js:6990-6996`).
            content = strip_subagent_text_and_thinking(&content);
        }
        let msg_role = if role == "user" {
            MsgRole::User
        } else {
            MsgRole::Assistant
        };
        let id = payload
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| message.get("uuid").and_then(Value::as_str))
            .filter(|s| !s.is_empty())
            .map(String::from);
        if let Some(id) = id {
            self.message_id = id;
        }
        let updates = replay_content(
            &content,
            msg_role,
            &mut self.state,
            &mut self.task_state,
            ReplayFlags {
                air: self.air,
                plan_ops: self.plan_ops,
                top_level,
            },
            &mut self.last_published,
            nested_parent.as_deref(),
        );
        let message_id = if self.message_id.is_empty() {
            None
        } else {
            Some(self.message_id.clone())
        };
        for update in updates {
            let mut update = match update {
                // Group chunk updates under the API message id
                // (`apply_stream_message_id`); plan updates stay untouched.
                Value::Object(mut map)
                    if map
                        .get("sessionUpdate")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| {
                            matches!(
                                kind,
                                "agent_message_chunk"
                                    | "agent_thought_chunk"
                                    | "user_message_chunk"
                            )
                        }) =>
                {
                    if let Some(id) = &message_id {
                        map.insert("messageId".into(), json!(id));
                    }
                    Value::Object(map)
                }
                other => other,
            };
            // `stampParentToolUseId(update, options.parentToolUseId)`
            // (`acp-agent.ts:13356`): every update of a nested message carries
            // the Agent/Task call that launched the subagent.
            if let Some(parent) = &nested_parent {
                Self::stamp_child_parent_tool_use_id(&mut update, parent);
            }
            // AIR (`acp-agent.ts:9447-9452`): a native Agent/Task control call is
            // not a transcript tool; its child announcement replaces it.
            if self.air_replay.is_some()
                && matches!(
                    update.get("sessionUpdate").and_then(Value::as_str),
                    Some("tool_call" | "tool_call_update")
                )
                && update
                    .pointer("/_meta/claudeCode/toolName")
                    .and_then(Value::as_str)
                    .is_some_and(|name| {
                        crate::native_subagents::is_native_subagent_control_tool(Some(name))
                    })
            {
                continue;
            }
            if air_target == self.session_id {
                self.push(update);
            } else {
                self.push_to(&air_target, update);
            }
        }
        // AIR (`acp-agent.ts:9461-9470`): announce each launch of the message,
        // after its own updates.
        if self.air_replay.is_some() && role == "assistant" {
            for launch_id in crate::native_subagents::subagent_launch_ids(&content) {
                self.announce_air_child(&launch_id);
            }
        }
        self.seed_child_launches(&content, role);
    }

    /// One message of the replay, then the children its launches announced.
    fn process(&mut self, message: &Value) {
        self.process_message(message);
        self.drain_children();
    }

    /// [`ReplayRun::process`] with the upstream `messageLoopYielder` cadence
    /// (`acp-agent.ts:9289-9291`): a turn between each child message too, so a
    /// long child history does not starve the other sessions.
    async fn process_async(
        &mut self,
        message: &Value,
        yielder: &mut crate::event_loop_yield::EventLoopYielder,
    ) {
        self.process_message(message);
        self.drain_children_async(yielder).await;
    }

    /// B-52 (`acp-agent.ts:9479-9493`): right after an assistant message that
    /// launched a subagent, announce the child and replay its own transcript
    /// into the child's session. A launch inside that transcript recurses: the
    /// same replay runs on the grandchild with the child as its parent.
    fn seed_child_launches(&mut self, content: &Value, role: &str) {
        if role != "assistant" {
            return;
        }
        self.push_child_launches(content, 1);
    }

    /// Pushes the launches in `content` that are not announced yet onto the
    /// child stack, at `depth`.
    fn push_child_launches(&mut self, content: &Value, depth: usize) {
        if depth > MAX_SUBAGENT_REPLAY_DEPTH {
            return;
        }
        let Some(state) = self.subagents.as_ref() else {
            return;
        };
        let launches = crate::native_subagents::subagent_launch_ids(content);
        for launch_id in launches.into_iter().rev() {
            let Some(child) = state.replay.child_of_launch(&launch_id) else {
                // A launch without metadata is not announced (B-105).
                continue;
            };
            if child.announced
                || self
                    .child_tasks
                    .iter()
                    .any(|task| task.launch_id == launch_id)
            {
                continue;
            }
            self.child_tasks.push(ChildTask {
                launch_id,
                session_id: child.session_id.clone(),
                depth,
                started: false,
                messages: Vec::new(),
                index: 0,
                state: MapState::default(),
                task_state: Vec::new(),
                last_published: None,
            });
        }
    }

    fn drain_children(&mut self) {
        while !self.child_tasks.is_empty() {
            self.step_child();
        }
    }

    async fn drain_children_async(
        &mut self,
        yielder: &mut crate::event_loop_yield::EventLoopYielder,
    ) {
        while !self.child_tasks.is_empty() {
            yielder.maybe_yield().await;
            let needs_start = self.child_tasks.last().is_some_and(|t| !t.started);
            if needs_start {
                let Some(task) = self.child_tasks.last() else {
                    break;
                };
                let launch_id = task.launch_id.clone();
                let Some(state) = self.subagents.as_mut() else {
                    self.child_tasks.pop();
                    continue;
                };
                let Some(child) = state.replay.child_of_launch(&launch_id).cloned() else {
                    self.child_tasks.pop();
                    continue;
                };
                let produced = state.replay.announce(&launch_id);
                self.out.extend(produced);

                let session_id = self.session_id.to_string();
                let child_session_id = child.session_id.clone();
                let history = state.context.history.clone();
                let logger = state.context.logger.clone();

                let read_res = tokio::task::spawn_blocking(move || {
                    crate::subagent_history::read_subagent_messages(
                        &session_id,
                        &child_session_id,
                        &history,
                        logger.as_deref(),
                    )
                })
                .await;

                let messages = match read_res {
                    Ok(msgs) => msgs,
                    Err(err) => {
                        if let Some(logger) = &state.context.logger {
                            logger.error(&format!(
                                "JoinError reading subagent transcript for {}: {err}",
                                child.session_id
                            ));
                        } else {
                            eprintln!(
                                "JoinError reading subagent transcript for {}: {err}",
                                child.session_id
                            );
                        }
                        Vec::new()
                    }
                };

                state.replay.scan(&messages, &child.session_id);
                if let Some(task) = self.child_tasks.last_mut() {
                    task.started = true;
                    task.messages = messages;
                }
            } else {
                self.step_child();
            }
        }
    }

    /// Stamps `_meta.claudeCode.parentToolUseId` on child updates (`acp-agent.ts:1899-1907`).
    fn stamp_child_parent_tool_use_id(update: &mut Value, parent_tool_use_id: &str) {
        let Some(update_obj) = update.as_object_mut() else {
            return;
        };
        let meta_entry = update_obj.entry("_meta").or_insert_with(|| json!({}));
        if !meta_entry.is_object() {
            *meta_entry = json!({});
        }
        let Some(meta_obj) = meta_entry.as_object_mut() else {
            return;
        };
        let claude_code_entry = meta_obj.entry("claudeCode").or_insert_with(|| json!({}));
        if !claude_code_entry.is_object() {
            *claude_code_entry = json!({});
        }
        let Some(claude_code_obj) = claude_code_entry.as_object_mut() else {
            return;
        };
        claude_code_obj.insert("parentToolUseId".into(), json!(parent_tool_use_id));
    }

    /// One step of the depth-first child walk: begin the child on the stack's
    /// top (announce it, read and scan its transcript), replay one of its
    /// messages, or finish it when its history is exhausted.
    fn step_child(&mut self) {
        let Some(task) = self.child_tasks.last() else {
            return;
        };
        let launch_id = task.launch_id.clone();
        let depth = task.depth;
        let started = task.started;
        let index = task.index;
        let length = task.messages.len();
        if !started {
            let Some(state) = self.subagents.as_mut() else {
                self.child_tasks.pop();
                return;
            };
            let Some(child) = state.replay.child_of_launch(&launch_id).cloned() else {
                self.child_tasks.pop();
                return;
            };
            let produced = state.replay.announce(&launch_id);
            self.out.extend(produced);
            let messages = crate::subagent_history::read_subagent_messages(
                self.session_id,
                &child.session_id,
                &state.context.history,
                state.context.logger.as_deref(),
            );
            state.replay.scan(&messages, &child.session_id);
            if let Some(task) = self.child_tasks.last_mut() {
                task.started = true;
                task.messages = messages;
            }
            return;
        }
        if index >= length {
            let count = length;
            self.child_tasks.pop();
            if let Some(state) = self.subagents.as_mut() {
                let produced = state.replay.child_replayed(&launch_id, count);
                self.out.extend(produced);
            }
            return;
        }
        let message = self
            .child_tasks
            .last()
            .map(|task| task.messages[index].clone());
        let Some(message) = message else {
            return;
        };
        if let Some(task) = self.child_tasks.last_mut() {
            task.index += 1;
        }
        if is_hidden_meta_message(&message) {
            return;
        }
        let kind = message.get("type").and_then(Value::as_str).unwrap_or("");
        // `getSubagentMessages` keeps only user/assistant, but a transcript a
        // caller supplied may carry other kinds.
        if kind != "user" && kind != "assistant" {
            return;
        }
        let mut content = message
            .get("message")
            .and_then(|m| m.get("content"))
            .cloned()
            .unwrap_or(Value::Null);
        let (publications, rest) = if kind == "user" {
            for n in task_notifications_of(&content) {
                self.restore_task_notification(&n);
            }
            // `acp-agent.ts:9370-9374`: a persisted task notification is not a
            // child message, and local-command metadata is stripped first.
            if is_task_notification_record(&message) {
                return;
            }
            let Some(stripped) = strip_local_command_metadata(content) else {
                return;
            };
            content = stripped;
            match self.subagents.as_mut() {
                Some(state) => state.replay.child_content(&launch_id, &message, &content),
                None => (Vec::new(), Some(content)),
            }
        } else {
            (Vec::new(), Some(content))
        };
        self.out.extend(publications);
        let Some(content) = rest else {
            return;
        };
        let msg_role = if kind == "user" {
            MsgRole::User
        } else {
            MsgRole::Assistant
        };
        let child_message_id = message
            .get("message")
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .or_else(|| message.get("uuid").and_then(Value::as_str))
            .filter(|id| !id.is_empty())
            .map(ToString::to_string);
        let air = self.air;
        let (updates, child_session_id) = {
            let Some(task) = self.child_tasks.last_mut() else {
                return;
            };
            let updates = replay_content(
                &content,
                msg_role,
                &mut task.state,
                &mut task.task_state,
                ReplayFlags {
                    air,
                    plan_ops: false,
                    top_level: false,
                },
                &mut task.last_published,
                None,
            );
            (updates, task.session_id.clone())
        };
        // Group the child's chunk updates under its API message id
        // (`apply_stream_message_id`), as the root's are.
        for update in updates {
            let mut update = match update {
                Value::Object(mut map)
                    if map
                        .get("sessionUpdate")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| {
                            matches!(
                                kind,
                                "agent_message_chunk"
                                    | "agent_thought_chunk"
                                    | "user_message_chunk"
                            )
                        }) =>
                {
                    if let Some(id) = &child_message_id {
                        map.insert("messageId".into(), json!(id));
                    }
                    Value::Object(map)
                }
                other => other,
            };
            Self::stamp_child_parent_tool_use_id(&mut update, &launch_id);
            self.out.push(json!({
                "sessionId": child_session_id,
                "update": update,
            }));
        }
        if kind == "assistant" {
            // `replayMessage(childMessage)` runs the same launch loop on the
            // child's assistant message: its launches are the grandchildren.
            self.push_child_launches(&content, depth + 1);
        }
    }

    fn finish(
        mut self,
    ) -> (
        Vec<Value>,
        Vec<(String, crate::hooks::TaskStateEntry)>,
        Option<String>,
    ) {
        self.drain_children();
        if let Some(mut state) = self.subagents.take() {
            let notices = state.context.notices;
            self.out
                .extend(state.replay.finish(notices, self.session_id));
        }
        // AIR (`acp-agent.ts:9500-9524`): every announced child ends before the
        // replay does, in reverse registration order. The end is the child's
        // last task notification, else its launch result, else `disconnected`
        // for a child the history cannot place.
        if let Some(replay) = self.air_replay.take() {
            let mut states = Vec::new();
            for (_, child) in replay.children.iter().rev() {
                if !child.announced {
                    continue;
                }
                let parent_session_id = child
                    .parent_tool_use_id
                    .as_deref()
                    .and_then(|parent| replay.child(parent))
                    .map(|parent| parent.session_id.clone())
                    .unwrap_or_else(|| self.session_id.to_string());
                let state = if child.reconstructable {
                    child
                        .notified_state
                        .or(child.terminal_state)
                        .unwrap_or(crate::acp_subagents::SubagentState::Disconnected)
                } else {
                    crate::acp_subagents::SubagentState::Disconnected
                };
                states.push(crate::acp_subagents::subagent_state_update(
                    &parent_session_id,
                    &child.session_id,
                    state,
                ));
            }
            self.out.extend(states);
        }
        (self.out, self.task_state, self.last_published)
    }
}

/// Attaches the RFD subagent replay (B-52) to `run`.
fn attach_subagents(
    run: &mut ReplayRun<'_>,
    messages: &[Value],
    session_id: &str,
    subagents: Option<crate::subagent_replay::SubagentReplayContext>,
) {
    run.subagents = subagents.map(|context| {
        let mut replay = crate::subagent_replay::RfdSubagentReplay::new(context.history.clone());
        replay.scan(messages, session_id);
        SubagentReplayState { replay, context }
    });
}

/// [`replay_with_task_state`] for a client that reads AIR session failures:
/// `failures` is the session's failure book, and every synthetic usage-limit
/// message of the history replays as a typed failure at its transcript
/// position instead of as chat text (N-53 / V-15, `acp-agent.ts:9307-9343`).
/// The record is named after the prompt it answered, so it recreates the live
/// incident identity; only the latest limit not followed by a real model
/// answer stays active in the book.
pub fn replay_with_failures(
    messages: &[Value],
    session_id: &str,
    air: bool,
    plan_ops: bool,
    failures: Option<&mut crate::session::failures::SessionFailures>,
) -> (
    Vec<Value>,
    Vec<(String, crate::hooks::TaskStateEntry)>,
    Option<String>,
) {
    let mut run = ReplayRun::new(
        messages,
        session_id,
        air,
        plan_ops,
        ReplaySinks {
            failures,
            ..ReplaySinks::default()
        },
    );
    for message in messages {
        run.process(message);
    }
    run.finish()
}

/// A replay pass with every sink (failure book, AIR async tasks, messageId
/// map). Used by the live `session/load` path; unit callers use
/// [`replay_with_failures`].
pub fn replay_session(
    messages: &[Value],
    session_id: &str,
    air: bool,
    plan_ops: bool,
    sinks: ReplaySinks<'_>,
) -> (
    Vec<Value>,
    Vec<(String, crate::hooks::TaskStateEntry)>,
    Option<String>,
) {
    let mut run = ReplayRun::new(messages, session_id, air, plan_ops, sinks);
    for message in messages {
        run.process(message);
    }
    run.finish()
}

/// [`replay_session`] with the upstream `messageLoopYielder` cadence: the loop
/// yields to the event loop before each message when its budget is spent
/// (`acp-agent.ts:9289-9291`), so other sessions and incoming requests are not
/// starved by a long history (B-55).
pub async fn replay_session_async(
    messages: &[Value],
    session_id: &str,
    air: bool,
    plan_ops: bool,
    sinks: ReplaySinks<'_>,
    yielder: &mut crate::event_loop_yield::EventLoopYielder,
) -> (
    Vec<Value>,
    Vec<(String, crate::hooks::TaskStateEntry)>,
    Option<String>,
) {
    let mut run = ReplayRun::new(messages, session_id, air, plan_ops, sinks);
    for message in messages {
        yielder.maybe_yield().await;
        run.process_async(message, yielder).await;
    }
    run.finish()
}

/// [`replay_session`] with the RFD subagent replay (B-52): the children of the
/// history's launches are announced after their launch and their own
/// transcripts are replayed into their sessions, and every child's end goes out
/// before the replay ends.
pub fn replay_session_with_subagents(
    messages: &[Value],
    session_id: &str,
    air: bool,
    plan_ops: bool,
    sinks: ReplaySinks<'_>,
    subagents: Option<crate::subagent_replay::SubagentReplayContext>,
) -> (
    Vec<Value>,
    Vec<(String, crate::hooks::TaskStateEntry)>,
    Option<String>,
) {
    let mut run = ReplayRun::new(messages, session_id, air, plan_ops, sinks);
    attach_subagents(&mut run, messages, session_id, subagents);
    for message in messages {
        run.process(message);
    }
    run.finish()
}

/// [`replay_with_failures`] with the RFD subagent replay (B-52). The live
/// `session/load` branch calls this with the context
/// [`crate::subagent_replay::SubagentReplayContext`] describes (lane G), so a
/// loaded live session replays its subagents like `start_resumed` does
/// (`acp-agent.ts:3156-3160` uses `replaySessionHistory` in both branches).
pub fn replay_with_failures_rfd(
    messages: &[Value],
    session_id: &str,
    air: bool,
    plan_ops: bool,
    sinks: ReplaySinks<'_>,
    subagents: Option<crate::subagent_replay::SubagentReplayContext>,
) -> (
    Vec<Value>,
    Vec<(String, crate::hooks::TaskStateEntry)>,
    Option<String>,
) {
    replay_session_with_subagents(messages, session_id, air, plan_ops, sinks, subagents)
}

/// [`replay_session_async`] with the RFD subagent replay (B-52).
pub async fn replay_session_async_with_subagents(
    messages: &[Value],
    session_id: &str,
    air: bool,
    plan_ops: bool,
    sinks: ReplaySinks<'_>,
    yielder: &mut crate::event_loop_yield::EventLoopYielder,
    subagents: Option<crate::subagent_replay::SubagentReplayContext>,
) -> (
    Vec<Value>,
    Vec<(String, crate::hooks::TaskStateEntry)>,
    Option<String>,
) {
    let mut run = ReplayRun::new(messages, session_id, air, plan_ops, sinks);
    attach_subagents(&mut run, messages, session_id, subagents);
    for message in messages {
        yielder.maybe_yield().await;
        run.process_async(message, yielder).await;
    }
    run.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

    /// Write a seed transcript under a fresh temp `CLAUDE_CONFIG_DIR` and return
    /// the env snapshot pointing at it (never the host's real `~/.claude`).
    struct TempProjects(std::path::PathBuf);

    /// Monotonic per-binary counter (PC-2.2): the old local `AtomicUsize` was
    /// recreated on every call, so `n` was always 0 and concurrent tests shared
    /// one temp dir.
    static NEXT_TEMP_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    impl TempProjects {
        fn new(body: &str) -> (Self, EnvSnapshot) {
            let n = NEXT_TEMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("inv67-{}-{n}", std::process::id()));
            let projects = dir.join("projects");
            let slug_dir = projects.join("-p");
            std::fs::create_dir_all(&slug_dir).unwrap();
            std::fs::write(slug_dir.join(format!("{ID}.jsonl")), body).unwrap();
            let env = EnvSnapshot::from_pairs([(
                "CLAUDE_CONFIG_DIR".to_string(),
                dir.to_string_lossy().into_owned(),
            )]);
            (TempProjects(dir), env)
        }
    }

    impl Drop for TempProjects {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn line(entry: Value) -> String {
        entry.to_string()
    }

    fn user(uuid: &str, parent: Option<&str>, content: Value, cwd: &str) -> Value {
        json!({
            "type": "user", "uuid": uuid, "parentUuid": parent, "cwd": cwd,
            "sessionId": ID, "timestamp": "2026-01-01T00:00:00.000Z",
            "message": {"role": "user", "content": content}
        })
    }

    fn assistant(uuid: &str, parent: Option<&str>, content: Value) -> Value {
        json!({
            "type": "assistant", "uuid": uuid, "parentUuid": parent,
            "sessionId": ID, "timestamp": "2026-01-01T00:00:00.001Z",
            "message": {"role": "assistant", "content": content}
        })
    }

    /// 3.5 — `session_messages` walks the chain from the latest leaf by
    /// parentUuid and reverses, dropping sidechain records and off-chain
    /// branches the leaf cannot reach.
    #[test]
    fn inv_67_chain_from_leaf() {
        let body = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n",
            line(assistant(
                "o1",
                Some("u1"),
                json!([{"type":"text","text":"orphan"}])
            )),
            line(user("u1", None, json!("first"), "/p")),
            line(assistant(
                "a1",
                Some("u1"),
                json!([{"type":"text","text":"hi"}])
            )),
            line(user(
                "u2",
                Some("a1"),
                json!([{"type":"tool_result","tool_use_id":"t1","content":"ok"}]),
                "/p"
            )),
            line(assistant(
                "a2",
                Some("u2"),
                json!([{"type":"text","text":"done"}])
            )),
            line(
                json!({"type":"user","uuid":"s1","parentUuid":null,"isSidechain":true,
                "sessionId":ID,"message":{"role":"user","content":"side"}})
            ),
        );
        let (_tmp, env) = TempProjects::new(&body);
        let msgs = session_messages(&env, ID);
        let uuids: Vec<&str> = msgs
            .iter()
            .filter_map(|m| m.get("uuid").and_then(Value::as_str))
            .collect();
        assert_eq!(uuids, vec!["u1", "a1", "u2", "a2"]);
        // Output shape: parent ids always null, session_id mapped.
        for m in &msgs {
            assert_eq!(m["parent_tool_use_id"], Value::Null);
            assert_eq!(m["session_id"], json!(ID));
        }
        // A non-UUID id reads no transcript.
        assert!(session_messages(&env, "sess-1234-abcd").is_empty());
    }

    /// 3.5 — hidden meta origin kinds are skipped by replay; other meta (e.g.
    /// a compact summary) is not, and a non-user message is never hidden.
    #[test]
    fn inv_67_hidden_meta_skipped() {
        for kind in [
            "peer",
            "channel",
            "observer",
            "observer-activity",
            "slack-ping",
        ] {
            let m = json!({"type":"user","is_meta":true,"origin":{"kind":kind}});
            assert!(is_hidden_meta_message(&m), "{kind} must be hidden");
            let not = json!({"type":"user","is_meta":true,"origin":{"kind":"task-notification"}});
            assert!(!is_hidden_meta_message(&not));
        }
        // user without is_meta, or non-user, is never hidden.
        assert!(!is_hidden_meta_message(
            &json!({"type":"user","origin":{"kind":"peer"}})
        ));
        assert!(!is_hidden_meta_message(&json!({"type":"assistant"})));
        assert!(!is_hidden_meta_message(&Value::Null));
    }

    /// 3.5 — replay strips local-command markers from user content; a
    /// marker-only message is skipped, mixed prose survives.
    #[test]
    fn inv_67_local_command_metadata_stripped() {
        let marker_only = json!({
            "type":"user","message":{"role":"user","content":
                [{"type":"text","text":"<command-name>/model</command-name><command-args>sonnet</command-args>"}
            ]}
        });
        let notif = replay_notifications(&[marker_only], ID);
        assert!(notif.is_empty(), "marker-only user must be skipped");

        let mixed = json!({
            "type":"user","message":{"role":"user","content":
                [{"type":"text","text":"hi<system-reminder>internal</system-reminder>"}]
            }
        });
        let notif = replay_notifications(&[mixed], ID);
        assert_eq!(notif.len(), 1);
        assert_eq!(
            notif[0]["update"]["sessionUpdate"],
            json!("user_message_chunk")
        );
        assert_eq!(notif[0]["update"]["content"]["text"], json!("hi"));
    }

    /// PC-2.1 — the SDK `Iwe` filter drops every `isMeta` record except the
    /// replay-visible origin kinds (and a peer that names its sender task), so
    /// caveats and skill expansions never reach the client; replay then hides
    /// the surviving meta origins like the adapter does.
    #[test]
    fn pc2_1_meta_filter_matches_sdk() {
        let body = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            line(
                json!({"type":"user","uuid":"m1","parentUuid":null,"isMeta":true,"sessionId":ID,
                "message":{"role":"user","content":"<local-command-caveat>Caveat</local-command-caveat>"}})
            ),
            line(
                json!({"type":"user","uuid":"m2","parentUuid":"m1","isMeta":true,"sessionId":ID,
                "message":{"role":"user","content":"Base directory for this skill: /x"}})
            ),
            line(
                json!({"type":"user","uuid":"m3","parentUuid":"m2","isMeta":true,
                "origin":{"kind":"channel"},"sessionId":ID,
                "message":{"role":"user","content":"channel message"}})
            ),
            line(
                json!({"type":"user","uuid":"m4","parentUuid":"m3","isMeta":true,
                "origin":{"kind":"peer","senderTaskId":"t1"},"sessionId":ID,
                "message":{"role":"user","content":"peer message"}})
            ),
            line(
                json!({"type":"user","uuid":"m5","parentUuid":"m4","isMeta":true,
                "origin":{"kind":"peer"},"sessionId":ID,
                "message":{"role":"user","content":"peer without sender"}})
            ),
            line(user("u1", Some("m5"), json!("real question"), "/p")),
            line(assistant(
                "a1",
                Some("u1"),
                json!([{"type":"text","text":"answer"}])
            )),
        );
        let (_tmp, env) = TempProjects::new(&body);
        let msgs = session_messages(&env, ID);
        let uuids: Vec<&str> = msgs
            .iter()
            .filter_map(|m| m.get("uuid").and_then(Value::as_str))
            .collect();
        // Caveat and skill expansion are dropped by the SDK filter; the
        // replay-visible origins (including a peer without a sender task, which
        // `ZA` keeps with `udsInboxEnabled`) survive it with `is_meta`, and
        // replay hides them.
        assert_eq!(uuids, vec!["m3", "m4", "m5", "u1", "a1"]);
        assert_eq!(msgs[0]["is_meta"], json!(true));
        assert_eq!(msgs[1]["is_meta"], json!(true));
        assert_eq!(msgs[2]["is_meta"], json!(true));
        let notif = replay_notifications(&msgs, ID);
        let texts: Vec<&str> = notif
            .iter()
            .filter_map(|n| n["update"]["content"]["text"].as_str())
            .collect();
        assert_eq!(texts, vec!["real question", "answer"]);
    }

    /// PC-2.2 — the leaf is the SDK's terminal-node walk: a tip that is not a
    /// user/assistant record walks up to its nearest user/assistant ancestor,
    /// so the newest record in file order does not win when it sits on a
    /// different branch.
    #[test]
    fn pc2_2_leaf_terminal_walk_ignores_latest_record() {
        let body = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n",
            line(user("u1", None, json!("first"), "/p")),
            line(assistant(
                "a1",
                Some("u1"),
                json!([{"type":"text","text":"answer one"}])
            )),
            line(user("u2", Some("a1"), json!("branch A"), "/p")),
            line(assistant(
                "a2",
                Some("u2"),
                json!([{"type":"text","text":"branch A answer"}])
            )),
            line(user("u3", Some("a1"), json!("branch B"), "/p")),
            line(
                json!({"type":"attachment","uuid":"x1","parentUuid":"a1","sessionId":ID,
                "attachment":{"type":"file","path":"/p/f"},
                "message":{"role":"user","content":"attached"}})
            ),
        );
        let (_tmp, env) = TempProjects::new(&body);
        let msgs = session_messages(&env, ID);
        let uuids: Vec<&str> = msgs
            .iter()
            .filter_map(|m| m.get("uuid").and_then(Value::as_str))
            .collect();
        // The latest tip is the attachment; it walks up to `a1`, not to the
        // later branch-B user `u3`.
        assert_eq!(uuids, vec!["u1", "a1"]);
    }

    /// PC-2.2 — a transcript larger than the SDK's 5 MiB threshold replays
    /// only from the last compact boundary (the SDK's precompact skip), and
    /// the read streams instead of slurping the whole file.
    #[test]
    fn pc2_2_precompact_skip_starts_at_last_boundary() {
        let body = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            line(user("u1", None, json!("before"), "/p")),
            line(assistant(
                "a1",
                Some("u1"),
                json!([{"type":"text","text":"before answer"}])
            )),
            line(
                json!({"type":"system","subtype":"compact_boundary","uuid":"b1",
                "parentUuid":"a1","sessionId":ID,
                "compactMetadata":{"preservedMessages":{"anchorUuid":"a1","uuids":["u1"]}}})
            ),
            line(user("u2", Some("a1"), json!("after"), "/p")),
            line(assistant(
                "a2",
                Some("u2"),
                json!([{"type":"text","text":"after answer"}])
            )),
        );
        let (_tmp, env) = TempProjects::new(&body);
        let root = session_store::projects_root_with_env(&env);
        // A zero cap forces the skip on this small fixture.
        let (records, _) = read_records_capped(&root, ID, 0).unwrap();
        let uuids: Vec<&str> = records.iter().map(|r| r.uuid.as_str()).collect();
        assert_eq!(uuids, vec!["b1", "u2", "a2"]);
        // The chain from the post-boundary leaf stops at the missing parent.
        let msgs = session_messages_capped(&env, ID, 0);
        let uuids: Vec<&str> = msgs
            .iter()
            .filter_map(|m| m.get("uuid").and_then(Value::as_str))
            .collect();
        assert_eq!(uuids, vec!["u2", "a2"]);
        // Without the cap the whole transcript is read.
        let (records, _) = read_records_capped(&root, ID, u64::MAX).unwrap();
        assert_eq!(records.len(), 5);
    }

    /// PC-2.1 — a completed local command's output record carries
    /// `isCompletedLocalCommand`; the caveat is filtered, the command replays,
    /// and the stdout record is skipped by the marker stripping.
    #[test]
    fn pc2_1_completed_local_command_flagged() {
        let body = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            line(
                json!({"type":"user","uuid":"c1","parentUuid":null,"isMeta":true,"sessionId":ID,
                "message":{"role":"user","content":"<local-command-caveat>Caveat: local command</local-command-caveat>"}})
            ),
            line(user(
                "r1",
                Some("c1"),
                json!([{"type":"text","text":"<command-name>/help</command-name><command-args>topic</command-args>"}]),
                "/p"
            )),
            line(user(
                "o1",
                Some("r1"),
                json!([{"type":"text","text":"<local-command-stdout>help output</local-command-stdout>"}]),
                "/p"
            )),
            line(user("u1", Some("o1"), json!("hi"), "/p")),
            line(assistant(
                "a1",
                Some("u1"),
                json!([{"type":"text","text":"hello"}])
            )),
        );
        let (_tmp, env) = TempProjects::new(&body);
        let msgs = session_messages(&env, ID);
        let uuids: Vec<&str> = msgs
            .iter()
            .filter_map(|m| m.get("uuid").and_then(Value::as_str))
            .collect();
        assert_eq!(uuids, vec!["r1", "o1", "u1", "a1"]);
        assert_eq!(msgs[1]["isCompletedLocalCommand"], json!(true));
        assert!(msgs[0].get("isCompletedLocalCommand").is_none());
        let notif = replay_notifications(&msgs, ID);
        let texts: Vec<&str> = notif
            .iter()
            .filter_map(|n| n["update"]["content"]["text"].as_str())
            .collect();
        assert_eq!(texts, vec!["/help topic", "hi", "hello"]);
    }

    /// PC-2.3 — replay rebuilds the task state from TaskCreate/TaskUpdate and
    /// publishes the plan as Node does, so a status-only TaskUpdate after the
    /// load is applied instead of dropped.
    #[test]
    fn pc2_3_replay_rebuilds_task_state_and_publishes_plan() {
        let messages = vec![
            json!({"type":"assistant","uuid":"a1","parent_tool_use_id":null,
                "message":{"role":"assistant","content":[
                    {"type":"tool_use","id":"toolu_T1","name":"TaskCreate",
                     "input":{"subject":"First task","description":"D"}}]}}),
            json!({"type":"user","uuid":"u2","parent_tool_use_id":null,
                "message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_T1",
                     "content":{"task":{"id":"t1","subject":"First task","status":"pending"}}}]}}),
            json!({"type":"assistant","uuid":"a2","parent_tool_use_id":null,
                "message":{"role":"assistant","content":[
                    {"type":"tool_use","id":"toolu_T2","name":"TaskUpdate",
                     "input":{"taskId":"t1","status":"in_progress","activeForm":"Working"}}]}}),
            json!({"type":"user","uuid":"u3","parent_tool_use_id":null,
                "message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_T2",
                     "content":{"success":true,"taskId":"t1","updatedFields":["status"]}}]}}),
        ];
        let (notifications, mut task_state, last_published) =
            replay_with_task_state(&messages, ID, false);
        let plans: Vec<&Value> = notifications
            .iter()
            .filter(|n| n["update"]["sessionUpdate"] == json!("plan"))
            .map(|n| &n["update"])
            .collect();
        assert_eq!(plans.len(), 2);
        assert_eq!(
            plans[0]["entries"],
            json!([{"content":"First task","status":"pending","priority":"medium"}])
        );
        assert_eq!(
            plans[1]["entries"],
            json!([{"content":"Working","status":"in_progress","priority":"medium"}])
        );
        assert_eq!(task_state.len(), 1);
        assert_eq!(task_state[0].1.status, "in_progress");
        assert_eq!(last_published, None, "a plain client gets every plan");

        // After seeding the live state, a status-only TaskUpdate applies.
        crate::hooks::apply_task_update(
            &mut task_state,
            &json!({"taskId":"t1","status":"completed"}),
        );
        assert_eq!(task_state[0].1.subject, "First task");
        assert_eq!(task_state[0].1.status, "completed");

        // An AIR client dedupes the repeated plan (Node `changedTaskPlanEntries`).
        let (notifications, _, last_published) = replay_with_task_state(&messages, ID, true);
        let plans = notifications
            .iter()
            .filter(|n| n["update"]["sessionUpdate"] == json!("plan"))
            .count();
        assert_eq!(plans, 2);
        assert!(last_published.is_some());
    }

    /// PC-2.3 — two identical Task* results publish one plan to an AIR client.
    #[test]
    fn pc2_3_air_dedupes_replayed_plans() {
        let messages = vec![
            json!({"type":"assistant","uuid":"a1","parent_tool_use_id":null,
                "message":{"role":"assistant","content":[
                    {"type":"tool_use","id":"toolu_TL","name":"TaskList","input":{}}]}}),
            json!({"type":"user","uuid":"u2","parent_tool_use_id":null,
                "message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_TL",
                     "content":{"tasks":[{"id":"t1","subject":"Only","status":"pending"}]}}]}}),
            json!({"type":"assistant","uuid":"a3","parent_tool_use_id":null,
                "message":{"role":"assistant","content":[
                    {"type":"tool_use","id":"toolu_TL2","name":"TaskList","input":{}}]}}),
            json!({"type":"user","uuid":"u4","parent_tool_use_id":null,
                "message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_TL2",
                     "content":{"tasks":[{"id":"t1","subject":"Only","status":"pending"}]}}]}}),
        ];
        let (notifications, _, _) = replay_with_task_state(&messages, ID, true);
        assert_eq!(
            notifications
                .iter()
                .filter(|n| n["update"]["sessionUpdate"] == json!("plan"))
                .count(),
            1
        );
        let (notifications, _, _) = replay_with_task_state(&messages, ID, false);
        assert_eq!(
            notifications
                .iter()
                .filter(|n| n["update"]["sessionUpdate"] == json!("plan"))
                .count(),
            2
        );
    }

    /// 3.5 — a nested assistant's subagent text/thinking is stripped but its
    /// tool blocks still replay onto the root.
    #[test]
    fn inv_67_subagent_text_stripped() {
        let memsg = json!({
            "type":"assistant","parent_tool_use_id":"toolu_PARENT",
            "session_id":ID,
            "message":{"role":"assistant",
                "content":[
                    {"type":"text","text":"thinking out loud"},
                    {"type":"tool_use","id":"toolu_CHILD","name":"Bash","input":{"command":"ls"}}
                ]}
        });
        let notif = replay_notifications(&[memsg], ID);
        // Only the tool_use -> tool_call replays; the text is stripped.
        assert_eq!(notif.len(), 1);
        assert_eq!(notif[0]["update"]["sessionUpdate"], json!("tool_call"));
        assert_eq!(notif[0]["update"]["name"], json!("Bash"));
    }
    fn limit_message(uuid: &str, text: &str) -> Value {
        json!({"type":"assistant","uuid":uuid,"parent_tool_use_id":null,
            "message":{"role":"assistant","model":"<synthetic>",
                "content":[{"type":"text","text":text}]}})
    }

    fn user_message(uuid: &str, text: &str) -> Value {
        json!({"type":"user","uuid":uuid,"parent_tool_use_id":null,
            "message":{"role":"user","content":[{"type":"text","text":text}]}})
    }

    fn real_answer(uuid: &str) -> Value {
        json!({"type":"assistant","uuid":uuid,"parent_tool_use_id":null,
            "message":{"id":"msg_real","role":"assistant","model":"claude-sonnet-4-5",
                "content":[{"type":"text","text":"fine"}]}})
    }

    /// N-53 / V-15 — a capable client gets each synthetic usage-limit message as
    /// a typed failure at its position, named after the prompt it answered; only
    /// the latest limit with no real answer after it stays active.
    #[test]
    fn t3a_replay_restores_usage_limit_failures_at_their_position() {
        let msgs = vec![
            user_message("u1", "first"),
            limit_message("l1", "You've hit your limit"),
            user_message("u2", "second"),
            limit_message("l2", "You've hit your limit again"),
        ];
        let mut book = crate::session::failures::SessionFailures::default();
        let (notifications, _, _) = replay_with_failures(&msgs, ID, false, false, Some(&mut book));
        let kinds: Vec<&str> = notifications
            .iter()
            .map(|n| n["update"]["sessionUpdate"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            kinds,
            vec![
                "user_message_chunk",
                "session_info_update",
                "user_message_chunk",
                "session_info_update"
            ]
        );
        let sf = |n: &Value| n["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"].clone();
        assert_eq!(sf(&notifications[1])["id"], "u1:error");
        assert_eq!(sf(&notifications[1])["title"], "You've hit your limit");
        assert_eq!(sf(&notifications[3])["id"], "u2:error");
        assert_eq!(book.active().len(), 1, "only the latest limit stays active");
        assert_eq!(book.active()[0].failure.id, "u2:error");
        // A plain client keeps the legacy text.
        let (plain, _, _) = replay_with_failures(&msgs, ID, false, false, None);
        assert!(plain
            .iter()
            .all(|n| n["update"]["sessionUpdate"] != "session_info_update"));
    }

    /// N-53 — a real model answer after the limit proves recovery: the record
    /// replays but is not left active.
    #[test]
    fn t3a_replay_limit_followed_by_a_real_answer_is_not_active() {
        let msgs = vec![
            user_message("u1", "first"),
            limit_message("l1", "You've hit your limit"),
            user_message("u2", "second"),
            real_answer("a2"),
        ];
        let mut book = crate::session::failures::SessionFailures::default();
        let (notifications, _, _) = replay_with_failures(&msgs, ID, false, false, Some(&mut book));
        assert!(notifications
            .iter()
            .any(|n| n["update"]["sessionUpdate"] == "session_info_update"));
        assert!(book.active().is_empty());
    }

    fn exit_plan_history(is_error: bool, parent: Option<&str>) -> Vec<Value> {
        vec![
            json!({"type":"assistant","uuid":"a1","parent_tool_use_id":parent,
                "message":{"id":"msg_p","role":"assistant","model":"claude-sonnet-4-5",
                    "content":[{"type":"tool_use","id":"toolu_P","name":"ExitPlanMode",
                        "input":{"plan":"  step one\nstep two \n"}}]}}),
            json!({"type":"user","uuid":"u1","parent_tool_use_id":parent,
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_P",
                    "is_error":is_error,"content":"approved"}]}}),
        ]
    }

    /// U-24 — replay publishes an approved ExitPlanMode plan (from the tool
    /// input, trimmed) as a markdown plan after the tool's result, for a client
    /// with plan operations; not for one without, a failed result or a subagent.
    #[test]
    fn t3a_replay_publishes_the_approved_plan_for_plan_clients() {
        let kinds = |notifications: &[Value]| -> Vec<String> {
            notifications
                .iter()
                .map(|n| {
                    n["update"]["sessionUpdate"]
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                })
                .collect()
        };
        let (with_ops, _, _) =
            replay_with_failures(&exit_plan_history(false, None), ID, false, true, None);
        let kinds_with = kinds(&with_ops);
        assert_eq!(
            kinds_with.last().map(String::as_str),
            Some("plan_update"),
            "{kinds_with:?}"
        );
        assert_eq!(
            with_ops.last().unwrap()["update"]["plan"],
            json!({"type":"markdown","planId":"plan-mode","content":"step one\nstep two"})
        );
        for (history, plan_ops) in [
            (exit_plan_history(false, None), false),
            (exit_plan_history(true, None), true),
            (exit_plan_history(false, Some("toolu_PARENT")), true),
        ] {
            let (out, _, _) = replay_with_failures(&history, ID, false, plan_ops, None);
            assert!(!kinds(&out).contains(&"plan_update".to_string()));
        }
    }

    /// B-50 (`acp-agent.ts:9403-9422`) — a persisted compact summary replays as
    /// one completed compaction entity at its position for a client that takes
    /// compaction updates, and as legacy text for one that does not.
    #[test]
    fn b50_replay_materializes_a_compact_summary_as_a_completed_entity() {
        let summary = "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\nSummary:\n1. Primary Request and Intent:\n   Count upward.\n\nIf you need specific details from before compaction (like exact code snippets, error messages, or content you generated), read the full transcript at: /tmp/session.jsonl\nContinue the conversation from where it left off without asking the user any further questions. Resume directly — do not acknowledge the summary.";
        let msgs = vec![
            user_message("prompt-uuid", "count"),
            json!({"type":"user","uuid":"summary-uuid","isCompactSummary":true,
                "message":{"role":"user","content":[{"type":"text","text":summary}]}}),
            json!({"type":"assistant","uuid":"a-after",
                "message":{"id":"msg_after","role":"assistant","content":[{"type":"text","text":"4"}]}}),
        ];
        let (out, _, _) = replay_session(
            &msgs,
            ID,
            false,
            false,
            ReplaySinks {
                compaction: true,
                ..ReplaySinks::default()
            },
        );
        let kinds: Vec<&str> = out
            .iter()
            .map(|n| n["update"]["sessionUpdate"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(
            kinds,
            vec![
                "user_message_chunk",
                "compaction_update",
                "agent_message_chunk"
            ],
            "{out:?}"
        );
        assert_eq!(
            out[1]["update"],
            json!({"sessionUpdate":"compaction_update","compactionId":"summary-uuid",
                "status":"completed","summary":[{"type":"text",
                    "text":"1. Primary Request and Intent:\n   Count upward."}]})
        );

        let (legacy, _, _) = replay_session(&msgs, ID, false, false, ReplaySinks::default());
        assert!(legacy
            .iter()
            .all(|n| n["update"]["sessionUpdate"] != "compaction_update"));
        assert_eq!(
            legacy[1]["update"]["sessionUpdate"],
            json!("user_message_chunk")
        );
    }

    fn notification_text(task_id: &str, status: &str, tool_use_id: &str) -> String {
        format!(
            "<task-notification><task-id>{task_id}</task-id><status>{status}</status><tool-use-id>{tool_use_id}</tool-use-id><output-file>/tmp/{task_id}.output</output-file></task-notification>"
        )
    }

    /// B-45 — replay skips a task-notification record but restores the card of
    /// the background task its tool call started (`restoreTaskNotification`,
    /// `acp-agent.ts:9257-9284`).
    #[test]
    fn b45_replay_restores_a_background_task_card() {
        let messages = vec![
            json!({"type":"assistant","uuid":"a1","parentUuid":null,
                "parent_tool_use_id":null,"sessionId":ID,
                "message":{"id":"m1","role":"assistant","content":[
                    {"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"sleep 5"}}]}}),
            json!({"type":"user","uuid":"u2","parentUuid":"a1",
                "parent_tool_use_id":null,"sessionId":ID,
                "message":{"role":"user","content":[
                    {"type":"text","text":notification_text("t1", "completed", "toolu_1")}]}}),
        ];
        let mut tasks = crate::async_tasks::AsyncTaskRuntime::new(true);
        let (out, _, _) = replay_session(
            &messages,
            ID,
            true,
            false,
            ReplaySinks {
                async_tasks: Some(&mut tasks),
                ..ReplaySinks::default()
            },
        );
        let spawned: Vec<&Value> = out
            .iter()
            .filter(|n| n["update"]["sessionUpdate"] == "async_task_spawned")
            .collect();
        assert_eq!(spawned.len(), 1, "{out:?}");
        assert_eq!(spawned[0]["update"]["asyncTaskId"], "t1");
        assert_eq!(spawned[0]["update"]["toolCallId"], "toolu_1");
        let finished: Vec<&Value> = out
            .iter()
            .filter(|n| n["update"]["sessionUpdate"] == "async_task_state_update")
            .collect();
        assert_eq!(finished.len(), 1, "{out:?}");
        assert_eq!(finished[0]["update"]["state"], "completed");
        // The record itself is not replayed as a user prompt.
        assert!(
            !out.iter()
                .any(|n| n["update"]["sessionUpdate"] == "user_message_chunk"),
            "{out:?}"
        );
    }

    /// B-45 — a notification of a native subagent launch is the child's state,
    /// not an async task, so no task card is restored.
    #[test]
    fn b45_native_subagent_notification_restores_no_async_task() {
        let messages = vec![
            json!({"type":"assistant","uuid":"a1","parentUuid":null,
                "parent_tool_use_id":null,"sessionId":ID,
                "message":{"id":"m1","role":"assistant","content":[
                    {"type":"tool_use","id":"toolu_1","name":"Agent","input":{"prompt":"x"}}]}}),
            json!({"type":"user","uuid":"u2","parentUuid":"a1",
                "parent_tool_use_id":null,"sessionId":ID,
                "message":{"role":"user","content":[
                    {"type":"text","text":notification_text("t1", "completed", "toolu_1")}]}}),
        ];
        let mut tasks = crate::async_tasks::AsyncTaskRuntime::new(true);
        let (out, _, _) = replay_session(
            &messages,
            ID,
            true,
            false,
            ReplaySinks {
                async_tasks: Some(&mut tasks),
                ..ReplaySinks::default()
            },
        );
        assert!(
            !out.iter()
                .any(|n| n["update"]["sessionUpdate"] == "async_task_spawned"),
            "{out:?}"
        );
    }

    /// T8A — every `stripLocalCommandMetadata` case of the upstream suite:
    /// marker-only strings, reconstructed model-bound skill prompts, hidden
    /// local-command output, real prose, injected reminders, persisted task
    /// notifications, mixed arrays and non-container shapes.
    #[test]
    fn t8a_strip_local_command_metadata_cases() {
        let strip = |content: Value| strip_local_command_metadata(content);
        let text = |s: &str| strip(Value::String(s.to_string()));

        // Pure marker metadata -> null.
        assert!(text("<command-name>/model</command-name>").is_none());
        assert!(text("<local-command-stdout>out</local-command-stdout>").is_none());
        assert!(text("<local-command-stderr>err</local-command-stderr>").is_none());
        assert!(text(
            "<command-name>/model</command-name>\n            <command-message>model</command-message>\n            <command-args>opus</command-args>"
        )
        .is_none());

        // A marker-only model-bound slash skill prompt is reconstructed.
        let skill = "<command-message>example-skill</command-message>\n<command-name>/example-skill</command-name>\n<command-args>explain the startup decision</command-args>";
        assert_eq!(
            text(skill),
            Some(json!("/example-skill explain the startup decision"))
        );
        assert_eq!(
            strip(json!([{"type": "text", "text": skill}])),
            Some(json!([{"type": "text", "text": "/example-skill explain the startup decision"}]))
        );

        // A local-command stdout payload keeps the message hidden.
        assert!(text(
            "<command-name>/example-skill</command-name><command-args>explain</command-args><local-command-stdout>handled locally</local-command-stdout>"
        )
        .is_none());

        // Real content is unchanged.
        assert_eq!(text("hi"), Some(json!("hi")));
        assert_eq!(
            text("please run /model with args"),
            Some(json!("please run /model with args"))
        );

        // Mixed marker tags keep the real prose.
        let mixed = "<command-name>/model</command-name>\n            <command-message>model</command-message>\n            <command-args>opus</command-args><local-command-stdout>Set model to opus (claude-opus-4-7)</local-command-stdout><command-name>/model</command-name>\n            <command-message>model</command-message>\n            <command-args>opus[1m]</command-args><local-command-stdout>Set model to opus[1m] (claude-opus-4-7[1m])</local-command-stdout>hi";
        let stripped = text(mixed).expect("the prose survives");
        let stripped = stripped.as_str().unwrap_or("");
        assert!(!stripped.contains("<command-name>"));
        assert!(!stripped.contains("<command-message>"));
        assert!(!stripped.contains("<command-args>"));
        assert!(!stripped.contains("<local-command-stdout>"));
        assert!(stripped.trim_end().ends_with("hi"));

        // An injected reminder is stripped, keeping the user's prose.
        assert_eq!(
            text("is that right?<system-reminder>be nice</system-reminder>"),
            Some(json!("is that right?"))
        );
        assert!(text("<system-reminder>do not mention this</system-reminder>").is_none());

        // A persisted task notification is stripped, keeping the prose.
        assert_eq!(
            text("<task-notification>\n<task-id>bg1</task-id>\n</task-notification>\nwhat now?"),
            Some(json!("\nwhat now?"))
        );
        assert!(text("<task-notification><task-id>bg1</task-id></task-notification>").is_none());

        // Arrays keep only the real blocks.
        assert_eq!(
            strip(json!([
                {"type": "text", "text": "is that right?"},
                {"type": "text", "text": "<system-reminder>be nice</system-reminder>"}
            ])),
            Some(json!([{"type": "text", "text": "is that right?"}]))
        );
        assert_eq!(
            strip(json!([
                {"type": "text", "text": "<command-name>/model</command-name>"},
                {"type": "text", "text": "<local-command-stdout>ok</local-command-stdout>"},
                {"type": "text", "text": "hi"}
            ])),
            Some(json!([{"type": "text", "text": "hi"}]))
        );
        assert!(strip(json!([
            {"type": "text", "text": "<command-name>/model</command-name>"},
            {"type": "text", "text": "<local-command-stdout>ok</local-command-stdout>"}
        ]))
        .is_none());
        assert_eq!(
            strip(json!([{
                "type": "text",
                "text": "<command-name>/model</command-name><local-command-stdout>ok</local-command-stdout>hi"
            }])),
            Some(json!([{"type": "text", "text": "hi"}]))
        );

        // A non-text block is left alone.
        let image = json!({"type": "image", "source": {"type": "base64", "data": "", "media_type": "image/png"}});
        assert_eq!(
            strip(json!([
                {"type": "text", "text": "<command-name>/model</command-name>"},
                image
            ])),
            Some(json!([image]))
        );

        // Null, non-container shapes.
        assert_eq!(strip(Value::Null), Some(Value::Null));
        let object = json!({"arbitrary": "object"});
        assert_eq!(strip(object.clone()), Some(object));
    }

    /// T8A — `isLocalCommandMetadata` is `stripLocalCommandMetadata === null`.
    #[test]
    fn t8a_is_local_command_metadata() {
        let is_marker = |content: Value| strip_local_command_metadata(content).is_none();
        assert!(is_marker(json!("<command-name>/model</command-name>")));
        assert!(is_marker(json!([
            {"type": "text", "text": "<command-name>/model</command-name>"}
        ])));
        assert!(!is_marker(json!("hi")));
        assert!(!is_marker(json!("<command-name>/model</command-name>hi")));
        assert!(!is_marker(json!([
            {"type": "text", "text": "<command-name>/model</command-name>"},
            {"type": "text", "text": "hi"}
        ])));
    }

    /// T8A — a replay hands the client what the user typed, without the
    /// `<system-reminder>` the CLI appended.
    #[test]
    fn t8a_replay_strips_the_appended_reminder() {
        let message = json!({
            "type": "user", "uuid": "u1", "parentUuid": null,
            "parent_tool_use_id": null, "sessionId": ID,
            "message": {"role": "user", "content":
                "is that right?<system-reminder>Do not mention this reminder to the user.</system-reminder>"}
        });
        let updates = replay_notifications(&[message], ID);
        let chunks: Vec<&str> = updates
            .iter()
            .filter(|n| n["update"]["sessionUpdate"] == "user_message_chunk")
            .filter_map(|n| n["update"]["content"]["text"].as_str())
            .collect();
        assert_eq!(chunks, vec!["is that right?"]);
        assert!(!serde_json::to_string(&updates)
            .unwrap()
            .contains("system-reminder"));
    }

    /// T8A — replay skips the CLI's synthetic login reply but keeps the prompt.
    #[test]
    fn t8a_replay_skips_the_synthetic_login_message() {
        let messages = vec![
            user(
                "u1",
                None,
                json!([{"type": "text", "text": "hi, say one word"}]),
                "/p",
            ),
            json!({
                "type": "assistant", "uuid": "a1", "parentUuid": "u1",
                "parent_tool_use_id": null, "sessionId": ID,
                "message": {"model": "<synthetic>", "content": [
                    {"type": "text", "text": "Please run /login"}
                ]}
            }),
        ];
        let updates = replay_notifications(&messages, ID);
        let chunks: Vec<&str> = updates
            .iter()
            .filter(|n| n["update"]["sessionUpdate"] == "user_message_chunk")
            .filter_map(|n| n["update"]["content"]["text"].as_str())
            .collect();
        assert_eq!(chunks, vec!["hi, say one word"]);
        let serialized = serde_json::to_string(&updates).unwrap();
        assert!(!serialized.contains("Please run /login"));
        assert!(!serialized.contains("/login"));
    }

    // ----- the task-notification replay fixture of the upstream suite -------

    fn t8a_notification_text(status: &str, tool_use_id: &str) -> String {
        format!(
            "<task-notification>\n<task-id>bg1</task-id>\n<tool-use-id>{tool_use_id}</tool-use-id>\n<output-file>/tmp/tasks/bg1.output</output-file>\n<status>{status}</status>\n<summary>Background command \"npm test\" finished</summary>\n<note>A task-notification fires each time the task stops.</note>\n<result><status>failed</status></result>\n</task-notification>"
        )
    }

    fn t8a_notification(status: &str, extra: Value) -> Value {
        let mut record = json!({
            "type": "user", "uuid": format!("notification-{status}"),
            "parentUuid": "u2", "parent_tool_use_id": null, "sessionId": ID,
            "message": {"role": "user", "content": t8a_notification_text(status, "toolu_bash")},
            "origin": {"kind": "task-notification"},
        });
        if let Value::Object(extra) = extra {
            for (key, value) in extra {
                record[key] = value;
            }
        }
        record
    }

    fn t8a_bash_launch(tool: &str) -> Vec<Value> {
        vec![
            json!({"type": "user", "uuid": "u1", "parentUuid": null,
                "parent_tool_use_id": null, "sessionId": ID,
                "message": {"role": "user", "content": "run the tests"}}),
            json!({"type": "assistant", "uuid": "a1", "parentUuid": "u1",
                "parent_tool_use_id": null, "sessionId": ID,
                "message": {"id": "m1", "role": "assistant", "model": "claude-opus-5",
                    "content": [{"type": tool, "id": "toolu_bash", "name": "Bash",
                        "input": {"command": "npm test", "run_in_background": true}}]}}),
            json!({"type": "user", "uuid": "u2", "parentUuid": "a1",
                "parent_tool_use_id": null, "sessionId": ID,
                "message": {"role": "user", "content": [{"type": "tool_result",
                    "tool_use_id": "toolu_bash",
                    "content": "Command running in background with ID: bg1."}]}}),
        ]
    }

    fn t8a_replay_with_async(messages: &[Value]) -> Vec<Value> {
        let mut tasks = crate::async_tasks::AsyncTaskRuntime::new(true);
        let (out, _, _) = replay_session(
            messages,
            ID,
            true,
            false,
            ReplaySinks {
                async_tasks: Some(&mut tasks),
                ..ReplaySinks::default()
            },
        );
        out
    }

    fn t8a_async_updates(out: &[Value]) -> Vec<Value> {
        out.iter()
            .filter(|n| {
                n["update"]["sessionUpdate"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("async_task_"))
            })
            .cloned()
            .collect()
    }

    /// T8A — an origin-less notification is detected by its text, and `killed`
    /// maps to `stopped`.
    #[test]
    fn t8a_replay_detects_an_originless_notification_and_maps_killed_to_stopped() {
        let mut messages = t8a_bash_launch("tool_use");
        let mut notification = t8a_notification("killed", json!({}));
        notification.as_object_mut().unwrap().remove("origin");
        messages.push(notification);
        let out = t8a_replay_with_async(&messages);
        let tasks = t8a_async_updates(&out);
        assert_eq!(
            tasks.last().unwrap()["update"]["state"],
            "stopped",
            "{tasks:?}"
        );
        let chunks: Vec<&str> = out
            .iter()
            .filter(|n| n["update"]["sessionUpdate"] == "user_message_chunk")
            .filter_map(|n| n["update"]["content"]["text"].as_str())
            .collect();
        assert_eq!(chunks, vec!["run the tests"]);
    }

    /// T8A — a background task launched by a `server_tool_use` or
    /// `mcp_tool_use` block is restored like a `tool_use` launch.
    #[test]
    fn t8a_replay_restores_server_and_mcp_tool_use_launches() {
        for block_type in ["server_tool_use", "mcp_tool_use"] {
            let mut messages = t8a_bash_launch(block_type);
            messages.push(t8a_notification("completed", json!({})));
            let out = t8a_replay_with_async(&messages);
            let tasks = t8a_async_updates(&out);
            let kinds: Vec<&str> = tasks
                .iter()
                .map(|n| n["update"]["sessionUpdate"].as_str().unwrap_or(""))
                .collect();
            assert_eq!(
                kinds,
                vec!["async_task_spawned", "async_task_state_update"],
                "{block_type}"
            );
            assert_eq!(tasks.last().unwrap()["update"]["state"], "completed");
        }
    }

    /// T8A — the user's text next to an embedded notification replays, and the
    /// notification is hidden.
    #[test]
    fn t8a_replay_keeps_user_text_next_to_an_embedded_notification() {
        let mut messages = t8a_bash_launch("tool_use");
        messages.push(json!({
            "type": "user", "uuid": "u3", "parentUuid": "u2",
            "parent_tool_use_id": null, "sessionId": ID,
            "message": {"role": "user", "content": [
                {"type": "text", "text": t8a_notification_text("failed", "toolu_bash")},
                {"type": "text", "text": "why did it fail?"}
            ]}
        }));
        let out = t8a_replay_with_async(&messages);
        let chunks: Vec<&str> = out
            .iter()
            .filter(|n| n["update"]["sessionUpdate"] == "user_message_chunk")
            .filter_map(|n| n["update"]["content"]["text"].as_str())
            .collect();
        assert_eq!(chunks, vec!["run the tests", "why did it fail?"]);
        assert!(!serde_json::to_string(&out)
            .unwrap()
            .contains("task-notification"));
        assert_eq!(
            t8a_async_updates(&out).last().unwrap()["update"]["state"],
            "failed"
        );
    }

    /// T8A — a notification of a task no tool call started restores nothing.
    #[test]
    fn t8a_replay_ignores_a_notification_no_tool_started() {
        let messages = vec![
            json!({"type": "user", "uuid": "u1", "parentUuid": null,
                "parent_tool_use_id": null, "sessionId": ID,
                "message": {"role": "user", "content": "hi"}}),
            t8a_notification("completed", json!({})),
        ];
        let out = t8a_replay_with_async(&messages);
        assert!(t8a_async_updates(&out).is_empty());
        let chunks: Vec<&str> = out
            .iter()
            .filter(|n| n["update"]["sessionUpdate"] == "user_message_chunk")
            .filter_map(|n| n["update"]["content"]["text"].as_str())
            .collect();
        assert_eq!(chunks, vec!["hi"]);
    }

    /// T8A — the output path of a notification without one comes from the tool
    /// result that moved the command to the background.
    #[test]
    fn t8a_replay_takes_the_output_path_from_the_tool_result() {
        let timeout_result = json!({
            "type": "user", "uuid": "u2b", "parentUuid": "a1",
            "parent_tool_use_id": null, "sessionId": ID,
            "message": {"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "toolu_bash",
                "content": "Command did not complete within its 120s timeout and was moved to the background (ID: bg1). Output is being written to: /tmp/claude/tasks/bg1.output."
            }]}
        });
        let bare = json!({
            "type": "user", "uuid": "notification-bare", "parentUuid": "u2b",
            "parent_tool_use_id": null, "sessionId": ID,
            "message": {"role": "user", "content":
                "<task-notification>\n<task-id>bg1</task-id>\n<tool-use-id>toolu_bash</tool-use-id>\n<status>completed</status>\n</task-notification>"},
            "origin": {"kind": "task-notification"},
        });
        let launch = t8a_bash_launch("tool_use");
        let messages = vec![launch[0].clone(), launch[1].clone(), timeout_result, bare];
        let out = t8a_replay_with_async(&messages);
        let spawned = t8a_async_updates(&out)
            .into_iter()
            .find(|n| n["update"]["sessionUpdate"] == "async_task_spawned")
            .expect("the task is announced");
        assert_eq!(spawned["update"]["asyncTaskId"], "bg1");
        assert_eq!(
            spawned["update"]["outputFilePath"],
            "/tmp/claude/tasks/bg1.output"
        );
    }

    /// T8A — a repeated notification sends one terminal state.
    #[test]
    fn t8a_replay_sends_one_terminal_state_for_a_repeated_notification() {
        let mut messages = t8a_bash_launch("tool_use");
        messages.push(t8a_notification("completed", json!({})));
        messages.push(t8a_notification(
            "completed",
            json!({"uuid": "notification-again"}),
        ));
        let out = t8a_replay_with_async(&messages);
        let tasks = t8a_async_updates(&out);
        let kinds: Vec<&str> = tasks
            .iter()
            .map(|n| n["update"]["sessionUpdate"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(kinds, vec!["async_task_spawned", "async_task_state_update"]);
    }

    /// T8A — a task-notification delivery with a subkind is a prompt, not a
    /// hidden notification.
    #[test]
    fn t8a_replay_keeps_a_subkind_task_notification_as_a_prompt() {
        let messages = vec![json!({
            "type": "user", "uuid": "u1", "parentUuid": null,
            "parent_tool_use_id": null, "sessionId": ID,
            "message": {"role": "user", "content": "Run the nightly report"},
            "origin": {"kind": "task-notification", "subkind": "scheduled-trigger"},
        })];
        let out = replay_notifications(&messages, ID);
        let chunks: Vec<&str> = out
            .iter()
            .filter(|n| n["update"]["sessionUpdate"] == "user_message_chunk")
            .filter_map(|n| n["update"]["content"]["text"].as_str())
            .collect();
        assert_eq!(chunks, vec!["Run the nightly report"]);
    }

    /// T8A — `isReplayHiddenMetaMessage` matches only meta user messages with an
    /// inter-agent origin.
    #[test]
    fn t8a_hidden_meta_predicate_matches_inter_agent_origins() {
        let peer = json!({
            "type": "user", "uuid": "peer-1", "session_id": "s1",
            "is_meta": true, "origin": {"kind": "peer"},
            "message": {"role": "user", "content": "framed"},
        });
        assert!(is_hidden_meta_message(&peer));
        for kind in ["channel", "observer", "observer-activity", "slack-ping"] {
            let mut message = peer.clone();
            message["origin"] = json!({"kind": kind});
            assert!(is_hidden_meta_message(&message), "{kind}");
        }
        let mut not_meta = peer.clone();
        not_meta.as_object_mut().unwrap().remove("is_meta");
        assert!(!is_hidden_meta_message(&not_meta));
        let mut no_origin = peer.clone();
        no_origin.as_object_mut().unwrap().remove("origin");
        no_origin["isCompactSummary"] = json!(true);
        assert!(!is_hidden_meta_message(&no_origin));
        let mut human = peer.clone();
        human["origin"] = json!({"kind": "human"});
        assert!(!is_hidden_meta_message(&human));
        let mut assistant = peer.clone();
        assistant["type"] = json!("assistant");
        assert!(!is_hidden_meta_message(&assistant));
    }

    /// T8A — replay skips the inter-agent meta messages but keeps the user's own
    /// prompt.
    #[test]
    fn t8a_replay_skips_inter_agent_meta_messages() {
        let peer = json!({
            "type": "user", "uuid": "peer-1", "parentUuid": null,
            "parent_tool_use_id": null, "sessionId": ID, "is_meta": true,
            "origin": {"kind": "peer", "from": "uds:/tmp/fake.sock", "hostInjected": true},
            "message": {"role": "user", "content":
                "Another Claude session sent a message:\n<cross-session-message from=\"uds:/tmp/fake.sock\">enveloped hi</cross-session-message>\n\nThis came from another Claude session — not typed by your user."},
        });
        let own = user("u1", Some("peer-1"), json!("my own prompt"), "/p");
        let out = replay_notifications(&[peer, own], ID);
        let chunks: Vec<&str> = out
            .iter()
            .filter(|n| n["update"]["sessionUpdate"] == "user_message_chunk")
            .filter_map(|n| n["update"]["content"]["text"].as_str())
            .collect();
        assert_eq!(chunks, vec!["my own prompt"]);
    }

    /// T8A — the SDK's parser sanitizes a raw usage-limit transcript (drops the
    /// `error` field) and the adapter still recognizes it through the parsed
    /// shape, replaying the typed failure.
    #[test]
    fn t8a_sanitized_raw_transcript_is_recognized_through_the_parser() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../src/tests/fixtures/usage-limit-session.jsonl");
        let raw = std::fs::read_to_string(&path).expect("the upstream fixture");
        let entries: Vec<Value> = raw
            .trim()
            .lines()
            .map(|line| serde_json::from_str(line).expect("jsonl"))
            .collect();
        assert_eq!(entries[1]["error"], json!("rate_limit"));
        assert_eq!(entries[1]["isApiErrorMessage"], json!(true));

        let (tmp, env) = TempProjects::new(&raw);
        let messages = session_messages(&env, ID);
        let parsed = messages
            .iter()
            .find(|message| message["uuid"] == entries[1]["uuid"])
            .expect("the limit message survives the SDK parser");
        assert!(
            parsed.get("error").is_none(),
            "the sanitized shape drops the error field: {parsed}"
        );

        let mut book = crate::session::failures::SessionFailures::default();
        let (notifications, _, _) =
            replay_with_failures(&messages, ID, false, false, Some(&mut book));
        let failure = notifications
            .iter()
            .find_map(|n| n["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"].as_object())
            .expect("the usage limit replays as a typed failure");
        assert!(
            failure["title"]
                .as_str()
                .is_some_and(|title| title.contains("individual spend limit")),
            "{failure:?}"
        );
        drop(tmp);
    }

    /// B-48 — a marker-only command without `<command-args>` is not dropped:
    /// upstream returns the command name (`acp-agent.ts:2187-2188`).
    #[test]
    fn b48_marker_only_command_without_args_is_kept() {
        let marker = |text: &str| {
            json!({"type":"user","message":{"role":"user","content":[
                {"type":"text","text":text}]}})
        };
        let no_args = replay_notifications(&[marker("<command-name>/review</command-name>")], ID);
        assert_eq!(no_args.len(), 1);
        assert_eq!(no_args[0]["update"]["content"]["text"], json!("/review"));
        let with_args = replay_notifications(
            &[marker(
                "<command-name>/review</command-name><command-args>123</command-args>",
            )],
            ID,
        );
        assert_eq!(
            with_args[0]["update"]["content"]["text"],
            json!("/review 123")
        );
        // A hidden local command is still dropped.
        assert!(
            replay_notifications(&[marker("<command-name>/model</command-name>")], ID).is_empty()
        );
    }

    /// B-54 — the replay backfills the ACP messageId -> SDK uuid map, so a
    /// resumed/loaded session can resolve rewind/fork ids without another
    /// `getSessionMessages` read (`acp-agent.ts:9306-9314`).
    #[test]
    fn b54_replay_backfills_message_ids() {
        let messages = vec![
            json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":ID,
                "message":{"role":"user","content":"hi"}}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":ID,
                "message":{"id":"m1","role":"assistant",
                    "content":[{"type":"text","text":"ok"}]}}),
        ];
        let mut ids = crate::message_ids::MessageIdMap::default();
        let _ = replay_session(
            &messages,
            ID,
            false,
            false,
            ReplaySinks {
                message_ids: Some(&mut ids),
                ..ReplaySinks::default()
            },
        );
        assert_eq!(ids.get("m1"), Some("a1"));
        assert_eq!(ids.get("u1"), Some("u1"));
    }

    /// B-53 — the load-replay-end marker is one `session_info_update` with only
    /// the AIR `loadReplayEnd` payload (`acp-agent.ts:9529-9544`).
    #[test]
    fn b53_load_replay_end_marker_shape() {
        let marker = load_replay_end_marker(ID);
        assert_eq!(marker["sessionId"], ID);
        assert_eq!(marker["update"]["sessionUpdate"], "session_info_update");
        assert!(marker["update"].get("title").is_none());
        assert!(marker["update"].get("updatedAt").is_none());
        assert_eq!(
            marker["update"]["_meta"]["jetbrains"]["air"],
            json!({"version": 1, "loadReplayEnd": true})
        );
    }

    /// B-56 — a delivered queued command becomes the user prompt the client
    /// saw live (`nR`/`kwe`/`Pwe`), and the record carries `isQueuedCommand`.
    #[test]
    fn b56_delivered_queued_command_becomes_a_user_prompt() {
        let body = format!(
            "{}\n{}\n{}\n{}\n",
            line(
                json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":ID,
                "message":{"role":"user","content":"hi"}})
            ),
            line(
                json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":ID,
                "message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"one"}]}})
            ),
            line(json!({"type":"queue-operation","operation":"remove",
                "reason":"absorbed_mid_turn","deliveryId":"d1","sessionId":ID})),
            line(
                json!({"type":"attachment","uuid":"q1","parentUuid":"a1","sessionId":ID,
                "attachment":{"type":"queued_command","prompt":"queued text",
                    "delivery_id":"d1"}})
            ),
        );
        let (_tmp, env) = TempProjects::new(&body);
        let msgs = session_messages(&env, ID);
        let queued: Vec<&Value> = msgs
            .iter()
            .filter(|m| m["isQueuedCommand"] == json!(true))
            .collect();
        assert_eq!(queued.len(), 1, "{msgs:?}");
        assert_eq!(queued[0]["type"], "user");
        assert_eq!(queued[0]["uuid"], "q1");
        assert_eq!(queued[0]["message"]["content"], "queued text");
        // The chain is unchanged; the queued prompt follows its parent.
        let uuids: Vec<&str> = msgs
            .iter()
            .filter_map(|m| m.get("uuid").and_then(Value::as_str))
            .collect();
        assert_eq!(uuids, vec!["u1", "a1", "q1"]);
    }

    /// B-56 — a queued command on an off-chain branch that is not an ancestor
    /// of another turn is walked in (`Dwe`).
    #[test]
    fn b56_branch_turn_walk_finds_trailing_queued_command() {
        let body = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            line(user("u1", None, json!("hi"), "/p")),
            line(assistant(
                "a1",
                Some("u1"),
                json!([{"type":"text","text":"one"}])
            )),
            line(
                json!({"type":"attachment","uuid":"q1","parentUuid":"a2","sessionId":ID,
                "attachment":{"type":"queued_command","prompt":"off-chain"}})
            ),
            line(user("u2", Some("a1"), json!("next"), "/p")),
            line(assistant(
                "a2",
                Some("u2"),
                json!([{"type":"text","text":"two"}])
            )),
        );
        let (_tmp, env) = TempProjects::new(&body);
        let msgs = session_messages(&env, ID);
        let uuids: Vec<&str> = msgs
            .iter()
            .filter_map(|m| m.get("uuid").and_then(Value::as_str))
            .collect();
        // q1 branches off the current leaf and is not an ancestor of another
        // turn, so the walk keeps it and the trailing-id rule converts it.
        assert_eq!(uuids, vec!["u1", "a1", "u2", "a2", "q1"]);
        assert_eq!(msgs[4]["isQueuedCommand"], json!(true));
        assert_eq!(msgs[4]["message"]["content"], "off-chain");
    }

    /// B-56 — the SDK output shape carries the optional flags (`rR`), not the
    /// whole transcript record.
    #[test]
    fn b56_sdk_output_shape_carries_optional_flags() {
        let body = format!(
            "{}\n",
            line(
                json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":ID,
                "interruptedByShutdown":true,"toolDenialUnanswered":"stream-closed",
                "origin":{"kind":"task-notification","runId":"run-1"},
                "message":{"role":"user","content":"stopped"}})
            ),
        );
        let (_tmp, env) = TempProjects::new(&body);
        let msgs = session_messages(&env, ID);
        assert_eq!(msgs.len(), 1);
        let message = &msgs[0];
        assert_eq!(message["interruptedByShutdown"], json!(true));
        assert_eq!(message["toolDenialUnanswered"], json!("stream-closed"));
        assert_eq!(
            message["origin"],
            json!({"kind":"task-notification","runId":"run-1"})
        );
        assert!(message.get("parentUuid").is_none());
        assert!(message.get("isSidechain").is_none());
    }

    /// T6FB2: a temporary `subagents` directory that the fixture removes.
    struct TempSubagents(std::path::PathBuf);

    impl Drop for TempSubagents {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A `SubagentReplayContext` over a fresh directory, with one agent
    /// transcript per `(agent id, launch id, background, records)`.
    fn subagents_fixture(
        tag: &str,
        agents: &[(&str, &str, bool, Vec<Value>)],
        notices: bool,
    ) -> (TempSubagents, crate::subagent_replay::SubagentReplayContext) {
        let dir = std::env::temp_dir().join(format!("t6fb2-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut history = crate::subagent_history::SubagentHistory {
            subagents_dir: Some(dir.clone()),
            ..Default::default()
        };
        for (agent_id, launch_id, background, records) in agents {
            history.agents.insert(
                (*launch_id).to_string(),
                crate::subagent_history::SubagentRecord {
                    agent_id: (*agent_id).to_string(),
                    name: Some("scout".into()),
                    agent_type: Some("Explore".into()),
                    description: Some("Work".into()),
                    background: *background,
                    stopped_by_user: false,
                    tool_use_id: Some((*launch_id).to_string()),
                    parent_agent_id: None,
                },
            );
            std::fs::write(
                dir.join(format!("agent-{agent_id}.meta.json")),
                json!({
                    "toolUseId": *launch_id,
                    "name": "scout",
                    "agentType": "Explore",
                    "description": "Work",
                })
                .to_string(),
            )
            .unwrap();
            let mut parent: Option<String> = None;
            let mut body = String::new();
            for record in records {
                let mut record = record.clone();
                if let Some(object) = record.as_object_mut() {
                    object.insert("parentUuid".into(), json!(parent));
                    object.insert("isSidechain".into(), json!(true));
                    object.insert("agentId".into(), json!(agent_id));
                }
                parent = record
                    .get("uuid")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                body.push_str(&record.to_string());
                body.push('\n');
            }
            std::fs::write(dir.join(format!("agent-{agent_id}.jsonl")), body).unwrap();
        }
        (
            TempSubagents(dir),
            crate::subagent_replay::SubagentReplayContext {
                history,
                notices,
                logger: None,
            },
        )
    }

    fn launch_message(uuid: &str, parent: Option<&str>, launch_id: &str, prompt: &str) -> Value {
        assistant(
            uuid,
            parent,
            json!([{
                "type": "tool_use",
                "id": launch_id,
                "name": "Agent",
                "input": {"name": "scout", "subagent_type": "Explore",
                    "description": "Work", "prompt": prompt},
            }]),
        )
    }

    fn child_user(uuid: &str, text: &str) -> Value {
        user(uuid, None, json!(text), "/p")
    }

    fn child_assistant(uuid: &str, parent: Option<&str>, text: &str) -> Value {
        assistant(uuid, parent, json!([{"type": "text", "text": text}]))
    }

    /// T6FB2 HIGH 1: a persisted task notification of a background child ends
    /// it with the persisted state, not `idle` (`acp-agent.ts:9261`).
    #[test]
    fn a_background_child_ends_with_its_task_notification() {
        for (status, expected) in [
            (
                "completed",
                json!({"state": "idle", "stopReason": "end_turn"}),
            ),
            ("failed", json!({"state": "idle", "stopReason": "error"})),
        ] {
            let child = vec![
                child_user("c1", "Work"),
                child_assistant("c2", Some("c1"), "Done"),
            ];
            let (_tmp, context) = subagents_fixture(
                &format!("notify-{status}"),
                &[("bg1", "toolu_bg", true, child)],
                false,
            );
            let notification = format!(
                "<task-notification>\n<task-id>bg1</task-id>\n<status>{status}</status>\n</task-notification>"
            );
            let messages = vec![
                user("u1", None, json!("Go"), "/p"),
                launch_message("a1", Some("u1"), "toolu_bg", "Work"),
                json!({
                    "type": "user", "uuid": "u2", "parentUuid": "a1", "sessionId": ID,
                    "origin": {"kind": "task-notification"},
                    "message": {"role": "user", "content": [{"type": "text", "text": notification}]},
                }),
            ];
            let (out, _, _) = replay_session_with_subagents(
                &messages,
                ID,
                false,
                false,
                ReplaySinks::default(),
                Some(context),
            );
            let end = out
                .iter()
                .rfind(|value| {
                    value["update"]["sessionUpdate"] == "subagent_update"
                        && value["update"]["sessionId"] == "bg1"
                })
                .expect("the background child ends");
            assert_eq!(end["sessionId"], ID);
            assert_eq!(end["update"]["state"], expected);
            // The notification is not also replayed as a task card.
            assert!(
                !out.iter()
                    .any(|value| value["update"]["sessionUpdate"] == "task_notification"),
                "{out:?}"
            );
        }
    }

    /// T6FB2 HIGH 2: the child replay recurses like `replayMessage` on a child
    /// (`acp-agent.ts:9479-9493`): A launches B, B launches C; all announced,
    /// all histories sent, all ended, C before B.
    #[test]
    fn a_nested_launch_is_replayed_before_its_parent_ends() {
        let c_records = vec![
            child_user("g1", "C work"),
            child_assistant("g2", Some("g1"), "C report"),
        ];
        let b_records = vec![
            child_user("b1", "B work"),
            launch_message("b2", Some("b1"), "toolu_c", "C work"),
            child_assistant("b3", Some("b2"), "B report"),
        ];
        let (_tmp, context) = subagents_fixture(
            "nested",
            &[
                ("agent-b", "toolu_b", false, b_records),
                ("agent-c", "toolu_c", false, c_records),
            ],
            false,
        );
        let messages = vec![
            user("u1", None, json!("Go"), "/p"),
            launch_message("a1", Some("u1"), "toolu_b", "B work"),
        ];
        let (out, _, _) = replay_session_with_subagents(
            &messages,
            ID,
            false,
            false,
            ReplaySinks::default(),
            Some(context),
        );
        let index = |predicate: &dyn Fn(&Value) -> bool| -> usize {
            out.iter().position(predicate).expect("present")
        };
        let announce_b = index(&|value| {
            value["sessionId"] == ID
                && value["update"]["sessionUpdate"] == "subagent_update"
                && value["update"]["sessionId"] == "agent-b"
                && value["update"]["state"]["state"] == "running"
        });
        let prompt_b = index(&|value| {
            value["sessionId"] == "agent-b"
                && value["update"]["sessionUpdate"] == "session_message"
                && value["update"]["messageId"] == "toolu_b"
        });
        let announce_c = index(&|value| {
            value["sessionId"] == "agent-b"
                && value["update"]["sessionUpdate"] == "subagent_update"
                && value["update"]["sessionId"] == "agent-c"
                && value["update"]["state"]["state"] == "running"
        });
        let prompt_c = index(&|value| {
            value["sessionId"] == "agent-c"
                && value["update"]["sessionUpdate"] == "session_message"
                && value["update"]["messageId"] == "toolu_c"
        });
        let c_text = index(&|value| {
            value["sessionId"] == "agent-c"
                && value["update"]["sessionUpdate"] == "agent_message_chunk"
                && value["update"]["content"]["text"] == "C report"
        });
        let end_c = index(&|value| {
            value["sessionId"] == "agent-b"
                && value["update"]["sessionUpdate"] == "subagent_update"
                && value["update"]["sessionId"] == "agent-c"
                && value["update"]["state"]["state"] == "idle"
        });
        let b_text = index(&|value| {
            value["sessionId"] == "agent-b"
                && value["update"]["sessionUpdate"] == "agent_message_chunk"
                && value["update"]["content"]["text"] == "B report"
        });
        let end_b = index(&|value| {
            value["sessionId"] == ID
                && value["update"]["sessionUpdate"] == "subagent_update"
                && value["update"]["sessionId"] == "agent-b"
                && value["update"]["state"]["state"] == "idle"
        });
        // C is announced and replayed right after the launch in B, before B's
        // own next message; the ends all come at the replay's end, children
        // first (reverse registration order).
        assert!(announce_b < prompt_b);
        assert!(prompt_b < announce_c);
        assert!(announce_c < prompt_c);
        assert!(prompt_c < c_text);
        assert!(c_text < b_text);
        assert!(b_text < end_c);
        assert!(end_c < end_b);
        assert_eq!(out[prompt_c]["update"]["senderSessionId"], "agent-b");
        assert_eq!(out[end_c]["sessionId"], "agent-b");
    }

    /// T6FB2 MEDIUM 7: the child loop applies the same user-message filters as
    /// `replayMessage`: hidden meta, task notifications and marker-only local
    /// commands are skipped before `childContent`.
    #[test]
    fn child_user_messages_are_filtered_like_replay_message() {
        let child = vec![
            json!({
                "type": "user", "uuid": "h1", "sessionId": ID, "isMeta": true,
                "origin": {"kind": "peer"},
                "message": {"role": "user", "content": "hidden"},
            }),
            json!({
                "type": "user", "uuid": "t1", "sessionId": ID,
                "origin": {"kind": "task-notification"},
                "message": {"role": "user", "content": "stopped"},
            }),
            json!({
                "type": "user", "uuid": "l1", "sessionId": ID,
                "message": {"role": "user", "content": [
                    {"type": "text", "text": "<local-command-stdout>x</local-command-stdout>"}
                ]},
            }),
            child_user("p1", "Real prompt"),
            child_assistant("p2", Some("p1"), "Report"),
        ];
        let (_tmp, context) =
            subagents_fixture("filters", &[("agent-x", "toolu_x", false, child)], false);
        let messages = vec![
            user("u1", None, json!("Go"), "/p"),
            launch_message("a1", Some("u1"), "toolu_x", "Real prompt"),
        ];
        let (out, _, _) = replay_session_with_subagents(
            &messages,
            ID,
            false,
            false,
            ReplaySinks::default(),
            Some(context),
        );
        let prompts: Vec<&Value> = out
            .iter()
            .filter(|value| {
                value["sessionId"] == "agent-x"
                    && value["update"]["sessionUpdate"] == "session_message"
            })
            .collect();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0]["update"]["content"][0]["text"], "Real prompt");
        assert_eq!(prompts[0]["update"]["messageId"], "toolu_x");
        let chunks: Vec<&str> = out
            .iter()
            .filter(|value| value["sessionId"] == "agent-x")
            .filter_map(|value| value["update"]["content"]["text"].as_str())
            .collect();
        assert_eq!(chunks, vec!["Report"]);
        assert!(
            !out.iter().any(|value| {
                value["update"]["content"]["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("hidden") || text.contains("stopped"))
            }),
            "{out:?}"
        );
    }

    /// T6FB2 STEP 3: the live-load variant replays the subagents too
    /// (`acp-agent.ts:3156-3160` uses `replaySessionHistory` in both branches).
    #[test]
    fn the_rfd_variant_replays_subagents_for_a_live_load() {
        let child = vec![
            child_user("c1", "Work"),
            child_assistant("c2", Some("c1"), "Done"),
        ];
        let (_tmp, context) =
            subagents_fixture("rfd", &[("agent-live", "toolu_live", false, child)], false);
        let messages = vec![
            user("u1", None, json!("Go"), "/p"),
            launch_message("a1", Some("u1"), "toolu_live", "Work"),
        ];
        let (out, _, _) = replay_with_failures_rfd(
            &messages,
            ID,
            false,
            false,
            ReplaySinks::default(),
            Some(context),
        );
        assert!(out.iter().any(|value| {
            value["sessionId"] == ID
                && value["update"]["sessionUpdate"] == "subagent_update"
                && value["update"]["sessionId"] == "agent-live"
                && value["update"]["state"]["state"] == "running"
        }));
        assert!(out.iter().any(|value| {
            value["sessionId"] == "agent-live"
                && value["update"]["sessionUpdate"] == "agent_message_chunk"
                && value["update"]["content"]["text"] == "Done"
        }));
        assert!(out.iter().any(|value| {
            value["update"]["sessionUpdate"] == "subagent_update"
                && value["update"]["sessionId"] == "agent-live"
                && value["update"]["state"]["state"] == "idle"
        }));
    }

    /// T6FB2 MEDIUM 5: the async replay prefetches the child transcripts
    /// off-thread and yields between child messages, with the same frames as
    /// the synchronous walk.
    #[tokio::test]
    async fn the_async_replay_prefetches_and_replays_nested_children() {
        let c_records = vec![
            child_user("g1", "C work"),
            child_assistant("g2", Some("g1"), "C report"),
        ];
        let b_records = vec![
            child_user("b1", "B work"),
            launch_message("b2", Some("b1"), "toolu_c", "C work"),
            child_assistant("b3", Some("b2"), "B report"),
        ];
        let (_tmp, context) = subagents_fixture(
            "async-nested",
            &[
                ("agent-b", "toolu_b", false, b_records),
                ("agent-c", "toolu_c", false, c_records),
            ],
            false,
        );
        let messages = vec![
            user("u1", None, json!("Go"), "/p"),
            launch_message("a1", Some("u1"), "toolu_b", "B work"),
        ];
        let mut yielder = crate::event_loop_yield::EventLoopYielder::with_clock(
            crate::event_loop_yield::MESSAGE_LOOP_BUDGET_MS,
        );
        let (out, _, _) = replay_session_async_with_subagents(
            &messages,
            ID,
            false,
            false,
            ReplaySinks::default(),
            &mut yielder,
            Some(context),
        )
        .await;
        let announced: Vec<&str> = out
            .iter()
            .filter(|value| {
                value["update"]["sessionUpdate"] == "subagent_update"
                    && value["update"]["state"]["state"] == "running"
            })
            .filter_map(|value| value["update"]["sessionId"].as_str())
            .collect();
        assert_eq!(announced, vec!["agent-b", "agent-c"]);
        assert!(out.iter().any(|value| {
            value["sessionId"] == "agent-c" && value["update"]["content"]["text"] == "C report"
        }));
        let ends: Vec<&str> = out
            .iter()
            .filter(|value| {
                value["update"]["sessionUpdate"] == "subagent_update"
                    && value["update"]["state"]["state"] == "idle"
            })
            .filter_map(|value| value["update"]["sessionId"].as_str())
            .collect();
        assert_eq!(ends, vec!["agent-c", "agent-b"]);
    }

    /// The trace the upstream `replayRfd` helper builds: `<session> <kind>
    /// <detail>` for the update kinds it prints.
    fn rfd_trace(out: &[Value], root: &str) -> Vec<String> {
        fn text(content: &Value) -> String {
            if let Some(text) = content.as_str() {
                return text.to_string();
            }
            if let Some(text) = content.get("text").and_then(Value::as_str) {
                return text.to_string();
            }
            content
                .as_array()
                .and_then(|blocks| blocks.first())
                .and_then(|block| block["text"].as_str())
                .unwrap_or("")
                .to_string()
        }
        out.iter()
            .filter_map(|value| {
                let session = value["sessionId"].as_str()?;
                let update = &value["update"];
                let on = if session == root { "root" } else { session };
                let field = |key: &str| update[key].as_str().unwrap_or("");
                match update["sessionUpdate"].as_str()? {
                    "subagent_update" => Some(format!(
                        "{on} subagent_update {} {}",
                        field("sessionId"),
                        update["state"]
                    )),
                    "session_message" => Some(format!(
                        "{on} session_message {} from={} {}",
                        field("messageId"),
                        field("senderSessionId"),
                        text(&update["content"])
                    )),
                    "tool_call" => Some(format!("{on} tool_call {}", field("toolCallId"))),
                    "notice" => Some(format!(
                        "{on} notice {}: {}",
                        field("title"),
                        field("description")
                    )),
                    "user_message_chunk" | "agent_message_chunk" => Some(format!(
                        "{on} {} {}",
                        field("sessionUpdate"),
                        text(&update["content"])
                    )),
                    _ => None,
                }
            })
            .collect()
    }

    /// T6FB2: a `subagent_type: "fork"` child replays without the copy of the
    /// launching message and the boilerplate it inherited (the upstream
    /// `acp-agent.test.ts` fork trace).
    #[test]
    fn a_fork_child_replays_without_the_launching_message() {
        let launching = json!([
            {"type": "text", "text": "I'll fork a worker."},
            {"type": "tool_use", "id": "toolu_fork", "name": "Agent",
                "input": {"subagent_type": "fork", "description": "Check the parser",
                    "prompt": "Inspect the parser"}},
            {"type": "tool_use", "id": "toolu_read", "name": "Read",
                "input": {"file_path": "/tmp/proj/a.ts"}},
        ]);
        let fork_records = vec![
            assistant("f1", None, launching.clone()),
            user(
                "f2",
                None,
                json!([
                    {"type": "tool_result", "tool_use_id": "toolu_fork",
                        "content": [{"type": "text", "text": "Fork started — processing in background"}]},
                    {"type": "tool_result", "tool_use_id": "toolu_read",
                        "content": [{"type": "text", "text": "Fork started — processing in background"}]},
                    {"type": "text", "text": "<fork-boilerplate>\nYou are a worker fork.\n</fork-boilerplate>\nYour directive: Inspect the parser"},
                ]),
                "/p",
            ),
            child_assistant("f3", Some("f2"), "Parser is fine."),
        ];
        let (_tmp, context) = subagents_fixture(
            "fork",
            &[("fork1", "toolu_fork", false, fork_records)],
            false,
        );
        let messages = vec![
            user("u1", None, json!("Check it"), "/p"),
            assistant("a1", Some("u1"), launching),
            user(
                "u2",
                Some("a1"),
                json!([
                    {"type": "tool_result", "tool_use_id": "toolu_fork", "content": "Parser is fine."},
                    {"type": "tool_result", "tool_use_id": "toolu_read", "content": "x"},
                ]),
                "/p",
            ),
            assistant("a2", Some("u2"), json!([{"type": "text", "text": "Done."}])),
        ];
        let (out, _, _) = replay_session_with_subagents(
            &messages,
            ID,
            false,
            false,
            ReplaySinks::default(),
            Some(context),
        );
        assert_eq!(
            rfd_trace(&out, ID),
            vec![
                "root user_message_chunk Check it".to_string(),
                "root agent_message_chunk I'll fork a worker.".to_string(),
                "root tool_call toolu_fork".to_string(),
                "root tool_call toolu_read".to_string(),
                r#"root subagent_update fork1 {"state":"running"}"#.to_string(),
                format!("fork1 session_message toolu_fork from={ID} Inspect the parser"),
                "fork1 agent_message_chunk Parser is fine.".to_string(),
                "root agent_message_chunk Done.".to_string(),
                r#"root subagent_update fork1 {"state":"idle","stopReason":"end_turn"}"#
                    .to_string(),
            ]
        );
    }

    /// T6FB2: an RFD replay leaves out a subagent message that the session
    /// transcript stores inline (`acp-agent.ts:9297`); the child's own
    /// transcript carries it.
    #[test]
    fn an_inline_subagent_message_is_skipped_on_the_rfd_form() {
        let child = vec![
            child_user("c1", "Work"),
            child_assistant("c2", Some("c1"), "Child text"),
        ];
        let (_tmp, context) =
            subagents_fixture("inline", &[("agent-i", "toolu_i", false, child)], false);
        let messages = vec![
            user("u1", None, json!("Go"), "/p"),
            launch_message("a1", Some("u1"), "toolu_i", "Work"),
            // The SDK returns an inline subagent message with its parent tool
            // use id; it must not replay onto the root.
            json!({
                "type": "assistant", "uuid": "inline1", "parentUuid": "a1",
                "sessionId": ID, "parent_tool_use_id": "toolu_i",
                "message": {"role": "assistant",
                    "content": [{"type": "text", "text": "Inline child text"}]},
            }),
        ];
        let (out, _, _) = replay_session_with_subagents(
            &messages,
            ID,
            false,
            false,
            ReplaySinks::default(),
            Some(context),
        );
        assert!(
            !out.iter().any(|value| {
                value["sessionId"] == ID
                    && value["update"]["content"]["text"] == "Inline child text"
            }),
            "{out:?}"
        );
        assert!(out.iter().any(|value| {
            value["sessionId"] == "agent-i" && value["update"]["content"]["text"] == "Child text"
        }));
    }

    /// T6FB2: the full upstream RFD trace (`acp-agent.test.ts`, "replays each
    /// child under its live session id, with its incoming messages, before the
    /// response"): nested announce, SendMessage delivery, task-notification end
    /// and the missing-history notice.
    #[test]
    fn replays_each_child_under_its_live_session_id() {
        let agent_records = vec![
            child_user("c1", "Look around"),
            assistant(
                "c2",
                None,
                json!([
                    {"type": "text", "text": "Child report."},
                    {"type": "tool_use", "id": "toolu_nested", "name": "Agent",
                        "input": {"name": "helper", "subagent_type": "general-purpose",
                            "description": "Read the spec", "prompt": "Read spec.md"}},
                ]),
            ),
            user(
                "c3",
                None,
                json!([{"type": "tool_result", "tool_use_id": "toolu_nested",
                    "content": "The spec says X."}]),
                "/p",
            ),
            child_user("c4", "[Request interrupted by user for tool use]"),
            json!({
                "type": "user", "uuid": "c5", "sessionId": ID,
                "isMeta": true, "origin": {"kind": "coordinator"},
                "message": {"role": "user", "content":
                    "The coordinator sent a message while you were working:\nCheck the tests too\n\nAddress this before completing your current task."},
            }),
            child_assistant("c6", Some("c5"), "Tests pass."),
        ];
        let helper_records = vec![
            child_user("g1", "Read spec.md"),
            child_assistant("g2", Some("g1"), "X."),
        ];
        let (_tmp, context) = subagents_fixture(
            "live-id",
            &[
                ("abc123", "toolu_agent", false, agent_records),
                ("def456", "toolu_nested", false, helper_records),
            ],
            true,
        );
        let messages = vec![
            user("u1", None, json!("Explore it"), "/p"),
            assistant(
                "a1",
                Some("u1"),
                json!([
                    {"type": "tool_use", "id": "toolu_agent", "name": "Agent",
                        "input": {"name": "scout", "subagent_type": "Explore",
                            "description": "Explore the code", "prompt": "Look around"}},
                    {"type": "tool_use", "id": "toolu_lost", "name": "Agent",
                        "input": {"description": "Lost", "prompt": "No file"}},
                ]),
            ),
            user(
                "u2",
                Some("a1"),
                json!([
                    {"type": "tool_result", "tool_use_id": "toolu_agent", "content": "Found it."},
                    {"type": "tool_result", "tool_use_id": "toolu_lost", "content": "Gone."},
                ]),
                "/p",
            ),
            assistant(
                "a2",
                Some("u2"),
                json!([{"type": "tool_use", "id": "toolu_send", "name": "SendMessage",
                    "input": {"to": "scout", "message": "Check the tests too"}}]),
            ),
            user(
                "u3",
                Some("a2"),
                json!([{"type": "tool_result", "tool_use_id": "toolu_send",
                    "content": "Message sent."}]),
                "/p",
            ),
            json!({
                "type": "user", "uuid": "u4", "parentUuid": "u3", "sessionId": ID,
                "origin": {"kind": "task-notification"},
                "message": {"role": "user", "content":
                    "<task-notification>\n<task-id>abc123</task-id>\n<tool-use-id>toolu_send</tool-use-id>\n<status>completed</status>\n</task-notification>"},
            }),
            assistant("a3", Some("u4"), json!([{"type": "text", "text": "Done."}])),
        ];
        let (out, _, _) = replay_session_with_subagents(
            &messages,
            ID,
            false,
            false,
            ReplaySinks::default(),
            Some(context),
        );
        assert_eq!(
            rfd_trace(&out, ID),
            vec![
                "root user_message_chunk Explore it".to_string(),
                "root tool_call toolu_agent".to_string(),
                "root tool_call toolu_lost".to_string(),
                r#"root subagent_update abc123 {"state":"running"}"#.to_string(),
                format!("abc123 session_message toolu_agent from={ID} Look around"),
                "abc123 agent_message_chunk Child report.".to_string(),
                "abc123 tool_call toolu_nested".to_string(),
                r#"abc123 subagent_update def456 {"state":"running"}"#.to_string(),
                "def456 session_message toolu_nested from=abc123 Read spec.md".to_string(),
                "def456 agent_message_chunk X.".to_string(),
                format!("abc123 session_message toolu_send from={ID} Check the tests too"),
                "abc123 agent_message_chunk Tests pass.".to_string(),
                "root tool_call toolu_send".to_string(),
                "root agent_message_chunk Done.".to_string(),
                r#"abc123 subagent_update def456 {"state":"idle","stopReason":"end_turn"}"#
                    .to_string(),
                r#"root subagent_update abc123 {"state":"idle","stopReason":"end_turn"}"#
                    .to_string(),
                "root notice Some subagent history is not available: The history of 1 subagent of this session could not be restored.".to_string(),
            ]
        );
    }

    /// T6FB3 STEP 1 (N1): sibling launches in one message are replayed in launch order
    /// (first launch announced + history + end before the second launch).
    #[test]
    fn sibling_launches_replay_in_launch_order() {
        let (_tmp, context) = subagents_fixture(
            "sib_order",
            &[
                (
                    "agent-x",
                    "toolu_x",
                    false,
                    vec![
                        child_user("x1", "X work"),
                        child_assistant("x2", Some("x1"), "X report"),
                    ],
                ),
                (
                    "agent-y",
                    "toolu_y",
                    false,
                    vec![
                        child_user("y1", "Y work"),
                        child_assistant("y2", Some("y1"), "Y report"),
                    ],
                ),
            ],
            false,
        );
        let both = assistant(
            "a1",
            Some("u1"),
            json!([
                {"type": "tool_use", "id": "toolu_x", "name": "Agent", "input": {"name": "scout", "prompt": "X work"}},
                {"type": "tool_use", "id": "toolu_y", "name": "Agent", "input": {"name": "scout", "prompt": "Y work"}},
            ]),
        );
        let messages = vec![user("u1", None, json!("Go"), "/p"), both];
        let (out, _, _) = replay_session_with_subagents(
            &messages,
            ID,
            false,
            false,
            ReplaySinks::default(),
            Some(context),
        );
        let order: Vec<String> = out
            .iter()
            .filter_map(|v| {
                let update = &v["update"];
                let kind = update["sessionUpdate"].as_str()?;
                match kind {
                    "subagent_update" => {
                        let state = update["state"]["state"].as_str().unwrap_or("");
                        let agent = update["sessionId"].as_str().unwrap_or("");
                        Some(format!("subagent_update {agent} {state}"))
                    }
                    "session_message" => {
                        let to = v["sessionId"].as_str().unwrap_or("");
                        Some(format!("session_message {to}"))
                    }
                    "agent_message_chunk" => {
                        let agent = v["sessionId"].as_str().unwrap_or("");
                        Some(format!("chunk {agent}"))
                    }
                    _ => None,
                }
            })
            .collect();
        assert_eq!(
            order,
            vec![
                "subagent_update agent-x running",
                "session_message agent-x",
                "chunk agent-x",
                "subagent_update agent-y running",
                "session_message agent-y",
                "chunk agent-y",
                "subagent_update agent-y idle",
                "subagent_update agent-x idle",
            ]
        );
    }

    /// T6FB3 STEP 2 (N2): task notifications inside a child transcript update nested background children.
    #[test]
    fn nested_child_task_notification_ends_background_child() {
        let note = "<task-notification>\n<task-id>agent-c</task-id>\n<status>completed</status>\n</task-notification>";
        let b_records = vec![
            child_user("b1", "B work"),
            launch_message("b2", Some("b1"), "toolu_c", "C work"),
            json!({
                "type": "user",
                "uuid": "b3",
                "sessionId": ID,
                "origin": {"kind": "task-notification"},
                "message": {"role": "user", "content": [{"type": "text", "text": note}]},
            }),
            child_assistant("b4", Some("b3"), "B report"),
        ];
        let c_records = vec![
            child_user("g1", "C work"),
            child_assistant("g2", Some("g1"), "C report"),
            assistant(
                "g3",
                Some("g2"),
                json!([{"type": "tool_use", "id": "toolu_read", "name": "Read", "input": {"file_path": "/p/x"}}]),
            ),
        ];
        let (_tmp, context) = subagents_fixture(
            "zznest",
            &[
                ("agent-b", "toolu_b", false, b_records),
                ("agent-c", "toolu_c", true, c_records),
            ],
            false,
        );
        let messages = vec![
            user("u1", None, json!("Go"), "/p"),
            launch_message("a1", Some("u1"), "toolu_b", "B work"),
        ];
        let (out, _, _) = replay_session_with_subagents(
            &messages,
            ID,
            false,
            false,
            ReplaySinks::default(),
            Some(context),
        );
        let c_end = out
            .iter()
            .rfind(|v| {
                v["update"]["sessionUpdate"] == "subagent_update"
                    && v["update"]["sessionId"] == "agent-c"
            })
            .expect("agent-c ends");
        assert_eq!(
            c_end["update"]["state"],
            json!({"state": "idle", "stopReason": "end_turn"})
        );
    }

    /// T6FB3 STEP 3 (N3): child messages carry parent_tool_use_id & parent_agent_id,
    /// and child updates are stamped with `_meta.claudeCode.parentToolUseId`.
    #[test]
    fn child_messages_and_updates_carry_parent_tool_use_id() {
        let b_records = vec![
            child_user("b1", "B work"),
            launch_message("b2", Some("b1"), "toolu_c", "C work"),
            child_assistant("b3", Some("b2"), "B report"),
        ];
        let c_records = vec![
            child_user("c1", "C work"),
            child_assistant("c2", Some("c1"), "C report"),
            assistant(
                "c3",
                Some("c2"),
                json!([{"type": "tool_use", "id": "toolu_read", "name": "Read", "input": {"file_path": "/p/x"}}]),
            ),
        ];
        let (tmp, mut context) = subagents_fixture(
            "parent_tool_use_id",
            &[
                ("agent-b", "toolu_b", false, b_records),
                ("agent-c", "toolu_c", false, c_records),
            ],
            false,
        );
        // Stamp parentAgentId on agent-c's metadata file and record
        if let Some(record) = context.history.agents.get_mut("toolu_c") {
            record.parent_agent_id = Some("agent-b".to_string());
        }
        std::fs::write(
            tmp.0.join("agent-agent-c.meta.json"),
            json!({
                "toolUseId": "toolu_c",
                "parentAgentId": "agent-b",
                "name": "scout",
                "agentType": "Explore",
                "description": "Work",
            })
            .to_string(),
        )
        .unwrap();

        // Check read_subagent_messages carries parent_tool_use_id and parent_agent_id
        let c_msgs =
            crate::subagent_history::read_subagent_messages(ID, "agent-c", &context.history, None);
        assert!(!c_msgs.is_empty());
        for msg in &c_msgs {
            assert_eq!(msg["parent_tool_use_id"], json!("toolu_c"));
            assert_eq!(msg["parent_agent_id"], json!("agent-b"));
        }

        let messages = vec![
            user("u1", None, json!("Go"), "/p"),
            launch_message("a1", Some("u1"), "toolu_b", "B work"),
        ];
        let (out, _, _) = replay_session_with_subagents(
            &messages,
            ID,
            false,
            false,
            ReplaySinks::default(),
            Some(context),
        );

        // Find agent-c's chunk and tool_call updates
        let c_chunk = out
            .iter()
            .find(|v| {
                v["sessionId"] == "agent-c" && v["update"]["sessionUpdate"] == "agent_message_chunk"
            })
            .expect("agent-c chunk");
        assert_eq!(
            c_chunk["update"]["_meta"]["claudeCode"]["parentToolUseId"],
            json!("toolu_c")
        );
        let c_chunk_str = serde_json::to_string(&c_chunk["update"]).unwrap();
        assert!(c_chunk_str.contains(r#""parentToolUseId":"toolu_c""#));

        let c_tool_call = out
            .iter()
            .find(|v| v["sessionId"] == "agent-c" && v["update"]["sessionUpdate"] == "tool_call")
            .expect("agent-c tool call");
        assert_eq!(
            c_tool_call["update"]["_meta"]["claudeCode"]["parentToolUseId"],
            json!("toolu_c")
        );
        assert_eq!(
            c_tool_call["update"]["_meta"]["claudeCode"]["toolName"],
            json!("Read")
        );
        let c_tc_str = serde_json::to_string(&c_tool_call["update"]).unwrap();
        assert!(c_tc_str.contains(r#""parentToolUseId":"toolu_c""#));
        assert!(c_tc_str.contains(r#""toolName":"Read""#));
    }

    /// T6FB3 STEP 4 (N4): a 3-level subagent tree replays in order asynchronously
    /// without upfront prefetch of all child transcripts.
    #[tokio::test]
    async fn three_level_subagent_tree_replays_in_order_async() {
        let a_records = vec![
            child_user("a1", "A work"),
            launch_message("a2", Some("a1"), "toolu_b", "B work"),
            child_assistant("a3", Some("a2"), "A done"),
        ];
        let b_records = vec![
            child_user("b1", "B work"),
            launch_message("b2", Some("b1"), "toolu_c", "C work"),
            child_assistant("b3", Some("b2"), "B done"),
        ];
        let c_records = vec![
            child_user("c1", "C work"),
            child_assistant("c2", Some("c1"), "C done"),
        ];
        let (_tmp, context) = subagents_fixture(
            "three_level",
            &[
                ("agent-a", "toolu_a", false, a_records),
                ("agent-b", "toolu_b", false, b_records),
                ("agent-c", "toolu_c", false, c_records),
            ],
            false,
        );
        let messages = vec![
            user("u1", None, json!("Go"), "/p"),
            launch_message("m1", Some("u1"), "toolu_a", "A work"),
        ];
        let mut yielder = crate::event_loop_yield::EventLoopYielder::with_clock(
            crate::event_loop_yield::MESSAGE_LOOP_BUDGET_MS,
        );
        let (out, _, _) = replay_session_async_with_subagents(
            &messages,
            ID,
            false,
            false,
            ReplaySinks::default(),
            &mut yielder,
            Some(context),
        )
        .await;

        let sequence: Vec<String> = out
            .iter()
            .filter_map(|v| {
                let update = &v["update"];
                let kind = update["sessionUpdate"].as_str()?;
                match kind {
                    "subagent_update" => {
                        let state = update["state"]["state"].as_str().unwrap_or("");
                        let agent = update["sessionId"].as_str().unwrap_or("");
                        Some(format!("subagent_update {agent} {state}"))
                    }
                    "session_message" => {
                        let to = v["sessionId"].as_str().unwrap_or("");
                        Some(format!("session_message {to}"))
                    }
                    "agent_message_chunk" => {
                        let agent = v["sessionId"].as_str().unwrap_or("");
                        Some(format!("chunk {agent}"))
                    }
                    _ => None,
                }
            })
            .collect();

        assert_eq!(
            sequence,
            vec![
                "subagent_update agent-a running",
                "session_message agent-a",
                "subagent_update agent-b running",
                "session_message agent-b",
                "subagent_update agent-c running",
                "session_message agent-c",
                "chunk agent-c",
                "chunk agent-b",
                "chunk agent-a",
                "subagent_update agent-c idle",
                "subagent_update agent-b idle",
                "subagent_update agent-a idle",
            ]
        );
    }

    /// T6FB3 STEP 4: verify that SubagentReplayState has no prefetch transcripts map,
    /// and child transcripts are read on demand rather than all prefetched at once.
    #[test]
    fn prefetch_map_no_longer_exists() {
        let state = SubagentReplayState {
            replay: crate::subagent_replay::RfdSubagentReplay::new(Default::default()),
            context: crate::subagent_replay::SubagentReplayContext {
                history: Default::default(),
                notices: false,
                logger: None,
            },
        };
        assert_eq!(state.replay.omitted().len(), 0);
    }

    /// T6FB3 STEP 5 (N8): inline subagent messages are skipped on RFD form even when history has no launches.
    #[test]
    fn inline_subagent_messages_skipped_on_rfd_form_without_launches() {
        let messages = vec![
            user("u1", None, json!("Go"), "/p"),
            json!({
                "type": "assistant",
                "uuid": "inline1",
                "parent_tool_use_id": "cut_launch_toolu",
                "message": {"role": "assistant", "content": [{"type": "text", "text": "inline child msg"}]},
            }),
            assistant(
                "a1",
                Some("u1"),
                json!([{"type": "text", "text": "Root report"}]),
            ),
        ];
        let context = crate::subagent_replay::SubagentReplayContext {
            history: Default::default(),
            notices: false,
            logger: None,
        };
        let (out, _, _) = replay_session_with_subagents(
            &messages,
            ID,
            false,
            false,
            ReplaySinks::default(),
            Some(context),
        );
        // The inline message with parent_tool_use_id is skipped
        assert!(
            !out.iter()
                .any(|v| { v["update"]["content"]["text"] == "inline child msg" }),
            "{out:?}"
        );
        // Root report is present
        assert!(
            out.iter()
                .any(|v| { v["update"]["content"]["text"] == "Root report" }),
            "{out:?}"
        );
    }

    /// Sibling launches at grandchild level are also replayed in launch order.
    #[test]
    fn sibling_grandchild_launches_replay_in_launch_order() {
        let (_tmp, context) = subagents_fixture(
            "grandchild_order",
            &[
                (
                    "agent-x",
                    "toolu_x",
                    false,
                    vec![
                        child_user("x1", "X work"),
                        assistant(
                            "x2",
                            Some("x1"),
                            json!([
                                {"type": "tool_use", "id": "toolu_g1", "name": "Agent", "input": {"name": "scout", "prompt": "G1 work"}},
                                {"type": "tool_use", "id": "toolu_g2", "name": "Agent", "input": {"name": "scout", "prompt": "G2 work"}},
                            ]),
                        ),
                    ],
                ),
                (
                    "agent-g1",
                    "toolu_g1",
                    false,
                    vec![
                        child_user("g1_1", "G1 work"),
                        child_assistant("g1_2", Some("g1_1"), "G1 done"),
                    ],
                ),
                (
                    "agent-g2",
                    "toolu_g2",
                    false,
                    vec![
                        child_user("g2_1", "G2 work"),
                        child_assistant("g2_2", Some("g2_1"), "G2 done"),
                    ],
                ),
            ],
            false,
        );
        let messages = vec![
            user("u1", None, json!("Go"), "/p"),
            launch_message("a1", Some("u1"), "toolu_x", "X work"),
        ];
        let (out, _, _) = replay_session_with_subagents(
            &messages,
            ID,
            false,
            false,
            ReplaySinks::default(),
            Some(context),
        );
        let order: Vec<String> = out
            .iter()
            .filter_map(|v| {
                let update = &v["update"];
                let kind = update["sessionUpdate"].as_str()?;
                match kind {
                    "subagent_update" => {
                        let state = update["state"]["state"].as_str().unwrap_or("");
                        let agent = update["sessionId"].as_str().unwrap_or("");
                        Some(format!("subagent_update {agent} {state}"))
                    }
                    "session_message" => {
                        let to = v["sessionId"].as_str().unwrap_or("");
                        Some(format!("session_message {to}"))
                    }
                    "agent_message_chunk" => {
                        let agent = v["sessionId"].as_str().unwrap_or("");
                        Some(format!("chunk {agent}"))
                    }
                    _ => None,
                }
            })
            .collect();
        assert_eq!(
            order,
            vec![
                "subagent_update agent-x running",
                "session_message agent-x",
                "subagent_update agent-g1 running",
                "session_message agent-g1",
                "chunk agent-g1",
                "subagent_update agent-g2 running",
                "session_message agent-g2",
                "chunk agent-g2",
                "subagent_update agent-g2 idle",
                "subagent_update agent-g1 idle",
                "subagent_update agent-x idle",
            ]
        );
    }

    // ------------------------------------------------------------ AIR replay (G1)

    /// `replaySessionHistory` for a client on AIR's native subagent draft
    /// (`acp-agent.ts:9037-9527`).
    fn air_replay(messages: &[Value]) -> Vec<Value> {
        replay_session(
            messages,
            "s1",
            true,
            false,
            ReplaySinks {
                native_air_replay: true,
                ..ReplaySinks::default()
            },
        )
        .0
    }

    /// `replaySessionHistory` for a client without native subagent sessions
    /// (`forwardSubagentText` decides whether the nested text replays).
    fn legacy_replay(messages: &[Value], forward_subagent_text: bool) -> Vec<Value> {
        replay_session(
            messages,
            "s1",
            false,
            false,
            ReplaySinks {
                forward_subagent_text,
                ..ReplaySinks::default()
            },
        )
        .0
    }

    /// The upstream `replayHistory`: an unattributed sidechain assistant message.
    fn sidechain_assistant() -> Value {
        json!({
            "type": "assistant",
            "uuid": "subagent-message",
            "session_id": "s1",
            "parent_tool_use_id": "parent-agent-call",
            "parent_agent_id": "agent-1",
            "message": {
                "id": "api-subagent-message",
                "model": "claude-sonnet-4-5",
                "role": "assistant",
                "type": "message",
                "stop_reason": "tool_use",
                "content": [
                    {"type": "text", "text": "nested report"},
                    {"type": "tool_use", "id": "child-tool", "name": "Bash",
                        "input": {"command": "pwd"}}
                ]
            }
        })
    }

    /// The upstream launch message of a persisted Agent call.
    fn agent_launch() -> Value {
        json!({
            "type": "assistant",
            "uuid": "launch-message",
            "session_id": "s1",
            "parent_tool_use_id": null,
            "parent_agent_id": null,
            "message": {
                "id": "api-launch-message",
                "model": "claude-sonnet-4-5",
                "role": "assistant",
                "type": "message",
                "stop_reason": "tool_use",
                "content": [{
                    "type": "tool_use",
                    "id": "parent-agent-call",
                    "name": "Agent",
                    "input": {"name": "Reviewer", "prompt": "Review the implementation"}
                }]
            }
        })
    }

    /// The upstream `agentResult(isError, content)`: the launch's tool_result.
    fn agent_result(is_error: bool, content: &str) -> Value {
        json!({
            "type": "user",
            "uuid": format!("agent-result-{content}"),
            "session_id": "s1",
            "parent_tool_use_id": null,
            "parent_agent_id": null,
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "parent-agent-call",
                    "is_error": is_error,
                    "content": content
                }]
            }
        })
    }

    fn kinds(updates: &[Value]) -> Vec<(String, String)> {
        updates
            .iter()
            .map(|value| {
                (
                    value["sessionId"].as_str().unwrap_or("").to_string(),
                    value["update"]["sessionUpdate"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                )
            })
            .collect()
    }

    /// T8A gap (U-101): upstream "replays an unattributed sidechain as a
    /// deterministic disconnected child" — AIR synthesizes the child from the
    /// inline sidechain, replays its text and tool call into it, and ends it
    /// `disconnected`.
    #[test]
    fn air_replays_an_unattributed_sidechain_as_a_disconnected_child() {
        let updates = air_replay(&[sidechain_assistant()]);
        let child = "s1:replay-subagent:parent-agent-call";
        assert_eq!(
            kinds(&updates),
            vec![
                ("s1".to_string(), "subagent_spawned".to_string()),
                (child.to_string(), "agent_message_chunk".to_string()),
                (child.to_string(), "tool_call".to_string()),
                ("s1".to_string(), "subagent_state_update".to_string()),
            ]
        );
        // The spawn's wire shape and key order (`announceReplayChild`,
        // `acp-agent.ts:9163-9175`).
        assert_eq!(
            serde_json::to_string(&updates[0]).unwrap(),
            json!({
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "subagent_spawned",
                    "subagentSessionId": child,
                    "name": "Disconnected agent",
                    "task": "Subagent restored without persisted launch metadata",
                    "capabilities": {}
                }
            })
            .to_string()
        );
        assert_eq!(
            serde_json::to_string(&updates[3]).unwrap(),
            json!({
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "subagent_state_update",
                    "subagentSessionId": child,
                    "state": "disconnected"
                }
            })
            .to_string()
        );
        // The child's tool call carries the launch that owns it.
        assert_eq!(
            updates[2]["update"]["_meta"]["claudeCode"]["parentToolUseId"],
            json!("parent-agent-call")
        );
    }

    /// T8A gap (U-101): upstream "reconstructs a persisted Agent launch before
    /// child updates" — the launch announces the child with its name and task,
    /// the Agent control call never becomes a transcript tool, and the launch
    /// result ends the child `completed`.
    #[test]
    fn air_reconstructs_a_persisted_agent_launch() {
        let updates = air_replay(&[
            agent_launch(),
            sidechain_assistant(),
            agent_result(false, "done"),
        ]);
        assert_eq!(
            updates[0]["update"]["sessionUpdate"],
            json!("subagent_spawned")
        );
        assert_eq!(updates[0]["update"]["name"], json!("Reviewer"));
        assert_eq!(
            updates[0]["update"]["task"],
            json!("Review the implementation")
        );
        assert_eq!(
            updates.last().unwrap()["update"]["state"],
            json!("completed")
        );
        assert!(
            !updates.iter().any(|value| {
                let update = &value["update"];
                (update["sessionUpdate"] == json!("tool_call")
                    || update["sessionUpdate"] == json!("tool_call_update"))
                    && update["toolCallId"] == json!("parent-agent-call")
            }),
            "the AIR control call is not a transcript tool: {updates:?}"
        );
    }

    /// T8A gap (U-101): upstream "restores a persisted $state Agent result" —
    /// an error result that names a stop is `cancelled`, any other error
    /// `failed`.
    #[test]
    fn air_restores_the_persisted_agent_result_state() {
        for (content, state) in [
            ("subagent failed", "failed"),
            ("Request interrupted by user", "cancelled"),
        ] {
            let updates = air_replay(&[
                agent_launch(),
                sidechain_assistant(),
                agent_result(true, content),
            ]);
            assert_eq!(
                updates.last().unwrap()["update"]["state"],
                json!(state),
                "{content}"
            );
        }
    }

    /// T8A gap (U-101): upstream "marks a launched child without a persisted
    /// result as disconnected".
    #[test]
    fn air_marks_a_launch_without_a_result_disconnected() {
        let updates = air_replay(&[agent_launch(), sidechain_assistant()]);
        assert_eq!(
            updates.last().unwrap()["update"]["state"],
            json!("disconnected")
        );
    }

    /// T8A gap (U-101): upstream "breaks malformed replay lineage cycles
    /// deterministically" — two spawns, and a `disconnected` end.
    #[test]
    fn air_breaks_a_replay_lineage_cycle() {
        let cyclic = |id: &str, parent: &str| {
            json!({
                "type": "assistant",
                "uuid": format!("launch-{id}"),
                "session_id": "s1",
                "parent_tool_use_id": parent,
                "parent_agent_id": "malformed",
                "message": {
                    "id": format!("api-launch-{id}"),
                    "model": "claude-sonnet-4-5",
                    "role": "assistant",
                    "type": "message",
                    "stop_reason": "tool_use",
                    "content": [{
                        "type": "tool_use", "id": id, "name": "Agent",
                        "input": {"prompt": format!("Run {id}")}
                    }]
                }
            })
        };
        let mut sidechain = sidechain_assistant();
        sidechain["parent_tool_use_id"] = json!("agent-a");
        let updates = air_replay(&[
            cyclic("agent-a", "agent-b"),
            cyclic("agent-b", "agent-a"),
            sidechain,
        ]);
        let spawned = updates
            .iter()
            .filter(|value| value["update"]["sessionUpdate"] == json!("subagent_spawned"))
            .count();
        assert_eq!(spawned, 2, "{updates:?}");
        assert!(
            updates.iter().any(|value| {
                value["update"]["sessionUpdate"] == json!("subagent_state_update")
                    && value["update"]["state"] == json!("disconnected")
            }),
            "{updates:?}"
        );
    }

    /// T8A gap (U-101): upstream "keeps legacy text filtering without losing
    /// child tool attribution" — a client without native subagent sessions
    /// loses the nested text but keeps the child tool call with its parent.
    #[test]
    fn legacy_replay_keeps_the_child_tool_attribution() {
        let updates = legacy_replay(&[sidechain_assistant()], false);
        assert!(
            !updates
                .iter()
                .any(|value| value["update"]["sessionUpdate"] == json!("agent_message_chunk")),
            "{updates:?}"
        );
        let tool_call = updates
            .iter()
            .find(|value| value["update"]["sessionUpdate"] == json!("tool_call"))
            .expect("the child tool call replays");
        assert_eq!(tool_call["sessionId"], json!("s1"));
        assert_eq!(tool_call["update"]["toolCallId"], json!("child-tool"));
        assert_eq!(
            tool_call["update"]["_meta"]["claudeCode"]["parentToolUseId"],
            json!("parent-agent-call")
        );
    }

    /// T8A gap (U-101): upstream "replays the flattened transcript for the
    /// legacy opt-in" (`_meta["subagent-transcript"]`).
    #[test]
    fn legacy_replay_forwards_the_flattened_transcript() {
        let updates = legacy_replay(&[sidechain_assistant()], true);
        let chunk = updates
            .iter()
            .find(|value| value["update"]["sessionUpdate"] == json!("agent_message_chunk"))
            .expect("the nested text replays on the root");
        assert_eq!(chunk["sessionId"], json!("s1"));
        assert_eq!(chunk["update"]["content"]["text"], json!("nested report"));
        assert!(
            updates
                .iter()
                .any(|value| value["update"]["toolCallId"] == json!("child-tool")),
            "{updates:?}"
        );
    }

    /// T8A gap (125): upstream "routes a task to the session of its launching
    /// tool even when the root receives the stop" — the async-task updates go
    /// to the AIR child of the tool call that started the task.
    #[test]
    fn air_routes_a_restored_task_to_its_launching_child() {
        let child = "s1:replay-subagent:toolu_agent";
        let with_parent = |value: Value| {
            let mut value = value;
            value["parent_tool_use_id"] = json!("toolu_agent");
            value
        };
        let launch = with_parent(json!({
            "type": "assistant", "uuid": "a1", "session_id": "s1",
            "message": {"id": "m1", "role": "assistant", "model": "claude-opus-5",
                "content": [{"type": "tool_use", "id": "toolu_bash", "name": "Bash",
                    "input": {"command": "npm test", "run_in_background": true}}]}
        }));
        let result = with_parent(json!({
            "type": "user", "uuid": "u2", "session_id": "s1",
            "message": {"role": "user", "content": [{"type": "tool_result",
                "tool_use_id": "toolu_bash", "content": "Command running in background with ID: bg1."}]}
        }));
        let notification = json!({
            "type": "user", "uuid": "notification-completed", "session_id": "s1",
            "parent_tool_use_id": null,
            "origin": {"kind": "task-notification"},
            "message": {"role": "user", "content":
                "<task-notification>\n<task-id>bg1</task-id>\n<tool-use-id>toolu_bash</tool-use-id>\n<output-file>/tmp/tasks/bg1.output</output-file>\n<status>completed</status>\n<summary>Background command \"npm test\" finished</summary>\n<note>A task-notification fires each time the task stops.</note>\n<result><status>failed</status></result>\n</task-notification>"}
        });
        let user = json!({
            "type": "user", "uuid": "u1", "session_id": "s1",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": "run the tests"}
        });
        let updates = replay_session(
            &[user, launch, result, notification],
            "s1",
            true,
            false,
            ReplaySinks {
                native_air_replay: true,
                async_tasks: Some(&mut crate::async_tasks::AsyncTaskRuntime::new(true)),
                ..ReplaySinks::default()
            },
        )
        .0;
        let tasks: Vec<&Value> = updates
            .iter()
            .filter(|value| {
                value["update"]["sessionUpdate"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("async_task_"))
            })
            .collect();
        assert_eq!(tasks.len(), 2, "{updates:?}");
        assert_eq!(tasks[0]["sessionId"], json!(child));
        assert_eq!(tasks[1]["sessionId"], json!(child));
    }
}
