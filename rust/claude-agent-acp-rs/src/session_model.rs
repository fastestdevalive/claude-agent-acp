//! Model policy and cross-session context-window cache (ROADMAP F5b).
//!
//! Port of `dist/session-model.js` (the whole file), plus the model-related
//! pieces of `dist/acp-agent.js`: the allowlist/override/denied-model policy
//! (`acp-agent.ts:11471-11482`), gateway narrowing (`session-model.ts:418-443`),
//! and the cross-session `contextWindowCache` / `immediateContextWindow`
//! helpers (`acp-agent.ts:13647-13755`).
//!
//! `session_config.rs` re-exports the picker-facing helpers from here so its
//! config-option composition keeps its existing call sites.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::env_snapshot::EnvSnapshot;

pub const MODEL_CONFIG_ID: &str = "model";

/// Model metadata surfaced by the SDK during initialize.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub value: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_auto_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_effort: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_effort_levels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_fast_mode: Option<bool>,
}

/// An available model option in the picker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AvailableModel {
    pub model_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

const MODEL_FAMILY_ALIASES: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];

/// Replace every `[<digits>m]` token and a trailing `-<digits>m` suffix.
pub fn strip_context_hints(value: &str) -> String {
    let mut s = value.to_string();
    while let Some(start) = s.rfind('[') {
        let Some(rel_end) = s[start..].find(']') else {
            break;
        };
        let end = start + rel_end;
        let inner = &s[start + 1..end];
        let digits = inner
            .strip_suffix(['m', 'M'])
            .filter(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()));
        match digits {
            Some(_) => s.replace_range(start..=end, ""),
            None => break,
        }
    }
    if let Some(dash) = s.rfind('-') {
        let suffix = &s[dash + 1..];
        if suffix
            .strip_suffix(['m', 'M'])
            .filter(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
            .is_some()
        {
            s.truncate(dash);
        }
    }
    s
}

/// Lowercase, trim, and rewrite a trailing `-<digits>m` as `[<digits>m]`.
pub fn canonicalize_model_id(value: &str) -> String {
    let trimmed = value.trim().to_lowercase();
    if let Some(dash) = trimmed.rfind('-') {
        let suffix = &trimmed[dash + 1..];
        if let Some(digits) = suffix
            .strip_suffix('m')
            .filter(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
        {
            return format!("{}[{}m]", &trimmed[..dash], digits);
        }
    }
    trimmed
}

/// The `[<digits>m]` hint at the end of a canonicalized id.
pub fn context_hint_of(value: &str) -> Option<String> {
    let canon = canonicalize_model_id(value);
    let inner = canon.strip_suffix(']')?.rsplit_once('[')?.1;
    let digits = inner.strip_suffix('m')?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(inner.to_string())
}

/// First `\b(\d+)(?:[-.](\d+))?\b` in `value` after stripping context hints.
pub fn extract_model_family_version(value: &str) -> Option<String> {
    let stripped = strip_context_hints(value);
    let chars: Vec<char> = stripped.chars().collect();
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let boundary_before = i == 0 || !is_word(chars[i - 1]);
        if !boundary_before {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        let major: String = chars[start..i].iter().collect();
        // Optional `-`/`.` followed by digits.
        if i + 1 < chars.len()
            && (chars[i] == '-' || chars[i] == '.')
            && chars[i + 1].is_ascii_digit()
        {
            i += 1;
            let minor_start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let boundary_after = i == chars.len() || !is_word(chars[i]);
            if boundary_after {
                let minor: String = chars[minor_start..i].iter().collect();
                return Some(format!("{major}.{minor}"));
            }
            // Not a version pair (`4-6x`): fall through to the bare major only
            // if it itself ends at a boundary.
            i = minor_start;
        }
        let boundary_after = i == chars.len() || !is_word(chars[i]);
        if boundary_after {
            return Some(major);
        }
        i += 1;
    }
    None
}

/// Whether a preference's family version matches the candidate's.
pub fn model_versions_compatible(preference: &str, candidate: &ModelInfo) -> bool {
    let Some(preferred) = extract_model_family_version(preference) else {
        return true;
    };
    let candidate_version = extract_model_family_version(&candidate.value)
        .or_else(|| extract_model_family_version(&candidate.display_name))
        .or_else(|| extract_model_family_version(&candidate.description));
    match candidate_version {
        Some(version) => preferred == version,
        None => true,
    }
}

/// `\b<family>[- ]+(\d+)(?:[-.](\d+))?` over the SDK metadata sources.
fn display_model_version(model: &ModelInfo, family: &str) -> Option<String> {
    let family_lower = family.to_lowercase();
    for source in [
        model.resolved_model.as_deref(),
        Some(model.description.as_str()),
        Some(model.value.as_str()),
    ] {
        let Some(text) = source else { continue };
        let lower = text.to_lowercase();
        let chars: Vec<char> = lower.chars().collect();
        let family_chars: Vec<char> = family_lower.chars().collect();
        let mut i = 0;
        while i + family_chars.len() <= chars.len() {
            if chars[i..i + family_chars.len()] != family_chars[..] {
                i += 1;
                continue;
            }
            let boundary_before =
                i == 0 || !(chars[i - 1].is_ascii_alphanumeric() || chars[i - 1] == '_');
            if !boundary_before {
                i += 1;
                continue;
            }
            let mut j = i + family_chars.len();
            let mut separators = 0;
            while j < chars.len() && (chars[j] == '-' || chars[j] == ' ') {
                j += 1;
                separators += 1;
            }
            if separators == 0 || j >= chars.len() || !chars[j].is_ascii_digit() {
                i += 1;
                continue;
            }
            let major_start = j;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            let major: String = chars[major_start..j].iter().collect();
            if j + 1 < chars.len()
                && (chars[j] == '-' || chars[j] == '.')
                && chars[j + 1].is_ascii_digit()
            {
                let minor_start = j + 1;
                j = minor_start;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    j += 1;
                }
                let minor: String = chars[minor_start..j].iter().collect();
                return Some(format!("{major}.{minor}"));
            }
            return Some(major);
        }
    }
    None
}

/// `^(?:Claude\s+)?([a-z][a-z0-9-]*)(?:\s+\d+(?:\.\d+)?)?(?:\s+\(\d+[mk]\s+context\))?$`
/// (case-insensitive), returning the family.
fn versionable_model_family(name: &str) -> Option<String> {
    let chars: Vec<char> = name.chars().collect();
    let lower: Vec<char> = name.to_lowercase().chars().collect();
    let mut i = 0;
    let claude: Vec<char> = "claude".chars().collect();
    if lower.len() >= claude.len() && lower[..claude.len()] == claude[..] {
        let mut j = claude.len();
        let mut spaces = 0;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
            spaces += 1;
        }
        if spaces > 0 {
            i = j;
        }
    }
    if i >= chars.len() || !chars[i].is_ascii_alphabetic() {
        return None;
    }
    let family_start = i;
    i += 1;
    while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '-') {
        i += 1;
    }
    let family: String = chars[family_start..i].iter().collect();

    // Optional `\s+\d+(\.\d+)?`.
    let mut j = i;
    let mut spaces = 0;
    while j < chars.len() && chars[j].is_whitespace() {
        j += 1;
        spaces += 1;
    }
    if spaces > 0 && j < chars.len() && chars[j].is_ascii_digit() {
        while j < chars.len() && chars[j].is_ascii_digit() {
            j += 1;
        }
        if j < chars.len()
            && chars[j] == '.'
            && j + 1 < chars.len()
            && chars[j + 1].is_ascii_digit()
        {
            j += 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
        }
        i = j;
    }

    // Optional `\s+\(\d+[mk]\s+context\)`.
    let mut j = i;
    let mut spaces = 0;
    while j < chars.len() && chars[j].is_whitespace() {
        j += 1;
        spaces += 1;
    }
    if spaces > 0 && j < chars.len() && chars[j] == '(' {
        let mut k = j + 1;
        let digits_start = k;
        while k < chars.len() && chars[k].is_ascii_digit() {
            k += 1;
        }
        if k > digits_start
            && k < chars.len()
            && (chars[k] == 'm' || chars[k] == 'M' || chars[k] == 'k' || chars[k] == 'K')
        {
            k += 1;
            let mut after = 0;
            while k < chars.len() && chars[k].is_whitespace() {
                k += 1;
                after += 1;
            }
            let context: Vec<char> = "context".chars().collect();
            if after > 0
                && k + context.len() < chars.len()
                && chars[k..k + context.len()]
                    .iter()
                    .map(|c| c.to_ascii_lowercase())
                    .eq(context.iter().copied())
                && chars[k + context.len()] == ')'
            {
                i = k + context.len() + 1;
            }
        }
    }

    if i == chars.len() {
        Some(family)
    } else {
        None
    }
}

