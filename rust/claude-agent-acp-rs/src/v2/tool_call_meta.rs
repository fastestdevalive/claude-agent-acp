//! The ACP v2 tool-call `_meta` store (T-103, Q-90; upstream `src/v2/tool-call-meta.ts`).
//!
//! Tracks `_meta` per tool call with v2 patch semantics:
//! - Top-level keys replaced whole;
//! - A key omitted in a report keeps its value;
//! - A key set to null is removed;
//! - `claudeCode` namespace keys merged one-by-one;
//! - `claudeCode.toolName` becomes `name` once and is left out of `_meta`;
//! - Permission request subjects carry the full stored `_meta`.

use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};

fn normalize_js_numbers(val: &Value) -> Value {
    match val {
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f.is_finite() && f.fract() == 0.0 {
                    if f >= 0.0 && f <= (u64::MAX as f64) {
                        return Value::Number((f as u64).into());
                    } else if f < 0.0 && f >= (i64::MIN as f64) {
                        return Value::Number((f as i64).into());
                    }
                }
            }
            Value::Number(n.clone())
        }
        Value::Array(arr) => Value::Array(arr.iter().map(normalize_js_numbers).collect()),
        Value::Object(obj) => {
            let mut new_obj = Map::new();
            for (k, v) in obj {
                new_obj.insert(k.clone(), normalize_js_numbers(v));
            }
            Value::Object(new_obj)
        }
        other => other.clone(),
    }
}

/// The number of tool calls whose `_meta` a connection remembers.
pub const TOOL_CALL_LIMIT: usize = 2048;

#[derive(Debug, Clone, Default)]
struct CallState {
    meta: Map<String, Value>,
    named: bool,
}

/// The `_meta` of the tool calls of one ACP v2 connection.
#[derive(Debug, Default)]
pub struct V2ToolCallMeta {
    calls: HashMap<String, CallState>,
    order: VecDeque<String>,
}

impl V2ToolCallMeta {
    pub fn new() -> Self {
        Self::default()
    }

    /// The v2 reports of `report`: the report with the `_meta` keys it changes,
    /// without `_meta` when it changes none, and with `name` when it names the
    /// tool call for the first time.
    pub fn apply(
        &mut self,
        session_id: &str,
        report: &Value,
        starts: bool,
        replaying: bool,
    ) -> Vec<Value> {
        let tool_call_id = report
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let key = format!("{session_id} {tool_call_id}");
        let previous = self.calls.get(&key).cloned();
        let held = previous
            .as_ref()
            .map(|p| p.meta.clone())
            .unwrap_or_default();
        let mut call = if !starts {
            previous.clone().unwrap_or_else(|| CallState {
                meta: Map::new(),
                named: false,
            })
        } else {
            CallState {
                meta: Map::new(),
                named: false,
            }
        };
        self.remember(key.clone(), call.clone());

        let mut result = report.clone();
        let meta_val = result.as_object_mut().and_then(|o| o.shift_remove("_meta"));

        let claude_code_raw = meta_val
            .as_ref()
            .and_then(|m| m.get("claudeCode"))
            .and_then(Value::as_object);
        let tool_name = claude_code_raw
            .and_then(|cc| cc.get("toolName"))
            .and_then(Value::as_str);

        let mut claude_code = claude_code_raw.cloned().unwrap_or_default();
        claude_code.shift_remove("toolName");

        if report.get("name").is_some_and(|n| !n.is_null()) {
            call.named = true;
        } else if !call.named {
            if let Some(tn) = tool_name {
                if let Some(obj) = result.as_object_mut() {
                    obj.insert("name".to_string(), Value::String(tn.to_string()));
                }
                call.named = true;
            }
        }

        let mut new_meta = call.meta.clone();
        if let Some(meta_obj) = meta_val.as_ref().and_then(Value::as_object) {
            for (name, value) in meta_obj {
                if name == "claudeCode" {
                    continue;
                }
                if value.is_null() {
                    new_meta.shift_remove(name);
                } else {
                    new_meta.insert(name.clone(), normalize_js_numbers(value));
                }
            }
        }

        if !claude_code.is_empty() {
            let mut merged_cc = call
                .meta
                .get("claudeCode")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            for (k, v) in claude_code {
                merged_cc.insert(k, normalize_js_numbers(&v));
            }
            new_meta.insert("claudeCode".to_string(), Value::Object(merged_cc));
        }
        call.meta = new_meta.clone();
        if let Some(entry) = self.calls.get_mut(&key) {
            *entry = call;
        }

        if starts && replaying && previous.is_none() {
            if let Some(obj) = result.as_object_mut() {
                obj.insert("_meta".to_string(), Value::Null);
            }
            if new_meta.is_empty() {
                return vec![result];
            }
            let mut second = Map::new();
            second.insert(
                "toolCallId".to_string(),
                Value::String(tool_call_id.to_string()),
            );
            second.insert("_meta".to_string(), Value::Object(new_meta));
            return vec![result, Value::Object(second)];
        }

        let mut patch = Map::new();
        for name in held.keys() {
            if !new_meta.contains_key(name) {
                patch.insert(name.clone(), Value::Null);
            }
        }
        for (name, value) in &new_meta {
            let value_str = serde_json::to_string(value).unwrap_or_default();
            let held_str = held.get(name).and_then(|h| serde_json::to_string(h).ok());
            if held_str.as_deref() != Some(&value_str) {
                patch.insert(name.clone(), value.clone());
            }
        }

        if !patch.is_empty() {
            if let Some(obj) = result.as_object_mut() {
                obj.insert("_meta".to_string(), Value::Object(patch));
            }
        }
        vec![result]
    }

    /// The tool call of a permission request's subject: carries the full stored
    /// `_meta`.
    pub fn subject(&mut self, session_id: &str, tool_call: &Value) -> Value {
        let mut results = self.apply(session_id, tool_call, false, false);
        let mut result = results.remove(0);
        let tool_call_id = tool_call
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let key = format!("{session_id} {tool_call_id}");
        let stored = self
            .calls
            .get(&key)
            .map(|c| c.meta.clone())
            .unwrap_or_default();
        if !stored.is_empty() {
            let mut combined = match result.get("_meta") {
                Some(Value::Object(m)) => m.clone(),
                _ => Map::new(),
            };
            for (k, v) in stored {
                combined.insert(k, v);
            }
            if let Some(obj) = result.as_object_mut() {
                obj.insert("_meta".to_string(), Value::Object(combined));
            }
        }
        result
    }

    /// Forgets the tool calls of a session when it closes.
    pub fn forget(&mut self, session_id: &str) {
        let prefix = format!("{session_id} ");
        self.order.retain(|k| !k.starts_with(&prefix));
        self.calls.retain(|k, _| !k.starts_with(&prefix));
    }

    fn remember(&mut self, key: String, call: CallState) {
        self.order.retain(|k| k != &key);
        self.order.push_back(key.clone());
        self.calls.insert(key, call);
        if self.order.len() > TOOL_CALL_LIMIT {
            if let Some(oldest) = self.order.pop_front() {
                self.calls.remove(&oldest);
            }
        }
    }
}
