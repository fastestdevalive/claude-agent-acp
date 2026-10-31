//! Resumed session tail reader (rust-parity-fixes Decision 19).
//!
//! Port of `dist/resumed-session.js` `readResumedTail` and `TailScan`:
//! scans the JSONL transcript backwards from the end (in 64 KiB chunks)
//! to find:
//! 1. The concrete model of the last top-level assistant response
//! 2. The permission mode of the main thread (`user` frame with `permissionMode`),
//!    accounting for `plan_mode_exit`.

use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const TAIL_CHUNK_BYTES: usize = 64 * 1024;

/// Result of reading a resumed session's transcript tail.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResumedTail {
    pub model: Option<String>,
    pub permission_mode: Option<String>,
}

/// The concrete model name, or None for missing, empty, or synthetic `<...>` model.
pub fn concrete_model(model: Option<&str>) -> Option<String> {
    let m = model?.trim();
    if m.is_empty() {
        return None;
    }
    // FA3-15: Node's synthetic test is `/^<[^>]+>$/` — the interior must be
    // non-empty and contain no `>`, so `<a>b>` and `<>` are real names.
    if m.starts_with('<') && m.ends_with('>') && m.len() > 2 && !m[1..m.len() - 1].contains('>') {
        return None;
    }
    Some(m.to_string())
}

/// Find the transcript file for `session_id` under any project directory using an env snapshot.
pub fn find_transcript_with_env(
    session_id: &str,
    env: &crate::env_snapshot::EnvSnapshot,
) -> Option<PathBuf> {
    let root = crate::session_store::projects_root_with_env(env);
    crate::session_store::find_transcripts(&root, session_id)
        .into_iter()
        .next()
}

/// Find the transcript file for `session_id` under any project directory.
pub fn find_transcript(session_id: &str) -> Option<PathBuf> {
    find_transcript_with_env(
        session_id,
        &crate::env_snapshot::EnvSnapshot::from_process(),
    )
}

/// A full local transcript snapshot of a resumed session (`readResumedSession`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResumedSessionSnapshot {
    pub messages: Vec<Value>,
    pub model: Option<String>,
}

/// `resumedModelFromTranscript` (`resumed-session.ts:27-42`): the concrete model
/// of the last real top-level assistant response, else `None`.
pub fn resumed_model_from_transcript(messages: &[Value]) -> Option<String> {
    for entry in messages.iter().rev() {
        if entry.get("type").and_then(Value::as_str) != Some("assistant")
            || entry
                .get("parent_tool_use_id")
                .is_some_and(|v| !v.is_null())
            || entry.get("parent_agent_id").is_some_and(|v| !v.is_null())
        {
            continue;
        }
        let Some(message) = entry.get("message").filter(|m| m.is_object()) else {
            continue;
        };
        if let Some(model) = concrete_model(message.get("model").and_then(Value::as_str)) {
            return Some(model);
        }
    }
    None
}

/// `readResumedSession` (`resumed-session.ts:47-66`): the SDK
/// `getSessionMessages` snapshot plus the model it records. A read failure is
/// an empty snapshot (Node logs and returns `{}`).
pub fn read_resumed_session_with_env(
    session_id: &str,
    env: &crate::env_snapshot::EnvSnapshot,
) -> ResumedSessionSnapshot {
    let messages = crate::transcript::session_messages(env, session_id);
    let model = resumed_model_from_transcript(&messages);
    ResumedSessionSnapshot { messages, model }
}

/// [`read_resumed_session_with_env`] against the process environment.
pub fn read_resumed_session(session_id: &str) -> ResumedSessionSnapshot {
    read_resumed_session_with_env(
        session_id,
        &crate::env_snapshot::EnvSnapshot::from_process(),
    )
}