/// `\s+\((\d+[mk])\s+context\)$` (case-insensitive): the suffix spelling and
/// its context token.
fn display_context_suffix(name: &str) -> Option<(String, String)> {
    let lower = name.to_lowercase();
    let start = lower.rfind(" (")?;
    let rest = &name[start..];
    let inner = rest.strip_prefix(" (")?.strip_suffix(')')?;
    let mut parts = inner.split_whitespace();
    let token = parts.next()?;
    let context = parts.next()?;
    if parts.next().is_some() || !context.eq_ignore_ascii_case("context") {
        return None;
    }
    let digits = token.strip_suffix(['m', 'M', 'k', 'K'])?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((rest.to_string(), token.to_lowercase()))
}

/// Whether `text` contains `\b<token>\s+context\b` (case-insensitive).
fn description_keeps_context(description: &str, token: &str) -> bool {
    let lower = description.to_lowercase();
    let needle = format!("{token} context");
    let mut from = 0;
    while let Some(pos) = lower[from..].find(&needle) {
        let at = from + pos;
        let before_ok = at == 0
            || !lower[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let after = at + needle.len();
        let after_ok = after == lower.len()
            || !lower[after..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if before_ok && after_ok {
            return true;
        }
        from = at + 1;
    }
    false
}

/// Whether `text` contains `\b\d+(?:\.\d+)?\b`.
fn has_version_number(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() || (i > 0 && is_word(chars[i - 1])) {
            i += 1;
            continue;
        }
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i + 1 < chars.len() && chars[i] == '.' && chars[i + 1].is_ascii_digit() {
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
        }
        if i == chars.len() || !is_word(chars[i]) {
            return true;
        }
        i += 1;
    }
    false
}

/// Add a concrete version when a terse SDK label names the same family as the
/// underlying model metadata (`session-model.ts:70-91`).
pub fn versioned_model_display_name(model: &ModelInfo) -> String {
    let Some(family) = versionable_model_family(&model.display_name) else {
        return model.display_name.clone();
    };
    let Some(version) = display_model_version(model, &family) else {
        return model.display_name.clone();
    };
    let suffix = display_context_suffix(&model.display_name);
    let description_keeps = suffix
        .as_ref()
        .is_some_and(|(_, token)| description_keeps_context(&model.description, token));
    let without_context = match &suffix {
        Some((full, _)) => model
            .display_name
            .trim_end_matches(full.as_str())
            .to_string(),
        None => model.display_name.clone(),
    };
    if has_version_number(&without_context) {
        return if description_keeps {
            without_context
        } else {
            model.display_name.clone()
        };
    }
    if description_keeps || suffix.is_none() {
        return format!("{without_context} {version}");
    }
    let (full, _) = suffix.unwrap_or_default();
    format!("{without_context} {version}{full}")
}

/// Tokenize a model preference for the fuzzy scoring tier. The context hint is
/// read only from a literal trailing `[<digits>m]` (upstream does not
/// canonicalize a `-1m` suffix here).
fn tokenize_model_preference(model: &str) -> (Vec<String>, Option<String>) {
    let lower = model.trim().to_lowercase();
    let context_hint = lower.strip_suffix(']').and_then(|rest| {
        let inner = rest.rsplit_once('[')?.1;
        let digits = inner.strip_suffix('m')?;
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        Some(inner.to_string())
    });
    let mut tokens = Vec::new();
    for part in lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        if part.is_empty() {
            continue;
        }
        let token = match part {
            "opusplan" => "opus",
            "best" | "default" | "claude" => continue,
            other => other,
        };
        if token.chars().any(|c| c.is_ascii_alphabetic()) || token.ends_with('m') {
            tokens.push(token.to_string());
        }
    }
    (tokens, context_hint)
}

fn score_model_match(model: &ModelInfo, tokens: &[String], context_hint: Option<&str>) -> usize {
    let haystack = format!("{} {}", model.value, model.display_name).to_lowercase();
    let mut score = 0;
    let mut non_hint_matched = false;
    for token in tokens {
        if !haystack.contains(token.as_str()) {
            continue;
        }
        if Some(token.as_str()) != context_hint {
            non_hint_matched = true;
        }
        score += if Some(token.as_str()) == context_hint {
            3
        } else {
            1
        };
    }
    if context_hint.is_some() && !non_hint_matched {
        0
    } else {
        score
    }
}

/// Resolve a user-supplied model preference through the upstream tiers:
/// direct, `resolvedModel`, substring includes, then token scoring.
pub fn resolve_model_preference(models: &[ModelInfo], preference: &str) -> Option<ModelInfo> {
    let trimmed = preference.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();
    let canon_pref = canonicalize_model_id(trimmed);

    if let Some(m) = models.iter().find(|m| {
        m.value == trimmed
            || canonicalize_model_id(&m.value) == canon_pref
            || m.display_name.to_lowercase() == lower
    }) {
        return Some(m.clone());
    }

    let matches_resolved = |m: &ModelInfo| {
        m.resolved_model
            .as_deref()
            .is_some_and(|r| canonicalize_model_id(r) == canon_pref)
    };
    if let Some(m) = models
        .iter()
        .find(|m| m.value != "default" && matches_resolved(m))
        .or_else(|| models.iter().find(|m| matches_resolved(m)))
    {
        return Some(m.clone());
    }

    let pref_hint = context_hint_of(trimmed);
    if let Some(m) = models.iter().find(|m| {
        if !model_versions_compatible(trimmed, m) {
            return false;
        }
        if context_hint_of(&m.value) != pref_hint {
            return false;
        }
        let v = m.value.to_lowercase();
        let d = m.display_name.to_lowercase();
        v.contains(&lower) || d.contains(&lower) || lower.contains(&v)
    }) {
        return Some(m.clone());
    }

    let (tokens, context_hint) = tokenize_model_preference(trimmed);
    if tokens.is_empty() {
        return None;
    }
    let mut best_match: Option<&ModelInfo> = None;
    let mut best_score = 0;
    for model in models {
        if !model_versions_compatible(trimmed, model) {
            continue;
        }
        let score = score_model_match(model, &tokens, context_hint.as_deref());
        if score > 0 && score > best_score {
            best_match = Some(model);
            best_score = score;
        }
    }
    best_match.cloned()
}

/// Match a resumed session's live transcript model back onto the picker list.
pub fn match_resumed_model(models: &[ModelInfo], live_model: &str) -> ModelInfo {
    let live = canonicalize_model_id(live_model);
    let default_entry = models.iter().find(|m| m.value == "default");
    let default_resolved = default_entry
        .and_then(|m| m.resolved_model.as_deref())
        .map(canonicalize_model_id);

    if let Some(default_entry) = default_entry {
        if default_resolved.as_deref() == Some(&live) {
            return default_entry.clone();
        }
    }

    if let Some(exact) = models.iter().find(|m| {
        m.resolved_model
            .as_deref()
            .is_some_and(|r| canonicalize_model_id(r) == live)
    }) {
        return exact.clone();
    }

    if let Some(default_entry) = default_entry {
        if let Some(ref d_res) = default_resolved {
            if strip_context_hints(d_res) == strip_context_hints(&live) {
                return default_entry.clone();
            }
        }
    }

    resolve_model_preference(models, live_model).unwrap_or_else(|| ModelInfo {
        value: live_model.to_string(),
        display_name: live_model.to_string(),
        description: String::new(),
        ..Default::default()
    })
}

