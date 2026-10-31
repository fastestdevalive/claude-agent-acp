//! Session modes, models, and config options management (rust-parity-fixes Decision 10/11, P6).
//!
//! Port of `dist/session-mode.js`, `dist/session-model.js`, and `dist/session-effort.js`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

pub const MODE_CONFIG_ID: &str = "mode";
pub const MODEL_CONFIG_ID: &str = "model";
pub const EFFORT_CONFIG_ID: &str = "effort";
pub const AUTO_MODE_FALLBACK: &str = "acceptEdits";

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

/// An available permission mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModeInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub _meta: Option<Value>,
}

/// State of session permission modes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModesState {
    pub current_mode_id: String,
    pub available_modes: Vec<ModeInfo>,
}

/// A config option surfaced to the client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category: String,
    #[serde(rename = "type")]
    pub option_type: String,
    pub current_value: String,
    pub options: Vec<ConfigOptionValue>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub _meta: Option<Value>,
}

/// A selectable option value inside a select-type config option.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionValue {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub _meta: Option<Value>,
}

/// Actor-owned session configuration state (rust-parity-fixes Decision 10).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionConfigState {
    pub modes: ModesState,
    pub available_models: Vec<AvailableModel>,
    pub current_model_id: String,
    pub model_infos: Vec<ModelInfo>,
    pub config_options: Vec<ConfigOption>,
    pub current_effort: Option<String>,
    pub effort_pinned: Option<String>,
    pub applied_effort: Option<String>,
    pub allow_bypass: bool,
    pub is_air: bool,
    pub ultracode: bool,
    pub pre_plan_mode: Option<String>,
}

impl Default for ModesState {
    fn default() -> Self {
        Self {
            current_mode_id: "default".into(),
            available_modes: Vec::new(),
        }
    }
}

/// Build the list of available modes (SessionModeManager.buildAvailableModes).
pub fn build_available_modes(allow_bypass: bool, air: bool) -> Vec<ModeInfo> {
    let air_kind = |kind: &str| -> Option<Value> {
        if air {
            Some(json!({
                "jetbrains": {
                    "air": {
                        "version": 1,
                        "kind": kind
                    }
                }
            }))
        } else {
            None
        }
    };

    let mut modes = vec![
        ModeInfo {
            id: "default".into(),
            name: "Manual".into(),
            description: "Always ask before making changes".into(),
            _meta: air_kind("standard"),
        },
        ModeInfo {
            id: "acceptEdits".into(),
            name: "Accept edits".into(),
            description: "Automatically accept all file edits".into(),
            _meta: air_kind("standard"),
        },
        ModeInfo {
            id: "plan".into(),
            name: "Plan".into(),
            description: "Create a plan before making changes".into(),
            _meta: air_kind("plan"),
        },
        ModeInfo {
            id: "auto".into(),
            name: "Auto".into(),
            description: "Claude handles permission decisions".into(),
            _meta: air_kind("auto_review"),
        },
    ];

    if allow_bypass {
        modes.push(ModeInfo {
            id: "bypassPermissions".into(),
            name: "Bypass permissions".into(),
            description: "Accepts all permissions".into(),
            _meta: air_kind("full_access"),
        });
    }

    modes
}

/// Build the `mode` config option from the current modes state.
pub fn build_mode_config_option(modes: &ModesState) -> ConfigOption {
    ConfigOption {
        id: MODE_CONFIG_ID.into(),
        name: "Mode".into(),
        description: "Session permission mode".into(),
        category: "mode".into(),
        option_type: "select".into(),
        current_value: modes.current_mode_id.clone(),
        options: modes
            .available_modes
            .iter()
            .map(|m| ConfigOptionValue {
                value: m.id.clone(),
                name: m.name.clone(),
                description: Some(m.description.clone()),
                _meta: m._meta.clone(),
            })
            .collect(),
        _meta: None,
    }
}

// ---------------------------------------------------------------------------
// Model matching and resolution (session-model.js)
// ---------------------------------------------------------------------------

