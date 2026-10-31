//! Session modes, models, and config options management (rust-parity-fixes Decision 10/11, P6).
//!
//! Port of `dist/session-mode.js`, `dist/session-model.js`, and `dist/session-effort.js`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

use crate::air::{with_air_meta, AirClient, AIR_RECOMMENDED_CONFIG_VALUE_CAPABILITY};

pub use crate::session_model::{
    canonicalize_model_id, context_hint_of, extract_model_family_version, get_available_models,
    match_resumed_model, model_versions_compatible, resolve_model_preference, strip_context_hints,
    versioned_model_display_name, AvailableModel, ModelInfo, ModelSelection, MODEL_CONFIG_ID,
};

pub const MODE_CONFIG_ID: &str = "mode";
pub const EFFORT_CONFIG_ID: &str = "effort";
// Shared sentinels and ids (`session-config-ids.ts`, `acp-agent.ts:12358-12366`).
pub const DEFAULT_MODEL_ID: &str = "default";
pub const DEFAULT_AGENT_ID: &str = "default";
pub const AGENT_CONFIG_ID: &str = "agent";
pub const FAST_MODE_CONFIG_ID: &str = "fast";
/// Select-fallback values used when the client has not opted into boolean
/// config options.
pub const FAST_MODE_ON: &str = "on";
pub const FAST_MODE_OFF: &str = "off";
const FAST_MODE_DESCRIPTION: &str = "Faster responses on supported models";
pub const AUTO_MODE_FALLBACK: &str = "acceptEdits";

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
///
/// A `select` option carries a string `currentValue` and `options`. A
/// `boolean` option (`option_type == "boolean"`, the Fast mode toggle for a
/// client that opted in) keeps its value as `"true"`/`"false"` here and
/// serializes it as a JSON boolean without an `options` key. An empty
/// `category` is omitted from the wire (the Agent option has none).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "ConfigOptionWire", into = "ConfigOptionWire")]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category: String,
    pub option_type: String,
    pub current_value: String,
    pub options: Vec<ConfigOptionValue>,
    pub _meta: Option<Value>,
}

/// The wire shape of a [`ConfigOption`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigOptionWire {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    #[serde(rename = "type")]
    option_type: String,
    current_value: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    options: Option<Vec<ConfigOptionValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    _meta: Option<Value>,
}

impl From<ConfigOption> for ConfigOptionWire {
    fn from(option: ConfigOption) -> Self {
        let boolean = option.option_type == BOOLEAN_OPTION_TYPE;
        Self {
            id: option.id,
            name: option.name,
            description: option.description,
            category: (!option.category.is_empty()).then_some(option.category),
            option_type: option.option_type,
            current_value: if boolean {
                Value::Bool(option.current_value == "true")
            } else {
                Value::String(option.current_value)
            },
            options: (!boolean).then_some(option.options),
            _meta: option._meta,
        }
    }
}

impl From<ConfigOptionWire> for ConfigOption {
    fn from(wire: ConfigOptionWire) -> Self {
        Self {
            id: wire.id,
            name: wire.name,
            description: wire.description,
            category: wire.category.unwrap_or_default(),
            option_type: wire.option_type,
            current_value: match wire.current_value {
                Value::String(text) => text,
                other => other.to_string(),
            },
            options: wire.options.unwrap_or_default(),
            _meta: wire._meta,
        }
    }
}

const BOOLEAN_OPTION_TYPE: &str = "boolean";

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
    /// Backend key for the cross-session context-window cache (F5b/B-70).
    pub provider_cache_key: String,
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
// Model config option composition
// ---------------------------------------------------------------------------

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
        // The concrete recommendation rides `_meta.jetbrains.air.recommendedValue`
        // (`session-model.ts:225-239`).
        _meta: if concrete_recommendation {
            recommended.map(|m| {
                with_air_meta(
                    None,
                    AIR_RECOMMENDED_CONFIG_VALUE_CAPABILITY,
                    json!(m.value),
                )
            })
        } else {
            None
        },
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
        // `session-effort.ts:117-126`.
        _meta: use_recommended_value.then(|| {
            with_air_meta(
                None,
                AIR_RECOMMENDED_CONFIG_VALUE_CAPABILITY,
                json!(recommended_effort),
            )
        }),
    })
}

