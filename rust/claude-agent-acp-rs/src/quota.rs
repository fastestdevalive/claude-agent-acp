//! Per-model quota rows (`_meta.quota.model_usage`) — a port of the Node
//! 0.94.0 helpers `normalizeModelUsage`, `modelUsageIncrement`,
//! `addModelUsage`, `resumedFirstResultModelUsage`, `getMatchingModelUsage`
//! and `quotaTokenCount` (`dist/acp-agent.js:9099-9290,10616`).
//!
//! `result.modelUsage` is a running total for the whole `query()` call, so a
//! turn's own spend per model is the increment against the previous reading.
//! Rows keep insertion order like a JS object (`serde_json` runs with
//! `preserve_order`), so the wire order matches Node's.

use std::sync::Arc;

use serde_json::{json, Value};

/// One model's four token counters. Signed: a rewound reading subtracts to a
/// negative that [`increment`] detects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tally {
    pub input: i64,
    pub output: i64,
    pub cached_read: i64,
    pub cached_write: i64,
}

impl Tally {
    /// The sum of the four counters (`tallyTotal`).
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cached_read + self.cached_write
    }

    fn add(&self, other: &Tally) -> Tally {
        Tally {
            input: self.input + other.input,
            output: self.output + other.output,
            cached_read: self.cached_read + other.cached_read,
            cached_write: self.cached_write + other.cached_write,
        }
    }

    fn sub(&self, other: &Tally) -> Tally {
        Tally {
            input: self.input - other.input,
            output: self.output - other.output,
            cached_read: self.cached_read - other.cached_read,
            cached_write: self.cached_write - other.cached_write,
        }
    }

    fn any_negative(&self) -> bool {
        self.input < 0 || self.output < 0 || self.cached_read < 0 || self.cached_write < 0
    }

    /// `quotaTokenCount` — one `token_count` object.
    pub fn token_count(&self) -> Value {
        json!({
            "totalTokens": self.total(),
            "inputTokens": self.input,
            "cachedInputTokens": self.cached_read,
            "cachedWriteTokens": self.cached_write,
            "outputTokens": self.output,
            "reasoningOutputTokens": 0,
        })
    }
}

/// Insertion-ordered per-model tallies (a JS object keyed by model).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelRows(Vec<(String, Tally)>);

impl ModelRows {
    /// Whether there are no rows.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The rows in insertion order.
    pub fn rows(&self) -> &[(String, Tally)] {
        &self.0
    }

    fn get(&self, model: &str) -> Option<&Tally> {
        self.0.iter().find(|(m, _)| m == model).map(|(_, t)| t)
    }

    /// `model_usage` as the wire array `[{model, token_count}]`.
    pub fn to_json(&self) -> Value {
        Value::Array(
            self.0
                .iter()
                .map(|(model, tally)| json!({"model": model, "token_count": tally.token_count()}))
                .collect(),
        )
    }

    /// A single-row set (test and resume helper).
    pub fn single(model: &str, tally: Tally) -> Self {
        ModelRows(vec![(model.to_string(), tally)])
    }
}

/// A cheaply clonable snapshot carried on `TurnEvent::Settled`.
pub type SharedRows = Arc<ModelRows>;

/// `finiteCount`: a finite JSON number, else 0 (truncated to an integer).
fn finite_count(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                i
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() {
                    f as i64
                } else {
                    0
                }
            } else {
                0
            }
        }
        _ => 0,
    }
}

/// `normalizeModelUsage` — project `result.modelUsage` into tallies.
pub fn normalize(model_usage: Option<&Value>) -> ModelRows {
    let Some(Value::Object(map)) = model_usage else {
        return ModelRows::default();
    };
    ModelRows(
        map.iter()
            .map(|(model, usage)| {
                (
                    model.clone(),
                    Tally {
                        input: finite_count(usage.get("inputTokens")),
                        output: finite_count(usage.get("outputTokens")),
                        cached_read: finite_count(usage.get("cacheReadInputTokens")),
                        cached_write: finite_count(usage.get("cacheCreationInputTokens")),
                    },
                )
            })
            .collect(),
    )
}

/// `modelUsageIncrement` — `current - previous` per model, dropping zero-total
/// models. A reading that fell below the previous one (a restarted running
/// total) is its own increment.
pub fn increment(current: &ModelRows, previous: &ModelRows) -> ModelRows {
    let mut out = Vec::new();
    for (model, usage) in &current.0 {
        let subtracted = match previous.get(model) {
            Some(base) => usage.sub(base),
            None => *usage,
        };
        let resolved = if subtracted.any_negative() {
            *usage
        } else {
            subtracted
        };
        if resolved.total() > 0 {
            out.push((model.clone(), resolved));
        }
    }
    ModelRows(out)
}

/// `addModelUsage` — fold `inc` into `base` per model.
pub fn add(base: &ModelRows, inc: &ModelRows) -> ModelRows {
    let mut merged = base.0.clone();
    for (model, usage) in &inc.0 {
        match merged.iter_mut().find(|(m, _)| m == model) {
            Some((_, existing)) => *existing = existing.add(usage),
            None => merged.push((model.clone(), *usage)),
        }
    }
    ModelRows(merged)
}