pub fn strip_context_hints(value: &str) -> String {
    let mut s = value.to_string();
    // Strip `[<digits>m]`
    while let Some(start) = s.rfind('[') {
        if let Some(rel_end) = s[start..].find(']') {
            let end = start + rel_end;
            let inner = &s[start + 1..end];
            if inner.ends_with('m') || inner.ends_with('M') {
                let digits = &inner[..inner.len() - 1];
                if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                    s.replace_range(start..=end, "");
                    continue;
                }
            }
        }
        break;
    }
    // Strip `-<digits>m` at the end
    if let Some(dash) = s.rfind('-') {
        let suffix = &s[dash + 1..];
        if suffix.ends_with('m') || suffix.ends_with('M') {
            let digits = &suffix[..suffix.len() - 1];
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                s.truncate(dash);
            }
        }
    }
    s
}

pub fn canonicalize_model_id(value: &str) -> String {
    let trimmed = value.trim().to_lowercase();
    if let Some(dash) = trimmed.rfind('-') {
        let suffix = &trimmed[dash + 1..];
        if suffix.ends_with('m') {
            if let Some(digits) = suffix.strip_suffix('m') {
                if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                    return format!("{}[{suffix}]", &trimmed[..dash]);
                }
            }
        }
    }
    trimmed
}

pub fn context_hint_of(value: &str) -> Option<String> {
    let canon = canonicalize_model_id(value);
    let start = canon.rfind('[')?;
    let end = canon.rfind(']')?;
    if end > start + 1 {
        let inner = &canon[start + 1..end];
        if inner.ends_with('m') && inner[..inner.len() - 1].chars().all(|c| c.is_ascii_digit()) {
            return Some(inner.to_string());
        }
    }
    None
}

pub fn extract_model_family_version(value: &str) -> Option<String> {
    let stripped = strip_context_hints(value);
    let words = stripped.split(|c: char| !c.is_alphanumeric() && c != '.' && c != '-');
    for word in words {
        let parts: Vec<&str> = word.split(['.', '-']).collect();
        if !parts.is_empty() && parts[0].chars().all(|c| c.is_ascii_digit()) && !parts[0].is_empty()
        {
            if parts.len() > 1
                && parts[1].chars().all(|c| c.is_ascii_digit())
                && !parts[1].is_empty()
            {
                return Some(format!("{}.{}", parts[0], parts[1]));
            } else {
                return Some(parts[0].to_string());
            }
        }
    }
    None
}

pub fn model_versions_compatible(preference: &str, candidate: &ModelInfo) -> bool {
    let Some(preferred) = extract_model_family_version(preference) else {
        return true;
    };
    let candidate_version = extract_model_family_version(&candidate.value)
        .or_else(|| extract_model_family_version(&candidate.display_name))
        .or_else(|| extract_model_family_version(&candidate.description));
    match candidate_version {
        Some(cand) => preferred == cand,
        None => true,
    }
}

fn display_model_version(model: &ModelInfo, family: &str) -> Option<String> {
    let family_lower = family.to_lowercase();
    for source in [
        model.resolved_model.as_deref(),
        Some(model.description.as_str()),
        Some(model.value.as_str()),
    ] {
        let Some(text) = source else { continue };
        let lower = text.to_lowercase();
        if let Some(pos) = lower.find(&family_lower) {
            let after = &lower[pos + family_lower.len()..];
            let after_trimmed = after.trim_start_matches(['-', ' ']);
            let mut parts =
                after_trimmed.split(|c: char| !c.is_alphanumeric() && c != '.' && c != '-');
            if let Some(candidate) = parts.next() {
                let subparts: Vec<&str> = candidate.split(['.', '-']).collect();
                if !subparts.is_empty()
                    && subparts[0].chars().all(|c| c.is_ascii_digit())
                    && !subparts[0].is_empty()
                {
                    if subparts.len() > 1
                        && subparts[1].chars().all(|c| c.is_ascii_digit())
                        && !subparts[1].is_empty()
                    {
                        return Some(format!("{}.{}", subparts[0], subparts[1]));
                    } else {
                        return Some(subparts[0].to_string());
                    }
                }
            }
        }
    }
    None
}

