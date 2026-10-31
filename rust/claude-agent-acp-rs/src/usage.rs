//! Context window and usage update management (rust-parity-fixes Decision 9, P4).
//!
//! Node emits `usage_update` notifications at 4 sites:
//! 1. `stream_event` (`message_start` / `message_delta` when `parent_tool_use_id == null`)
//! 2. `result` (when top-level assistant usage was recorded, carries `cost`)
//! 3. `compact_boundary` (`post_tokens` from metadata)
//! 4. `rate_limit_event` (attaches `_meta["_claude/rateLimit"]`)
//!
//! Top-level `assistant` frames record usage and model, but do not emit an update.

use serde_json::{json, Value};

/// Default context window size in tokens when not otherwise inferred or discovered.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

/// Snapshot of token counters from an API usage object.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl UsageSnapshot {
    /// Sum all four counters as a proxy for post-turn context occupancy.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_input_tokens
            + self.cache_creation_input_tokens
    }
}

/// Sum all four fields from a [`UsageSnapshot`].
pub fn total_tokens(usage: &UsageSnapshot) -> u64 {
    usage.total_tokens()
}

/// `rawMaxTokens` from a `get_context_usage` reply, at the SDK's payload level
/// (`frame.response.response`, see `control::raw_max_tokens`). Returns `None`
/// for a missing / non-numeric / zero window, so both refresh sites can apply
/// the answer unconditionally (D1 / Decision 1).
pub fn raw_max_tokens(frame: &Value) -> Option<u64> {
    crate::control::raw_max_tokens(frame).filter(|tokens| *tokens > 0)
}

