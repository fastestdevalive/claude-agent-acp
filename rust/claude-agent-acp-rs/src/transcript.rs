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
//! Not ported (PARITY `pending`): the SDK's queue-command re-linking and
//! branch-turn walk, the `{dir}`-option quirk, native-subagent (AIR) replay and
//! the >5 MiB precompact-skip. The linear parentUuid chain, compaction
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
    let args = marker_tag_text(text, "command-args")?.trim().to_string();
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

/// A parsed transcript record that participates in the chain.
#[derive(Debug, Clone)]
struct Record {
    uuid: String,
    parent_uuid: Option<String>,
    entry: Value,
}

/// Parse the message-bearing transcript lines of `session_id` from its
/// (first-found) file, in file order, with the SDK's JSON and uuid tolerance.
fn read_records(root: &std::path::Path, session_id: &str) -> Vec<Record> {
    let mut out = Vec::new();
    let Some(path) = session_store::find_transcripts(root, session_id)
        .into_iter()
        .next()
    else {
        return out;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return out;
    };
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
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

/// Whether a record is a chain-candidate (a non-sidechain user/assistant).
fn is_chain_candidate(record: &Record) -> bool {
    if record.entry.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    let kind = record
        .entry
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("");
    kind == "user" || kind == "assistant"
}

/// Port of the SDK `getSessionMessages` core: build the ordered messages
/// (oldest first) for `session_id`'s transcript, dropping sidechain / teamName
/// records, and mapping each to the SDK output shape with
/// `parent_tool_use_id`/`parent_agent_id` always `null`.
pub fn session_messages(env: &EnvSnapshot, session_id: &str) -> Vec<Value> {
    if !session_store::is_session_id(session_id) {
        return Vec::new();
    }
    let root = session_store::projects_root_with_env(env);
    let raw = read_records(&root, session_id);
    let mut records: HashMap<String, Record> =
        raw.iter().map(|r| (r.uuid.clone(), r.clone())).collect();
    let order: HashMap<&str, usize> = raw
        .iter()
        .enumerate()
        .map(|(i, r)| (r.uuid.as_str(), i))
        .collect();
    apply_compaction_records(&mut records);

    // Walk from the latest user/assistant leaf up the parentUuid chain to the
    // root, then reverse (oldest first).
    let mut candidates: Vec<&Record> = records.values().filter(|r| is_chain_candidate(r)).collect();
    candidates.sort_by_key(|r| std::cmp::Reverse(order.get(r.uuid.as_str()).copied().unwrap_or(0)));
    let leaf = candidates.first().cloned();
    let mut chain: Vec<Record> = Vec::new();
    if let Some(leaf) = leaf {
        let mut seen: HashSet<String> = HashSet::new();
        let mut cursor = Some(leaf.clone());
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

    let mut out = Vec::new();
    for record in chain {
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
        out.push(mapped);
    }
    out
}

/// Give a batch of chunk updates each the group's `message_id`
/// (`apply_stream_message_id`, `acp-agent.js:9789-9800`). Reused by the replay
/// only (Node groups replay chunks under the API message id).
fn apply_replay_message_id(
    updates: Vec<agent_client_protocol::schema::v1::SessionUpdate>,
    message_id: &str,
) -> Vec<agent_client_protocol::schema::v1::SessionUpdate> {
    use agent_client_protocol::schema::v1::{MessageId, SessionUpdate};
    if message_id.is_empty() {
        return updates;
    }
    let mid = MessageId::new(message_id.to_string());
    updates
        .into_iter()
        .map(|u| match u {
            SessionUpdate::AgentMessageChunk(c) => {
                SessionUpdate::AgentMessageChunk(c.message_id(mid.clone()))
            }
            SessionUpdate::AgentThoughtChunk(c) => {
                SessionUpdate::AgentThoughtChunk(c.message_id(mid.clone()))
            }
            SessionUpdate::UserMessageChunk(c) => {
                SessionUpdate::UserMessageChunk(c.message_id(mid.clone()))
            }
            other => other,
        })
        .collect()
}

/// Build the ACP `session/update` notification JSON a `session/load` replay
/// sends (oldest first), as `{"sessionId", "update"}` values ready for
/// `session/update`. Mirrors `replaySessionHistory` → `toAcpNotifications` for
/// a plain client: hidden meta, synthetic login, task-notification and empty
/// marker-only user messages are skipped; a nested assistant's subagent
/// text/thinking is stripped; local-command metadata is stripped from user
/// content; chunks are grouped under the API message id.
pub fn replay_notifications(messages: &[Value], session_id: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut state = MapState::default();
    let mut message_id = String::new();
    for message in messages {
        if is_hidden_meta_message(message) || is_task_notification_record(message) {
            continue;
        }
        let role = message.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = message.get("message").cloned().unwrap_or(Value::Null);
        if role == "assistant" && is_synthetic_login_message(message) {
            continue;
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
        // For a user role, the consolidated map drops nothing; tool results and
        // text both map. For an assistant role, map the whole content
        // (unstreamedRemainder does not apply to replay: nothing streamed).
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
        let updates = apply_replay_message_id(
            map_consolidated(&content, msg_role, &mut state),
            &message_id,
        );
        for update in updates {
            let update_value = serde_json::to_value(update).unwrap_or(Value::Null);
            out.push(json!({ "sessionId": session_id, "update": update_value }));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

    /// Write a seed transcript under a fresh temp `CLAUDE_CONFIG_DIR` and return
    /// the env snapshot pointing at it (never the host's real `~/.claude`).
    struct TempProjects(std::path::PathBuf);

    impl TempProjects {
        fn new(body: &str) -> (Self, EnvSnapshot) {
            let n = std::sync::atomic::AtomicUsize::new(0);
            let n = n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
}
