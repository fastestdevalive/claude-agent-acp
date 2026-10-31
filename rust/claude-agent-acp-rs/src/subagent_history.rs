//! The subagent history of a session (`subagent-history.ts`, F37 / Q-60).
//!
//! Claude Code keeps each subagent's history in its own transcript,
//! `<projects>/<slug>/<session>/subagents/agent-<id>.jsonl`, next to the
//! session transcript. The `agent-<id>.meta.json` file beside it names the
//! Agent or Task call that launched it. The replay (`subagent_replay`) and the
//! restore of agents from before a load/resume (`native_subagents::runtime`)
//! read the history from these files.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::env_snapshot::EnvSnapshot;
use crate::native_subagents::runtime::{HistoricalSubagent, LaunchedAgentFn, LineageFn};
use crate::native_subagents::{is_native_subagent_control_tool, subagent_launch_ids};

/// What the metadata file of a subagent says about it (`SubagentRecord`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubagentRecord {
    /// The SDK agent id, the name of its transcript.
    pub agent_id: String,
    /// The `name` that the Agent call gave it.
    pub name: Option<String>,
    /// Its role, the `subagent_type` of the Agent call.
    pub agent_type: Option<String>,
    /// The short `description` of the Agent call.
    pub description: Option<String>,
    /// Whether the Agent call ran it in the background.
    pub background: bool,
    /// Whether the user stopped it.
    pub stopped_by_user: bool,
    /// The toolUseId from the metadata file.
    pub tool_use_id: Option<String>,
    /// The parentAgentId from the metadata file (if nested subagent).
    pub parent_agent_id: Option<String>,
}

/// Where the subagent transcripts of a session are (`SubagentHistory`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubagentHistory {
    /// The subagent of each Agent or Task tool use that launched one, by launch
    /// id (sorted, so the order is deterministic).
    pub agents: BTreeMap<String, SubagentRecord>,
    /// The project directory of the session, as the SDK takes it in `dir`.
    pub dir: Option<String>,
    /// The `subagents` directory the transcripts live in.
    pub subagents_dir: Option<PathBuf>,
    /// The session transcript the history was read from, so a lineage read over
    /// a cached history does not search the project directories again.
    pub transcript: Option<PathBuf>,
}

impl SubagentHistory {
    /// The record of the agent that the launch `launch_id` started.
    #[must_use]
    pub fn launched_by(&self, launch_id: &str) -> Option<&SubagentRecord> {
        self.agents.get(launch_id)
    }
}

/// Whether `path` is a regular file (follows symlinks like upstream `readFile`).
/// Special files (such as FIFOs) and directories return false.
pub(crate) fn is_regular_metadata_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

/// Finds the subagent transcripts of `session_id` (`subagentHistory`).
#[must_use]
pub fn subagent_history(session_id: &str, env: &EnvSnapshot) -> SubagentHistory {
    let mut history = SubagentHistory::default();
    let Some(transcript) = crate::resumed_session::find_transcript_with_env(session_id, env) else {
        return history;
    };
    let directory = subagents_directory(&transcript, session_id);
    let Ok(names) = std::fs::read_dir(&directory) else {
        return history;
    };
    for entry in names.flatten() {
        // A special file (such as a FIFO) or a directory is skipped: opening it
        // could block forever or fail. Symlinks are followed like upstream.
        if !is_regular_metadata_file(&entry.path()) {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(agent_id) = agent_id_of_meta_name(name) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            // A damaged meta file leaves its subagent without history.
            continue;
        };
        let Ok(meta) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(tool_use_id) = meta.get("toolUseId").and_then(Value::as_str) else {
            continue;
        };
        let text_field = |key: &str| {
            meta.get(key)
                .and_then(Value::as_str)
                .map(ToString::to_string)
        };
        history.agents.insert(
            tool_use_id.to_string(),
            SubagentRecord {
                agent_id: agent_id.to_string(),
                name: text_field("name"),
                agent_type: text_field("agentType"),
                description: text_field("description"),
                background: meta.get("requestShape").and_then(Value::as_str) == Some("background"),
                stopped_by_user: meta.get("stoppedByUser") == Some(&Value::Bool(true)),
                tool_use_id: Some(tool_use_id.to_string()),
                parent_agent_id: text_field("parentAgentId"),
            },
        );
    }
    if history.agents.is_empty() {
        return history;
    }
    history.subagents_dir = Some(directory);
    history.transcript = Some(transcript.clone());
    // The folder name encodes the project directory, and the encoding loses
    // characters; the session's own transcript carries the real cwd.
    history.dir = session_cwd(&transcript);
    history
}

/// The `subagents` folder of the session transcript `transcript`
/// (`subagentsDirectory`).
fn subagents_directory(transcript: &Path, session_id: &str) -> PathBuf {
    transcript
        .parent()
        .unwrap_or(Path::new("."))
        .join(session_id)
        .join("subagents")
}

/// The agent id of an `agent-<id>.meta.json` file name.
fn agent_id_of_meta_name(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("agent-")?;
    let id = rest.strip_suffix(".meta.json")?;
    (!id.is_empty()).then_some(id)
}

/// The `cwd` the session transcript records (`getSessionInfo(sessionId).cwd`).
/// Streamed: the first record that names one wins, and the read stops there
/// rather than loading a whole (possibly huge) transcript.
fn session_cwd(transcript: &Path) -> Option<String> {
    let file = std::fs::File::open(transcript).ok()?;
    for line in std::io::BufRead::lines(std::io::BufReader::new(file)) {
        let Ok(line) = line else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(cwd) = record.get("cwd").and_then(Value::as_str) {
            return Some(cwd.to_string());
        }
    }
    None
}

/// The agent that the Agent or Task call `launch_id` launched
/// (`subagentLaunchedBy`).
#[must_use]
pub fn subagent_launched_by(
    session_id: &str,
    launch_id: &str,
    env: &EnvSnapshot,
) -> Option<String> {
    subagent_history(session_id, env)
        .launched_by(launch_id)
        .map(|record| record.agent_id.clone())
}