/// A model id without the spellings that name the same model: provider
/// prefixes, dates, version suffixes, `:variant` suffixes, dotted versions,
/// `-fast` and context hints (`session-model.ts:272-290`).
pub fn normalize_model_id(value: &str) -> String {
    let mut id = strip_context_hints(&value.trim().to_lowercase());
    if let Some(pos) = id.rfind("anthropic.") {
        id = id[pos + "anthropic.".len()..].to_string();
    }
    if let Some(rest) = id.strip_prefix("anthropic/") {
        id = rest.to_string();
    }
    // `:variant` — a letter must follow the colon, so Bedrock's `-v1:0` stays
    // for the version-suffix step below.
    if let Some(colon) = id.rfind(':') {
        let variant = &id[colon + 1..];
        let mut chars = variant.chars();
        if chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            id.truncate(colon);
        }
    }
    // `claude-opus-5.5` -> `claude-opus-5-5`; only dots between two digits.
    let chars: Vec<char> = id.chars().collect();
    let mut dotted = String::with_capacity(id.len());
    for (index, ch) in chars.iter().enumerate() {
        if *ch == '.'
            && index > 0
            && index + 1 < chars.len()
            && chars[index - 1].is_ascii_digit()
            && chars[index + 1].is_ascii_digit()
        {
            dotted.push('-');
        } else {
            dotted.push(*ch);
        }
    }
    id = dotted;
    if let Some(at) = id.find('@') {
        id.truncate(at);
    }
    id = strip_bedrock_version_suffix(&id);
    if let Some(rest) = id.strip_suffix("-fast") {
        id = rest.to_string();
    }
    // `/-\d{8}$/`: a bare `-20260101` strips to the empty string.
    if id.len() >= 9 && id.is_char_boundary(id.len() - 9) {
        let (head, tail) = id.split_at(id.len() - 9);
        if tail.starts_with('-') && tail[1..].chars().all(|c| c.is_ascii_digit()) {
            id = head.to_string();
        }
    }
    id
}

/// `-v\d+(?::\d+)?$`
fn strip_bedrock_version_suffix(id: &str) -> String {
    let Some(dash) = id.rfind("-v") else {
        return id.to_string();
    };
    let rest = &id[dash + 2..];
    let (digits, after) = match rest.split_once(':') {
        Some((d, a)) => (d, Some(a)),
        None => (rest, None),
    };
    let digits_ok = !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit());
    let after_ok = after.is_none_or(|a| !a.is_empty() && a.chars().all(|c| c.is_ascii_digit()));
    if digits_ok && after_ok {
        id[..dash].to_string()
    } else {
        id.to_string()
    }
}

/// Whether managed `deniedModels` blocks a model id (`session-model.ts:302-318`).
pub fn is_denied_model(model_id: &str, denied_models: &[String]) -> bool {
    let id = normalize_model_id(model_id);
    if id.is_empty() {
        return false;
    }
    denied_models.iter().any(|entry| {
        let denied = normalize_model_id(entry);
        if denied.is_empty() {
            return false;
        }
        if MODEL_FAMILY_ALIASES.contains(&denied.as_str()) {
            id == denied || id.contains(&format!("-{denied}"))
        } else {
            id == denied || id.starts_with(&format!("{denied}-"))
        }
    })
}

/// Restrict the SDK's model list to the `availableModels` allowlist
/// (`session-model.ts:331-398`).
pub fn apply_available_models_allowlist(
    sdk_models: &[ModelInfo],
    allowlist: &[String],
    model_overrides: Option<&HashMap<String, String>>,
    denied_models: &[String],
    custom_model_option: Option<&str>,
) -> Vec<ModelInfo> {
    let default_model = sdk_models
        .iter()
        .find(|m| m.value == "default")
        .cloned()
        .unwrap_or_else(|| ModelInfo {
            value: "default".to_string(),
            display_name: "Default".to_string(),
            description: String::new(),
            ..Default::default()
        });
    let mut result = vec![default_model.clone()];
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(default_model.value);

    let sdk_without_default: Vec<&ModelInfo> =
        sdk_models.iter().filter(|m| m.value != "default").collect();

    for entry in allowlist {
        let trimmed = entry.trim();
        if trimmed.is_empty() || seen.contains(trimmed) {
            continue;
        }
        let effective = model_overrides
            .and_then(|overrides| overrides.get(trimmed))
            .map(String::as_str)
            .unwrap_or(trimmed);
        if seen.contains(effective) {
            continue;
        }
        let sdk_match = resolve_model_preference(
            &sdk_without_default
                .iter()
                .map(|m| (*m).clone())
                .collect::<Vec<_>>(),
            trimmed,
        );
        if [
            Some(trimmed),
            Some(effective),
            sdk_match.as_ref().and_then(|m| m.resolved_model.as_deref()),
        ]
        .into_iter()
        .flatten()
        .any(|id| is_denied_model(id, denied_models))
        {
            continue;
        }
        match sdk_match {
            Some(mut matched) => {
                matched.value = effective.to_string();
                result.push(matched);
            }
            None => result.push(ModelInfo {
                value: effective.to_string(),
                display_name: trimmed.to_string(),
                description: String::new(),
                ..Default::default()
            }),
        }
        seen.insert(effective.to_string());
    }

    // `ANTHROPIC_CUSTOM_MODEL_OPTION` is exempt from the allowlist and appended
    // last, matching the docs' "appears at the bottom of the /model picker".
    if let Some(custom) = custom_model_option.map(str::trim).filter(|s| !s.is_empty()) {
        if !seen.contains(custom) {
            if let Some(model) = sdk_models.iter().find(|m| m.value == custom) {
                result.push(model.clone());
                seen.insert(model.value.clone());
            }
        }
    }
    result
}

/// The cross-region inference-profile prefixes the CLI knows
/// (`gateway-models.ts:24-37`).
const BEDROCK_REGION_PREFIXES: [&str; 7] = ["us", "eu", "apac", "jp", "au", "us-gov", "global"];

/// `bedrockRegionPrefix` (`gateway-models.ts:107-110`).
pub fn bedrock_region_prefix(model_id: &str) -> Option<&'static str> {
    let id = model_id
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .unwrap_or(model_id)
        .to_lowercase();
    BEDROCK_REGION_PREFIXES
        .into_iter()
        .find(|prefix| id.starts_with(&format!("{prefix}.anthropic.")))
}

/// Keep only the SDK models a `providers/set` gateway listed
/// (`session-model.ts:418-443`). The listing source itself arrives with F33.
pub fn narrow_to_gateway_models(
    models: &[ModelInfo],
    listed_model_ids: &[String],
    region_prefix: Option<&str>,
) -> Vec<ModelInfo> {
    let listed: HashSet<String> = listed_model_ids
        .iter()
        .map(|id| normalize_model_id(id))
        .filter(|id| !id.is_empty())
        .collect();
    let narrowed: Vec<ModelInfo> = models
        .iter()
        .filter(|model| {
            let model_id = model.resolved_model.as_deref().unwrap_or(&model.value);
            let prefix = bedrock_region_prefix(model_id);
            if region_prefix.is_some() && prefix.is_some() && prefix != region_prefix {
                return false;
            }
            let id = normalize_model_id(model_id);
            if listed.contains(&id) {
                return true;
            }
            if model.resolved_model.is_some() {
                return false;
            }
            if model.value == "default" {
                return true;
            }
            MODEL_FAMILY_ALIASES.contains(&id.as_str())
                && listed.iter().any(|entry| entry.contains(&format!("-{id}")))
        })
        .cloned()
        .collect();
    if narrowed.iter().any(|model| {
        listed.contains(&normalize_model_id(
            model.resolved_model.as_deref().unwrap_or(&model.value),
        ))
    }) {
        narrowed
    } else {
        Vec::new()
    }
}

