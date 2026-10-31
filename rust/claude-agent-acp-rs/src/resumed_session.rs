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
    if m.starts_with('<') && m.ends_with('>') {
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

/// Read the resume model and the permission mode of a session with an env snapshot.
pub fn read_resumed_tail_with_env(
    session_id: &str,
    with_model: bool,
    env: &crate::env_snapshot::EnvSnapshot,
) -> ResumedTail {
    let Some(path) = find_transcript_with_env(session_id, env) else {
        return ResumedTail::default();
    };
    read_tail_from_file(&path, with_model).unwrap_or_default()
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