/// A logger for the history readers (`{error(...)}` in the TypeScript port).
pub type HistoryLogger<'a> = Option<&'a crate::logger::Logger>;

fn log_error(logger: HistoryLogger<'_>, message: &str) {
    if let Some(logger) = logger {
        logger.log(message);
    }
}

/// The agent `agent_id` and its ancestors, root first, from the subagent
/// history of `session_id` (`subagentLineage`).
///
/// The runtime restores an agent from it when Claude Code resumes one that ran
/// before the session was loaded or resumed. The metadata file of an agent
/// names the call that launched it, but not the agent that made the call, so
/// the parent of a launch is the session whose transcript has the call: the
/// session's own transcript, or an agent's. The list is empty when the agent or
/// one of its ancestors has no metadata, or no transcript has its launch.
#[must_use]
pub fn subagent_lineage(
    session_id: &str,
    agent_id: &str,
    env: &EnvSnapshot,
    logger: HistoryLogger<'_>,
) -> Vec<HistoricalSubagent> {
    let history = subagent_history(session_id, env);
    subagent_lineage_with(&history, session_id, agent_id, env, logger)
}

/// [`subagent_lineage`] over a `history` the caller already read (the runtime
/// lookups cache one snapshot per session, so a lineage read does not re-scan
/// the project directories).
#[must_use]
pub fn subagent_lineage_with(
    history: &SubagentHistory,
    session_id: &str,
    agent_id: &str,
    env: &EnvSnapshot,
    logger: HistoryLogger<'_>,
) -> Vec<HistoricalSubagent> {
    let mut launch_of: BTreeMap<String, (String, SubagentRecord)> = BTreeMap::new();
    for (launch_id, record) in &history.agents {
        launch_of.insert(record.agent_id.clone(), (launch_id.clone(), record.clone()));
    }
    let (history_storage, launch_of) = if !launch_of.contains_key(agent_id) {
        let fresh = subagent_history(session_id, env);
        let mut fresh_launch_of = BTreeMap::new();
        for (launch_id, record) in &fresh.agents {
            fresh_launch_of.insert(record.agent_id.clone(), (launch_id.clone(), record.clone()));
        }
        if !fresh_launch_of.contains_key(agent_id) {
            return Vec::new();
        }
        (Some(fresh), fresh_launch_of)
    } else {
        (None, launch_of)
    };
    let history = history_storage.as_ref().unwrap_or(history);
    // The SDK's readers return only the active chain, which starts after the
    // last compaction, so the launch calls come from the transcript files.
    let Some(transcript) = history
        .transcript
        .clone()
        .or_else(|| crate::resumed_session::find_transcript_with_env(session_id, env))
    else {
        return Vec::new();
    };
    let root_launches = read_launch_ids_from_transcript(&transcript, None, logger);
    let mut agent_launches: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut launcher_of = |launch_id: &str| -> Option<Option<String>> {
        if root_launches.iter().any(|id| id == launch_id) {
            return Some(None);
        }
        for candidate in launch_of.keys() {
            let launches = agent_launches.entry(candidate.clone()).or_insert_with(|| {
                let file = history
                    .subagents_dir
                    .clone()
                    .unwrap_or_else(|| subagents_directory(&transcript, session_id))
                    .join(format!("agent-{candidate}.jsonl"));
                read_launch_ids_from_transcript(&file, Some(candidate), logger)
            });
            if launches.iter().any(|id| id == launch_id) {
                return Some(Some(candidate.clone()));
            }
        }
        None
    };

    let mut lineage: Vec<HistoricalSubagent> = Vec::new();
    let mut current = Some(agent_id.to_string());
    while let Some(agent) = current {
        let Some((launch_id, record)) = launch_of.get(&agent) else {
            return Vec::new();
        };
        // A cycle, or an ancestor without metadata.
        if lineage.iter().any(|entry| entry.agent_id == agent) {
            return Vec::new();
        }
        let Some(launcher) = launcher_of(launch_id) else {
            return Vec::new();
        };
        lineage.insert(
            0,
            HistoricalSubagent {
                agent_id: agent.clone(),
                launch_id: launch_id.clone(),
                parent_agent_id: launcher,
                // Upstream spreads only truthy metadata (`subagent-history.ts:139-141`):
                // an empty string does not fall back to the description.
                name: record.name.clone().filter(|s| !s.is_empty()),
                agent_type: record.agent_type.clone().filter(|s| !s.is_empty()),
                description: record.description.clone().filter(|s| !s.is_empty()),
            },
        );
        current = lineage[0].parent_agent_id.clone();
    }
    lineage
}

/// The ids of the Agent and Task calls that launched subagents in `content`
/// (`subagentLaunchIds`).
#[must_use]
pub fn launch_ids(content: &Value) -> Vec<String> {
    subagent_launch_ids(content)
}