/// `canonicalizeModelSettingsKey` (`session-effort.ts:29-34`): the same
/// spelling folding as the picker ids.
fn canonicalize_model_settings_key(value: &str) -> String {
    canonicalize_model_id(value)
}

/// Programmatic query settings have higher priority than file-backed settings
/// (`mergeEffortSettings`, `session-effort.ts:62-80`). Unrelated per-model
/// entries of the lower tier survive; entries the override supplies shadow
/// the lower tier's entries for the same canonical model key.
pub fn merge_effort_settings(base: &Value, override_settings: Option<&Value>) -> Value {
    let Some(override_settings) = override_settings else {
        return base.clone();
    };
    let base_models = base.get("modelSettings").and_then(Value::as_object);
    let override_models = override_settings
        .get("modelSettings")
        .and_then(Value::as_object);
    let mut merged = base.as_object().cloned().unwrap_or_default();
    if let Some(over) = override_settings.as_object() {
        for (key, value) in over {
            merged.insert(key.clone(), value.clone());
        }
    }
    if base_models.is_none() && override_models.is_none() {
        merged.remove("modelSettings");
        return Value::Object(merged);
    }
    let overridden: HashSet<String> = override_models
        .map(|m| {
            m.keys()
                .map(|k| canonicalize_model_settings_key(k))
                .collect()
        })
        .unwrap_or_default();
    let mut model_settings = serde_json::Map::new();
    for (key, entry) in base_models.into_iter().flatten() {
        if !overridden.contains(&canonicalize_model_settings_key(key)) {
            model_settings.insert(key.clone(), entry.clone());
        }
    }
    for (key, entry) in override_models.into_iter().flatten() {
        model_settings.insert(key.clone(), entry.clone());
    }
    merged.insert("modelSettings".into(), Value::Object(model_settings));
    Value::Object(merged)
}

/// The `appliedEffortLevel` after a direct effort pick
/// (`acp-agent.ts:10706`): the picked level, or none for `default`.
pub fn applied_effort_after_pick(value: &str) -> Option<String> {
    (value != DEFAULT_MODEL_ID).then(|| value.to_string())
}

