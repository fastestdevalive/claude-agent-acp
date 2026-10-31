//! Session modes, models, and config options management (rust-parity-fixes Decision 10/11, P6).
//!
//! Port of `dist/session-mode.js`, `dist/session-model.js`, and `dist/session-effort.js`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

pub use crate::session_model::{
    canonicalize_model_id, context_hint_of, extract_model_family_version, get_available_models,
    match_resumed_model, model_versions_compatible, resolve_model_preference, strip_context_hints,
    versioned_model_display_name, AvailableModel, ModelInfo, ModelSelection, MODEL_CONFIG_ID,
};

pub const MODE_CONFIG_ID: &str = "mode";
pub const EFFORT_CONFIG_ID: &str = "effort";
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
// Config option composition
// ---------------------------------------------------------------------------

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