/// Project a nullable API usage JSON object into a non-null snapshot.
/// Coerces missing or null fields to 0.
pub fn snapshot_from_usage(usage: &Value) -> UsageSnapshot {
    UsageSnapshot {
        input_tokens: usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_input_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_creation_input_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

/// Apply a `message_delta` event's usage to the prior snapshot.
///
/// Per Anthropic API, `message_delta` usage fields are cumulative.
/// Nullable fields (`input_tokens`, cache fields) fall back to `prev` when omitted/null.
/// Only `output_tokens` is guaranteed non-null in the delta.
pub fn apply_message_delta(prev: Option<&UsageSnapshot>, delta_usage: &Value) -> UsageSnapshot {
    let empty = UsageSnapshot::default();
    let p = prev.unwrap_or(&empty);
    UsageSnapshot {
        input_tokens: delta_usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(p.input_tokens),
        output_tokens: delta_usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(p.output_tokens),
        cache_read_input_tokens: delta_usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(p.cache_read_input_tokens),
        cache_creation_input_tokens: delta_usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(p.cache_creation_input_tokens),
    }
}

/// Scan string slice(s) for a `\b1m\b` word boundary token (case-insensitive).
/// Returns `Some(1_000_000)` if found, else `None`.
pub fn infer_context_window_from_model(texts: &[&str]) -> Option<u64> {
    for text in texts {
        // Fast word-boundary scan for "1m" (case-insensitive)
        let bytes = text.as_bytes();
        let len = bytes.len();
        let mut i = 0;
        while i + 1 < len {
            if (bytes[i] == b'1') && (bytes[i + 1] == b'm' || bytes[i + 1] == b'M') {
                let prev_ok = if i == 0 {
                    true
                } else {
                    !bytes[i - 1].is_ascii_alphanumeric()
                };
                let next_ok = if i + 2 >= len {
                    true
                } else {
                    !bytes[i + 2].is_ascii_alphanumeric()
                };
                if prev_ok && next_ok {
                    return Some(1_000_000);
                }
            }
            i += 1;
        }
    }
    None
}

/// Find longest common prefix length between two ASCII/UTF-8 strings.
fn common_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// Given a `modelUsage` JSON map (`{"<model>": { ... }}`) and the `current_model`,
/// find the entry whose key has the longest common prefix with `current_model`.
pub fn get_matching_model_usage<'a>(
    model_usage: &'a Value,
    current_model: &str,
) -> Option<(&'a str, &'a Value)> {
    let obj = model_usage.as_object()?;
    let mut best_key = None;
    let mut best_len = 0;
    for (key, val) in obj {
        let len = common_prefix_len(key, current_model);
        if len > best_len {
            best_len = len;
            best_key = Some((key.as_str(), val));
        }
    }
    best_key
}

/// State tracking for usage and context window during a session.
#[derive(Debug, Clone)]
pub struct UsageState {
    /// Token snapshot from the latest top-level assistant or stream frame in the current turn.
    pub last_usage: Option<UsageSnapshot>,
    /// Last total tokens count emitted or computed in the current turn.
    pub last_total: Option<u64>,
    /// Latest assistant model string recorded in the current turn.
    pub last_model: Option<String>,
    /// Current known context window size in tokens.
    pub context_window: u64,
    /// Whether the context window is authoritative (learned from cache, `modelUsage`, or `getContextUsage`).
    pub authoritative: bool,
    /// Latest context used tokens (mirrors Node `session.contextUsedTokens`).
    pub used: u64,
    /// Whether `used` was ever set: Node's `contextUsedTokens` starts
    /// undefined, and the ExitPlanMode options only show a percentage once
    /// there is a measurement.
    pub used_known: bool,
    /// Stable identifier of the LLM backend the session's query was created
    /// against (F5b/B-70); scopes the cross-session context-window cache.
    pub provider_cache_key: String,
}

impl Default for UsageState {
    fn default() -> Self {
        Self {
            last_usage: None,
            last_total: None,
            last_model: None,
            context_window: DEFAULT_CONTEXT_WINDOW,
            authoritative: false,
            used: 0,
            used_known: false,
            provider_cache_key: String::new(),
        }
    }
}

impl UsageState {
    /// Reset per-turn scratch fields on turn activation (mirrors Node `resetTurnScratch`).
    pub fn reset_turn_scratch(&mut self) {
        self.last_usage = None;
        self.last_total = None;
        self.last_model = None;
    }

    /// Record a context measurement (`session.contextUsedTokens = n`).
    pub fn set_used(&mut self, tokens: u64) {
        self.used = tokens;
        self.used_known = true;
    }

    /// `contextUsedPercent` of the ExitPlanMode options: the used share of the
    /// window, rounded and clamped to 0..=100; none before a measurement.
    pub fn used_percent(&self) -> Option<u32> {
        if !self.used_known || self.context_window == 0 {
            return None;
        }
        let percent = (self.used as f64 / self.context_window as f64 * 100.0).round();
        Some(percent.clamp(0.0, 100.0) as u32)
    }

    /// Try to update the context window from an authoritative `contextWindow` value.
    /// Only updates when `raw > 0`.
    pub fn update_window_if_positive(&mut self, raw: Option<u64>) -> bool {
        if let Some(size) = raw {
            if size > 0 {
                self.context_window = size;
                self.authoritative = true;
                return true;
            }
        }
        false
    }
}

// ---------------------------------------------------------------- `/usage`
//
// The structured replacement for a local `/usage` turn (`acp-agent.ts:501-533`,
// `3718-3733`): the CLI runs the command itself, and the replacement takes the
// place of its text. The producer starts at turn activation and is memoised per
// turn, like Node's `turn.localCommandMarkdown`.

/// `STRUCTURED_USAGE_TIMEOUT_MS` (`acp-agent.ts:428`): the timeout that keeps
/// an unstable control request from holding an otherwise-completed local
/// command indefinitely.
pub const STRUCTURED_USAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The structured replacement for the text of a local `/usage` turn, or `None`
/// to keep the CLI's original text (timeout, transport error, incompatible
/// response) (`structuredUsageMarkdown`).
pub async fn structured_usage_markdown(
    control: &crate::control::Control,
    zone: &crate::usage_markdown::Zone,
) -> Option<String> {
    let receipt = tokio::time::timeout(
        STRUCTURED_USAGE_TIMEOUT,
        control.send_request(json!({ "subtype": "get_usage" })),
    )
    .await
    .ok()?
    .ok()?;
    // `request()` resolves with the response payload of a success verdict.
    let payload = receipt.get("response")?.get("response")?;
    let usage = crate::usage_markdown::parse_usage_response(payload)?;
    Some(crate::usage_markdown::format_usage_response(&usage, zone))
}

/// One `/usage` turn's local-command state (`Turn.localCommandMarkdown` plus
/// its `localCommandDelivered` / `localCommandOriginalOutput` bookkeeping).
#[derive(Debug, Default)]
pub struct UsageTurn {
    /// The CLI text of the first output shape that carried the command's output
    /// (the `originalOutput` of `takeLocalCommandMarkdown`).
    pub original_output: Option<String>,
    /// The produced replacement, once the control request finishes:
    /// `None` = still pending, `Some(None)` = produce failed or timed out (keep
    /// the CLI text), `Some(Some(markdown))` = the replacement.
    pub prepared: Option<Option<String>>,
    /// Whether the replacement (or the kept CLI text) has been published.
    pub delivered: bool,
    /// The control request has been started at activation
    /// (`startsAtActivation: true`).
    pub run_started: bool,
    /// The turn was cancelled: the producer is aborted and its output is not
    /// published (`localCommandAbort.abort()`).
    pub aborted: bool,
}

impl UsageTurn {
    /// Whether the turn's result must wait for the replacement
    /// (`await pending` in `takeLocalCommandMarkdown`).
    #[must_use]
    pub fn waits_for_replacement(&self) -> bool {
        self.run_started && !self.aborted && self.prepared.is_none() && !self.delivered
    }
}

/// The `/usage` turns of one session, keyed by prompt uuid.
#[derive(Debug, Default)]
pub struct UsageTurns {
    turns: std::collections::HashMap<String, UsageTurn>,
}

impl UsageTurns {
    pub fn insert(&mut self, uuid: &str) {
        self.turns.insert(uuid.to_string(), UsageTurn::default());
    }

    #[must_use]
    pub fn get(&self, uuid: &str) -> Option<&UsageTurn> {
        self.turns.get(uuid)
    }

    pub fn get_mut(&mut self, uuid: &str) -> Option<&mut UsageTurn> {
        self.turns.get_mut(uuid)
    }

    pub fn remove(&mut self, uuid: &str) -> Option<UsageTurn> {
        self.turns.remove(uuid)
    }

    /// `localCommandAbort.abort()`: every in-flight `/usage` turn is aborted, so
    /// its pending replacement resolves to "keep the CLI text" and its output is
    /// not published.
    pub fn abort_all(&mut self) {
        for turn in self.turns.values_mut() {
            turn.aborted = true;
            turn.prepared.get_or_insert(None);
        }
    }

    pub fn clear(&mut self) {
        self.turns.clear();
    }
}

/// The process's local zone, resolved once per process: `date` is a blocking
/// subprocess, so a `/usage` producer must not pay for it on every call.
static SYSTEM_ZONE: std::sync::OnceLock<crate::usage_markdown::Zone> = std::sync::OnceLock::new();

/// The process's local zone for `/usage` reset times: what
/// `Intl.DateTimeFormat(.., { timeZoneName: "short" })` renders under `TZ`.
/// The port has no tz-database dependency, so it asks the system `date` for the
/// offset and short name and falls back to UTC when that is unavailable. The
/// answer is cached for the life of the process.
#[must_use]
pub fn system_zone() -> crate::usage_markdown::Zone {
    SYSTEM_ZONE.get_or_init(resolve_system_zone).clone()
}

/// The zone a `/usage` producer may use, bounded: a cached answer is returned
/// immediately, otherwise `date` runs off the runtime with a short timeout. A
/// timeout or a failed blocking task answers `None`, so the producer reports
/// "keep the CLI text" instead of holding the turn.
pub async fn system_zone_bounded() -> Option<crate::usage_markdown::Zone> {
    if let Some(zone) = SYSTEM_ZONE.get() {
        return Some(zone.clone());
    }
    let resolved = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(system_zone),
    )
    .await
    .ok()?
    .ok()?;
    Some(resolved)
}