/// Wiring point for the gateway narrowing applied after the allowlist. With no
/// listing yet (F33 supplies it) the models pass through unchanged.
pub fn narrow_models_for_gateway(
    models: Vec<ModelInfo>,
    listed_model_ids: Option<&[String]>,
    region_prefix: Option<&str>,
) -> (Vec<ModelInfo>, bool) {
    match listed_model_ids {
        Some(listed) => (
            narrow_to_gateway_models(&models, listed, region_prefix),
            true,
        ),
        None => (models, false),
    }
}

/// Whether a rejected `setModel` was vetoed by a user-configured
/// PreModelSwitch hook (`session-model.ts:442-448`).
pub fn is_pre_model_switch_hook_block(error: &str) -> bool {
    error.contains("blocked by a PreModelSwitch hook")
}

/// The `typeof` label the settings diagnostic uses for a non-string `model`.
fn json_type_label(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => "object",
    }
}

/// Outcome of the model selection: what to offer, what to run, and whether a
/// `setModel` round-trip is needed (`session-model.ts:450-600`).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelSelection {
    pub available_models: Vec<AvailableModel>,
    pub current_model_id: String,
    pub skip_set_model: bool,
    pub switch_to_listed: bool,
    pub model_before_switch: String,
    /// `Ignoring model from settings: …` when `settings.model` is not a string.
    pub diagnostic: Option<String>,
}

/// Build the picker and decide the current model (`getAvailableModels`).
pub fn get_available_models(
    models: &[ModelInfo],
    sdk_models: &[ModelInfo],
    settings_model: Option<&serde_json::Value>,
    env_model: Option<&str>,
    is_resumed_session: bool,
    resumed_model_hint: Option<&str>,
    narrowed_by_gateway: bool,
) -> ModelSelection {
    if models.is_empty() {
        let current = if is_resumed_session {
            resumed_model_hint.unwrap_or_else(|| {
                sdk_models
                    .first()
                    .map(|m| m.value.as_str())
                    .unwrap_or("default")
            })
        } else {
            sdk_models
                .first()
                .map(|m| m.value.as_str())
                .unwrap_or("default")
        };
        return ModelSelection {
            available_models: Vec::new(),
            current_model_id: current.to_string(),
            skip_set_model: true,
            switch_to_listed: false,
            model_before_switch: current.to_string(),
            diagnostic: None,
        };
    }

    let mut current_model = models[0].clone();
    let mut resolved_from_input: Option<String> = None;
    let mut diagnostic: Option<String> = None;

    // Model priority (highest to lowest):
    // 1. ANTHROPIC_MODEL environment variable
    // 2. settings.model (user configuration)
    // 3. the resumed session's live model (resumed sessions only)
    // 4. models[0] (default first model)
    if let Some(env_model) = env_model.filter(|value| !value.is_empty()) {
        if let Some(matched) = resolve_model_preference(models, env_model) {
            current_model = matched;
            resolved_from_input = Some(env_model.to_string());
        }
    } else if let Some(settings_value) = settings_model {
        match settings_value.as_str() {
            Some(settings_model) => {
                if let Some(matched) = resolve_model_preference(models, settings_model) {
                    current_model = matched;
                    resolved_from_input = Some(settings_model.to_string());
                }
            }
            None => {
                diagnostic = Some(format!(
                    "Ignoring model from settings: expected a string, got {}.",
                    json_type_label(settings_value)
                ));
            }
        }
    }

    // A resumed session restores the model from its last real assistant
    // transcript record; an unlisted transcript model stays the current model
    // when no gateway narrowed the list.
    if resolved_from_input.is_none() && is_resumed_session {
        if let Some(hint) = resumed_model_hint {
            current_model = match_resumed_model(models, hint);
        }
    }

    let sdk_saw_same_value = sdk_models.iter().any(|m| m.value == current_model.value);
    let sdk_default = sdk_models.iter().find(|m| m.value == "default");
    let sdk_default_dropped = sdk_default.is_some() && !models.iter().any(|m| m.value == "default");
    let mut switch_to_listed = false;
    if resolved_from_input.is_none() {
        if !is_resumed_session || resumed_model_hint.is_none() {
            switch_to_listed = sdk_default_dropped;
        } else if narrowed_by_gateway
            && (!models.iter().any(|m| m.value == current_model.value)
                || (current_model.resolved_model.is_some()
                    && normalize_model_id(current_model.resolved_model.as_deref().unwrap_or(""))
                        != normalize_model_id(resumed_model_hint.unwrap_or(""))))
        {
            current_model = models[0].clone();
            switch_to_listed = true;
        }
    }
    // When the switch fails, the SDK stays on the model it ran before it.
    let model_before_switch = if switch_to_listed {
        sdk_default.cloned().unwrap_or_else(|| models[0].clone())
    } else {
        models[0].clone()
    };

    let skip_set_model = (resolved_from_input.is_none() && !switch_to_listed)
        || (!is_resumed_session
            && resolved_from_input.as_deref() == Some(current_model.value.as_str())
            && sdk_saw_same_value);

    let display_names: Vec<String> = models.iter().map(versioned_model_display_name).collect();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for name in &display_names {
        *counts.entry(name.clone()).or_insert(0) += 1;
    }
    let available_models: Vec<AvailableModel> = models
        .iter()
        .enumerate()
        .map(|(index, model)| {
            let name = if counts.get(&display_names[index]) == Some(&1) {
                display_names[index].clone()
            } else {
                model.display_name.clone()
            };
            AvailableModel {
                model_id: model.value.clone(),
                name,
                description: if model.description.is_empty() {
                    None
                } else {
                    Some(model.description.clone())
                },
            }
        })
        .collect();

    ModelSelection {
        available_models,
        current_model_id: current_model.value,
        skip_set_model,
        switch_to_listed,
        model_before_switch: model_before_switch.value,
        diagnostic,
    }
}

/// What to do when the session-creation `setModel` fails (`session-model.ts:500-600`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSwitchFailure {
    /// Advisory: stay on this model and report it.
    StayOn(String),
    /// Defining option: fail session creation loudly.
    Fail(String),
}

pub fn resolve_model_switch_failure(
    is_resumed_session: bool,
    switch_to_listed: bool,
    error: &str,
    models: &[ModelInfo],
    model_before_switch: &str,
    resumed_model_hint: Option<&str>,
) -> ModelSwitchFailure {
    if !is_resumed_session {
        // A switch onto a listed model is advisory too: the session stays on
        // the model the SDK runs and reports it. A PreModelSwitch veto is the
        // other advisory case.
        if !switch_to_listed && !is_pre_model_switch_hook_block(error) {
            return ModelSwitchFailure::Fail(error.to_string());
        }
        return ModelSwitchFailure::StayOn(model_before_switch.to_string());
    }
    // A resumed session already runs fine on the transcript's model: read that
    // back rather than report the pin the session isn't running.
    match resumed_model_hint {
        Some(hint) => ModelSwitchFailure::StayOn(match_resumed_model(models, hint).value),
        None => ModelSwitchFailure::StayOn(model_before_switch.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Cross-session context-window cache (acp-agent.ts:13647-13755)
// ---------------------------------------------------------------------------

/// The env vars that determine which LLM backend — and which context lane on
/// it — a query's API traffic reaches (`acp-agent.ts:13679-13712`).
pub const PROVIDER_ROUTING_ENV_VARS: [&str; 12] = [
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "ANTHROPIC_VERTEX_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "CLOUD_ML_REGION",
    "AWS_REGION",
    "ANTHROPIC_CUSTOM_HEADERS",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
];

/// Stable backend identifier: positional `\0`-join of the routing vars. The key
/// only lives in memory and is never logged.
pub fn provider_cache_key_for(env: &EnvSnapshot) -> String {
    PROVIDER_ROUTING_ENV_VARS
        .iter()
        .map(|name| env.get(name).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\0")
}

/// Compose the cache key from a provider key and a model id.
pub fn context_window_cache_key(provider_cache_key: &str, model_id: &str) -> String {
    format!("{provider_cache_key}\0{model_id}")
}

/// One operation for the context-window cache thread. The cache is a module
/// global in Node; the Rust port keeps it on a dedicated thread so no lock type
/// (which the workspace clippy config disallows) is needed and reads stay
/// synchronous.
enum CacheOp {
    Get {
        key: String,
        reply: std::sync::mpsc::Sender<Option<u64>>,
    },
    Put {
        key: String,
        window: u64,
    },
    Clear,
}

fn cache_sender() -> &'static std::sync::mpsc::Sender<CacheOp> {
    static TX: OnceLock<std::sync::mpsc::Sender<CacheOp>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<CacheOp>();
        std::thread::spawn(move || {
            let mut cache: HashMap<String, u64> = HashMap::new();
            while let Ok(op) = rx.recv() {
                match op {
                    CacheOp::Get { key, reply } => {
                        let _ = reply.send(cache.get(&key).copied());
                    }
                    CacheOp::Put { key, window } => {
                        cache.insert(key, window);
                    }
                    CacheOp::Clear => cache.clear(),
                }
            }
        });
        tx
    })
}

/// Record an authoritative window (`cacheContextWindow`, with its `> 0` guard).
pub fn cache_context_window(model_key: &str, window: u64) {
    if window > 0 {
        let _ = cache_sender().send(CacheOp::Put {
            key: model_key.to_string(),
            window,
        });
    }
}

/// The cached authoritative window for a composed key.
pub fn cached_context_window(model_key: &str) -> Option<u64> {
    let (reply, reply_rx) = std::sync::mpsc::channel();
    cache_sender()
        .send(CacheOp::Get {
            key: model_key.to_string(),
            reply,
        })
        .ok()?;
    reply_rx.recv().ok().flatten()
}

/// Clear the cache on `logout`: 1M-context entitlement can differ per account.
pub fn clear_context_window_cache() {
    let _ = cache_sender().send(CacheOp::Clear);
}

/// A synchronously seeded window and whether it came from the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImmediateContextWindow {
    pub size: u64,
    pub authoritative: bool,
}