/// `resumedFirstResultModelUsage` — the first result of a resumed session has
/// no earlier reading, so the result's own per-turn `usage` stands in under
/// the turn's top-level model `key`.
pub fn resumed_first(usage: Option<&Value>, key: Option<&str>) -> ModelRows {
    let Some(key) = key else {
        return ModelRows::default();
    };
    let get = |name: &str| finite_count(usage.and_then(|u| u.get(name)));
    let row = Tally {
        input: get("input_tokens"),
        output: get("output_tokens"),
        cached_read: get("cache_read_input_tokens"),
        cached_write: get("cache_creation_input_tokens"),
    };
    if row.total() > 0 {
        ModelRows::single(key, row)
    } else {
        ModelRows::default()
    }
}

/// `getMatchingModelUsage` — the `modelUsage` key sharing the longest common
/// prefix (strictly longer than 0, first wins a tie) with `current_model`.
pub fn matching_key(model_usage: &ModelRows, current_model: &str) -> Option<String> {
    let mut best: Option<&str> = None;
    let mut best_len = 0usize;
    for (key, _) in &model_usage.0 {
        let len = key
            .chars()
            .zip(current_model.chars())
            .take_while(|(a, b)| a == b)
            .count();
        if len > best_len {
            best_len = len;
            best = Some(key);
        }
    }
    best.map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(i: i64, o: i64, cr: i64, cw: i64) -> Tally {
        Tally {
            input: i,
            output: o,
            cached_read: cr,
            cached_write: cw,
        }
    }

    fn rows(items: &[(&str, Tally)]) -> ModelRows {
        ModelRows(items.iter().map(|(m, t)| (m.to_string(), *t)).collect())
    }

    #[test]
    fn inv_48_normalize_coerces_missing_counters() {
        let v = json!({"m1": {"inputTokens": 5, "outputTokens": "x"}, "m2": {}});
        let got = normalize(Some(&v));
        assert_eq!(got, rows(&[("m1", t(5, 0, 0, 0)), ("m2", t(0, 0, 0, 0))]));
        assert!(normalize(None).is_empty());
    }

    #[test]
    fn inv_48_increment_subtracts_previous_reading() {
        let cur = rows(&[("m1", t(30, 20, 5, 1))]);
        let prev = rows(&[("m1", t(10, 5, 2, 0))]);
        assert_eq!(increment(&cur, &prev), rows(&[("m1", t(20, 15, 3, 1))]));
    }

    #[test]
    fn inv_48_increment_new_model_is_its_reading() {
        let cur = rows(&[("m1", t(3, 4, 0, 0))]);
        assert_eq!(increment(&cur, &ModelRows::default()), cur);
    }

    #[test]
    fn inv_48_increment_rewind_uses_reading_itself() {
        // The running total fell below the previous one: no usable reference.
        let cur = rows(&[("m1", t(4, 4, 0, 0))]);
        let prev = rows(&[("m1", t(10, 10, 0, 0))]);
        assert_eq!(increment(&cur, &prev), cur);
    }

    #[test]
    fn inv_48_increment_drops_zero_total_models() {
        let cur = rows(&[("m1", t(5, 5, 0, 0)), ("m2", t(7, 7, 0, 0))]);
        let prev = rows(&[("m1", t(5, 5, 0, 0)), ("m2", t(1, 1, 0, 0))]);
        assert_eq!(increment(&cur, &prev), rows(&[("m2", t(6, 6, 0, 0))]));
    }

    #[test]
    fn inv_48_add_merges_per_model_in_insertion_order() {
        let base = rows(&[("m1", t(1, 1, 0, 0))]);
        let inc = rows(&[("m2", t(2, 2, 0, 0)), ("m1", t(3, 0, 0, 1))]);
        assert_eq!(
            add(&base, &inc),
            rows(&[("m1", t(4, 1, 0, 1)), ("m2", t(2, 2, 0, 0))])
        );
    }

    #[test]
    fn inv_48_resumed_seed_uses_result_usage_under_key() {
        let usage = json!({"input_tokens": 9, "output_tokens": 1,
            "cache_read_input_tokens": 2, "cache_creation_input_tokens": 0});
        assert_eq!(
            resumed_first(Some(&usage), Some("claude-x")),
            rows(&[("claude-x", t(9, 1, 2, 0))])
        );
        assert!(resumed_first(Some(&usage), None).is_empty());
        assert!(resumed_first(Some(&json!({})), Some("claude-x")).is_empty());
    }

    #[test]
    fn inv_48_matching_key_longest_common_prefix() {
        let mu = rows(&[
            ("claude-haiku-4", t(1, 0, 0, 0)),
            ("claude-sonnet-5[1m]", t(1, 0, 0, 0)),
        ]);
        assert_eq!(
            matching_key(&mu, "claude-sonnet-5").as_deref(),
            Some("claude-sonnet-5[1m]")
        );
        assert_eq!(matching_key(&mu, "zzz"), None);
    }

    #[test]
    fn inv_48_to_json_shape() {
        let v = rows(&[("m1", t(1, 2, 3, 4))]).to_json();
        assert_eq!(
            v,
            json!([{"model": "m1", "token_count": {
                "totalTokens": 10, "inputTokens": 1, "cachedInputTokens": 3,
                "cachedWriteTokens": 4, "outputTokens": 2, "reasoningOutputTokens": 0}}])
        );
    }
}