/// Read the resume model and the permission mode of a session with an env snapshot.
///
/// A session without a local transcript file falls back to a full-transcript
/// read for the model (`readResumedTail`, `resumed-session.ts:97-110`); a
/// caller that already has the model (`with_model == false`) gets no fallback.
pub fn read_resumed_tail_with_env(
    session_id: &str,
    with_model: bool,
    env: &crate::env_snapshot::EnvSnapshot,
) -> ResumedTail {
    if let Some(path) = find_transcript_with_env(session_id, env) {
        if let Ok(tail) = read_tail_from_file(&path, with_model) {
            return tail;
        }
    }
    if with_model {
        ResumedTail {
            model: read_resumed_session_with_env(session_id, env).model,
            permission_mode: None,
        }
    } else {
        ResumedTail::default()
    }
}

/// Read the resume model and the permission mode of a session from the end of its local transcript.
pub fn read_resumed_tail(session_id: &str, with_model: bool) -> ResumedTail {
    read_resumed_tail_with_env(
        session_id,
        with_model,
        &crate::env_snapshot::EnvSnapshot::from_process(),
    )
}

/// Read the tail records from a specific file path.
pub fn read_tail_from_file(path: &Path, with_model: bool) -> std::io::Result<ResumedTail> {
    let mut file = File::open(path)?;
    let mut end = file.metadata()?.len();
    let mut tail = ResumedTail::default();
    let mut mode_done = false;
    let mut plan_exited = false;
    let mut pending = Vec::new();

    while end > 0 {
        let chunk_size = std::cmp::min(end, TAIL_CHUNK_BYTES as u64) as usize;
        let start = end - chunk_size as u64;
        file.seek(SeekFrom::Start(start))?;
        let mut chunk = vec![0u8; chunk_size];
        file.read_exact(&mut chunk)?;

        let mut at = chunk.len();
        while let Some(pos) = chunk[..at].iter().rposition(|&b| b == b'\n') {
            let mut line_bytes = chunk[pos + 1..at].to_vec();
            line_bytes.extend_from_slice(&pending);
            pending.clear();
            if visit_line(
                &line_bytes,
                with_model,
                &mut tail,
                &mut mode_done,
                &mut plan_exited,
            ) {
                return Ok(tail);
            }
            at = pos;
        }
        let mut rem = chunk[..at].to_vec();
        rem.extend_from_slice(&pending);
        pending = rem;
        end = start;
    }

    if !pending.is_empty() {
        visit_line(
            &pending,
            with_model,
            &mut tail,
            &mut mode_done,
            &mut plan_exited,
        );
    }

    Ok(tail)
}

fn parse_main_thread_record(line_str: &str) -> Option<Value> {
    let val: Value = serde_json::from_str(line_str).ok()?;
    if val.as_object().is_some() && val.get("isSidechain") != Some(&Value::Bool(true)) {
        Some(val)
    } else {
        None
    }
}