/// The ids of the launch calls in the transcript `file`, every record of it,
/// including those before a compaction (`readLaunchIdsFromTranscript`).
///
/// `owner` is the agent the transcript belongs to, `None` for the session's.
/// The session's transcript can hold sidechain records, which are an agent's,
/// not the root's; an agent's transcript keeps only the records of that agent.
/// A file that cannot be read has no launches, so the agent is not placed under
/// the root.
fn read_launch_ids_from_transcript(
    file: &Path,
    owner: Option<&str>,
    logger: HistoryLogger<'_>,
) -> Vec<String> {
    let mut launches: Vec<String> = Vec::new();
    // A special file (such as a FIFO) or a directory is skipped: opening it could
    // block forever or fail. Symlinks are followed like upstream.
    match std::fs::metadata(file) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Vec::new(),
        Err(error) => {
            log_error(
                logger,
                &format!("Failed to read the transcript {}: {error}", file.display()),
            );
            return Vec::new();
        }
    }
    let Ok(handle) = std::fs::File::open(file) else {
        log_error(
            logger,
            &format!("Failed to read the transcript {}", file.display()),
        );
        return Vec::new();
    };
    for line in std::io::BufRead::lines(std::io::BufReader::new(handle)) {
        let Ok(line) = line else {
            continue;
        };
        let fork_directive = owner.is_some() && line.contains(FORK_BOILERPLATE_OPEN);
        if !line.contains("\"tool_use\"") && !fork_directive {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Every record of a fork child's transcript before its directive
        // message is inherited context: a copy of the launching message, whose
        // launch calls are the parent's, not the fork's.
        if fork_directive && is_fork_directive_message(&record) {
            launches.clear();
            continue;
        }
        if record.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let skip = match owner {
            None => record.get("isSidechain") == Some(&Value::Bool(true)),
            // `record.agentId !== undefined && record.agentId !== owner`: a
            // non-string id (`null`, a number) differs from the owner too.
            Some(owner) => record
                .get("agentId")
                .is_some_and(|agent| agent.as_str() != Some(owner)),
        };
        if skip {
            continue;
        }
        let content = record
            .get("message")
            .and_then(|m| m.get("content"))
            .cloned()
            .unwrap_or(Value::Null);
        for id in subagent_launch_ids(&content) {
            if !launches.contains(&id) {
                launches.push(id);
            }
        }
    }
    launches
}

/// The SDK `pu` regexp `/^[A-Za-z0-9_.:-]{1,128}$/` for parent tool use ids (`rR`).
fn valid_parent_tool_use_id(id: &str) -> Option<&str> {
    if !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b':' || b == b'-')
    {
        Some(id)
    } else {
        None
    }
}

/// The SDK `Cy` regexp `/^[\w-]{1,128}$/` for parent agent ids (`rR`).
fn valid_parent_agent_id(id: &str) -> Option<&str> {
    if !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        Some(id)
    } else {
        None
    }
}

/// A dedicated port of the SDK walk `gve` (`mve` -> `gve` -> `rR` in sdk303):
/// reads records line by line (keeping user/assistant/attachment with uuid),
/// selects the last user/assistant record in file order, and walks backwards
/// along `parentUuid` to the root (plain parent walk, no compaction relink,
/// cycles guarded). Stamped with `parent_tool_use_id` and `parent_agent_id`.
fn gve_walk_file(
    file: &Path,
    session_id: &str,
    parent_tool_use_id: Option<&str>,
    parent_agent_id: Option<&str>,
) -> Vec<Value> {
    let Ok(handle) = std::fs::File::open(file) else {
        return Vec::new();
    };
    let mut records: Vec<Value> = Vec::new();
    let mut uuid_to_idx: HashMap<String, usize> = HashMap::new();

    for line in std::io::BufRead::lines(std::io::BufReader::new(handle)) {
        let Ok(line) = line else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(kind) = record.get("type").and_then(Value::as_str) else {
            continue;
        };
        if kind != "user" && kind != "assistant" && kind != "attachment" {
            continue;
        }
        let Some(uuid) = record.get("uuid").and_then(Value::as_str) else {
            continue;
        };
        let idx = records.len();
        uuid_to_idx.insert(uuid.to_string(), idx);
        records.push(record);
    }

    if records.is_empty() {
        return Vec::new();
    }

    let last_idx = records.iter().rposition(|r| {
        let kind = r.get("type").and_then(Value::as_str);
        kind == Some("user") || kind == Some("assistant")
    });
    let Some(mut cur_idx) = last_idx else {
        return Vec::new();
    };

    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(cur_idx) {
            break;
        }
        chain.push(cur_idx);
        let parent_uuid = records[cur_idx]
            .get("parentUuid")
            .or_else(|| records[cur_idx].get("parent_uuid"))
            .and_then(Value::as_str);
        let Some(parent) = parent_uuid else {
            break;
        };
        let Some(&parent_idx) = uuid_to_idx.get(parent) else {
            break;
        };
        cur_idx = parent_idx;
    }
    chain.reverse();

    let mut out = Vec::new();
    for idx in chain {
        let rec = &records[idx];
        let kind = rec.get("type").and_then(Value::as_str);
        if kind != Some("user") && kind != Some("assistant") {
            continue;
        }
        let mut msg = crate::transcript::to_sdk_message(rec, parent_tool_use_id, parent_agent_id);
        if let Some(obj) = msg.as_object_mut() {
            obj.insert("session_id".into(), json!(session_id));
        }
        out.push(msg);
    }
    strip_fork_prefix(out)
}

/// The transcript of the subagent `agent_id` of a session, or an empty list
/// when it cannot be read (`readSubagentMessages`).
///
/// Upstream calls the SDK's `getSubagentMessages`, which returns the active
/// chain of the agent's transcript (`subagents/agent-<id>.jsonl`) in the SDK
/// output shape via the SDK `gve` walk; each child message carries the
/// metadata's `parent_tool_use_id` and `parent_agent_id`.
#[must_use]
pub fn read_subagent_messages(
    session_id: &str,
    agent_id: &str,
    history: &SubagentHistory,
    logger: HistoryLogger<'_>,
) -> Vec<Value> {
    let Some(directory) = &history.subagents_dir else {
        return Vec::new();
    };
    let file = directory.join(format!("agent-{agent_id}.jsonl"));
    // A special file (such as a FIFO) or a directory is skipped: opening it could
    // block forever or fail. Symlinks are followed like upstream.
    match std::fs::metadata(&file) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Vec::new(),
        Err(error) => {
            log_error(
                logger,
                &format!("Failed to read the history of subagent {agent_id}: {error}"),
            );
            return Vec::new();
        }
    }
    let (tool_use_id, parent_agent_id) =
        if let Some(record) = history.agents.values().find(|r| r.agent_id == agent_id) {
            (record.tool_use_id.clone(), record.parent_agent_id.clone())
        } else {
            let meta_file = directory.join(format!("agent-{agent_id}.meta.json"));
            if let Ok(text) = std::fs::read_to_string(&meta_file) {
                if let Ok(meta) = serde_json::from_str::<Value>(&text) {
                    let tool_use = meta
                        .get("toolUseId")
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                    let parent_agent = meta
                        .get("parentAgentId")
                        .and_then(Value::as_str)
                        .map(ToString::to_string);
                    (tool_use, parent_agent)
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            }
        };
    let parent_tool_use_id = tool_use_id.as_deref().and_then(valid_parent_tool_use_id);
    let parent_agent_id = parent_agent_id.as_deref().and_then(valid_parent_agent_id);
    gve_walk_file(&file, session_id, parent_tool_use_id, parent_agent_id)
}