fn resolve_system_zone() -> crate::usage_markdown::Zone {
    let output = std::process::Command::new("date").arg("+%z %Z").output();
    let Ok(output) = output else {
        return crate::usage_markdown::Zone::utc();
    };
    if !output.status.success() {
        return crate::usage_markdown::Zone::utc();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_zone(&text).unwrap_or_else(crate::usage_markdown::Zone::utc)
}

/// Parse `date "+%z %Z"` output (`-0700 PDT`).
#[must_use]
pub fn parse_zone(text: &str) -> Option<crate::usage_markdown::Zone> {
    let mut parts = text.split_whitespace();
    let offset = parts.next()?;
    let name = parts.next()?;
    let sign = match offset.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits = offset.get(1..)?;
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = digits[..2].parse().ok()?;
    let minutes: i32 = digits[2..].parse().ok()?;
    Some(crate::usage_markdown::Zone::fixed(
        sign * (hours * 60 + minutes),
        name,
    ))
}

/// Attach `_meta["_claude/model"]` if `model` is provided.
fn attach_usage_model(mut update: serde_json::Map<String, Value>, model: Option<&str>) -> Value {
    if let Some(m) = model {
        let meta = update
            .entry("_meta")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Value::Object(map) = meta {
            map.insert("_claude/model".to_string(), json!(m));
        }
    }
    Value::Object(update)
}

/// Build a `usage_update` payload for a stream event or compact boundary.
pub fn build_stream_usage_update(used: u64, size: u64, model: Option<&str>) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("sessionUpdate".into(), json!("usage_update"));
    map.insert("used".into(), json!(used));
    map.insert("size".into(), json!(size));
    attach_usage_model(map, model)
}

