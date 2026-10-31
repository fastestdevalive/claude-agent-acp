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

/// The SDK's meta-origin exception (`hOe`/`ZA`, `sdk.mjs`): a meta record is
/// returned only for the replay-visible origin kinds, and a peer only when it
/// names its sender task. Every other meta record is dropped before it can
/// replay (caveats, skill expansions, harness framing).
fn meta_origin_kept(message: &Value) -> bool {
    let Some(kind) = message.pointer("/origin/kind").and_then(Value::as_str) else {
        return false;
    };
    match kind {
        "channel" | "observer" | "observer-activity" | "slack-ping" => true,
        "peer" => message.pointer("/origin/senderTaskId").is_some(),
        _ => false,
    }
}

/// The SDK `Iwe` filter's meta clause: drop `isMeta` unless the origin is a
/// replay-visible exception. The adapter hides those surviving origins during
/// replay (`isReplayHiddenMetaMessage`), so no meta record reaches the client.
fn is_filtered_meta_record(entry: &Value) -> bool {
    entry.get("isMeta").and_then(Value::as_bool) == Some(true) && !meta_origin_kept(entry)
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
fn strip_local_command_metadata(content: &Value) -> Option<Value> {
    match content {
        Value::String(s) => strip_local_command_metadata_text(s).map(Value::String),
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

/// Stream the transcript line by line (never `read_to_string`), and — when the
/// file exceeds `cap` — keep only the records from the last `compact_boundary`
/// on (the SDK's precompact skip, `Twe`/`HL`). The boundary record itself is
/// kept; its preserved-message anchors are gone, so the compaction re-linking
/// below skips it.
fn read_records_capped(root: &std::path::Path, session_id: &str, cap: u64) -> Vec<Record> {
    let mut out = Vec::new();
    let Some(path) = session_store::find_transcripts(root, session_id)
        .into_iter()
        .next()
    else {
        return out;
    };
    let skip_precompact = std::fs::metadata(&path)
        .map(|metadata| metadata.len() > cap)
        .unwrap_or(false);
    let Ok(file) = std::fs::File::open(&path) else {
        return out;
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
        }
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
    out
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
    let raw = read_records_capped(&root, session_id, cap);
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
    if let Some(leaf) = select_leaf(&records, &order) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut cursor = Some(leaf);
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
    }

    let completed_local_commands = completed_local_command_indices(&chain);
    let mut out = Vec::new();
    for (index, record) in chain.iter().enumerate() {
        let entry = &record.entry;
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "user" && kind != "assistant" {
            continue;
        }
        if entry.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if entry.get("teamName").is_some() {
            continue;
        }
        // The SDK `Iwe` filter drops every other meta record before replay
        // ever sees it (`sdk.mjs`): caveats, skill expansions and harness
        // framing must not replay as `user_message_chunk`.
        if is_filtered_meta_record(entry) {
            continue;
        }
        let mut mapped = entry.clone();
        let Some(map) = mapped.as_object_mut() else {
            // A malformed user/assistant record that is not a JSON object
            // cannot be mapped to the SDK output shape; Node would leave such
            // a record unmapped, so replay skips it.
            continue;
        };
        map.insert(
            "session_id".into(),
            entry.get("sessionId").cloned().unwrap_or(Value::Null),
        );
        map.insert("parent_tool_use_id".into(), Value::Null);
        map.insert("parent_agent_id".into(), Value::Null);
        // A compact summary and the meta/transcript-only flags are all meta
        // messages (`isReplayHiddenMetaMessage`, `acp-agent.js:945-947`): the
        // persisted record is marked `is_meta` and kept (compact summaries stay
        // visible as transcript text for a plain client, `acp-agent.js:938`).
        if entry.get("isCompactSummary") == Some(&Value::Bool(true))
            || entry.get("isMeta") == Some(&Value::Bool(true))
            || entry.get("isVisibleInTranscriptOnly") == Some(&Value::Bool(true))
        {
            map.insert("is_meta".into(), json!(true));
        }
        if completed_local_commands.contains(&index) {
            map.insert("isCompletedLocalCommand".into(), json!(true));
        }
        out.push(mapped);
    }
    out
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
pub fn replay_with_failures(
    messages: &[Value],
    session_id: &str,
    air: bool,
    plan_ops: bool,
    mut failures: Option<&mut crate::session::failures::SessionFailures>,
) -> (
    Vec<Value>,
    Vec<(String, crate::hooks::TaskStateEntry)>,
    Option<String>,
) {
    let active_limit_uuid = failures
        .as_ref()
        .and_then(|_| crate::session_failure::active_usage_limit_message(messages))
        .map(|(uuid, _)| uuid);
    let mut replay_turn_id: Option<String> = None;
    let mut out = Vec::new();
    let mut state = MapState::default();
    let mut task_state: Vec<(String, crate::hooks::TaskStateEntry)> = Vec::new();
    let mut last_published: Option<String> = None;
    let mut message_id = String::new();
    for message in messages {
        if is_hidden_meta_message(message) || is_task_notification_record(message) {
            continue;
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
                replay_turn_id = Some(uuid.to_string());
            }
        }
        if role == "assistant" && is_synthetic_login_message(message) {
            continue;
        }
        if let Some(book) = failures.as_deref_mut() {
            if role == "assistant"
                && top_level
                && crate::session_failure::is_synthetic_usage_limit_message(&payload)
            {
                if let Some(title) = crate::session_failure::assistant_message_text(&payload) {
                    let uuid = message.get("uuid").and_then(Value::as_str).unwrap_or("");
                    let id = match &replay_turn_id {
                        Some(turn) => format!("{turn}:error"),
                        None => format!("{session_id}:history-error:{uuid}"),
                    };
                    out.push(book.restore(
                        session_id,
                        &id,
                        crate::session_failure::ClaudeFailureKind::QuotaExhausted,
                        &title,
                        active_limit_uuid.as_deref() == Some(uuid),
                    ));
                }
                continue;
            }
        }
        let mut content = payload.get("content").cloned().unwrap_or(Value::Null);
        if role == "assistant" && parent_tool_use_id_of(message).is_some() {
            // Plain client without native subagent sessions: the nested
            // assistant's text/thinking is dropped (not forwarded to the root),
            // but its tool blocks still replay (`stripSubagentTextAndThinking`,
            // `acp-agent.js:6990-6996`).
            content = strip_subagent_text_and_thinking(&content);
        }
        if role == "user" {
            let Some(stripped) = strip_local_command_metadata(&content) else {
                continue;
            };
            content = stripped;
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
            message_id = id;
        }
        let updates = replay_content(
            &content,
            msg_role,
            &mut state,
            &mut task_state,
            ReplayFlags {
                air,
                plan_ops,
                top_level,
            },
            &mut last_published,
        );
        let message_id = if message_id.is_empty() {
            None
        } else {
            Some(message_id.clone())
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
            out.push(json!({ "sessionId": session_id, "update": update }));
        }
    }
    (out, task_state, last_published)
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
        // Caveat, skill expansion and the peer without a sender task are
        // dropped by the SDK filter; the replay-visible origins survive it
        // with `is_meta`, and replay hides them.
        assert_eq!(uuids, vec!["m3", "m4", "u1", "a1"]);
        assert_eq!(msgs[0]["is_meta"], json!(true));
        assert_eq!(msgs[1]["is_meta"], json!(true));
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
        let records = read_records_capped(&root, ID, 0);
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
        let records = read_records_capped(&root, ID, u64::MAX);
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
}
