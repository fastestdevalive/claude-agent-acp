//! The AIR `_meta` filter (rust-air-gaps Decision 25), a port of
//! `dist/tool-calls/changed-meta-filter.js`.
//!
//! AIR merges the `_meta` keys of a tool call across updates, so an update only
//! needs to carry the keys that changed. [`ChangedMetaFilter`] remembers, per
//! tool call, the JSON of every compared key it already sent and strips an
//! unchanged key from later `tool_call_update`s; an update left with nothing is
//! dropped. Only AIR sessions use it (another client gets every key every time,
//! ACP defines no merge).
//!
//! Compared keys: those of `claudeCode`, those of `jetbrains.air`, and the flags
//! `is_mcp_tool_call` and `terminal_info`. Never compared: `terminal_output*`,
//! `terminal_exit`, `mcp_output_delta` (a client appends their data).

use std::collections::{HashMap, VecDeque};

use agent_client_protocol::schema::v1::{SessionUpdate, ToolCallUpdateFields};
use serde_json::{Map, Value};

/// The `_meta` namespaces whose keys a client merges one by one.
const MERGED_NAMESPACES: [&str; 1] = ["claudeCode"];
/// The top-level `_meta` flags that stay the same for a tool call.
const FLAGS: [&str; 2] = ["is_mcp_tool_call", "terminal_info"];
/// The number of tool calls whose `_meta` the filter remembers.
pub const TOOL_CALL_LIMIT: usize = 2048;

/// Removes the `_meta` keys of a `tool_call_update` that an earlier report of
/// the same tool call already sent with the same value.
#[derive(Debug, Default)]
pub struct ChangedMetaFilter {
    sent: HashMap<String, HashMap<String, String>>,
    /// Tool-call ids, least recently remembered first.
    order: VecDeque<String>,
}

impl ChangedMetaFilter {
    /// A filter that has sent nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The update without its unchanged keys, or `None` when nothing is left.
    /// Anything but a tool call / tool call update passes through.
    pub fn apply(&mut self, update: SessionUpdate) -> Option<SessionUpdate> {
        match update {
            SessionUpdate::ToolCall(mut call) => {
                let id = call.tool_call_id.to_string();
                let mut held = HashMap::new();
                if let Some(meta) = call.meta.as_mut() {
                    record_and_strip(&mut held, meta, false);
                }
                self.remember(id, held);
                Some(SessionUpdate::ToolCall(call))
            }
            SessionUpdate::ToolCallUpdate(mut upd) => {
                let id = upd.tool_call_id.to_string();
                let Some(mut held) = self.sent.remove(&id) else {
                    // First report the filter sees for this id: record it whole.
                    let mut held = HashMap::new();
                    if let Some(meta) = upd.meta.as_mut() {
                        record_and_strip(&mut held, meta, false);
                    }
                    self.remember(id, held);
                    return Some(SessionUpdate::ToolCallUpdate(upd));
                };
                let Some(meta) = upd.meta.as_mut() else {
                    // No `_meta` at all: the update is sent as it is.
                    self.remember(id, held);
                    return Some(SessionUpdate::ToolCallUpdate(upd));
                };
                record_and_strip(&mut held, meta, true);
                self.remember(id, held);
                if upd.meta.as_ref().is_some_and(Map::is_empty) {
                    upd.meta = None;
                }
                let carries_something = upd.meta.is_some() || has_fields(&upd.fields);
                carries_something.then_some(SessionUpdate::ToolCallUpdate(upd))
            }
            other => Some(other),
        }
    }

    /// Store the entry as the most recent one; forget the oldest past the limit.
    fn remember(&mut self, id: String, held: HashMap<String, String>) {
        self.order.retain(|known| *known != id);
        self.order.push_back(id.clone());
        self.sent.insert(id, held);
        if self.order.len() > TOOL_CALL_LIMIT {
            if let Some(oldest) = self.order.pop_front() {
                self.sent.remove(&oldest);
            }
        }
    }
}

/// Whether an update carries any field besides its id.
fn has_fields(fields: &ToolCallUpdateFields) -> bool {
    fields.status.is_some()
        || fields.kind.is_some()
        || fields.title.is_some()
        || fields.content.is_some()
        || fields.locations.is_some()
        || fields.raw_input.is_some()
        || fields.raw_output.is_some()
}