/// How Claude Code opens the directive message of a `subagent_type: "fork"`
/// child.
pub const FORK_BOILERPLATE_OPEN: &str = "<fork-boilerplate>";
/// What follows the boilerplate, right before the prompt of the Agent call.
pub const FORK_DIRECTIVE_MARKER: &str = "</fork-boilerplate>\nYour directive: ";

/// Whether `block` is the text block of a fork child's directive message
/// (`isForkDirectiveText`).
fn is_fork_directive_text(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("text")
        && block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| text.starts_with(FORK_BOILERPLATE_OPEN))
}

/// Whether the transcript record `message` is the directive message of a fork
/// child (`isForkDirectiveMessage`).
fn is_fork_directive_message(message: &Value) -> bool {
    if message.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    message
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .is_some_and(|content| content.iter().any(is_fork_directive_text))
}

/// The transcript of a `subagent_type: "fork"` child without the context it
/// inherited (`stripForkPrefix`).
///
/// A fork transcript starts with a copy of the parent's launching assistant
/// message (its text, thinking and every tool_use, including the Agent call
/// itself), then a user message with a "Fork started — processing in
/// background" tool_result for each of those tool_uses and the text
/// `<fork-boilerplate>…</fork-boilerplate>\nYour directive: <prompt>`. A fork
/// launched from inside a subagent also gets a copy of that subagent's history
/// before them. None of it is the child's own work. This drops every record
/// before the directive message, and reduces that message to the prompt; other
/// transcripts are returned unchanged.
#[must_use]
pub fn strip_fork_prefix(messages: Vec<Value>) -> Vec<Value> {
    let Some(index) = messages.iter().position(is_fork_directive_message) else {
        return messages;
    };
    let directive = messages[index].clone();
    let content = directive
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let prompt: Vec<Value> = content
        .iter()
        .filter(|block| is_fork_directive_text(block))
        .filter_map(|block| {
            let text = block.get("text").and_then(Value::as_str)?;
            let start = text.find(FORK_DIRECTIVE_MARKER)?;
            Some(json!({ "type": "text", "text": &text[start + FORK_DIRECTIVE_MARKER.len()..] }))
        })
        .collect();
    let rest: Vec<Value> = messages.into_iter().skip(index + 1).collect();
    if prompt.is_empty() {
        return rest;
    }
    let mut reduced = directive;
    if let Some(message) = reduced.get_mut("message").and_then(Value::as_object_mut) {
        message.insert("content".to_string(), Value::Array(prompt));
    }
    let mut out = vec![reduced];
    out.extend(rest);
    out
}

/// The synchronous history lookups the native subagent runtime restores agents
/// from (B-98, `native-subagents.ts:721-870`).
///
/// Node awaits `subagentLineage`/`subagentLaunchedBy`; the crate's runtime is a
/// synchronous state machine. The history snapshot is read once and cached, so
/// a lineage lookup does not re-scan the project directories; `launched` serves
/// a positive hit from that snapshot and re-reads on a miss (upstream
/// `subagentLaunchedBy` re-reads every call), so a launch whose metadata file
/// appeared after the first lookup is still resolved.
pub fn history_lookups(
    session_id: &str,
    env: &EnvSnapshot,
) -> (Option<LineageFn>, Option<LaunchedAgentFn>) {
    let cache: std::sync::Arc<std::sync::OnceLock<SubagentHistory>> =
        std::sync::Arc::new(std::sync::OnceLock::new());
    let lineage_session = session_id.to_string();
    let lineage_env = env.clone();
    let lineage_cache = cache.clone();
    let lineage = Box::new(move |agent_id: &str| {
        let history =
            lineage_cache.get_or_init(|| subagent_history(&lineage_session, &lineage_env));
        let res = subagent_lineage_with(history, &lineage_session, agent_id, &lineage_env, None);
        if !res.is_empty() {
            return res;
        }
        // A miss: the metadata may have been written after the snapshot.
        let fresh = subagent_history(&lineage_session, &lineage_env);
        subagent_lineage_with(&fresh, &lineage_session, agent_id, &lineage_env, None)
    });
    let launched_session = session_id.to_string();
    let launched_env = env.clone();
    let launched_cache = cache;
    let launched = Box::new(move |launch_id: &str| {
        let history =
            launched_cache.get_or_init(|| subagent_history(&launched_session, &launched_env));
        if let Some(agent_id) = history.launched_by(launch_id) {
            return Some(agent_id.agent_id.clone());
        }
        // A miss: the metadata may have been written after the snapshot.
        subagent_history(&launched_session, &launched_env)
            .launched_by(launch_id)
            .map(|record| record.agent_id.clone())
    });
    (Some(lineage), Some(launched))
}

/// The native subagent runtime of a session, with the history lookups that
/// restore agents from before a load/resume (B-98, `native-subagents.ts:721-870`).
///
/// A client without native subagent sessions has no runtime and no lookups.
#[must_use]
pub fn native_runtime_with_history(
    form: Option<crate::client_caps::SubagentForm>,
    session_id: &str,
    env: &EnvSnapshot,
) -> crate::native_subagents::NativeSubagentRuntime {
    let runtime = crate::native_subagents::NativeSubagentRuntime::new(form, session_id);
    if form.is_none() {
        return runtime;
    }
    let (lineage, launched) = history_lookups(session_id, env);
    runtime.with_history(lineage, launched)
}

