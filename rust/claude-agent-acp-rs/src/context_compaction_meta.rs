//! `context-compaction-meta.ts`: the AIR context-compaction facts.
//!
//! The `_meta` of a context compaction report is
//! `_meta.jetbrains.air.contextCompaction`, as `docs/air-extensions.md`
//! defines. The standard `toolCallId` / `status` fields own the lifecycle
//! identity and phase; this extension carries only compaction-specific facts,
//! and only AIR gets it.

use serde_json::{Map, Value};

use crate::air::with_air_meta;

/// `CONTEXT_COMPACTION_META_VERSION`.
pub const CONTEXT_COMPACTION_META_VERSION: u64 = 1;

/// The AIR extension key of the compaction facts (`AIR_CONTEXT_COMPACTION_KEY`,
/// `air-extension.ts:57`).
pub const AIR_CONTEXT_COMPACTION_KEY: &str = "contextCompaction";

/// `ContextCompactionMetadata` without its `version` (`CompactionFacts`).
///
/// Every field is optional; a missing field is omitted from the wire object,
/// exactly as `JSON.stringify` drops `undefined` values.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompactionFacts {
    /// `manual` or `automatic` (`trigger`).
    pub trigger: Option<String>,
    /// `preTokens`: the token count before the compaction.
    pub pre_tokens: Option<Value>,
    /// `postTokens`: the token count the boundary retained.
    pub post_tokens: Option<Value>,
    /// `durationMs`: how long the compaction took.
    pub duration_ms: Option<Value>,
    /// `error`: the SDK's failure text of a failed compaction.
    pub error: Option<String>,
}

impl CompactionFacts {
    /// Whether the facts object has any key (`Object.keys(metadata).length > 0`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.trigger.is_none()
            && self.pre_tokens.is_none()
            && self.post_tokens.is_none()
            && self.duration_ms.is_none()
            && self.error.is_none()
    }

    /// The same facts without `error` (`withoutError`).
    #[must_use]
    pub fn without_error(&self) -> Self {
        let mut facts = self.clone();
        facts.error = None;
        facts
    }

    /// The facts as the JSON object `createContextCompactionMeta` spreads.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        if let Some(trigger) = &self.trigger {
            map.insert("trigger".into(), Value::String(trigger.clone()));
        }
        if let Some(pre) = &self.pre_tokens {
            map.insert("preTokens".into(), pre.clone());
        }
        if let Some(post) = &self.post_tokens {
            map.insert("postTokens".into(), post.clone());
        }
        if let Some(duration) = &self.duration_ms {
            map.insert("durationMs".into(), duration.clone());
        }
        if let Some(error) = &self.error {
            map.insert("error".into(), Value::String(error.clone()));
        }
        Value::Object(map)
    }
}

/// `createContextCompactionMeta`: `_meta.jetbrains.air.contextCompaction` with
/// the version stamped FIRST into the payload, then the facts
/// (`{version, ...metadata}`, `context-compaction-meta.ts:31-36`). Insertion
/// order is the wire order (`serde_json`'s `preserve_order`).
#[must_use]
pub fn create_context_compaction_meta(facts: &CompactionFacts) -> Value {
    let mut map = Map::new();
    map.insert(
        "version".into(),
        Value::from(CONTEXT_COMPACTION_META_VERSION),
    );
    if let Value::Object(facts) = facts.to_json() {
        map.extend(facts);
    }
    with_air_meta(None, AIR_CONTEXT_COMPACTION_KEY, Value::Object(map))
}

/// `contextCompactionMetadataFromBoundary` (`context-compaction.ts:411-427`):
/// the facts of a `compact_boundary` frame.
///
/// `trigger` maps the SDK's `auto` to the AIR `automatic`, and every other
/// value (including a missing one) to `manual`.
#[must_use]
pub fn context_compaction_metadata_from_boundary(compact_metadata: &Value) -> CompactionFacts {
    let trigger = if compact_metadata.get("trigger").and_then(Value::as_str) == Some("auto") {
        "automatic"
    } else {
        "manual"
    };
    CompactionFacts {
        trigger: Some(trigger.to_string()),
        pre_tokens: compact_metadata.get("pre_tokens").cloned(),
        post_tokens: compact_metadata.get("post_tokens").cloned(),
        duration_ms: compact_metadata.get("duration_ms").cloned(),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn meta_is_the_air_key_with_version_and_facts() {
        let facts = CompactionFacts {
            trigger: Some("manual".into()),
            pre_tokens: Some(json!(100)),
            post_tokens: Some(json!(10)),
            duration_ms: Some(json!(5)),
            error: None,
        };
        assert_eq!(
            create_context_compaction_meta(&facts),
            json!({"jetbrains": {"air": {"version": 1, "contextCompaction": {
                "version": 1, "trigger": "manual", "preTokens": 100, "postTokens": 10, "durationMs": 5
            }}}})
        );
        // `{version, ...metadata}`: the version is inserted FIRST, and the wire
        // bytes are what a client sees (`context-compaction-meta.ts:31-36`).
        assert_eq!(
            serde_json::to_string(&create_context_compaction_meta(&facts)).unwrap(),
            r#"{"jetbrains":{"air":{"version":1,"contextCompaction":{"version":1,"trigger":"manual","preTokens":100,"postTokens":10,"durationMs":5}}}}"#
        );
        let empty = create_context_compaction_meta(&CompactionFacts::default());
        assert_eq!(
            empty,
            json!({"jetbrains": {"air": {"version": 1, "contextCompaction": {"version": 1}}}})
        );
        assert_eq!(
            serde_json::to_string(&empty).unwrap(),
            r#"{"jetbrains":{"air":{"version":1,"contextCompaction":{"version":1}}}}"#
        );
    }

    #[test]
    fn meta_carries_the_error_for_the_tool_call_presentation() {
        let facts = CompactionFacts {
            error: Some("summary rejected".into()),
            ..CompactionFacts::default()
        };
        assert_eq!(
            create_context_compaction_meta(&facts),
            json!({"jetbrains": {"air": {"version": 1, "contextCompaction": {
                "version": 1, "error": "summary rejected"
            }}}})
        );
        assert!(
            create_context_compaction_meta(&facts.without_error())["jetbrains"]["air"]
                ["contextCompaction"]
                .get("error")
                .is_none()
        );
    }

    #[test]
    fn boundary_maps_auto_to_automatic_and_keeps_present_facts_only() {
        let facts = context_compaction_metadata_from_boundary(&json!({
            "trigger": "auto", "pre_tokens": 50, "post_tokens": 12, "duration_ms": 7
        }));
        assert_eq!(
            facts.to_json(),
            json!({"trigger": "automatic", "preTokens": 50, "postTokens": 12, "durationMs": 7})
        );
        // A missing trigger is `manual`; missing optional facts are omitted.
        let facts = context_compaction_metadata_from_boundary(&json!({"pre_tokens": 3}));
        assert_eq!(
            facts.to_json(),
            json!({"trigger": "manual", "preTokens": 3})
        );
        assert!(!facts.is_empty());
    }
}