fn visit_line(
    line: &[u8],
    with_model: bool,
    tail: &mut ResumedTail,
    mode_done: &mut bool,
    plan_exited: &mut bool,
) -> bool {
    let Ok(line_str) = std::str::from_utf8(line) else {
        return false;
    };

    let mut parsed: Option<Value> = None;

    if with_model && tail.model.is_none() && line_str.contains("\"assistant\"") {
        if parsed.is_none() {
            parsed = parse_main_thread_record(line_str);
        }
        if let Some(entry) = &parsed {
            if entry.get("type").and_then(Value::as_str) == Some("assistant")
                && entry.get("parent_tool_use_id").is_none_or(Value::is_null)
                && entry.get("parent_agent_id").is_none_or(Value::is_null)
            {
                let m = entry
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(Value::as_str);
                if let Some(concrete) = concrete_model(m) {
                    tail.model = Some(concrete);
                }
            }
        }
    }

    if !*mode_done {
        if line_str.contains("\"permissionMode\"") {
            if parsed.is_none() {
                parsed = parse_main_thread_record(line_str);
            }
            if let Some(entry) = &parsed {
                if entry.get("type").and_then(Value::as_str) == Some("user") {
                    if let Some(pm) = entry.get("permissionMode").and_then(Value::as_str) {
                        let trimmed = pm.trim();
                        if !trimmed.is_empty() {
                            *mode_done = true;
                            if !(*plan_exited && trimmed == "plan") {
                                tail.permission_mode = Some(trimmed.to_string());
                            }
                        }
                    }
                }
            }
        }
        if !*mode_done && line_str.contains("\"plan_mode_exit\"") {
            if parsed.is_none() {
                parsed = parse_main_thread_record(line_str);
            }
            if let Some(entry) = &parsed {
                if entry
                    .get("attachment")
                    .and_then(|a| a.get("type"))
                    .and_then(Value::as_str)
                    == Some("plan_mode_exit")
                {
                    *plan_exited = true;
                }
            }
        }
        if !*mode_done && line_str.contains("\"human\"") {
            if parsed.is_none() {
                parsed = parse_main_thread_record(line_str);
            }
            if let Some(entry) = &parsed {
                if entry.get("type").and_then(Value::as_str) == Some("user")
                    && entry
                        .get("origin")
                        .and_then(|o| o.get("kind"))
                        .and_then(Value::as_str)
                        == Some("human")
                {
                    *mode_done = true;
                }
            }
        }
    }

    (!with_model || tail.model.is_some()) && *mode_done
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn concrete_model_filtering() {
        assert_eq!(
            concrete_model(Some("claude-sonnet-4-5")),
            Some("claude-sonnet-4-5".to_string())
        );
        assert_eq!(
            concrete_model(Some("  claude-haiku-4-5  ")),
            Some("claude-haiku-4-5".to_string())
        );
        assert_eq!(concrete_model(Some("<synthetic>")), None);
        assert_eq!(concrete_model(Some("<custom-placeholder>")), None);
        // FA3-15: Node's `/^<[^>]+>$/` only rejects a simple synthetic name.
        assert_eq!(concrete_model(Some("<a>b>")), Some("<a>b>".to_string()));
        assert_eq!(concrete_model(Some("<>")), Some("<>".to_string()));
        assert_eq!(concrete_model(Some("")), None);
        assert_eq!(concrete_model(Some("   ")), None);
        assert_eq!(concrete_model(None), None);
    }

    #[test]
    fn scan_tail_from_file_model_and_permission() {
        let path =
            std::env::temp_dir().join(format!("scan_tail_test_{}.jsonl", std::process::id()));
        let mut file = File::create(&path).unwrap();
        let lines = [
            r#"{"type":"user","permissionMode":"default","origin":{"kind":"human"},"message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"model":"claude-3-haiku-20240307","role":"assistant"}}"#,
            r#"{"type":"user","permissionMode":"acceptEdits","message":{"role":"user","content":"do edit"}}"#,
            r#"{"type":"assistant","parent_tool_use_id":"toolu_1","message":{"model":"claude-subagent","role":"assistant"}}"#,
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"model":"<synthetic>","role":"assistant"}}"#,
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"model":"claude-opus-4-20250514","role":"assistant"}}"#,
            r#"{"type":"result","subtype":"success","result":"done"}"#,
        ];
        for l in lines {
            writeln!(file, "{l}").unwrap();
        }
        file.flush().unwrap();
        drop(file);

        let tail = read_tail_from_file(&path, true).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(tail.model.as_deref(), Some("claude-opus-4-20250514"));
        assert_eq!(tail.permission_mode.as_deref(), Some("acceptEdits"));
    }

    /// O-13 — `readResumedSession` returns the SDK message snapshot and the
    /// model of the last real top-level assistant response; a synthetic or a
    /// nested assistant record does not describe the resumed model.
    #[test]
    fn o13_read_resumed_session_model() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("o13-{}-{n}", std::process::id()));
        let slug = dir.join("projects").join("-p");
        std::fs::create_dir_all(&slug).unwrap();
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let body = [
            r#"{"type":"user","uuid":"u1","parentUuid":null,"sessionId":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","message":{"role":"assistant","model":"claude-sonnet-4-5","content":[{"type":"text","text":"hello"}]}}"#,
            r#"{"type":"assistant","uuid":"a2","parentUuid":"a1","sessionId":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"x"}]}}"#,
        ]
        .join("\n");
        std::fs::write(slug.join(format!("{id}.jsonl")), format!("{body}\n")).unwrap();
        let env = crate::env_snapshot::EnvSnapshot::from_pairs([(
            "CLAUDE_CONFIG_DIR".to_string(),
            dir.to_string_lossy().into_owned(),
        )]);
        let snapshot = read_resumed_session_with_env(id, &env);
        assert_eq!(snapshot.messages.len(), 3);
        assert_eq!(snapshot.model.as_deref(), Some("claude-sonnet-4-5"));
        // The tail read uses the same file and reports the same model.
        let tail = read_resumed_tail_with_env(id, true, &env);
        assert_eq!(tail.model.as_deref(), Some("claude-sonnet-4-5"));
        // A missing transcript falls back to a full read: no messages, no model.
        let missing =
            read_resumed_tail_with_env("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", true, &env);
        assert_eq!(missing, ResumedTail::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resumed_model_skips_nested_and_synthetic_records() {
        let messages = vec![
            serde_json::json!({"type":"assistant","parent_tool_use_id":null,
                "message":{"model":"claude-haiku-4-5"}}),
            serde_json::json!({"type":"assistant","parent_tool_use_id":"toolu_1",
                "message":{"model":"claude-subagent"}}),
            serde_json::json!({"type":"assistant","parent_agent_id":"agent-1",
                "message":{"model":"claude-subagent"}}),
            serde_json::json!({"type":"assistant","parent_tool_use_id":null,
                "message":{"model":"<synthetic>"}}),
        ];
        assert_eq!(
            resumed_model_from_transcript(&messages).as_deref(),
            Some("claude-haiku-4-5")
        );
        assert_eq!(resumed_model_from_transcript(&[]), None);
    }

    /// Write `lines` to a fresh transcript file and read its tail.
    fn tail_of(tag: &str, lines: &[String], with_model: bool) -> ResumedTail {
        let path = std::env::temp_dir().join(format!(
            "t9a_tail_{tag}_{}_{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut file = File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        file.flush().unwrap();
        drop(file);
        let tail = read_tail_from_file(&path, with_model).unwrap();
        let _ = std::fs::remove_file(&path);
        tail
    }

    fn assistant_record(model: &str, extra: &str) -> String {
        format!(
            r#"{{"type":"assistant","parent_tool_use_id":null,"message":{{"role":"assistant","model":"{model}","content":[{{"type":"text","text":"ok"}}]}}{extra}}}"#
        )
    }

    fn user_record(text: &str, extra: &str) -> String {
        format!(r#"{{"type":"user","message":{{"role":"user","content":"{text}"}}{extra}}}"#)
    }

    /// `resumed-session.test.ts` > readResumedTail > "reads a record that is
    /// longer than one backward read": a record above the 64 KiB chunk is joined
    /// across chunks.
    #[test]
    fn t9a_tail_reads_a_record_longer_than_one_chunk() {
        let padding = "y".repeat(200 * 1024);
        let lines = vec![
            assistant_record("claude-opus-5", &format!(r#","padding":"{padding}""#)),
            user_record("after", ""),
        ];
        assert_eq!(
            tail_of("wide", &lines, true).model.as_deref(),
            Some("claude-opus-5")
        );
    }

    /// "reads the first line of the file": a single record is the pending
    /// remainder after the only chunk.
    #[test]
    fn t9a_tail_reads_the_first_line() {
        let lines = vec![assistant_record("claude-opus-5", "")];
        assert_eq!(
            tail_of("single", &lines, true).model.as_deref(),
            Some("claude-opus-5")
        );
    }

    /// "returns undefined when no real model is recorded": only a user record
    /// and a synthetic assistant record.
    #[test]
    fn t9a_tail_no_real_model_is_none() {
        let lines = vec![
            user_record("hello", ""),
            assistant_record("<synthetic>", ""),
        ];
        let tail = tail_of("none", &lines, true);
        assert_eq!(tail.model, None);
        assert_eq!(tail.permission_mode, None);
    }

    /// "reads a permission mode that is before the last model": the backward
    /// read passes 200 records to find the mode.
    #[test]
    fn t9a_tail_mode_before_the_model() {
        let mut lines = vec![user_record(
            "first",
            r#","permissionMode":"acceptEdits","origin":{"kind":"human"}"#,
        )];
        lines.extend((0..200).map(|_| user_record(&"x".repeat(1000), "")));
        lines.push(assistant_record("claude-opus-5", ""));
        let tail = tail_of("early-mode", &lines, true);
        assert_eq!(tail.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(tail.permission_mode.as_deref(), Some("acceptEdits"));
    }

    /// "ignores the permission-mode records of the CLI metadata block": the
    /// `type: "permission-mode"` metadata record is not a user prompt.
    #[test]
    fn t9a_tail_ignores_the_cli_metadata_mode_block() {
        let lines = vec![
            user_record(
                "first",
                r#","permissionMode":"bypassPermissions","origin":{"kind":"human"}"#,
            ),
            assistant_record("claude-opus-5", ""),
            r#"{"type":"permission-mode","permissionMode":"auto","sessionId":"metadata-mode"}"#
                .to_string(),
        ];
        assert_eq!(
            tail_of("metadata-mode", &lines, true)
                .permission_mode
                .as_deref(),
            Some("bypassPermissions")
        );
    }

    /// "stops the mode search at the last human prompt without a mode": an older
    /// prompt's mode is not resurrected.
    #[test]
    fn t9a_tail_stops_at_a_human_prompt_without_a_mode() {
        let lines = vec![
            user_record(
                "old",
                r#","permissionMode":"bypassPermissions","origin":{"kind":"human"}"#,
            ),
            user_record("new", r#","origin":{"kind":"human"}"#),
            assistant_record("claude-opus-5", ""),
        ];
        let tail = tail_of("bounded", &lines, true);
        assert_eq!(tail.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(tail.permission_mode, None);
    }

    /// "reads the mode of the prompt before a compaction": the compact summary
    /// is not a prompt, so the mode before the boundary is the session's.
    #[test]
    fn t9a_tail_reads_the_mode_before_a_compaction() {
        let lines = vec![
            user_record(
                "old",
                r#","permissionMode":"plan","origin":{"kind":"human"}"#,
            ),
            r#"{"type":"system","subtype":"compact_boundary"}"#.to_string(),
            user_record("summary", r#","isCompactSummary":true"#),
            assistant_record("claude-opus-5", ""),
        ];
        assert_eq!(
            tail_of("compacted", &lines, true)
                .permission_mode
                .as_deref(),
            Some("plan")
        );
    }

    /// "returns no permission mode when the transcript records none".
    #[test]
    fn t9a_tail_without_a_mode_is_none() {
        let lines = vec![
            user_record("hello", ""),
            assistant_record("claude-opus-5", ""),
        ];
        let tail = tail_of("no-mode", &lines, true);
        assert_eq!(tail.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(tail.permission_mode, None);
    }

    #[test]
    fn scan_tail_plan_mode_exit_clears_plan_mode() {
        let path =
            std::env::temp_dir().join(format!("scan_tail_plan_{}.jsonl", std::process::id()));
        let mut file = File::create(&path).unwrap();
        let lines = [
            r#"{"type":"user","permissionMode":"plan","origin":{"kind":"human"},"message":{"role":"user"}}"#,
            r#"{"type":"assistant","parent_tool_use_id":null,"message":{"model":"claude-sonnet-4-5"}}"#,
            r#"{"type":"user","attachment":{"type":"plan_mode_exit"}}"#,
            r#"{"type":"result","subtype":"success","result":"done"}"#,
        ];
        for l in lines {
            writeln!(file, "{l}").unwrap();
        }
        file.flush().unwrap();
        drop(file);

        let tail = read_tail_from_file(&path, true).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(tail.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(tail.permission_mode, None);
    }
}