/// The context window to report right now for a model, with no IPC: the cached
/// authoritative value, else the `\b1m\b` heuristic, else the default
/// (`immediateContextWindow`).
pub fn immediate_context_window(
    provider_cache_key: &str,
    model_id: &str,
    model_info: Option<&ModelInfo>,
) -> ImmediateContextWindow {
    let resolved = model_info
        .and_then(|info| info.resolved_model.as_deref())
        .unwrap_or(model_id);
    if let Some(window) =
        cached_context_window(&context_window_cache_key(provider_cache_key, resolved))
    {
        return ImmediateContextWindow {
            size: window,
            authoritative: true,
        };
    }
    let mut texts: Vec<&str> = vec![model_id];
    if let Some(info) = model_info {
        if let Some(resolved) = info.resolved_model.as_deref() {
            texts.push(resolved);
        }
        texts.push(info.display_name.as_str());
        texts.push(info.description.as_str());
    }
    ImmediateContextWindow {
        size: crate::usage::infer_context_window_from_model(&texts)
            .unwrap_or(crate::usage::DEFAULT_CONTEXT_WINDOW),
        authoritative: false,
    }
}

/// Whether a background `getContextUsage` answer is still current
/// (`refreshContextWindowInBackground`'s `stillCurrent`): the window must still
/// be non-authoritative and the session must still be on the model the request
/// was issued for (F5b/A-10).
pub fn context_window_answer_is_current(
    authoritative: bool,
    current_model_id: &str,
    answer_model_id: &str,
) -> bool {
    !authoritative && current_model_id == answer_model_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(value: &str, display: &str) -> ModelInfo {
        ModelInfo {
            value: value.to_string(),
            display_name: display.to_string(),
            description: String::new(),
            ..Default::default()
        }
    }

    fn resolved(value: &str, display: &str, resolved: &str) -> ModelInfo {
        ModelInfo {
            resolved_model: Some(resolved.to_string()),
            ..model(value, display)
        }
    }

    // O-42 (session-model.ts:272-290): provider prefixes, `:variant`, dotted
    // versions, `@`, Bedrock `-vN:M`, `-fast` and date suffixes all collapse to
    // one id, while `anthropic/openai/gpt-5` keeps its `openai/` segment.
    #[test]
    fn f5b_normalize_model_id_spellings() {
        assert_eq!(
            normalize_model_id("anthropic/claude-opus-5.5:batch[1m]"),
            "claude-opus-5-5"
        );
        assert_eq!(
            normalize_model_id("anthropic/claude-opus-5-5-20260101"),
            "claude-opus-5-5"
        );
        assert_eq!(
            normalize_model_id("eu.anthropic.claude-sonnet-5-v1:0"),
            "claude-sonnet-5"
        );
        assert_eq!(
            normalize_model_id("us.anthropic.claude-opus-4-6-v1"),
            "claude-opus-4-6"
        );
        assert_eq!(
            normalize_model_id("claude-opus-4-6-fast"),
            "claude-opus-4-6"
        );
        assert_eq!(normalize_model_id("anthropic/openai/gpt-5"), "openai/gpt-5");
        assert_eq!(normalize_model_id("claude-sonnet-5[1m]"), "claude-sonnet-5");
        assert_eq!(
            normalize_model_id("  CLAUDE-HAIKU-4-5  "),
            "claude-haiku-4-5"
        );
    }

    // O-46: a family alias blocks the whole family; an id blocks its version
    // in every spelling plus later minor versions it prefixes.
    #[test]
    fn f5b_denied_models_family_and_prefix() {
        let denied = vec!["opus".to_string()];
        assert!(is_denied_model("claude-opus-5", &denied));
        assert!(is_denied_model("opus[1m]", &denied));
        assert!(is_denied_model("anthropic/claude-opus-5.5", &denied));
        assert!(!is_denied_model("claude-sonnet-5", &denied));

        let denied = vec!["claude-opus-5".to_string()];
        assert!(is_denied_model("claude-opus-5-5", &denied));
        assert!(is_denied_model("claude-opus-5-20260101", &denied));
        assert!(!is_denied_model("claude-opus-6", &denied));

        let denied = vec!["anthropic/claude-opus-4.5".to_string()];
        assert!(is_denied_model("claude-opus-4-5", &denied));
        assert!(!is_denied_model("", &denied));
    }

    // O-43/O-44/O-45: exact allowlist entries, overrides as the surfaced value,
    // empty allowlist keeps only Default, and the custom option is appended
    // last, exempt from the allowlist and deduped.
    #[test]
    fn f5b_allowlist_overrides_custom_option() {
        let sdk = vec![
            resolved("default", "Default", "claude-sonnet-5"),
            resolved("sonnet", "Sonnet", "claude-sonnet-5"),
            resolved("haiku", "Claude Haiku 4.5", "claude-haiku-4-5"),
            resolved("claude-opus-4-6", "Claude Opus 4.6", "claude-opus-4-6"),
            resolved("custom-model", "Custom", "custom-model"),
        ];

        let out = apply_available_models_allowlist(
            &sdk,
            &["haiku".into(), "claude-opus-4-6".into(), "haiku".into()],
            None,
            &[],
            None,
        );
        assert_eq!(
            out.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            ["default", "haiku", "claude-opus-4-6"]
        );
        // Display info is copied from the closest SDK match.
        assert_eq!(out[1].display_name, "Claude Haiku 4.5");

        let mut overrides = HashMap::new();
        overrides.insert(
            "claude-opus-4-6".to_string(),
            "us.anthropic.claude-opus-4-6-v1".to_string(),
        );
        let out = apply_available_models_allowlist(
            &sdk,
            &["claude-opus-4-6".into()],
            Some(&overrides),
            &[],
            None,
        );
        assert_eq!(out[1].value, "us.anthropic.claude-opus-4-6-v1");
        assert_eq!(out[1].resolved_model.as_deref(), Some("claude-opus-4-6"));

        // `[]` keeps only Default.
        let out = apply_available_models_allowlist(&sdk, &[], None, &[], None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value, "default");

        // Denied entries are dropped even when allowlisted.
        let out = apply_available_models_allowlist(
            &sdk,
            &["haiku".into(), "claude-opus-4-6".into()],
            None,
            &["opus".into()],
            None,
        );
        assert_eq!(
            out.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            ["default", "haiku"]
        );

        // Custom model option: appended last, exempt, not duplicated.
        let out = apply_available_models_allowlist(
            &sdk,
            &["haiku".into()],
            None,
            &[],
            Some(" custom-model "),
        );
        assert_eq!(
            out.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            ["default", "haiku", "custom-model"]
        );
        let out = apply_available_models_allowlist(&sdk, &[], None, &[], Some("custom-model"));
        assert_eq!(
            out.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
            ["default", "custom-model"]
        );
        let out = apply_available_models_allowlist(&sdk, &[], None, &[], Some(" "));
        assert_eq!(out.len(), 1);
    }

    // O-47/O-74: gateway narrowing matches in any spelling, drops rows resolved
    // to another Bedrock region, keeps an alias-only default, and empties the
    // list when the gateway serves none of the SDK's models.
    #[test]
    fn f5b_narrow_to_gateway_models() {
        let sdk = vec![
            resolved("default", "Default", "claude-opus-5"),
            resolved("sonnet", "Sonnet", "claude-sonnet-5"),
            resolved("haiku", "Haiku", "claude-haiku-4-5"),
        ];
        assert_eq!(
            narrow_to_gateway_models(
                &sdk,
                &["claude-sonnet-5".into(), "claude-haiku-4-5".into()],
                None
            )
            .iter()
            .map(|m| m.value.as_str())
            .collect::<Vec<_>>(),
            ["sonnet", "haiku"]
        );
        // A `default` row listed by its resolved model is kept.
        assert_eq!(
            narrow_to_gateway_models(&sdk, &["claude-opus-5".into()], None)
                .iter()
                .map(|m| m.value.as_str())
                .collect::<Vec<_>>(),
            ["default"]
        );
        // A listed id in another spelling matches.
        assert_eq!(
            narrow_to_gateway_models(&sdk, &["claude-sonnet-5-20260101".into()], None)
                .iter()
                .map(|m| m.value.as_str())
                .collect::<Vec<_>>(),
            ["sonnet"]
        );
        // The gateway serves none: no model at all.
        assert!(narrow_to_gateway_models(&sdk, &["claude-opus-6".into()], None).is_empty());

        // Bedrock region prefix: an `eu` gateway drops `us.` rows.
        let bedrock = vec![
            resolved("eu-opus", "Opus", "eu.anthropic.claude-opus-5"),
            resolved("us-opus", "Opus", "us.anthropic.claude-opus-5"),
        ];
        assert_eq!(
            narrow_to_gateway_models(
                &bedrock,
                &[
                    "eu.anthropic.claude-opus-5".into(),
                    "us.anthropic.claude-opus-5".into()
                ],
                Some("eu")
            )
            .iter()
            .map(|m| m.value.as_str())
            .collect::<Vec<_>>(),
            ["eu-opus"]
        );
        // An alias-only row is kept only when some row matches by its model id;
        // a list of aliases alone serves none of the SDK's models.
        let alias_only = vec![
            ModelInfo {
                value: "default".into(),
                display_name: "Default".into(),
                ..Default::default()
            },
            ModelInfo {
                value: "opus".into(),
                display_name: "Opus".into(),
                ..Default::default()
            },
        ];
        assert!(narrow_to_gateway_models(&alias_only, &["claude-opus-5".into()], None).is_empty());
        // With a matching row present, the alias-only family row survives.
        let mut with_match = alias_only.clone();
        with_match.push(resolved("sonnet", "Sonnet", "claude-sonnet-5"));
        assert_eq!(
            narrow_to_gateway_models(
                &with_match,
                &["claude-sonnet-5".into(), "claude-opus-5".into()],
                None
            )
            .iter()
            .map(|m| m.value.as_str())
            .collect::<Vec<_>>(),
            ["default", "opus", "sonnet"]
        );
        // No listing wired yet: pass-through with the narrowed flag false.
        let (same, narrowed) = narrow_models_for_gateway(sdk.clone(), None, None);
        assert_eq!(same, sdk);
        assert!(!narrowed);
    }

    // O-40: `-1m` canonicalizes to `[1m]`, but the tokenizer only reads a
    // literal trailing `[1m]` as a hint, and a hint anywhere else in the id is
    // not a hint at all.
    #[test]
    fn f5b_context_hint_helpers() {
        assert_eq!(
            canonicalize_model_id("Claude-Opus-4-6-1M"),
            "claude-opus-4-6[1m]"
        );
        assert_eq!(
            canonicalize_model_id("claude-opus-4-6[1m]"),
            "claude-opus-4-6[1m]"
        );
        assert_eq!(context_hint_of("claude-opus-4-6-1m"), Some("1m".into()));
        assert_eq!(context_hint_of("claude-opus-4-6[1m]"), Some("1m".into()));
        assert_eq!(context_hint_of("claude-opus-4-6"), None);
        // `[1m]` mid-id is not the trailing hint.
        assert_eq!(context_hint_of("claude[1m]-opus-4-6"), None);

        let (tokens, hint) = tokenize_model_preference("claude-opus-4-6-1m");
        assert_eq!(hint, None);
        // Numeric tokens are dropped by the upstream tokenizer filter.
        assert_eq!(tokens, ["opus", "1m"]);
        let (tokens, hint) = tokenize_model_preference("opus[1m]");
        assert_eq!(hint, Some("1m".to_string()));
        assert_eq!(tokens, ["opus", "1m"]);
        assert!(tokenize_model_preference("claude").0.is_empty());
    }

    // O-41: family versions are read from hyphenated ids too (`claude-opus-4-6`
    // is 4.6), and the guard rejects a different generation.
    #[test]
    fn f5b_family_version_guard() {
        assert_eq!(
            extract_model_family_version("claude-opus-4-6"),
            Some("4.6".into())
        );
        assert_eq!(
            extract_model_family_version("claude-opus-4"),
            Some("4".into())
        );
        assert_eq!(extract_model_family_version("Opus 4.7"), Some("4.7".into()));
        assert_eq!(
            extract_model_family_version("claude-sonnet-4-5-20251001"),
            Some("4.5".into())
        );
        assert_eq!(extract_model_family_version("sonnet"), None);

        let candidates = [
            resolved(
                "claude-opus-4-6",
                "Claude Opus 4.6",
                "claude-opus-4-6-20251101",
            ),
            resolved("claude-opus-5", "Claude Opus 5", "claude-opus-5"),
        ];
        assert!(model_versions_compatible("claude-opus-4-6", &candidates[0]));
        assert!(!model_versions_compatible(
            "claude-opus-4-6",
            &candidates[1]
        ));
        // A preference without a version accepts any candidate.
        assert!(model_versions_compatible("opus", &candidates[1]));
        // A candidate with no version metadata cannot be ruled out.
        assert!(model_versions_compatible(
            "claude-opus-4-6",
            &model("custom-opus", "Custom")
        ));
    }

    // O-39: opusplan folds to opus; best/default/claude tokens drop out.
    #[test]
    fn f5b_resolve_preference_opusplan_and_versions() {
        let models = vec![
            resolved(
                "claude-sonnet-4-5",
                "Claude Sonnet 4.5",
                "claude-sonnet-4-5-20251001",
            ),
            resolved(
                "claude-opus-4-6",
                "Claude Opus 4.6",
                "claude-opus-4-6-20251101",
            ),
            resolved("claude-opus-5", "Claude Opus 5", "claude-opus-5"),
        ];
        assert_eq!(
            resolve_model_preference(&models, "opusplan").unwrap().value,
            "claude-opus-4-6"
        );
        // `best`/`default` leave no tokens, so the fuzzy tier yields nothing.
        assert!(resolve_model_preference(&models, "best").is_none());
        // The version guard stops `opus 4.6` from landing on Opus 5.
        assert_eq!(
            resolve_model_preference(&models, "opus 4.6").unwrap().value,
            "claude-opus-4-6"
        );
    }

    // O-34: ANTHROPIC_MODEL truthiness — an empty value must fall through to
    // settings.model; whitespace is truthy and, resolving to nothing, does NOT
    // fall through.
    #[test]
    fn f5b_model_priority_and_empty_env() {
        let models = vec![
            model("claude-sonnet-4-5", "Claude Sonnet 4.5"),
            model("claude-haiku-4-5", "Claude Haiku 4.5"),
        ];
        let settings = json!("haiku");

        let selection = get_available_models(
            &models,
            &models,
            Some(&settings),
            Some(""),
            false,
            None,
            false,
        );
        assert_eq!(selection.current_model_id, "claude-haiku-4-5");
        assert!(!selection.skip_set_model);

        let selection = get_available_models(
            &models,
            &models,
            Some(&settings),
            Some(" "),
            false,
            None,
            false,
        );
        assert_eq!(selection.current_model_id, "claude-sonnet-4-5");
        assert!(selection.skip_set_model);

        let selection = get_available_models(
            &models,
            &models,
            Some(&settings),
            Some("haiku"),
            false,
            None,
            false,
        );
        assert_eq!(selection.current_model_id, "claude-haiku-4-5");

        // No override: models[0], no setModel round-trip.
        let selection = get_available_models(&models, &models, None, None, false, None, false);
        assert_eq!(selection.current_model_id, "claude-sonnet-4-5");
        assert!(selection.skip_set_model);
    }

    // O-35: a non-string settings.model is ignored with the upstream message.
    #[test]
    fn f5b_settings_model_non_string_logged_and_ignored() {
        let models = vec![model("claude-sonnet-4-5", "Claude Sonnet 4.5")];
        for (value, label) in [
            (json!(null), "null"),
            (json!(42), "number"),
            (json!(true), "boolean"),
            (json!(["haiku"]), "object"),
            (json!({"value": "haiku"}), "object"),
        ] {
            let selection =
                get_available_models(&models, &models, Some(&value), None, false, None, false);
            assert_eq!(selection.current_model_id, "claude-sonnet-4-5");
            assert_eq!(
                selection.diagnostic.as_deref(),
                Some(
                    format!("Ignoring model from settings: expected a string, got {label}.")
                        .as_str()
                )
            );
        }
    }

    // O-20/O-36: a resumed session keeps an unlisted transcript model when no
    // gateway narrowed the list, and does not send setModel.
    #[test]
    fn f5b_resumed_unlisted_model_readback() {
        let models = vec![
            resolved("default", "Default", "claude-sonnet-4-5"),
            resolved("sonnet", "Sonnet", "claude-sonnet-4-5"),
        ];
        let sdk = models.clone();

        let selection = get_available_models(
            &models,
            &sdk,
            None,
            None,
            true,
            Some("claude-opus-5-20260101"),
            false,
        );
        assert_eq!(selection.current_model_id, "claude-opus-5-20260101");
        assert!(selection.skip_set_model);
        assert!(!selection.switch_to_listed);

        // A listed transcript model maps onto its picker row.
        let selection = get_available_models(
            &models,
            &sdk,
            None,
            None,
            true,
            Some("claude-sonnet-4-5"),
            false,
        );
        assert_eq!(selection.current_model_id, "default");
        assert!(selection.skip_set_model);
    }

    // A-05: gateway narrowing switches a resumed session off an unlisted
    // transcript model (and off a dropped default) onto models[0].
    #[test]
    fn f5b_switch_to_listed_when_gateway_dropped_default() {
        let sdk = vec![
            resolved("default", "Default", "claude-opus-5"),
            resolved("sonnet", "Sonnet", "claude-sonnet-5"),
        ];
        let listed = ["claude-sonnet-5".to_string()];
        let narrowed = narrow_to_gateway_models(&sdk, &listed, None);

        let selection = get_available_models(&narrowed, &sdk, None, None, false, None, true);
        assert_eq!(selection.current_model_id, "sonnet");
        assert!(selection.switch_to_listed);
        assert!(!selection.skip_set_model);
        assert_eq!(selection.model_before_switch, "default");

        // A resumed session on an unlisted transcript model switches too.
        let selection = get_available_models(
            &narrowed,
            &sdk,
            None,
            None,
            true,
            Some("claude-opus-5"),
            true,
        );
        assert_eq!(selection.current_model_id, "sonnet");
        assert!(selection.switch_to_listed);
        assert_eq!(selection.model_before_switch, "default");

        // A listed transcript model stays put.
        let selection = get_available_models(
            &narrowed,
            &sdk,
            None,
            None,
            true,
            Some("claude-sonnet-5"),
            true,
        );
        assert_eq!(selection.current_model_id, "sonnet");
        assert!(!selection.switch_to_listed);
        assert!(selection.skip_set_model);
    }

    // A-06/A-07: advisory failures stay on the prior model; a non-hook failure
    // on a fresh session is fatal; a resumed failure reads the transcript back.
    #[test]
    fn f5b_model_switch_failure_outcomes() {
        let models = vec![
            resolved("default", "Default", "claude-sonnet-5"),
            resolved("sonnet", "Sonnet", "claude-sonnet-5"),
        ];
        assert!(is_pre_model_switch_hook_block(
            "Model switch blocked by a PreModelSwitch hook: no"
        ));
        assert!(!is_pre_model_switch_hook_block("transport exploded"));

        assert_eq!(
            resolve_model_switch_failure(
                false,
                false,
                "transport exploded",
                &models,
                "default",
                None
            ),
            ModelSwitchFailure::Fail("transport exploded".to_string())
        );
        assert_eq!(
            resolve_model_switch_failure(
                false,
                false,
                "Model switch blocked by a PreModelSwitch hook: no",
                &models,
                "default",
                None
            ),
            ModelSwitchFailure::StayOn("default".to_string())
        );
        assert_eq!(
            resolve_model_switch_failure(false, true, "boom", &models, "default", None),
            ModelSwitchFailure::StayOn("default".to_string())
        );
        assert_eq!(
            resolve_model_switch_failure(
                true,
                false,
                "boom",
                &models,
                "sonnet",
                Some("claude-sonnet-5-20260101")
            ),
            // The loose match maps the transcript id onto the sonnet row.
            ModelSwitchFailure::StayOn("sonnet".to_string())
        );
        assert_eq!(
            resolve_model_switch_failure(true, false, "boom", &models, "sonnet", None),
            ModelSwitchFailure::StayOn("sonnet".to_string())
        );
    }

    // B-70: the backend key is the positional join of the routing vars; unset
    // vars yield one stable default bucket, and distinct backends differ.
    #[test]
    fn f5b_provider_cache_key_for() {
        let empty = EnvSnapshot::empty();
        let key = provider_cache_key_for(&empty);
        assert_eq!(key.split('\0').count(), PROVIDER_ROUTING_ENV_VARS.len());

        let mut other =
            EnvSnapshot::empty().with_var("ANTHROPIC_BASE_URL", "https://example.invalid");
        assert_ne!(provider_cache_key_for(&other), key);

        // Two different vars never collide positionally.
        other = other.with_var("ANTHROPIC_API_KEY", "k");
        let key2 = provider_cache_key_for(&other);
        assert_ne!(key, key2);
        assert_ne!(
            key2,
            provider_cache_key_for(&EnvSnapshot::empty().with_var("AWS_REGION", "k"))
        );

        // Values never appear in the composed model key's readable parts.
        assert_eq!(context_window_cache_key("p", "m"), "p\0m".to_string());
    }

    // A-10: a background get_context_usage answer is dropped once the window is
    // authoritative or the session switched models while it was in flight.
    #[test]
    fn f5b_context_window_answer_still_current() {
        assert!(context_window_answer_is_current(false, "sonnet", "sonnet"));
        assert!(!context_window_answer_is_current(false, "opus", "sonnet"));
        assert!(!context_window_answer_is_current(true, "sonnet", "sonnet"));
    }

    // B-69/B-71: immediateContextWindow reads the cross-session cache first,
    // then the `1m` heuristic, then the default; a cached window is
    // authoritative even when it equals the default.
    #[test]
    fn f5b_context_window_cache_and_immediate_window() {
        let provider = "f5b-immediate-window";
        let plain = ModelInfo {
            value: "claude-sonnet-5".into(),
            display_name: "Claude Sonnet 5".into(),
            description: String::new(),
            ..Default::default()
        };
        let seeded = immediate_context_window(provider, "claude-sonnet-5", Some(&plain));
        assert_eq!(seeded.size, crate::usage::DEFAULT_CONTEXT_WINDOW);
        assert!(!seeded.authoritative);

        // A cached authoritative window wins over the heuristic.
        cache_context_window(
            &context_window_cache_key(provider, "claude-sonnet-5"),
            967_000,
        );
        let seeded = immediate_context_window(provider, "claude-sonnet-5", Some(&plain));
        assert_eq!(seeded.size, 967_000);
        assert!(seeded.authoritative);

        // Cache key derivation uses `resolvedModel ?? modelId`.
        let resolved_plain = ModelInfo {
            resolved_model: Some("claude-sonnet-5[1m]".into()),
            ..plain.clone()
        };
        cache_context_window(
            &context_window_cache_key(provider, "claude-sonnet-5[1m]"),
            1_000_000,
        );
        let seeded = immediate_context_window(provider, "sonnet", Some(&resolved_plain));
        assert_eq!(seeded.size, 1_000_000);
        assert!(seeded.authoritative);

        // No cache: the heuristic from the id / metadata.
        let hinted = immediate_context_window(provider, "claude-opus-4-7[1m]", None);
        assert_eq!(hinted.size, 1_000_000);
        assert!(!hinted.authoritative);
        let alias = ModelInfo {
            value: "sonnet".into(),
            display_name: "Sonnet".into(),
            description: "1M context".into(),
            ..Default::default()
        };
        assert_eq!(
            immediate_context_window(provider, "sonnet", Some(&alias)).size,
            1_000_000
        );

        // Another backend's cache entry does not leak.
        assert_eq!(
            immediate_context_window("other-provider", "claude-sonnet-5", Some(&plain)).size,
            crate::usage::DEFAULT_CONTEXT_WINDOW
        );

        // `> 0` guard: zero is never cached.
        cache_context_window(&context_window_cache_key(provider, "zero"), 0);
        assert_eq!(
            cached_context_window(&context_window_cache_key(provider, "zero")),
            None
        );
    }

    // The versioned display-name rules: a terse label gains the concrete
    // version from independent metadata, while multi-word/custom labels stay.
    #[test]
    fn f5b_versioned_display_names() {
        let terse = ModelInfo {
            value: "sonnet".into(),
            display_name: "Sonnet".into(),
            description: String::new(),
            resolved_model: Some("claude-sonnet-4-5-20251001".into()),
            ..Default::default()
        };
        assert_eq!(versioned_model_display_name(&terse), "Sonnet 4.5");

        let with_context = ModelInfo {
            value: "opus".into(),
            display_name: "Opus (1M context)".into(),
            description: "Claude Opus 4.7 (1M context)".into(),
            resolved_model: Some("claude-opus-4-7[1m]".into()),
            ..Default::default()
        };
        // The description keeps the context, so the suffix is dropped.
        assert_eq!(versioned_model_display_name(&with_context), "Opus 4.7");

        let custom = ModelInfo {
            value: "custom".into(),
            display_name: "My Custom Model".into(),
            description: String::new(),
            resolved_model: Some("claude-opus-4-7".into()),
            ..Default::default()
        };
        assert_eq!(versioned_model_display_name(&custom), "My Custom Model");
    }

    /// FX-5 (O-37) — versioned model display name goldens from `model-presentation.test.ts`.
    #[test]
    fn model_presentation_goldens() {
        // Case 1: standard-family versions and preserves model identity and recommendation
        let opus = ModelInfo {
            value: "opus[1m]".into(),
            resolved_model: Some("claude-opus-5[1m]".into()),
            display_name: "Opus (1M context)".into(),
            description: "Opus 5 with 1M context · Best for everyday, complex tasks".into(),
            ..Default::default()
        };
        assert_eq!(versioned_model_display_name(&opus), "Opus 5");

        let sonnet_with_context = ModelInfo {
            value: "sonnet[1m]".into(),
            display_name: "Sonnet 5 (1M context)".into(),
            description: String::new(),
            ..Default::default()
        };
        assert_eq!(
            versioned_model_display_name(&sonnet_with_context),
            "Sonnet 5 (1M context)"
        );

        let custom_opus = ModelInfo {
            value: "custom".into(),
            display_name: "Custom Opus (1M context)".into(),
            description: String::new(),
            ..Default::default()
        };
        assert_eq!(
            versioned_model_display_name(&custom_opus),
            "Custom Opus (1M context)"
        );

        // Case 2: adds major and minor versions without changing custom or already-versioned names
        let models = [
            ModelInfo {
                value: "sonnet".into(),
                resolved_model: Some("claude-sonnet-5".into()),
                display_name: "Sonnet".into(),
                description: "Sonnet 5 · Efficient for routine tasks".into(),
                ..Default::default()
            },
            ModelInfo {
                value: "haiku".into(),
                resolved_model: Some("claude-haiku-4-5-20251001".into()),
                display_name: "Claude Haiku".into(),
                description: "Fast".into(),
                ..Default::default()
            },
            ModelInfo {
                value: "fable".into(),
                resolved_model: Some("claude-fable-5-1".into()),
                display_name: "Fable".into(),
                description: "Fable 5.1 · Creative model".into(),
                ..Default::default()
            },
            ModelInfo {
                value: "opus[1m]".into(),
                resolved_model: Some("claude-opus-5[1m]".into()),
                display_name: "Opus 5 (1M context)".into(),
                description: String::new(),
                ..Default::default()
            },
            ModelInfo {
                value: "custom".into(),
                display_name: "My Sonnet".into(),
                description: String::new(),
                ..Default::default()
            },
            ModelInfo {
                value: "future".into(),
                display_name: "Haiku (1M context)".into(),
                description: String::new(),
                ..Default::default()
            },
            ModelInfo {
                value: "custom-versioned".into(),
                display_name: "Fable 5.1 (1M context)".into(),
                description: String::new(),
                ..Default::default()
            },
        ];
        let names: Vec<String> = models.iter().map(versioned_model_display_name).collect();
        assert_eq!(
            names,
            vec![
                "Sonnet 5",
                "Claude Haiku 4.5",
                "Fable 5.1",
                "Opus 5 (1M context)",
                "My Sonnet",
                "Haiku (1M context)",
                "Fable 5.1 (1M context)",
            ]
        );

        // Case 3: only moves the context hint out of the name when the description retains it
        let described = ModelInfo {
            value: "opus-described[1m]".into(),
            resolved_model: Some("claude-opus-5[1m]".into()),
            display_name: "Opus 5 (1M context)".into(),
            description: "Opus 5 with 1M context".into(),
            ..Default::default()
        };
        assert_eq!(versioned_model_display_name(&described), "Opus 5");

        let undescribed = ModelInfo {
            value: "opus-undescribed[1m]".into(),
            resolved_model: Some("claude-opus-5[1m]".into()),
            display_name: "Opus 5 (1M context)".into(),
            description: String::new(),
            ..Default::default()
        };
        assert_eq!(
            versioned_model_display_name(&undescribed),
            "Opus 5 (1M context)"
        );

        let fable_undescribed = ModelInfo {
            value: "fable-undescribed[1m]".into(),
            resolved_model: Some("claude-fable-5-1[1m]".into()),
            display_name: "Fable (1M context)".into(),
            description: String::new(),
            ..Default::default()
        };
        assert_eq!(
            versioned_model_display_name(&fable_undescribed),
            "Fable 5.1 (1M context)"
        );

        // Case 4: collision fallback in get_available_models
        let collision_models = vec![
            opus.clone(),
            ModelInfo {
                value: "opus".into(),
                resolved_model: Some("claude-opus-5".into()),
                display_name: "Opus".into(),
                description: String::new(),
                ..Default::default()
            },
        ];
        let state = get_available_models(
            &collision_models,
            &collision_models,
            None,
            None,
            false,
            None,
            false,
        );
        let names: Vec<String> = state.available_models.into_iter().map(|m| m.name).collect();
        assert_eq!(names, vec!["Opus (1M context)", "Opus"]);
    }
}