/// Whether `name` is the name of an Agent or Task tool (`isNativeSubagentControlTool`).
#[must_use]
pub fn is_launch_tool(name: Option<&str>) -> bool {
    is_native_subagent_control_tool(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "33333333-3333-4333-8333-333333333333";

    fn base(record: Value) -> Value {
        let mut map = json!({
            "sessionId": SESSION,
            "cwd": "/tmp/proj",
            "version": "2.0.0",
            "userType": "external",
            "isSidechain": false,
            "timestamp": "2026-10-13T00:00:00.000Z",
        });
        if let (Value::Object(target), Value::Object(other)) = (&mut map, record) {
            for (key, value) in other {
                target.insert(key, value);
            }
        }
        map
    }

    fn user(uuid: &str, text: &str) -> Value {
        base(json!({"type": "user", "uuid": uuid, "message": {"role": "user", "content": text}}))
    }

    fn launch(uuid: &str, launch_id: &str) -> Value {
        base(json!({
            "type": "assistant",
            "uuid": uuid,
            "message": {
                "id": format!("m-{uuid}"),
                "role": "assistant",
                "model": "claude-opus-5",
                "content": [{
                    "type": "tool_use",
                    "id": launch_id,
                    "name": "Agent",
                    "input": {"description": "Work", "prompt": "Work"},
                }],
            },
        }))
    }

    fn fork_directive(uuid: &str, launch_ids: &[&str], prompt: &str) -> Value {
        let mut content: Vec<Value> = launch_ids
            .iter()
            .map(|id| {
                json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": [{"type": "text", "text": "Fork started — processing in background"}],
                })
            })
            .collect();
        content.push(json!({
            "type": "text",
            "text": format!("<fork-boilerplate>\nYou are a worker fork.\n</fork-boilerplate>\nYour directive: {prompt}"),
        }));
        base(json!({"type": "user", "uuid": uuid, "message": {"role": "user", "content": content}}))
    }

    fn meta(tool_use_id: &str, name: &str) -> Value {
        json!({"agentType": "general-purpose", "description": "Work", "name": name, "toolUseId": tool_use_id})
    }

    fn boundary(uuid: &str) -> Value {
        base(json!({
            "type": "system",
            "subtype": "compact_boundary",
            "uuid": uuid,
            "parentUuid": null,
            "content": "Conversation compacted",
        }))
    }

    struct Fixture {
        dir: PathBuf,
        env: EnvSnapshot,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn fixture(tag: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("t6b-history-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("projects").join("-tmp-proj")).expect("dirs");
        let env = EnvSnapshot::from_pairs([(
            "CLAUDE_CONFIG_DIR".to_string(),
            dir.to_string_lossy().into_owned(),
        )]);
        Fixture { dir, env }
    }

    /// Writes the session transcript and, for each agent, its transcript and
    /// metadata.
    fn write_session(fx: &Fixture, root: Vec<Value>, agents: Vec<(&str, Value, Vec<Value>)>) {
        let directory = fx.dir.join("projects").join("-tmp-proj");
        std::fs::create_dir_all(directory.join(SESSION).join("subagents")).expect("subagents");
        let lines = |records: Vec<Value>| {
            let mut parent: Option<String> = None;
            let mut out = String::new();
            for record in records {
                let mut record = record;
                if record.get("parentUuid").is_none() {
                    if let Some(object) = record.as_object_mut() {
                        object.insert("parentUuid".to_string(), json!(parent));
                    }
                }
                parent = record
                    .get("uuid")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                out.push_str(&record.to_string());
                out.push('\n');
            }
            out
        };
        std::fs::write(directory.join(format!("{SESSION}.jsonl")), lines(root)).expect("root");
        for (agent_id, meta, records) in agents {
            let sidechain: Vec<Value> = records
                .into_iter()
                .map(|record| {
                    let mut record = record;
                    if let Some(object) = record.as_object_mut() {
                        object.insert("isSidechain".to_string(), json!(true));
                        object.insert("agentId".to_string(), json!(agent_id));
                    }
                    record
                })
                .collect();
            let subagents = directory.join(SESSION).join("subagents");
            std::fs::write(
                subagents.join(format!("agent-{agent_id}.jsonl")),
                lines(sidechain),
            )
            .expect("agent transcript");
            std::fs::write(
                subagents.join(format!("agent-{agent_id}.meta.json")),
                meta.to_string(),
            )
            .expect("meta");
        }
    }

    fn ids(lineage: &[HistoricalSubagent]) -> Vec<(String, String, Option<String>)> {
        lineage
            .iter()
            .map(|entry| {
                (
                    entry.agent_id.clone(),
                    entry.launch_id.clone(),
                    entry.parent_agent_id.clone(),
                )
            })
            .collect()
    }

    /// `subagentLineage` > "places an agent that the session launched under the root".
    #[test]
    fn places_an_agent_that_the_session_launched_under_the_root() {
        let fx = fixture("root");
        write_session(
            &fx,
            vec![user("u1", "Go"), launch("a1", "launch-echoer")],
            vec![(
                "echoer1",
                meta("launch-echoer", "echoer"),
                vec![user("e1", "Work")],
            )],
        );
        let lineage = subagent_lineage(SESSION, "echoer1", &fx.env, None);
        assert_eq!(
            lineage,
            vec![HistoricalSubagent {
                agent_id: "echoer1".into(),
                launch_id: "launch-echoer".into(),
                parent_agent_id: None,
                name: Some("echoer".into()),
                agent_type: Some("general-purpose".into()),
                description: Some("Work".into()),
            }]
        );
    }

    /// "places a nested agent under the agent whose transcript has its launch".
    #[test]
    fn places_a_nested_agent_root_first() {
        let fx = fixture("nested");
        write_session(
            &fx,
            vec![
                user("u1", "Go"),
                launch("a1", "launch-sibling"),
                launch("a2", "launch-coordinator"),
            ],
            vec![
                (
                    "sibling1",
                    meta("launch-sibling", "sibling"),
                    vec![user("s1", "Work")],
                ),
                (
                    "coordinator1",
                    meta("launch-coordinator", "coordinator"),
                    vec![user("c1", "Work"), launch("c2", "launch-worker")],
                ),
                (
                    "worker1",
                    meta("launch-worker", "worker"),
                    vec![user("w1", "Work")],
                ),
            ],
        );
        assert_eq!(
            ids(&subagent_lineage(SESSION, "worker1", &fx.env, None)),
            vec![
                (
                    "coordinator1".to_string(),
                    "launch-coordinator".to_string(),
                    None
                ),
                (
                    "worker1".to_string(),
                    "launch-worker".to_string(),
                    Some("coordinator1".to_string())
                ),
            ]
        );
    }

    /// "places an agent whose launch the root made before a compaction".
    #[test]
    fn places_an_agent_whose_launch_came_before_a_compaction() {
        let fx = fixture("compact");
        write_session(
            &fx,
            vec![
                user("u1", "Go"),
                launch("a1", "launch-echoer"),
                boundary("b1"),
                user("u2", "More"),
            ],
            vec![(
                "echoer1",
                meta("launch-echoer", "echoer"),
                vec![user("e1", "Work")],
            )],
        );
        assert_eq!(
            ids(&subagent_lineage(SESSION, "echoer1", &fx.env, None)),
            vec![("echoer1".to_string(), "launch-echoer".to_string(), None)]
        );
    }

    /// "places a nested agent whose launch its parent agent made before a compaction".
    #[test]
    fn places_a_nested_agent_whose_launch_came_before_a_compaction() {
        let fx = fixture("nested-compact");
        write_session(
            &fx,
            vec![user("u1", "Go"), launch("a1", "launch-coordinator")],
            vec![
                (
                    "coordinator1",
                    meta("launch-coordinator", "coordinator"),
                    vec![
                        user("c1", "Work"),
                        launch("c2", "launch-worker"),
                        boundary("c3"),
                        user("c4", "More"),
                    ],
                ),
                (
                    "worker1",
                    meta("launch-worker", "worker"),
                    vec![user("w1", "Work")],
                ),
            ],
        );
        assert_eq!(
            ids(&subagent_lineage(SESSION, "worker1", &fx.env, None)),
            vec![
                (
                    "coordinator1".to_string(),
                    "launch-coordinator".to_string(),
                    None
                ),
                (
                    "worker1".to_string(),
                    "launch-worker".to_string(),
                    Some("coordinator1".to_string())
                ),
            ]
        );
    }

    /// "does not take a launch in a sidechain record of the root file for the root's".
    #[test]
    fn ignores_a_sidechain_launch_in_the_root_file() {
        let fx = fixture("sidechain");
        let mut sidechain_launch = launch("a1", "launch-worker");
        if let Some(object) = sidechain_launch.as_object_mut() {
            object.insert("isSidechain".to_string(), json!(true));
        }
        write_session(
            &fx,
            vec![user("u1", "Go"), sidechain_launch],
            vec![(
                "worker1",
                meta("launch-worker", "worker"),
                vec![user("w1", "Work")],
            )],
        );
        assert!(subagent_lineage(SESSION, "worker1", &fx.env, None).is_empty());
    }

    /// "is empty for an agent whose launch no transcript has".
    #[test]
    fn is_empty_when_no_transcript_has_the_launch() {
        let fx = fixture("lost");
        write_session(
            &fx,
            vec![user("u1", "Go")],
            vec![(
                "lost1",
                meta("launch-lost", "lost"),
                vec![user("l1", "Work")],
            )],
        );
        assert!(subagent_lineage(SESSION, "lost1", &fx.env, None).is_empty());
    }

    /// "is empty for a nested agent when an ancestor's launch is in no transcript".
    #[test]
    fn is_empty_when_an_ancestor_has_no_transcript() {
        let fx = fixture("ancestor");
        write_session(
            &fx,
            vec![user("u1", "Go")],
            vec![
                (
                    "coordinator1",
                    meta("launch-coordinator", "coordinator"),
                    vec![user("c1", "Work"), launch("c2", "launch-worker")],
                ),
                (
                    "worker1",
                    meta("launch-worker", "worker"),
                    vec![user("w1", "Work")],
                ),
            ],
        );
        assert!(subagent_lineage(SESSION, "worker1", &fx.env, None).is_empty());
    }

    /// "does not take the launches copied into a fork's transcript for the fork's".
    #[test]
    fn ignores_launches_copied_into_a_fork_transcript() {
        let fx = fixture("fork");
        let coordinator_records = vec![user("c1", "Work"), launch("c2", "launch-worker")];
        let mut fork_records = coordinator_records.clone();
        fork_records.push(launch("f1", "launch-fork"));
        fork_records.push(fork_directive("f2", &["launch-fork"], "Inspect"));
        fork_records.push(user("f3", "own work"));
        let mut coordinator_meta = meta("launch-coordinator", "coordinator");
        if let Some(object) = coordinator_meta.as_object_mut() {
            object.insert("agentType".to_string(), json!("general-purpose"));
        }
        let mut fork_meta = meta("launch-fork", "fork");
        if let Some(object) = fork_meta.as_object_mut() {
            object.insert("agentType".to_string(), json!("fork"));
        }
        let mut coordinator_own = coordinator_records.clone();
        coordinator_own.push(launch("c3", "launch-fork"));
        write_session(
            &fx,
            vec![user("u1", "Go"), launch("a1", "launch-coordinator")],
            vec![
                ("afork1", fork_meta, fork_records),
                ("coordinator1", coordinator_meta, coordinator_own),
                (
                    "worker1",
                    meta("launch-worker", "worker"),
                    vec![user("w1", "Work")],
                ),
            ],
        );
        let parents = |agent: &str| -> Vec<Option<String>> {
            subagent_lineage(SESSION, agent, &fx.env, None)
                .into_iter()
                .map(|entry| entry.parent_agent_id)
                .collect()
        };
        assert_eq!(
            parents("worker1"),
            vec![None, Some("coordinator1".to_string())]
        );
        assert_eq!(
            parents("afork1"),
            vec![None, Some("coordinator1".to_string())]
        );
    }

    /// "is empty for an agent without metadata".
    #[test]
    fn is_empty_for_an_agent_without_metadata() {
        let fx = fixture("nommeta");
        write_session(
            &fx,
            vec![user("u1", "Go"), launch("a1", "launch-echoer")],
            vec![(
                "echoer1",
                meta("launch-echoer", "echoer"),
                vec![user("e1", "Work")],
            )],
        );
        assert!(subagent_lineage(SESSION, "other1", &fx.env, None).is_empty());
    }

    fn message(record: Value) -> Value {
        json!({
            "type": record["type"],
            "uuid": record["uuid"],
            "session_id": SESSION,
            "message": record["message"],
            "parent_tool_use_id": Value::Null,
            "parent_agent_id": Value::Null,
        })
    }

    /// `stripForkPrefix` > "drops the copy of the launching message and keeps only the directive's prompt".
    #[test]
    fn strip_drops_the_launching_copy_and_keeps_the_prompt() {
        let clone = message(launch("clone", "launch-fork"));
        let directive = message(fork_directive("d1", &["launch-fork"], "Inspect the parser"));
        let work = message(user("w1", "own work"));
        let stripped = strip_fork_prefix(vec![clone, directive.clone(), work.clone()]);
        assert_eq!(stripped.len(), 2);
        assert_eq!(stripped[0]["uuid"], "d1");
        assert_eq!(
            stripped[0]["message"],
            json!({"role": "user", "content": [{"type": "text", "text": "Inspect the parser"}]})
        );
        assert_eq!(stripped[1]["uuid"], "w1");
    }

    /// "drops the inherited history of a fork launched from inside a subagent".
    #[test]
    fn strip_drops_the_inherited_history() {
        let inherited = vec![
            message(user("c1", "Work")),
            message(launch("c2", "launch-worker")),
        ];
        let clone = message(launch("clone", "launch-fork"));
        let directive = message(fork_directive("d1", &["launch-fork"], "Inspect"));
        let work = message(user("w1", "own work"));
        let mut all = inherited;
        all.push(clone);
        all.push(directive);
        all.push(work);
        let stripped = strip_fork_prefix(all);
        let uuids: Vec<&str> = stripped
            .iter()
            .filter_map(|record| record["uuid"].as_str())
            .collect();
        assert_eq!(uuids, vec!["d1", "w1"]);
    }

    /// "returns a transcript without a fork directive unchanged".
    #[test]
    fn strip_returns_a_transcript_without_a_directive_unchanged() {
        let messages = vec![
            message(user("u1", "Work")),
            message(launch("a1", "launch-worker")),
        ];
        assert_eq!(strip_fork_prefix(messages.clone()), messages);
    }

    /// `subagentLaunchedBy` and `readSubagentMessages` read the agent's file.
    #[test]
    fn reads_the_launched_agent_and_its_messages() {
        let fx = fixture("read");
        write_session(
            &fx,
            vec![user("u1", "Go"), launch("a1", "launch-echoer")],
            vec![(
                "echoer1",
                meta("launch-echoer", "echoer"),
                vec![
                    user("e1", "Look around"),
                    base(json!({
                        "type": "assistant",
                        "uuid": "e2",
                        "message": {"id": "m-e2", "role": "assistant",
                            "content": [{"type": "text", "text": "Report."}]},
                    })),
                ],
            )],
        );
        assert_eq!(
            subagent_launched_by(SESSION, "launch-echoer", &fx.env),
            Some("echoer1".to_string())
        );
        let history = subagent_history(SESSION, &fx.env);
        assert_eq!(history.dir.as_deref(), Some("/tmp/proj"));
        let messages = read_subagent_messages(SESSION, "echoer1", &history, None);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["uuid"], "e1");
        assert_eq!(messages[1]["message"]["content"][0]["text"], "Report.");
        // A missing transcript is an empty list, not an error.
        assert!(read_subagent_messages(SESSION, "gone", &history, None).is_empty());
    }

    /// T6FB2 LOW 11: an empty `name`/`agentType`/`description` is dropped, so
    /// the display name falls back (`subagent-history.ts:139-141`).
    #[test]
    fn empty_metadata_values_are_dropped() {
        let fx = fixture("emptymeta");
        write_session(
            &fx,
            vec![user("u1", "Go"), launch("a1", "launch-echoer")],
            vec![(
                "echoer1",
                json!({"agentType": "", "description": "", "name": "",
                    "toolUseId": "launch-echoer"}),
                vec![user("e1", "Work")],
            )],
        );
        let lineage = subagent_lineage(SESSION, "echoer1", &fx.env, None);
        assert_eq!(lineage.len(), 1);
        assert_eq!(lineage[0].name, None);
        assert_eq!(lineage[0].agent_type, None);
        assert_eq!(lineage[0].description, None);
    }

    /// T6FB2 LOW 12: `record.agentId !== undefined && record.agentId !== owner`
    /// skips a defined non-string id (`null`) too.
    #[test]
    fn a_non_string_agent_id_is_not_the_owner() {
        let fx = fixture("agentid");
        let directory = fx.dir.join("projects").join("-tmp-proj");
        std::fs::create_dir_all(&directory).expect("dirs");
        let file = directory.join("agent-null.jsonl");
        let mut record = launch("c2", "launch-worker");
        if let Some(object) = record.as_object_mut() {
            object.insert("agentId".to_string(), Value::Null);
        }
        std::fs::write(&file, format!("{}\n", record)).expect("transcript");
        assert!(read_launch_ids_from_transcript(&file, Some("null"), None).is_empty());
        // The same id as a string is the owner's, so its launch counts.
        if let Some(object) = record.as_object_mut() {
            object.insert("agentId".to_string(), json!("null"));
        }
        std::fs::write(&file, format!("{}\n", record)).expect("transcript");
        assert_eq!(
            read_launch_ids_from_transcript(&file, Some("null"), None),
            vec!["launch-worker".to_string()]
        );
    }

    /// T6FB2 MEDIUM 7: `readSubagentMessages` is the SDK's
    /// `getSubagentMessages` — the active chain from the leaf, `user`/`assistant`
    /// only, so a rewound branch and the other record kinds stay out.
    #[test]
    fn reads_only_the_active_chain_of_a_rewound_agent_transcript() {
        let fx = fixture("rewound");
        let mut abandoned = base(json!({
            "type": "assistant",
            "uuid": "e2",
            "parentUuid": "e1",
            "message": {"role": "assistant",
                "content": [{"type": "text", "text": "abandoned"}]},
        }));
        if let Some(object) = abandoned.as_object_mut() {
            object.insert("parentUuid".to_string(), json!("e1"));
        }
        let mut rewound = user("e3", "More");
        if let Some(object) = rewound.as_object_mut() {
            object.insert("parentUuid".to_string(), json!("e1"));
        }
        let mut active = base(json!({
            "type": "assistant",
            "uuid": "e4",
            "message": {"role": "assistant",
                "content": [{"type": "text", "text": "active"}]},
        }));
        if let Some(object) = active.as_object_mut() {
            object.insert("parentUuid".to_string(), json!("e3"));
        }
        let progress = base(json!({
            "type": "progress",
            "uuid": "e5",
            "parentUuid": "e4",
            "content": "progress",
        }));
        write_session(
            &fx,
            vec![user("u1", "Go"), launch("a1", "launch-echoer")],
            vec![(
                "echoer1",
                meta("launch-echoer", "echoer"),
                vec![user("e1", "Work"), abandoned, rewound, active, progress],
            )],
        );
        let history = subagent_history(SESSION, &fx.env);
        let messages = read_subagent_messages(SESSION, "echoer1", &history, None);
        let uuids: Vec<&str> = messages
            .iter()
            .filter_map(|message| message["uuid"].as_str())
            .collect();
        assert_eq!(uuids, vec!["e1", "e3", "e4"]);
        assert_eq!(messages[2]["message"]["content"][0]["text"], "active");
        assert_eq!(messages[0]["session_id"], json!(SESSION));
        assert_eq!(messages[0]["parent_tool_use_id"], json!("launch-echoer"));
    }

    /// T6FB2 MEDIUM 5 / T6FB3 N6: a non-regular entry (`agent-x.meta.json` a directory,
    /// `agent-x.jsonl` a directory) is skipped, so a FIFO/special file of the same
    /// name cannot block the actor.
    #[test]
    fn a_non_regular_metadata_or_transcript_entry_is_skipped() {
        let fx = fixture("special");
        write_session(
            &fx,
            vec![user("u1", "Go"), launch("a1", "launch-echoer")],
            vec![(
                "echoer1",
                meta("launch-echoer", "echoer"),
                vec![user("e1", "Work")],
            )],
        );
        let subagents = fx
            .dir
            .join("projects")
            .join("-tmp-proj")
            .join(SESSION)
            .join("subagents");
        let bogus_meta = subagents.join("agent-bogus.meta.json");
        let bogus_jsonl = subagents.join("agent-bogus.jsonl");
        std::fs::create_dir_all(&bogus_meta).expect("dir");
        std::fs::create_dir_all(&bogus_jsonl).expect("dir");
        // Test helper directly on regular file, directory, and non-existent file
        assert!(!is_regular_metadata_file(&bogus_meta));
        assert!(is_regular_metadata_file(
            &subagents.join("agent-echoer1.meta.json")
        ));
        assert!(!is_regular_metadata_file(
            &subagents.join("agent-absent.meta.json")
        ));
        let history = subagent_history(SESSION, &fx.env);
        assert_eq!(history.agents.len(), 1);
        assert!(read_subagent_messages(SESSION, "bogus", &history, None).is_empty());
    }

    /// T6FB3 STEP 5 (N7): a lineage lookup that misses re-reads the history, so a metadata
    /// file written after the first lookup still resolves.
    #[test]
    fn a_lineage_lookup_miss_re_reads_the_history() {
        let fx = fixture("lineage_miss");
        write_session(
            &fx,
            vec![user("u1", "Go"), launch("a1", "launch-late")],
            vec![],
        );
        let (lineage, _) = history_lookups(SESSION, &fx.env);
        let lineage = lineage.expect("a lineage lookup");
        assert!(lineage("late1").is_empty());
        let subagents = fx
            .dir
            .join("projects")
            .join("-tmp-proj")
            .join(SESSION)
            .join("subagents");
        std::fs::write(
            subagents.join("agent-late1.meta.json"),
            meta("launch-late", "late").to_string(),
        )
        .expect("meta");
        assert_eq!(lineage("late1").len(), 1);
        assert_eq!(lineage("late1")[0].agent_id, "late1");
    }

    /// T6FB2 MEDIUM 6: a lookup that misses re-reads the history, so a metadata
    /// file written after the first lookup still resolves (`subagentLaunchedBy`
    /// re-reads every call upstream).
    #[test]
    fn a_lookup_miss_re_reads_the_history() {
        let fx = fixture("miss");
        write_session(
            &fx,
            vec![user("u1", "Go"), launch("a1", "launch-late")],
            vec![],
        );
        let (_, launched) = history_lookups(SESSION, &fx.env);
        let launched = launched.expect("a launched-agent lookup");
        assert_eq!(launched("launch-late"), None);
        let subagents = fx
            .dir
            .join("projects")
            .join("-tmp-proj")
            .join(SESSION)
            .join("subagents");
        std::fs::write(
            subagents.join("agent-late1.meta.json"),
            meta("launch-late", "late").to_string(),
        )
        .expect("meta");
        assert_eq!(launched("launch-late"), Some("late1".to_string()));
    }
}