/// Build a `usage_update` payload for a `result` frame.
pub fn build_result_usage_update(
    used: u64,
    size: u64,
    model: Option<&str>,
    cost_amount: Option<&Value>,
    origin: Option<&Value>,
) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("sessionUpdate".into(), json!("usage_update"));
    map.insert("used".into(), json!(used));
    map.insert("size".into(), json!(size));
    let mut cost_map = serde_json::Map::new();
    if let Some(cost) = cost_amount {
        cost_map.insert("amount".into(), cost.clone());
    }
    cost_map.insert("currency".into(), json!("USD"));
    map.insert("cost".into(), Value::Object(cost_map));
    if let Some(orig) = origin {
        if !orig.is_null() {
            let mut meta_map = serde_json::Map::new();
            meta_map.insert("_claude/origin".into(), orig.clone());
            map.insert("_meta".into(), Value::Object(meta_map));
        }
    }
    attach_usage_model(map, model)
}

/// Build a `usage_update` payload for a `rate_limit_event`.
pub fn build_rate_limit_usage_update(
    used: u64,
    size: u64,
    model: Option<&str>,
    rate_limit_info: &Value,
) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("sessionUpdate".into(), json!("usage_update"));
    map.insert("used".into(), json!(used));
    map.insert("size".into(), json!(size));
    let mut meta_map = serde_json::Map::new();
    meta_map.insert("_claude/rateLimit".into(), rate_limit_info.clone());
    map.insert("_meta".into(), Value::Object(meta_map));
    attach_usage_model(map, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D1 (Decision 1) — the REAL captured `get_context_usage` reply shape: the
    /// payload sits at `frame.response.response` (`control.rs` A-21).
    #[test]
    fn inv_70_raw_max_tokens_full_frame_shape() {
        let frame = json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "ctx-1",
                "response": {
                    "model": "claude-haiku-5-5",
                    "rawMaxTokens": 1000000,
                    "maxTokens": 1000000
                }
            }
        });
        assert_eq!(raw_max_tokens(&frame), Some(1_000_000));
    }

    /// D1 — the legacy / wrong levels are NOT the payload level and yield
    /// nothing (the pre-fix bug read the frame's top level).
    #[test]
    fn inv_70_raw_max_tokens_legacy_shapes() {
        let top_level = json!({"type": "control_response", "rawMaxTokens": 200000});
        assert_eq!(raw_max_tokens(&top_level), None);
        let response_level =
            json!({"type": "control_response", "response": {"rawMaxTokens": 200000}});
        assert_eq!(raw_max_tokens(&response_level), None);
    }

    /// D1 — zero, missing and non-numeric windows are rejected.
    #[test]
    fn inv_70_raw_max_tokens_rejects_zero_missing_string() {
        let zero = json!({"response": {"response": {"rawMaxTokens": 0}}});
        assert_eq!(raw_max_tokens(&zero), None);
        let missing = json!({"response": {"response": {"model": "m"}}});
        assert_eq!(raw_max_tokens(&missing), None);
        let string = json!({"response": {"response": {"rawMaxTokens": "1000000"}}});
        assert_eq!(raw_max_tokens(&string), None);
    }

    #[test]
    fn inv_61_total_tokens() {
        let snap = UsageSnapshot {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_input_tokens: 25,
            cache_creation_input_tokens: 10,
        };
        assert_eq!(snap.total_tokens(), 185);
        assert_eq!(total_tokens(&snap), 185);

        let json_val = json!({
            "input_tokens": 10,
            "output_tokens": 20,
            "cache_read_input_tokens": null
        });
        let snap2 = snapshot_from_usage(&json_val);
        assert_eq!(snap2.input_tokens, 10);
        assert_eq!(snap2.output_tokens, 20);
        assert_eq!(snap2.cache_read_input_tokens, 0);
        assert_eq!(snap2.cache_creation_input_tokens, 0);
        assert_eq!(snap2.total_tokens(), 30);

        // Test delta
        let delta = json!({
            "output_tokens": 35,
            "cache_read_input_tokens": 5
        });
        let snap3 = apply_message_delta(Some(&snap2), &delta);
        assert_eq!(snap3.input_tokens, 10); // preserved from prev
        assert_eq!(snap3.output_tokens, 35); // overwritten
        assert_eq!(snap3.cache_read_input_tokens, 5); // overwritten
        assert_eq!(snap3.cache_creation_input_tokens, 0); // preserved
        assert_eq!(snap3.total_tokens(), 50);
    }

    #[test]
    fn inv_61_infer_window_1m() {
        assert_eq!(
            infer_context_window_from_model(&["claude-opus-4-6-1m"]),
            Some(1_000_000)
        );
        assert_eq!(
            infer_context_window_from_model(&["claude-opus-4-8[1m]"]),
            Some(1_000_000)
        );
        assert_eq!(
            infer_context_window_from_model(&["Opus 4.7 (1M context)"]),
            Some(1_000_000)
        );
        assert_eq!(
            infer_context_window_from_model(&["default", "1m"]),
            Some(1_000_000)
        );
        assert_eq!(
            infer_context_window_from_model(&["claude-sonnet-10m"]),
            None
        );
        assert_eq!(
            infer_context_window_from_model(&["claude-sonnet-4-5"]),
            None
        );
        assert_eq!(infer_context_window_from_model(&[""]), None);
    }

    #[test]
    fn inv_61_matching_model_usage_prefix() {
        let usage = json!({
            "claude-sonnet-4-5[1m]": { "contextWindow": 1000000 },
            "claude-haiku-3-5": { "contextWindow": 200000 }
        });
        let m = get_matching_model_usage(&usage, "claude-sonnet-4-5");
        assert!(m.is_some());
        let (key, val) = m.unwrap();
        assert_eq!(key, "claude-sonnet-4-5[1m]");
        assert_eq!(val["contextWindow"], 1000000);

        let m2 = get_matching_model_usage(&usage, "gpt-4");
        assert!(m2.is_none());
    }

    #[test]
    fn inv_61_window_only_overwritten_when_positive() {
        let mut state = UsageState::default();
        assert_eq!(state.context_window, 200_000);
        assert!(!state.authoritative);

        // 0 or None should not overwrite
        assert!(!state.update_window_if_positive(Some(0)));
        assert_eq!(state.context_window, 200_000);
        assert!(!state.authoritative);

        assert!(!state.update_window_if_positive(None));
        assert_eq!(state.context_window, 200_000);

        // Positive value overwrites and sets authoritative
        assert!(state.update_window_if_positive(Some(1_000_000)));
        assert_eq!(state.context_window, 1_000_000);
        assert!(state.authoritative);
    }

    #[test]
    fn inv_61_update_builders_exact_json() {
        // Stream update without model
        let u1 = build_stream_usage_update(500, 200000, None);
        assert_eq!(
            u1,
            json!({
                "sessionUpdate": "usage_update",
                "used": 500,
                "size": 200000
            })
        );

        // Stream update with model
        let u2 = build_stream_usage_update(500, 200000, Some("claude-sonnet-4-5"));
        assert_eq!(
            u2,
            json!({
                "sessionUpdate": "usage_update",
                "used": 500,
                "size": 200000,
                "_meta": {
                    "_claude/model": "claude-sonnet-4-5"
                }
            })
        );

        // Result update with cost and origin
        let cost = json!(0.015);
        let origin = json!({ "kind": "user" });
        let u3 = build_result_usage_update(
            500,
            200000,
            Some("claude-sonnet-4-5"),
            Some(&cost),
            Some(&origin),
        );
        assert_eq!(
            u3,
            json!({
                "sessionUpdate": "usage_update",
                "used": 500,
                "size": 200000,
                "cost": {
                    "amount": 0.015,
                    "currency": "USD"
                },
                "_meta": {
                    "_claude/origin": { "kind": "user" },
                    "_claude/model": "claude-sonnet-4-5"
                }
            })
        );

        // Rate limit update
        let rl_info = json!({ "resetsAt": "2026-10-23T00:00:00Z" });
        let u4 = build_rate_limit_usage_update(500, 200000, Some("claude-sonnet-4-5"), &rl_info);
        assert_eq!(
            u4,
            json!({
                "sessionUpdate": "usage_update",
                "used": 500,
                "size": 200000,
                "_meta": {
                    "_claude/rateLimit": { "resetsAt": "2026-10-23T00:00:00Z" },
                    "_claude/model": "claude-sonnet-4-5"
                }
            })
        );
    }

    /// P-34 — the used share of the window is rounded and clamped, and unknown
    /// until a measurement exists.
    #[test]
    fn t3a_used_percent_rounds_clamps_and_needs_a_measurement() {
        let mut usage = UsageState::default();
        assert_eq!(usage.used_percent(), None);
        usage.context_window = 200_000;
        usage.set_used(0);
        assert_eq!(usage.used_percent(), Some(0));
        usage.set_used(70_000);
        assert_eq!(usage.used_percent(), Some(35));
        usage.set_used(1_000);
        assert_eq!(
            usage.used_percent(),
            Some(1),
            "0.5% rounds up like Math.round"
        );
        usage.set_used(900_000);
        assert_eq!(usage.used_percent(), Some(100));
    }
}
