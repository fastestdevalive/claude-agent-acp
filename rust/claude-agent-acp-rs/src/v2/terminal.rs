//! The ACP v2 display terminal translation (T-102, U-104, Q-89; upstream `src/v2/terminal.ts`).
//!
//! Translates terminal extension metadata (`terminal_info`, `terminal_output`,
//! `terminal_output_delta`, `terminal_exit`) on tool call reports into v2
//! `terminal_update` and `terminal_output_chunk` session updates.
//! Commands are tracked per session and tool call, and forgotten on session close.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

const TERMINAL_KEYS: &[&str] = &[
    "terminal_info",
    "terminal_output",
    "terminal_output_delta",
    "terminal_exit",
];

const BASE64_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn is_js_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// Standard base64 encoding with padding.
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(BASE64_CHARS[(n >> 18) as usize & 63] as char);
        out.push(BASE64_CHARS[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(BASE64_CHARS[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(BASE64_CHARS[n as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[derive(Debug, Clone)]
struct RunningCommand {
    terminal_id: String,
    command: Option<String>,
}

/// The result of splitting a tool call report into terminal updates and
/// remaining report fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalSplit {
    pub terminal: Vec<Value>,
    pub report: Option<Value>,
}

/// The terminal updates of the tool call reports of one v2 connection.
#[derive(Debug, Default)]
pub struct V2Terminals {
    running: HashMap<String, RunningCommand>,
}

impl V2Terminals {
    pub fn new() -> Self {
        Self::default()
    }

    /// The terminal updates that a tool call report carries in the extension's
    /// keys, and the report without them, or `None` when nothing else is left.
    pub fn split(&mut self, session_id: &str, report: &Value) -> TerminalSplit {
        let tool_call_id = report
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let key = format!("{session_id} {tool_call_id}");
        let meta = report.get("_meta");

        if let Some(t_info) = meta
            .and_then(|m| m.get("terminal_info"))
            .filter(|v| is_js_truthy(Some(v)))
        {
            if let Some(t_id) = t_info.get("terminal_id").and_then(Value::as_str) {
                self.running.insert(
                    key.clone(),
                    RunningCommand {
                        terminal_id: t_id.to_string(),
                        command: None,
                    },
                );
            }
        }

        let mut terminal_updates = Vec::new();
        if let Some(running) = self.running.get_mut(&key) {
            let command = command_of(report.get("rawInput"));
            let new_command = command.is_some() && command != running.command;
            if new_command {
                running.command = command.clone();
            }

            let output = meta
                .and_then(|m| m.get("terminal_output"))
                .filter(|v| is_js_truthy(Some(v)));
            let delta = meta
                .and_then(|m| m.get("terminal_output_delta"))
                .filter(|v| is_js_truthy(Some(v)));
            let exit = meta
                .and_then(|m| m.get("terminal_exit"))
                .filter(|v| is_js_truthy(Some(v)));
            let has_info = meta
                .and_then(|m| m.get("terminal_info"))
                .filter(|v| is_js_truthy(Some(v)))
                .is_some();

            if has_info || new_command || output.is_some() || exit.is_some() {
                let mut update = Map::new();
                update.insert(
                    "sessionUpdate".to_string(),
                    Value::String("terminal_update".to_string()),
                );
                update.insert(
                    "terminalId".to_string(),
                    Value::String(running.terminal_id.clone()),
                );
                if new_command {
                    if let Some(c) = command {
                        update.insert("command".to_string(), Value::String(c));
                    }
                }
                if let Some(out) = output {
                    let data = out.get("data").and_then(Value::as_str).unwrap_or("");
                    update.insert(
                        "output".to_string(),
                        json!({ "data": base64_encode(data.as_bytes()) }),
                    );
                }
                if let Some(ex) = exit {
                    let mut status = Map::new();
                    if let Some(code) = ex.get("exit_code") {
                        if !code.is_null() {
                            status.insert("exitCode".to_string(), code.clone());
                        }
                    }
                    if let Some(sig) = ex.get("signal") {
                        if !sig.is_null() {
                            status.insert("signal".to_string(), sig.clone());
                        }
                    }
                    update.insert("exitStatus".to_string(), Value::Object(status));
                }
                terminal_updates.push(Value::Object(update));
            }

            if let Some(d) = delta {
                let data = d.get("data").and_then(Value::as_str).unwrap_or("");
                terminal_updates.push(json!({
                    "sessionUpdate": "terminal_output_chunk",
                    "terminalId": running.terminal_id,
                    "data": base64_encode(data.as_bytes()),
                }));
            }

            if exit.is_some() {
                self.running.remove(&key);
            }
        }

        TerminalSplit {
            terminal: terminal_updates,
            report: without_terminal_keys(report),
        }
    }

    /// Forgets the terminals of a session when it closes.
    pub fn forget(&mut self, session_id: &str) {
        let prefix = format!("{session_id} ");
        self.running.retain(|k, _| !k.starts_with(&prefix));
    }
}

fn without_terminal_keys(report: &Value) -> Option<Value> {
    let Some(meta) = report.get("_meta").and_then(Value::as_object) else {
        return Some(report.clone());
    };
    let has_any = TERMINAL_KEYS.iter().any(|k| meta.contains_key(*k));
    if !has_any {
        return Some(report.clone());
    }
    let mut stripped_meta = meta.clone();
    for key in TERMINAL_KEYS {
        stripped_meta.shift_remove(*key);
    }
    let mut stripped_report = report.as_object()?.clone();
    if stripped_meta.is_empty() {
        stripped_report.shift_remove("_meta");
    } else {
        stripped_report.insert("_meta".to_string(), Value::Object(stripped_meta));
    }
    let has_other_fields = stripped_report
        .keys()
        .any(|k| k != "sessionUpdate" && k != "toolCallId" && k != "_meta");
    let is_tool_call =
        stripped_report.get("sessionUpdate").and_then(Value::as_str) == Some("tool_call");

    if has_other_fields || is_tool_call {
        Some(Value::Object(stripped_report))
    } else {
        None
    }
}

fn command_of(raw_input: Option<&Value>) -> Option<String> {
    let command = raw_input
        .and_then(|r| r.get("command"))
        .and_then(Value::as_str)?;
    if !command.is_empty() {
        Some(command.to_string())
    } else {
        None
    }
}