pub fn versioned_model_display_name(model: &ModelInfo) -> String {
    let name = model.display_name.trim();
    // Match `^(?:Claude\s+)?([a-z][a-z0-9-]*)(?:\s+\d+(?:\.\d+)?)?(?:\s+\(\d+[mk]\s+context\))?$`
    let stripped_claude = if let Some(rest) = name.strip_prefix("Claude ") {
        rest
    } else {
        name
    };
    let mut tokens = stripped_claude.split_whitespace();
    let Some(first_word) = tokens.next() else {
        return name.to_string();
    };
    if !first_word
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return name.to_string();
    }
    let family = first_word;
    let Some(version) = display_model_version(model, family) else {
        return name.to_string();
    };

    // If displayName already has version digits, keep original
    let without_context = if let Some(idx) = name.find(" (") {
        &name[..idx]
    } else {
        name
    };
    if without_context.split_whitespace().any(|word| {
        word.chars().any(|c| c.is_ascii_digit()) && !word.ends_with('k') && !word.ends_with('m')
    }) {
        return name.to_string();
    }

    if let Some(idx) = name.find(" (") {
        let suffix = &name[idx..];
        format!("{without_context} {version}{suffix}")
    } else {
        format!("{without_context} {version}")
    }
}

fn tokenize_model_preference(model: &str) -> (Vec<String>, Option<String>) {
    let lower = model.trim().to_lowercase();
    let context_hint = context_hint_of(&lower);
    let mut tokens = Vec::new();
    for part in lower.split(|c: char| !c.is_alphanumeric()) {
        if part.is_empty() {
            continue;
        }
        let token = if part == "opusplan" {
            "opus"
        } else if part == "best" || part == "default" || part == "claude" {
            continue;
        } else {
            part
        };
        if token.chars().any(|c| c.is_alphabetic()) || token.ends_with('m') {
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
        if !haystack.contains(token) {
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

pub fn resolve_model_preference(models: &[ModelInfo], preference: &str) -> Option<ModelInfo> {
    let trimmed = preference.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();
    let canon_pref = canonicalize_model_id(trimmed);

    // 1. Direct match on value, canonicalized value, or display_name
    if let Some(m) = models.iter().find(|m| {
        m.value == trimmed
            || canonicalize_model_id(&m.value) == canon_pref
            || m.display_name.to_lowercase() == lower
    }) {
        return Some(m.clone());
    }

    // 2. Matches resolvedModel
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

    // 3. Includes match
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

    // 4. Token-based scoring
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

pub fn build_model_config_option(
    available_models: &[AvailableModel],
    current_model_id: &str,
    model_infos: &[ModelInfo],
    use_recommended_value: bool,
) -> ConfigOption {
    let default_info = model_infos.iter().find(|m| m.value == "default");
    let selectable_ids: HashSet<&str> = available_models
        .iter()
        .map(|m| m.model_id.as_str())
        .collect();
    let concrete_infos: Vec<&ModelInfo> = model_infos
        .iter()
        .filter(|m| m.value != "default" && selectable_ids.contains(m.value.as_str()))
        .collect();

    let default_resolved = default_info.and_then(|m| m.resolved_model.as_deref());
    let recommended = default_resolved.and_then(|res| {
        let canon = canonicalize_model_id(res);
        concrete_infos.into_iter().find(|m| {
            let target = m.resolved_model.as_deref().unwrap_or(&m.value);
            canonicalize_model_id(target) == canon
        })
    });

    let concrete_recommendation = use_recommended_value && recommended.is_some();
    let available: Vec<&AvailableModel> = if concrete_recommendation {
        available_models
            .iter()
            .filter(|m| m.model_id != "default")
            .collect()
    } else {
        available_models.iter().collect()
    };

    let current_value = if concrete_recommendation && current_model_id == "default" {
        recommended
            .map(|m| m.value.clone())
            .unwrap_or_else(|| current_model_id.to_string())
    } else {
        current_model_id.to_string()
    };

    let options: Vec<ConfigOptionValue> = available
        .into_iter()
        .map(|model| {
            if model.model_id == "default" {
                if let Some(def) = default_info {
                    if let Some(ref res) = def.resolved_model {
                        let named = model_infos.iter().find(|m| {
                            m.value != "default"
                                && m.resolved_model.as_deref() == Some(res.as_str())
                        });
                        return ConfigOptionValue {
                            value: model.model_id.clone(),
                            name: model.name.clone(),
                            description: Some(
                                named
                                    .map(|n| n.display_name.clone())
                                    .unwrap_or_else(|| res.clone()),
                            ),
                            _meta: None,
                        };
                    }
                }
            }
            ConfigOptionValue {
                value: model.model_id.clone(),
                name: model.name.clone(),
                description: model.description.clone(),
                _meta: None,
            }
        })
        .collect();

    ConfigOption {
        id: MODEL_CONFIG_ID.into(),
        name: "Model".into(),
        description: "AI model to use".into(),
        category: "model".into(),
        option_type: "select".into(),
        current_value,
        options,
        _meta: None,
    }
}

// ---------------------------------------------------------------------------
// Effort option (session-effort.js)
// ---------------------------------------------------------------------------

pub fn to_sdk_effort_level(value: Option<&str>) -> Option<String> {
    match value {
        None | Some("default") => None,
        Some(v) => Some(v.to_string()),
    }
}

pub fn effort_flag_settings(value: Option<&str>, ultracode: bool) -> Value {
    let effort_level = to_sdk_effort_level(value);
    let mut map = serde_json::Map::new();
    match effort_level {
        Some(e) => map.insert("effortLevel".into(), json!(e)),
        None => map.insert("effortLevel".into(), Value::Null),
    };
    if ultracode {
        map.insert("ultracode".into(), json!(true));
    }
    Value::Object(map)
}

/// Resolve the effort the CLI will use: per-model settings first, then the legacy top-level effort setting.
pub fn settings_effort_for_model(
    model_settings: Option<&Value>,
    fallback_effort: Option<&str>,
    model_info: Option<&ModelInfo>,
    model_id: &str,
) -> Option<String> {
    if let Some(settings_obj) = model_settings.and_then(Value::as_object) {
        let keys_to_try = [
            model_info.and_then(|m| m.resolved_model.as_deref()),
            model_info.map(|m| m.value.as_str()),
            Some(model_id),
        ];
        for k in keys_to_try.into_iter().flatten() {
            if let Some(entry) = settings_obj.get(k) {
                if let Some(lvl) = entry.get("effortLevel").and_then(Value::as_str) {
                    return Some(lvl.to_string());
                }
            }
            let canon = canonicalize_model_id(k);
            for (candidate, entry) in settings_obj {
                if canonicalize_model_id(candidate) == canon {
                    if let Some(lvl) = entry.get("effortLevel").and_then(Value::as_str) {
                        return Some(lvl.to_string());
                    }
                }
            }
        }
    }
    fallback_effort.map(String::from)
}

pub fn build_effort_config_option(
    model_infos: &[ModelInfo],
    current_model_id: &str,
    current_effort_level: Option<&str>,
    use_recommended_value: bool,
) -> Option<ConfigOption> {
    let current_model_info = model_infos.iter().find(|m| m.value == current_model_id)?;
    if current_model_info.supports_effort != Some(true) {
        return None;
    }
    let supported_levels = current_model_info.supported_effort_levels.as_deref()?;
    if supported_levels.is_empty() {
        return None;
    }

    let recommended_effort = if supported_levels.iter().any(|l| l == "medium") {
        "medium"
    } else {
        &supported_levels[0]
    };

    let mut options = Vec::new();
    if !use_recommended_value {
        options.push(ConfigOptionValue {
            value: "default".into(),
            name: "Default".into(),
            description: None,
            _meta: None,
        });
    }

    for level in supported_levels {
        let capitalized = level
            .split(['_', '-'])
            .map(|part| {
                if part.is_empty() {
                    String::new()
                } else {
                    let mut chars = part.chars();
                    match chars.next() {
                        None => String::new(),
                        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" ");

        options.push(ConfigOptionValue {
            value: level.clone(),
            name: capitalized,
            description: None,
            _meta: None,
        });
    }

    let includes = |level: &str| {
        (!use_recommended_value && level == "default")
            || supported_levels.iter().any(|l| l == level)
    };
    let current_value = match current_effort_level {
        Some(lvl) if includes(lvl) => lvl.to_string(),
        _ => {
            if use_recommended_value {
                recommended_effort.to_string()
            } else {
                "default".to_string()
            }
        }
    };

    Some(ConfigOption {
        id: EFFORT_CONFIG_ID.into(),
        name: "Effort".into(),
        description: "Available effort levels for this model".into(),
        category: "thought_level".into(),
        option_type: "select".into(),
        current_value,
        options,
        _meta: None,
    })
}

// ---------------------------------------------------------------------------
// Model catalog computation (getAvailableModels)
// ---------------------------------------------------------------------------

pub fn get_available_models(
    models: &[ModelInfo],
    sdk_models: &[ModelInfo],
    settings_model: Option<&str>,
    env_model: Option<&str>,
    is_resumed_session: bool,
    resumed_model_hint: Option<&str>,
) -> (Vec<AvailableModel>, String, bool) {
    if models.is_empty() {
        let cur = if is_resumed_session {
            resumed_model_hint.unwrap_or(
                sdk_models
                    .first()
                    .map(|m| m.value.as_str())
                    .unwrap_or("default"),
            )
        } else {
            sdk_models
                .first()
                .map(|m| m.value.as_str())
                .unwrap_or("default")
        };
        return (Vec::new(), cur.to_string(), false);
    }

    let mut current_model = &models[0];
    let mut resolved_from_input: Option<String> = None;

    if let Some(env_m) = env_model {
        if !env_m.trim().is_empty() {
            if let Some(matched) = resolve_model_preference(models, env_m) {
                if let Some(pos) = models.iter().position(|m| m.value == matched.value) {
                    current_model = &models[pos];
                    resolved_from_input = Some(env_m.to_string());
                }
            }
        }
    } else if let Some(sett_m) = settings_model {
        if !sett_m.trim().is_empty() {
            if let Some(matched) = resolve_model_preference(models, sett_m) {
                if let Some(pos) = models.iter().position(|m| m.value == matched.value) {
                    current_model = &models[pos];
                    resolved_from_input = Some(sett_m.to_string());
                }
            }
        }
    }

    if resolved_from_input.is_none() && is_resumed_session {
        if let Some(hint) = resumed_model_hint {
            let matched = match_resumed_model(models, hint);
            if let Some(pos) = models.iter().position(|m| m.value == matched.value) {
                current_model = &models[pos];
            }
        }
    }

    let sdk_saw_same_value = sdk_models.iter().any(|m| m.value == current_model.value);
    let sdk_default = sdk_models.iter().find(|m| m.value == "default");
    let sdk_default_dropped = sdk_default.is_some() && !models.iter().any(|m| m.value == "default");
    let switch_to_listed = resolved_from_input.is_none()
        && (!is_resumed_session || resumed_model_hint.is_none())
        && sdk_default_dropped;

    let skip_set_model = (resolved_from_input.is_none() && !switch_to_listed)
        || (!is_resumed_session
            && resolved_from_input.as_deref() == Some(&current_model.value)
            && sdk_saw_same_value);

    let display_names: Vec<String> = models.iter().map(versioned_model_display_name).collect();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for name in &display_names {
        *counts.entry(name.clone()).or_insert(0) += 1;
    }

    let available: Vec<AvailableModel> = models
        .iter()
        .enumerate()
        .map(|(i, model)| {
            let name = if counts.get(&display_names[i]) == Some(&1) {
                display_names[i].clone()
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

    (available, current_model.value.clone(), !skip_set_model)
}

/// Compose all session config options for a session.
pub fn build_config_options(
    modes: &ModesState,
    available_models: &[AvailableModel],
    current_model_id: &str,
    model_infos: &[ModelInfo],
    current_effort: Option<&str>,
    use_recommended_value: bool,
) -> Vec<ConfigOption> {
    let mut options = Vec::new();
    options.push(build_mode_config_option(modes));
    if !available_models.is_empty() {
        options.push(build_model_config_option(
            available_models,
            current_model_id,
            model_infos,
            use_recommended_value,
        ));
    }
    if let Some(effort_opt) = build_effort_config_option(
        model_infos,
        current_model_id,
        current_effort,
        use_recommended_value,
    ) {
        options.push(effort_opt);
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inv_62_mode_catalog() {
        let modes_no_bypass = build_available_modes(false, false);
        assert_eq!(modes_no_bypass.len(), 4);
        assert_eq!(modes_no_bypass[0].id, "default");
        assert_eq!(modes_no_bypass[0].name, "Manual");
        assert_eq!(modes_no_bypass[1].id, "acceptEdits");
        assert_eq!(modes_no_bypass[1].name, "Accept edits");
        assert_eq!(modes_no_bypass[2].id, "plan");
        assert_eq!(modes_no_bypass[2].name, "Plan");
        assert_eq!(modes_no_bypass[3].id, "auto");
        assert_eq!(modes_no_bypass[3].name, "Auto");
        assert!(modes_no_bypass[0]._meta.is_none());

        let modes_bypass = build_available_modes(true, false);
        assert_eq!(modes_bypass.len(), 5);
        assert_eq!(modes_bypass[4].id, "bypassPermissions");
        assert_eq!(modes_bypass[4].name, "Bypass permissions");

        let modes_air = build_available_modes(true, true);
        assert_eq!(modes_air.len(), 5);
        assert_eq!(
            modes_air[0]
                ._meta
                .as_ref()
                .unwrap()
                .pointer("/jetbrains/air/kind")
                .and_then(Value::as_str),
            Some("standard")
        );
        assert_eq!(
            modes_air[2]
                ._meta
                .as_ref()
                .unwrap()
                .pointer("/jetbrains/air/kind")
                .and_then(Value::as_str),
            Some("plan")
        );
        assert_eq!(
            modes_air[3]
                ._meta
                .as_ref()
                .unwrap()
                .pointer("/jetbrains/air/kind")
                .and_then(Value::as_str),
            Some("auto_review")
        );
        assert_eq!(
            modes_air[4]
                ._meta
                .as_ref()
                .unwrap()
                .pointer("/jetbrains/air/kind")
                .and_then(Value::as_str),
            Some("full_access")
        );
    }

    #[test]
    fn inv_62_resolve_model_preference_table() {
        let models = vec![
            ModelInfo {
                value: "claude-sonnet-4-5".into(),
                display_name: "Claude Sonnet 4.5".into(),
                description: "Fast, efficient model".into(),
                ..Default::default()
            },
            ModelInfo {
                value: "claude-haiku-4-5".into(),
                display_name: "Claude Haiku 4.5".into(),
                description: "Lightweight model".into(),
                ..Default::default()
            },
            ModelInfo {
                value: "claude-opus-4".into(),
                display_name: "Claude Opus 4".into(),
                description: "Powerful reasoning model".into(),
                ..Default::default()
            },
        ];

        // Exact match
        assert_eq!(
            resolve_model_preference(&models, "claude-sonnet-4-5")
                .unwrap()
                .value,
            "claude-sonnet-4-5"
        );
        // Alias match
        assert_eq!(
            resolve_model_preference(&models, "sonnet").unwrap().value,
            "claude-sonnet-4-5"
        );
        assert_eq!(
            resolve_model_preference(&models, "haiku").unwrap().value,
            "claude-haiku-4-5"
        );
        assert_eq!(
            resolve_model_preference(&models, "opus").unwrap().value,
            "claude-opus-4"
        );
        // Unknown
        assert!(resolve_model_preference(&models, "gpt-4o").is_none());
        assert!(resolve_model_preference(&models, "").is_none());
    }

    #[test]
    fn inv_62_match_resumed_model() {
        let models = vec![
            ModelInfo {
                value: "claude-sonnet-4-5".into(),
                display_name: "Claude Sonnet 4.5".into(),
                description: "".into(),
                resolved_model: Some("claude-sonnet-4-5-20251001".into()),
                ..Default::default()
            },
            ModelInfo {
                value: "default".into(),
                display_name: "Default".into(),
                description: "".into(),
                resolved_model: Some("claude-sonnet-4-5-20251001".into()),
                ..Default::default()
            },
        ];

        let matched = match_resumed_model(&models, "claude-sonnet-4-5-20251001");
        assert_eq!(matched.value, "default");

        let matched_other = match_resumed_model(&models, "claude-haiku-4-5");
        assert_eq!(matched_other.value, "claude-haiku-4-5");
    }

    #[test]
    fn inv_62_get_available_models_env_and_settings() {
        let models = vec![
            ModelInfo {
                value: "claude-sonnet-4-5".into(),
                display_name: "Claude Sonnet 4.5".into(),
                description: "Desc".into(),
                ..Default::default()
            },
            ModelInfo {
                value: "claude-haiku-4-5".into(),
                display_name: "Claude Haiku 4.5".into(),
                description: "Desc".into(),
                ..Default::default()
            },
        ];

        // Default picks first model
        let (avail, cur, need_sync) =
            get_available_models(&models, &models, None, None, false, None);
        assert_eq!(avail.len(), 2);
        assert_eq!(cur, "claude-sonnet-4-5");
        assert!(!need_sync);

        // Env model overrides
        let (_, cur_env, need_sync_env) =
            get_available_models(&models, &models, None, Some("haiku"), false, None);
        assert_eq!(cur_env, "claude-haiku-4-5");
        assert!(need_sync_env);

        // Settings model overrides when env unset
        let (_, cur_sett, need_sync_sett) =
            get_available_models(&models, &models, Some("haiku"), None, false, None);
        assert_eq!(cur_sett, "claude-haiku-4-5");
        assert!(need_sync_sett);
    }

    #[test]
    fn inv_62_model_option_shape() {
        let models = vec![ModelInfo {
            value: "claude-sonnet-4-5".into(),
            display_name: "Claude Sonnet 4.5".into(),
            description: "Fast model".into(),
            supports_effort: Some(true),
            supported_effort_levels: Some(vec!["low".into(), "medium".into(), "high".into()]),
            ..Default::default()
        }];
        let (avail, cur, _) = get_available_models(&models, &models, None, None, false, None);
        let opt = build_model_config_option(&avail, &cur, &models, false);
        assert_eq!(opt.id, "model");
        assert_eq!(opt.current_value, "claude-sonnet-4-5");
        assert_eq!(opt.options.len(), 1);
        assert_eq!(opt.options[0].value, "claude-sonnet-4-5");

        let effort_opt = build_effort_config_option(&models, &cur, None, false).unwrap();
        assert_eq!(effort_opt.id, "effort");
        assert_eq!(effort_opt.current_value, "default");
        assert_eq!(effort_opt.options.len(), 4); // default, low, medium, high
        assert_eq!(effort_opt.options[0].value, "default");
        assert_eq!(effort_opt.options[1].value, "low");
        assert_eq!(effort_opt.options[1].name, "Low");
    }

    #[test]
    fn inv_62_set_config_validation_errors() {
        let modes = ModesState {
            current_mode_id: "default".into(),
            available_modes: build_available_modes(false, false),
        };
        let models = vec![ModelInfo {
            value: "claude-sonnet-4-5".into(),
            display_name: "Sonnet".into(),
            ..Default::default()
        }];
        let (avail, cur, _) = get_available_models(&models, &models, None, None, false, None);
        let config_opts = build_config_options(&modes, &avail, &cur, &models, None, false);

        // Unknown config option check
        let opt = config_opts.iter().find(|o| o.id == "bogus");
        assert!(opt.is_none());

        // Mode validation
        let mode_opt = config_opts.iter().find(|o| o.id == "mode").unwrap();
        assert!(mode_opt.options.iter().any(|o| o.value == "default"));
        assert!(!mode_opt
            .options
            .iter()
            .any(|o| o.value == "bypassPermissions")); // bypass not allowed
    }
}
