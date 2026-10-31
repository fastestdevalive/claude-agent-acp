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
use crate::map::{map_consolidated, MapState, MsgRole};
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
) -> (Vec<Record>, HashMap<String, usize>) {
    let mut out = Vec::new();
    let mut ledger = QueueLedger::default();
    let Some(path) = session_store::find_transcripts(root, session_id)
        .into_iter()
        .next()
    else {
        return (out, ledger.finish());
    };
    let skip_precompact = std::fs::metadata(&path)
        .map(|metadata| metadata.len() > cap)
        .unwrap_or(false);
    let Ok(file) = std::fs::File::open(&path) else {
        return (out, ledger.finish());
    };
    for line in std::io::BufRead::lines(std::io::BufReader::new(file)) {
        let Ok(line) = line else {
            continue;
        };
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
    (out, ledger.finish())
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
/// transcript read.
pub async fn session_messages_async(env: &EnvSnapshot, session_id: &str) -> Vec<Value> {
    let env = env.clone();
    let session_id = session_id.to_string();
    tokio::task::spawn_blocking(move || session_messages(&env, &session_id))
        .await
        .unwrap_or_default()
}

fn session_messages_capped(env: &EnvSnapshot, session_id: &str, cap: u64) -> Vec<Value> {
    if !session_store::is_session_id(session_id) {
        return Vec::new();
    }
    let root = session_store::projects_root_with_env(env);
    let (raw, delivered) = read_records_capped(&root, session_id, cap);
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
        out.push(to_sdk_message(&mapped));
    }
    out
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
fn to_sdk_message(entry: &Value) -> Value {
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
    out.insert("parent_tool_use_id".into(), Value::Null);
    out.insert("parent_agent_id".into(), Value::Null);
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
) -> Vec<Value> {
    let air = flags.air;
    let Value::Array(blocks) = content else {
        return map_consolidated(content, role, state)
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
                    let _ = map_consolidated(&Value::Array(vec![block.clone()]), role, state);
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
        for update in map_consolidated(&Value::Array(vec![block.clone()]), role, state) {
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
}

/// One replay pass: the ordered updates it produced plus the state the caller
/// seeds into the live session.
struct ReplayRun<'a> {
    session_id: &'a str,
    air: bool,
    plan_ops: bool,
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
        Self {
            session_id,
            air,
            plan_ops,
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
        }
    }

    fn push(&mut self, update: Value) {
        self.out
            .push(json!({ "sessionId": self.session_id, "update": update }));
    }

    /// `restoreTaskNotification` (`acp-agent.ts:9258-9284`): a persisted
    /// notification of a background task restores its card. A notification of
    /// a native subagent launch is the child's state, not an async task; a
    /// task that no tool call started stays unknown.
    fn restore_task_notification(&mut self, notification: &PersistedTaskNotification) {
        let Some(tool_use_id) = notification.tool_use_id.clone() else {
            return;
        };
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
        for update in updates {
            self.push(update);
        }
    }

    fn process(&mut self, message: &Value) {
        if is_hidden_meta_message(message) {
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
        if role == "user" {
            // B-45: like live, a tool result gives the output path of a known
            // task, and a persisted task notification restores the task card
            // (`acp-agent.ts:9363-9375`).
            if let Some(runtime) = self.async_tasks.as_deref_mut() {
                for update in runtime.tool_results(&content) {
                    self.push(update);
                }
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
                            },
                        );
                    }
                }
            }
        }
        if role == "assistant" && parent_tool_use_id_of(message).is_some() {
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
        );
        let message_id = if self.message_id.is_empty() {
            None
        } else {
            Some(self.message_id.clone())
        };
        for update in updates {
            let update = match update {
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
            self.push(update);
        }
    }

    fn finish(
        self,
    ) -> (
        Vec<Value>,
        Vec<(String, crate::hooks::TaskStateEntry)>,
        Option<String>,
    ) {
        (self.out, self.task_state, self.last_published)
    }
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
        run.process(message);
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
        let (records, _) = read_records_capped(&root, ID, 0);
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
        let (records, _) = read_records_capped(&root, ID, u64::MAX);
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
}