/// Record the compared keys of `meta`; with `strip`, delete the unchanged ones.
fn record_and_strip(
    held: &mut HashMap<String, String>,
    meta: &mut Map<String, Value>,
    strip: bool,
) {
    let mut keep = |key: String, value: &Value| -> bool {
        let json = value.to_string();
        if strip && held.get(&key) == Some(&json) {
            return false;
        }
        held.insert(key, json);
        true
    };
    let mut merge_keys = |prefix: &str, values: &mut Map<String, Value>| {
        let keys: Vec<String> = values.keys().cloned().collect();
        for key in keys {
            let unchanged = values
                .get(&key)
                .is_some_and(|value| !keep(format!("{prefix}{key}"), value));
            if unchanged {
                values.remove(&key);
            }
        }
    };
    for namespace in MERGED_NAMESPACES {
        let Some(values) = meta.get_mut(namespace).and_then(Value::as_object_mut) else {
            continue;
        };
        merge_keys(&format!("{namespace}."), values);
        if values.is_empty() {
            meta.remove(namespace);
        }
    }
    if let Some(jetbrains) = meta.get_mut("jetbrains").and_then(Value::as_object_mut) {
        // Node only prunes `jetbrains` when `jetbrains.air` is a record: an
        // already-empty `jetbrains:{}` with no `air` key stays.
        if let Some(air) = jetbrains.get_mut("air").and_then(Value::as_object_mut) {
            merge_keys("jetbrains.air.", air);
            if air.is_empty() {
                jetbrains.remove("air");
            }
            if jetbrains.is_empty() {
                meta.remove("jetbrains");
            }
        }
    }
    for flag in FLAGS {
        let unchanged = meta
            .get(flag)
            .is_some_and(|value| !keep(flag.to_string(), value));
        if unchanged {
            meta.remove(flag);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ToolCall, ToolCallStatus, ToolCallUpdate};
    use serde_json::json;

    fn meta(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn call(id: &str, m: Value) -> SessionUpdate {
        SessionUpdate::ToolCall(ToolCall::new(id.to_string(), "t").meta(meta(m)))
    }

    fn upd(id: &str, fields: ToolCallUpdateFields, m: Option<Value>) -> SessionUpdate {
        let mut u = ToolCallUpdate::new(id.to_string(), fields);
        u.meta = m.map(meta);
        SessionUpdate::ToolCallUpdate(u)
    }

    fn meta_of(update: &SessionUpdate) -> Option<Value> {
        match update {
            SessionUpdate::ToolCallUpdate(u) => u.meta.clone().map(Value::Object),
            SessionUpdate::ToolCall(c) => c.meta.clone().map(Value::Object),
            _ => None,
        }
    }

    /// An unchanged `claudeCode` / `jetbrains.air` key is stripped from a later
    /// update; a changed key stays; the first report is untouched.
    #[test]
    fn inv_51_meta_filter_strips_unchanged_keys() {
        let mut filter = ChangedMetaFilter::new();
        let first = json!({"claudeCode": {"toolName": "Bash"}});
        let kept = filter.apply(call("c1", first.clone())).unwrap();
        assert_eq!(meta_of(&kept), Some(first));

        // Same toolName, new air key: only the air key survives.
        let second = upd(
            "c1",
            ToolCallUpdateFields::new().title("x"),
            Some(json!({"claudeCode": {"toolName": "Bash"},
                        "jetbrains": {"air": {"version": 1, "commandTitle": "Say hi"}}})),
        );
        let out = filter.apply(second).unwrap();
        assert_eq!(
            meta_of(&out),
            Some(json!({"jetbrains": {"air": {"version": 1, "commandTitle": "Say hi"}}}))
        );

        // Everything unchanged now: `_meta` disappears, the title field stays.
        let third = upd(
            "c1",
            ToolCallUpdateFields::new().title("y"),
            Some(json!({"claudeCode": {"toolName": "Bash"},
                        "jetbrains": {"air": {"version": 1, "commandTitle": "Say hi"}}})),
        );
        let out = filter.apply(third).unwrap();
        assert_eq!(meta_of(&out), None);
    }

    /// An update left with no field and no `_meta` is dropped; one with a field
    /// is kept without `_meta`.
    #[test]
    fn inv_51_meta_filter_drops_empty_updates() {
        let mut filter = ChangedMetaFilter::new();
        filter.apply(call("c1", json!({"claudeCode": {"toolName": "Read"}})));
        let empty = upd(
            "c1",
            ToolCallUpdateFields::new(),
            Some(json!({"claudeCode": {"toolName": "Read"}})),
        );
        assert!(filter.apply(empty).is_none());
        let with_status = upd(
            "c1",
            ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            Some(json!({"claudeCode": {"toolName": "Read"}})),
        );
        let out = filter.apply(with_status).unwrap();
        assert_eq!(meta_of(&out), None);
        // No `_meta` at all passes through unchanged even when nothing else is set.
        let bare = upd("c1", ToolCallUpdateFields::new(), None);
        assert!(filter.apply(bare).is_some());
    }

    /// The flags `is_mcp_tool_call` / `terminal_info` are compared; output
    /// channels (`terminal_output*`, `terminal_exit`, `mcp_output_delta`) never.
    #[test]
    fn inv_51_meta_filter_flags_and_output_channels() {
        let mut filter = ChangedMetaFilter::new();
        filter.apply(call(
            "c1",
            json!({"claudeCode": {"toolName": "mcp__a__b"}, "is_mcp_tool_call": true,
                   "terminal_info": {"terminal_id": "c1"}}),
        ));
        let again = upd(
            "c1",
            ToolCallUpdateFields::new().title("z"),
            Some(
                json!({"is_mcp_tool_call": true, "terminal_info": {"terminal_id": "c1"},
                        "terminal_output": {"terminal_id": "c1", "data": "x"},
                        "terminal_exit": {"exit_code": 0},
                        "mcp_output_delta": {"d": 1}}),
            ),
        );
        let out = filter.apply(again).unwrap();
        assert_eq!(
            meta_of(&out),
            Some(
                json!({"terminal_output": {"terminal_id": "c1", "data": "x"},
                        "terminal_exit": {"exit_code": 0},
                        "mcp_output_delta": {"d": 1}})
            )
        );
        // The same output twice is NOT stripped.
        let twice = upd(
            "c1",
            ToolCallUpdateFields::new(),
            Some(json!({"terminal_output": {"terminal_id": "c1", "data": "x"}})),
        );
        assert!(filter.apply(twice).is_some());
    }

    /// A new `tool_call` for a known id resets what was sent; an unknown id's
    /// first update is recorded whole; non-tool updates pass through.
    #[test]
    fn inv_51_meta_filter_resets_on_tool_call_and_passes_other_updates() {
        let mut filter = ChangedMetaFilter::new();
        filter.apply(call("c1", json!({"claudeCode": {"toolName": "A"}})));
        filter.apply(call("c1", json!({"claudeCode": {"toolName": "B"}})));
        let out = filter
            .apply(upd(
                "c1",
                ToolCallUpdateFields::new().title("t"),
                Some(json!({"claudeCode": {"toolName": "B"}})),
            ))
            .unwrap();
        assert_eq!(meta_of(&out), None);
        let unknown = filter
            .apply(upd(
                "zz",
                ToolCallUpdateFields::new(),
                Some(json!({"claudeCode": {"toolName": "B"}})),
            ))
            .unwrap();
        assert_eq!(
            meta_of(&unknown),
            Some(json!({"claudeCode": {"toolName": "B"}}))
        );
    }

    /// Node prunes `jetbrains` only when `jetbrains.air` is a record: an empty
    /// `jetbrains:{}` without an `air` key survives, one emptied by the filter
    /// is removed (`changed-meta-filter.js`, `recordAndStrip`).
    #[test]
    fn inv_51_meta_filter_keeps_jetbrains_without_air() {
        let mut filter = ChangedMetaFilter::new();
        filter.apply(call(
            "c1",
            json!({"jetbrains": {"air": {"commandTitle": "x"}}}),
        ));
        let out = filter
            .apply(upd(
                "c1",
                ToolCallUpdateFields::new().title("t"),
                Some(json!({"jetbrains": {}})),
            ))
            .unwrap();
        assert_eq!(meta_of(&out), Some(json!({"jetbrains": {}})));
        let out = filter
            .apply(upd(
                "c1",
                ToolCallUpdateFields::new().title("t"),
                Some(json!({"jetbrains": {"air": {"commandTitle": "x"}}})),
            ))
            .unwrap();
        assert_eq!(meta_of(&out), None, "an emptied jetbrains is removed");
    }

    /// The filter remembers at most `TOOL_CALL_LIMIT` tool calls, forgetting the
    /// least recently remembered.
    #[test]
    fn inv_51_meta_filter_forgets_the_oldest_past_the_limit() {
        let mut filter = ChangedMetaFilter::new();
        for i in 0..=TOOL_CALL_LIMIT {
            filter.apply(call(
                &format!("c{i}"),
                json!({"claudeCode": {"toolName": "T"}}),
            ));
        }
        assert_eq!(filter.sent.len(), TOOL_CALL_LIMIT);
        // c0 was forgotten: its next update is treated as a first report.
        let out = filter
            .apply(upd(
                "c0",
                ToolCallUpdateFields::new(),
                Some(json!({"claudeCode": {"toolName": "T"}})),
            ))
            .unwrap();
        assert_eq!(
            meta_of(&out),
            Some(json!({"claudeCode": {"toolName": "T"}}))
        );
    }
}