/// The `appliedEffortLevel` after a successful effort sync on a model switch
/// (`acp-agent.ts:10654-10655`): only an opted-in (AIR) client applies the
/// concrete displayed level.
pub fn applied_effort_after_sync(
    use_recommended_value: bool,
    new_effort: Option<&str>,
) -> Option<String> {
    match new_effort {
        Some(level) if use_recommended_value && level != DEFAULT_MODEL_ID => {
            Some(level.to_string())
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Fast mode option (acp-agent.ts:12358-12470)
// ---------------------------------------------------------------------------

/// User-facing explanations for the SDK's `fast_mode_disabled_reason` values a
/// user can act on. Deliberately partial: `sdk_opt_in_required`, `preference`,
/// `pending`, `unknown` and any future reason are not surfaced.
fn fast_mode_unavailable_explanation(reason: &str) -> Option<&'static str> {
    match reason {
        "free" => Some("not available on the free plan"),
        "extra_usage_disabled" => Some("requires extra usage to be enabled for this account"),
        "model_not_allowed" => Some("not available for the selected model"),
        "not_first_party" => Some("not available on this API provider"),
        "disabled_by_env" => Some("disabled by environment configuration"),
        "network_error" => Some("eligibility could not be verified (network error)"),
        _ => None,
    }
}

/// Map the SDK's tri-state `fast_mode_state` onto the boolean toggle.
/// `cooldown` keeps the toggle on: only an explicit `off` clears it.
pub fn fast_mode_state_enabled(state: &str) -> bool {
    state != "off"
}

/// Normalize an SDK-reported `fast_mode_disabled_reason` to the one retained:
/// a reason with an explanation, else `None`.
pub fn normalize_fast_mode_disabled_reason(reason: Option<&str>) -> Option<String> {
    reason
        .filter(|r| fast_mode_unavailable_explanation(r).is_some())
        .map(String::from)
}

/// Whether the client advertised boolean session config options
/// (`session.configOptions.boolean` present and not null).
pub fn client_supports_boolean_config_options(client_capabilities: Option<&Value>) -> bool {
    client_capabilities
        .and_then(|c| c.get("session"))
        .and_then(|s| s.get("configOptions"))
        .and_then(|o| o.get("boolean"))
        .is_some_and(|v| !v.is_null())
}

/// Whether the client advertised the AIR `recommendedValue` capability.
pub fn client_supports_recommended_config_value(client_capabilities: Option<&Value>) -> bool {
    client_capabilities
        .and_then(AirClient::from_client_capabilities)
        .is_some_and(|air| air.supports(AIR_RECOMMENDED_CONFIG_VALUE_CAPABILITY))
}

/// Build the Fast mode config option: a native boolean toggle for a client that
/// opted in, an `on`/`off` select otherwise. While the toggle reads off, an
/// explainable `disabled_reason` is folded into the description.
pub fn create_fast_mode_config_option(
    enabled: bool,
    use_boolean_option: bool,
    disabled_reason: Option<&str>,
) -> ConfigOption {
    let explanation = if enabled {
        None
    } else {
        disabled_reason.and_then(fast_mode_unavailable_explanation)
    };
    let description = match explanation {
        Some(text) => format!("{FAST_MODE_DESCRIPTION} \u{2014} {text}"),
        None => FAST_MODE_DESCRIPTION.to_string(),
    };
    let base =
        |option_type: &str, current_value: &str, options: Vec<ConfigOptionValue>| ConfigOption {
            id: FAST_MODE_CONFIG_ID.into(),
            name: "Fast mode".into(),
            description: description.clone(),
            category: "model_config".into(),
            option_type: option_type.into(),
            current_value: current_value.into(),
            options,
            _meta: None,
        };
    if use_boolean_option {
        return base(
            BOOLEAN_OPTION_TYPE,
            if enabled { "true" } else { "false" },
            Vec::new(),
        );
    }
    let plain = |value: &str, name: &str| ConfigOptionValue {
        value: value.into(),
        name: name.into(),
        description: None,
        _meta: None,
    };
    base(
        "select",
        if enabled { FAST_MODE_ON } else { FAST_MODE_OFF },
        vec![plain(FAST_MODE_ON, "On"), plain(FAST_MODE_OFF, "Off")],
    )
}

/// JavaScript's template-literal rendering of a JSON value, for the
/// `Invalid value for config option` message.
fn js_display(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|v| {
                if v.is_null() {
                    String::new()
                } else {
                    js_display(v)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
        other => other.to_string(),
    }
}

/// Resolve the requested Fast mode value from a `session/set_config_option`
/// value: a native boolean or the `on`/`off` select fallback
/// (`resolveFastModeEnabled`, `acp-agent.ts:12464-12478`).
pub fn resolve_fast_mode_enabled(value: Option<&Value>) -> Result<bool, String> {
    match value {
        Some(Value::Bool(enabled)) => Ok(*enabled),
        Some(Value::String(text)) if text == FAST_MODE_ON => Ok(true),
        Some(Value::String(text)) if text == FAST_MODE_OFF => Ok(false),
        other => Err(format!(
            "Invalid value for config option {FAST_MODE_CONFIG_ID}: {}",
            other.map_or_else(|| "undefined".to_string(), js_display)
        )),
    }
}

/// Per-model Fast mode state threaded into [`build_config_options_with`]. The
/// option is only surfaced when the current model `supported`s fast mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FastModeOptionState {
    pub supported: bool,
    pub enabled: bool,
    /// Whether the client opted into boolean config options.
    pub use_boolean_option: bool,
    /// Latest explainable `fast_mode_disabled_reason`.
    pub disabled_reason: Option<String>,
}

/// Seed the Fast mode option from the SDK's `initialize` answer
/// (`acp-agent.ts:11565-11584`): the CLI may start with fast mode on, or force
/// it off. A reason is retained only while the toggle reads off and only when
/// it is explainable (a fresh SDK session reports `sdk_opt_in_required`, which
/// normalizes away). The toggle is surfaced only while the model supports it.
pub fn fast_mode_option_state_from_initialize(
    initialize: &Value,
    model_supports_fast_mode: bool,
    use_boolean_option: bool,
) -> FastModeOptionState {
    let enabled = initialize
        .get("fast_mode_state")
        .and_then(Value::as_str)
        .is_some_and(fast_mode_state_enabled);
    let disabled_reason = if enabled {
        None
    } else {
        normalize_fast_mode_disabled_reason(
            initialize
                .get("fast_mode_disabled_reason")
                .and_then(Value::as_str),
        )
    };
    FastModeOptionState {
        supported: model_supports_fast_mode,
        enabled,
        use_boolean_option,
        disabled_reason,
    }
}

// ---------------------------------------------------------------------------
// Agent option (acp-agent.ts:12318-12355, 12520-12560)
// ---------------------------------------------------------------------------

/// The agent to select at session creation: the requested `agent` option only
/// when the picker surfaces it, else `default`. A built-in or unknown name would
/// leave `currentValue` pointing at an entry outside its own `options`
/// (`acp-agent.ts:11553-11561`).
pub fn resolve_current_agent(requested: Option<&str>, agents: &[AgentInfo]) -> String {
    requested
        .filter(|name| !name.is_empty() && agents.iter().any(|a| a.name == *name))
        .unwrap_or(DEFAULT_AGENT_ID)
        .to_string()
}

/// A main-thread agent the SDK reports (`AgentInfo`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AgentInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// Claude Code's built-in subagents, which are not main-thread personas.
pub const BUILTIN_AGENT_NAMES: [&str; 5] = [
    "claude",
    "general-purpose",
    "Explore",
    "Plan",
    "statusline-setup",
];

/// Discover user/plugin/project-configured main-thread agents from the
/// `agents` the SDK's initialize response reports (`supportedAgents`),
/// excluding the built-in subagents and the reserved `default` sentinel. A
/// missing or malformed list yields none, so discovery never blocks session
/// creation (`discoverCustomAgents`).
pub fn discover_custom_agents(supported_agents: Option<&Value>) -> Vec<AgentInfo> {
    supported_agents
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let name = entry.get("name").and_then(Value::as_str)?;
                    let description = entry
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    Some(AgentInfo {
                        name: name.to_string(),
                        description: description.to_string(),
                    })
                })
                .filter(|agent| {
                    !BUILTIN_AGENT_NAMES.contains(&agent.name.as_str())
                        && agent.name != DEFAULT_AGENT_ID
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The Agent picker. Only built when at least one custom agent is configured,
/// since "Default" would otherwise be the only entry.
pub fn build_agent_config_option(
    agents: &[AgentInfo],
    current_agent: &str,
) -> Option<ConfigOption> {
    if agents.is_empty() {
        return None;
    }
    let mut options = vec![ConfigOptionValue {
        value: DEFAULT_AGENT_ID.into(),
        name: "Default".into(),
        description: Some("Standard Claude Code agent".into()),
        _meta: None,
    }];
    options.extend(agents.iter().map(|agent| ConfigOptionValue {
        value: agent.name.clone(),
        name: agent.name.clone(),
        description: (!agent.description.is_empty()).then(|| agent.description.clone()),
        _meta: None,
    }));
    Some(ConfigOption {
        id: AGENT_CONFIG_ID.into(),
        name: "Agent".into(),
        description: "Main-thread agent persona".into(),
        category: String::new(),
        option_type: "select".into(),
        current_value: current_agent.into(),
        options,
        _meta: None,
    })
}

/// The `applyFlagSettings` payload of a live agent switch: `null` clears the
/// flag layer back to the standard Claude Code agent (`acp-agent.ts:10683`).
pub fn agent_flag_settings(value: &str) -> Value {
    json!({ "agent": if value == DEFAULT_AGENT_ID { Value::Null } else { json!(value) } })
}

// ---------------------------------------------------------------------------
// Config option composition
// ---------------------------------------------------------------------------

/// Everything `buildConfigOptions` composes the option list from.
#[derive(Debug, Clone, Copy)]
pub struct ConfigOptionsInput<'a> {
    pub modes: &'a ModesState,
    pub available_models: &'a [AvailableModel],
    pub current_model_id: &'a str,
    pub model_infos: &'a [ModelInfo],
    pub current_effort: Option<&'a str>,
    pub agents: &'a [AgentInfo],
    pub current_agent: &'a str,
    pub fast_mode: Option<&'a FastModeOptionState>,
    pub use_recommended_value: bool,
}

/// Compose all session config options (`buildConfigOptions`,
/// `acp-agent.ts:12497-12562`): mode, model, effort, Fast mode (only when the
/// current model supports it), Agent (only when custom agents exist).
pub fn build_config_options_with(input: &ConfigOptionsInput<'_>) -> Vec<ConfigOption> {
    let mut options = vec![build_mode_config_option(input.modes)];
    // A gateway that lists none of the SDK's models leaves no model option.
    if !input.available_models.is_empty() {
        options.push(build_model_config_option(
            input.available_models,
            input.current_model_id,
            input.model_infos,
            input.use_recommended_value,
        ));
    }
    if let Some(effort_opt) = build_effort_config_option(
        input.model_infos,
        input.current_model_id,
        input.current_effort,
        input.use_recommended_value,
    ) {
        options.push(effort_opt);
    }
    if let Some(fast) = input.fast_mode.filter(|f| f.supported) {
        options.push(create_fast_mode_config_option(
            fast.enabled,
            fast.use_boolean_option,
            fast.disabled_reason.as_deref(),
        ));
    }
    options.extend(build_agent_config_option(input.agents, input.current_agent));
    options
}

/// Compose the mode, model and effort options (no Fast mode, no Agent).
pub fn build_config_options(
    modes: &ModesState,
    available_models: &[AvailableModel],
    current_model_id: &str,
    model_infos: &[ModelInfo],
    current_effort: Option<&str>,
    use_recommended_value: bool,
) -> Vec<ConfigOption> {
    build_config_options_with(&ConfigOptionsInput {
        modes,
        available_models,
        current_model_id,
        model_infos,
        current_effort,
        agents: &[],
        current_agent: DEFAULT_AGENT_ID,
        fast_mode: None,
        use_recommended_value,
    })
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
    fn inv_62_model_option_shape() {
        let models = vec![ModelInfo {
            value: "claude-sonnet-4-5".into(),
            display_name: "Claude Sonnet 4.5".into(),
            description: "Fast model".into(),
            supports_effort: Some(true),
            supported_effort_levels: Some(vec!["low".into(), "medium".into(), "high".into()]),
            ..Default::default()
        }];
        let selection = crate::session_model::get_available_models(
            &models, &models, None, None, false, None, false,
        );
        let opt = build_model_config_option(
            &selection.available_models,
            &selection.current_model_id,
            &models,
            false,
        );
        assert_eq!(opt.id, "model");
        assert_eq!(opt.current_value, "claude-sonnet-4-5");
        assert_eq!(opt.options.len(), 1);
        assert_eq!(opt.options[0].value, "claude-sonnet-4-5");

        let effort_opt =
            build_effort_config_option(&models, &selection.current_model_id, None, false).unwrap();
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
        let selection = crate::session_model::get_available_models(
            &models, &models, None, None, false, None, false,
        );
        let config_opts = build_config_options(
            &modes,
            &selection.available_models,
            &selection.current_model_id,
            &models,
            None,
            false,
        );

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
