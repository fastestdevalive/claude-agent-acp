//! Turn 1 lane B: pure config-option rows (F11 pure half) ported from the
//! upstream `recommended-config-values`, `fast-mode-config`, `model-resolution`
//! and `acp-agent` (agent picker) tests. Every case is a pure function call, so
//! no case can hang.

use claude_agent_acp_rs::session_config::{
    agent_flag_settings, applied_effort_after_pick, applied_effort_after_sync,
    build_config_options, build_config_options_with, client_supports_boolean_config_options,
    client_supports_recommended_config_value, create_fast_mode_config_option,
    discover_custom_agents, effort_flag_settings, fast_mode_option_state_from_initialize,
    fast_mode_state_enabled, merge_effort_settings, normalize_fast_mode_disabled_reason,
    resolve_current_agent, resolve_fast_mode_enabled, settings_effort_for_model, AgentInfo,
    AvailableModel, ConfigOption, ConfigOptionsInput, FastModeOptionState, ModeInfo, ModelInfo,
    ModesState, BUILTIN_AGENT_NAMES, FAST_MODE_CONFIG_ID, FAST_MODE_OFF, FAST_MODE_ON,
};
use serde_json::{json, Value};

fn modes() -> ModesState {
    ModesState {
        current_mode_id: "default".into(),
        available_modes: vec![
            ModeInfo {
                id: "default".into(),
                name: "Manual".into(),
                description: "Always ask before making changes".into(),
                _meta: None,
            },
            ModeInfo {
                id: "plan".into(),
                name: "Plan".into(),
                description: "Create a plan before making changes".into(),
                _meta: None,
            },
        ],
    }
}

fn available(ids: &[(&str, &str)]) -> Vec<AvailableModel> {
    ids.iter()
        .map(|(id, name)| AvailableModel {
            model_id: (*id).into(),
            name: (*name).into(),
            description: None,
        })
        .collect()
}

fn info(value: &str, display: &str, resolved: Option<&str>, levels: Option<&[&str]>) -> ModelInfo {
    ModelInfo {
        value: value.into(),
        display_name: display.into(),
        description: String::new(),
        resolved_model: resolved.map(String::from),
        supports_effort: levels.map(|_| true),
        supported_effort_levels: levels.map(|l| l.iter().map(|s| (*s).to_string()).collect()),
        ..Default::default()
    }
}

fn recommended_models() -> (Vec<AvailableModel>, Vec<ModelInfo>) {
    let levels: &[&str] = &["low", "medium", "high"];
    (
        available(&[
            ("default", "Default"),
            ("opus", "Claude Opus"),
            ("sonnet", "Claude Sonnet"),
            ("haiku", "Claude Haiku"),
        ]),
        vec![
            info(
                "default",
                "Default",
                Some("claude-sonnet-4-6"),
                Some(levels),
            ),
            info("opus", "Claude Opus", Some("claude-opus-4-6"), Some(levels)),
            info(
                "sonnet",
                "Claude Sonnet",
                Some("claude-sonnet-4-6"),
                Some(levels),
            ),
            info("haiku", "Claude Haiku", Some("claude-haiku-4-5"), None),
        ],
    )
}

fn option<'a>(options: &'a [ConfigOption], id: &str) -> &'a ConfigOption {
    options
        .iter()
        .find(|o| o.id == id)
        .unwrap_or_else(|| panic!("option {id} missing"))
}

fn values(o: &ConfigOption) -> Vec<&str> {
    o.options.iter().map(|v| v.value.as_str()).collect()
}

fn recommended_meta(value: &str) -> Value {
    json!({"jetbrains": {"air": {"version": 1, "recommendedValue": value}}})
}

// ---- recommended-config-values.test.ts ----

#[test]
fn recommended_capability_is_enabled_through_the_common_air_capability_list() {
    let caps = json!({"_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["recommendedValue"]}}}});
    assert!(client_supports_recommended_config_value(Some(&caps)));
}

#[test]
fn recommended_capability_keeps_legacy_behavior_for_omitted_and_malformed_air() {
    assert!(!client_supports_recommended_config_value(None));
    assert!(!client_supports_recommended_config_value(Some(
        &Value::Null
    )));
    assert!(!client_supports_recommended_config_value(Some(&json!({}))));
    assert!(!client_supports_recommended_config_value(Some(&json!({
        "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["someOtherCapability"]}}}
    }))));
    assert!(!client_supports_recommended_config_value(Some(&json!({
        "_meta": {"jetbrains": {"air": {"version": "1", "capabilities": ["recommendedValue"]}}}
    }))));
}

#[test]
fn recommended_retains_default_entries_without_the_capability() {
    let (models, infos) = recommended_models();
    let options = build_config_options(&modes(), &models, "default", &infos, None, false);
    let model = option(&options, "model");
    assert_eq!(model.current_value, "default");
    assert!(values(model).contains(&"default"));
    assert!(model._meta.is_none());
    let effort = option(&options, "effort");
    assert_eq!(effort.current_value, "default");
    assert!(values(effort).contains(&"default"));
    assert!(effort._meta.is_none());
}

#[test]
fn recommended_omits_default_entries_and_advertises_concrete_recommendations() {
    let (models, infos) = recommended_models();
    let options = build_config_options(&modes(), &models, "default", &infos, None, true);
    let mode = option(&options, "mode");
    assert_eq!(mode.current_value, "default");
    assert!(values(mode).contains(&"default"));
    assert!(mode._meta.is_none());

    let model = option(&options, "model");
    assert_eq!(model.current_value, "sonnet");
    assert_eq!(values(model), ["opus", "sonnet", "haiku"]);
    assert_eq!(model._meta, Some(recommended_meta("sonnet")));

    let effort = option(&options, "effort");
    assert_eq!(effort.current_value, "medium");
    assert!(!values(effort).contains(&"default"));
    assert_eq!(effort._meta, Some(recommended_meta("medium")));
}

#[test]
fn recommended_preserves_explicit_selections_with_independent_recommendations() {
    let (models, infos) = recommended_models();
    let options = build_config_options(&modes(), &models, "opus", &infos, Some("high"), true);
    let model = option(&options, "model");
    assert_eq!(model.current_value, "opus");
    assert_eq!(model._meta, Some(recommended_meta("sonnet")));
    let effort = option(&options, "effort");
    assert_eq!(effort.current_value, "high");
    assert_eq!(effort._meta, Some(recommended_meta("medium")));
}

#[test]
fn recommended_keeps_the_legacy_sentinel_when_the_recommendation_is_not_selectable() {
    let (models, infos) = recommended_models();
    let only_default: Vec<ModelInfo> = infos.into_iter().filter(|m| m.value == "default").collect();
    let options = build_config_options(&modes(), &models, "default", &only_default, None, true);
    let model = option(&options, "model");
    assert_eq!(model.current_value, "default");
    assert!(values(model).contains(&"default"));
    assert!(model._meta.is_none());
}

#[test]
fn recommended_does_not_substitute_a_similar_model_for_the_unavailable_default() {
    for resolved in ["claude-sonnet-4-6[1m]", "claude-sonnet-5"] {
        let (models, mut infos) = recommended_models();
        infos[0].resolved_model = Some(resolved.into());
        let options = build_config_options(&modes(), &models, "default", &infos, None, true);
        let model = option(&options, "model");
        assert_eq!(model.current_value, "default", "{resolved}");
        assert!(model._meta.is_none(), "{resolved}");
    }
}

#[test]
fn recommended_matches_equivalent_context_suffix_spellings_exactly() {
    let (models, mut infos) = recommended_models();
    infos[0].resolved_model = Some("claude-sonnet-4-6-1m".into());
    infos[2].resolved_model = Some("claude-sonnet-4-6[1m]".into());
    let options = build_config_options(&modes(), &models, "default", &infos, None, true);
    let model = option(&options, "model");
    assert_eq!(model.current_value, "sonnet");
    assert_eq!(model._meta, Some(recommended_meta("sonnet")));
}

#[test]
fn recommended_uses_an_available_effort_when_medium_is_not_supported() {
    let (models, mut infos) = recommended_models();
    for model in &mut infos {
        model.supported_effort_levels = Some(vec!["high".into(), "max".into()]);
    }
    let options = build_config_options(&modes(), &models, "default", &infos, None, true);
    let effort = option(&options, "effort");
    assert_eq!(effort.current_value, "high");
    assert_eq!(effort._meta, Some(recommended_meta("high")));
}

// ---- fast-mode-config.test.ts ----

fn fast_models() -> (Vec<AvailableModel>, Vec<ModelInfo>) {
    (
        available(&[("claude-opus-4-8", "Claude Opus")]),
        vec![info("claude-opus-4-8", "Claude Opus", None, None)],
    )
}

#[test]
fn fast_option_is_a_native_boolean_toggle_when_the_client_opted_in() {
    let option = create_fast_mode_config_option(true, true, None);
    assert_eq!(
        serde_json::to_value(&option).unwrap(),
        json!({
            "id": FAST_MODE_CONFIG_ID,
            "name": "Fast mode",
            "description": "Faster responses on supported models",
            "category": "model_config",
            "type": "boolean",
            "currentValue": true,
        })
    );
}

#[test]
fn fast_option_explains_a_blocking_reason_while_the_toggle_reads_off() {
    let option = create_fast_mode_config_option(false, true, Some("free"));
    assert_eq!(
        option.description,
        "Faster responses on supported models \u{2014} not available on the free plan"
    );
}

#[test]
fn fast_option_ignores_routine_and_on_state_reasons() {
    let plain = "Faster responses on supported models";
    assert_eq!(
        create_fast_mode_config_option(false, true, Some("sdk_opt_in_required")).description,
        plain
    );
    assert_eq!(
        create_fast_mode_config_option(false, true, Some("preference")).description,
        plain
    );
    assert_eq!(
        create_fast_mode_config_option(true, true, Some("free")).description,
        plain
    );
}

#[test]
fn fast_option_falls_back_to_an_on_off_select_without_boolean_support() {
    let option = create_fast_mode_config_option(false, false, None);
    assert_eq!(
        serde_json::to_value(&option).unwrap(),
        json!({
            "id": "fast",
            "name": "Fast mode",
            "description": "Faster responses on supported models",
            "category": "model_config",
            "type": "select",
            "currentValue": FAST_MODE_OFF,
            "options": [
                {"value": FAST_MODE_ON, "name": "On"},
                {"value": FAST_MODE_OFF, "name": "Off"},
            ],
        })
    );
}

#[test]
fn fast_boolean_option_roundtrips_through_the_wire_shape() {
    let option = create_fast_mode_config_option(true, true, Some("free"));
    let wire = serde_json::to_value(&option).unwrap();
    assert!(wire.get("options").is_none(), "boolean carries no options");
    let back: ConfigOption = serde_json::from_value(wire).unwrap();
    assert_eq!(back, option);
}

#[test]
fn client_boolean_config_capability_is_true_only_when_present() {
    assert!(client_supports_boolean_config_options(Some(
        &json!({"session": {"configOptions": {"boolean": {}}}})
    )));
    assert!(!client_supports_boolean_config_options(None));
    assert!(!client_supports_boolean_config_options(Some(&Value::Null)));
    assert!(!client_supports_boolean_config_options(Some(&json!({}))));
    assert!(!client_supports_boolean_config_options(Some(
        &json!({"session": {"configOptions": {}}})
    )));
    assert!(!client_supports_boolean_config_options(Some(
        &json!({"session": {"configOptions": {"boolean": null}}})
    )));
}

#[test]
fn resolve_fast_mode_accepts_booleans_and_the_select_fallback() {
    assert_eq!(resolve_fast_mode_enabled(Some(&json!(true))), Ok(true));
    assert_eq!(resolve_fast_mode_enabled(Some(&json!(false))), Ok(false));
    assert_eq!(resolve_fast_mode_enabled(Some(&json!("on"))), Ok(true));
    assert_eq!(resolve_fast_mode_enabled(Some(&json!("off"))), Ok(false));
}

#[test]
fn resolve_fast_mode_rejects_any_other_value() {
    assert_eq!(
        resolve_fast_mode_enabled(Some(&json!("maybe"))),
        Err("Invalid value for config option fast: maybe".to_string())
    );
    assert_eq!(
        resolve_fast_mode_enabled(Some(&json!(1))),
        Err("Invalid value for config option fast: 1".to_string())
    );
    assert_eq!(
        resolve_fast_mode_enabled(None),
        Err("Invalid value for config option fast: undefined".to_string())
    );
    assert_eq!(
        resolve_fast_mode_enabled(Some(&json!({"a": 1}))),
        Err("Invalid value for config option fast: [object Object]".to_string())
    );
}

#[test]
fn fast_mode_state_treats_cooldown_as_on() {
    assert!(fast_mode_state_enabled("on"));
    assert!(fast_mode_state_enabled("cooldown"));
    assert!(!fast_mode_state_enabled("off"));
}

#[test]
fn normalize_fast_mode_reason_retains_only_explainable_reasons() {
    for reason in [
        "free",
        "extra_usage_disabled",
        "model_not_allowed",
        "not_first_party",
        "disabled_by_env",
        "network_error",
    ] {
        assert_eq!(
            normalize_fast_mode_disabled_reason(Some(reason)).as_deref(),
            Some(reason)
        );
    }
}

#[test]
fn normalize_fast_mode_reason_drops_routine_states_and_unknown_values() {
    for reason in [
        "sdk_opt_in_required",
        "preference",
        "pending",
        "unknown",
        "some_future_reason",
    ] {
        assert_eq!(normalize_fast_mode_disabled_reason(Some(reason)), None);
    }
    assert_eq!(normalize_fast_mode_disabled_reason(None), None);
}

fn fast_options(fast: Option<&FastModeOptionState>) -> Vec<ConfigOption> {
    let (models, infos) = fast_models();
    build_config_options_with(&ConfigOptionsInput {
        modes: &modes(),
        available_models: &models,
        current_model_id: "claude-opus-4-8",
        model_infos: &infos,
        current_effort: None,
        agents: &[],
        current_agent: "default",
        fast_mode: fast,
        use_recommended_value: false,
    })
}

#[test]
fn build_options_omit_fast_mode_when_unsupported_or_without_state() {
    let unsupported = FastModeOptionState {
        supported: false,
        enabled: false,
        use_boolean_option: true,
        disabled_reason: None,
    };
    assert!(fast_options(Some(&unsupported))
        .iter()
        .all(|o| o.id != FAST_MODE_CONFIG_ID));
    assert!(fast_options(None)
        .iter()
        .all(|o| o.id != FAST_MODE_CONFIG_ID));
}

#[test]
fn build_options_surface_fast_mode_as_boolean_or_select() {
    let boolean = FastModeOptionState {
        supported: true,
        enabled: true,
        use_boolean_option: true,
        disabled_reason: None,
    };
    assert!(
        fast_options(Some(&boolean)).contains(&create_fast_mode_config_option(true, true, None))
    );
    let select = FastModeOptionState {
        supported: true,
        enabled: false,
        use_boolean_option: false,
        disabled_reason: None,
    };
    assert!(
        fast_options(Some(&select)).contains(&create_fast_mode_config_option(false, false, None))
    );
}

// ---- agent picker (acp-agent.test.ts "agent selection config option") ----

#[test]
fn discover_filters_out_builtin_subagents() {
    let agents = json!([
        {"name": "claude", "description": "catch-all"},
        {"name": "Explore", "description": "search"},
        {"name": "general-purpose", "description": "gp"},
        {"name": "Plan", "description": "architect"},
        {"name": "statusline-setup", "description": "status"},
        {"name": "my-reviewer", "description": "Reviews code"},
        {"name": "my-writer", "description": "Writes docs"},
    ]);
    let found = discover_custom_agents(Some(&agents));
    let names: Vec<&str> = found.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["my-reviewer", "my-writer"]);
}

#[test]
fn discover_excludes_the_reserved_default_name() {
    let agents = json!([
        {"name": "default", "description": "collides with the synthetic Default entry"},
        {"name": "my-reviewer", "description": "Reviews code"},
    ]);
    let names: Vec<String> = discover_custom_agents(Some(&agents))
        .into_iter()
        .map(|a| a.name)
        .collect();
    assert_eq!(names, ["my-reviewer"]);
}

#[test]
fn discover_returns_an_empty_list_for_a_missing_or_malformed_roster() {
    assert!(discover_custom_agents(None).is_empty());
    assert!(discover_custom_agents(Some(&json!("boom"))).is_empty());
    assert!(discover_custom_agents(Some(&json!([{"description": "no name"}, 3]))).is_empty());
}

#[test]
fn builtin_agent_names_match_the_upstream_roster() {
    let mut names = BUILTIN_AGENT_NAMES.to_vec();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "Explore",
            "Plan",
            "claude",
            "general-purpose",
            "statusline-setup"
        ]
    );
}

fn agent_options(agents: &[AgentInfo], current: &str) -> Vec<ConfigOption> {
    build_config_options_with(&ConfigOptionsInput {
        modes: &ModesState::default(),
        available_models: &[],
        current_model_id: "default",
        model_infos: &[],
        current_effort: None,
        agents,
        current_agent: current,
        fast_mode: None,
        use_recommended_value: false,
    })
}

#[test]
fn agent_option_is_omitted_without_custom_agents() {
    assert!(agent_options(&[], "default")
        .iter()
        .all(|o| o.id != "agent"));
}

#[test]
fn agent_option_adds_a_synthetic_default_entry() {
    let agents = [
        AgentInfo {
            name: "my-reviewer".into(),
            description: "Reviews code".into(),
        },
        AgentInfo {
            name: "my-writer".into(),
            description: String::new(),
        },
    ];
    let options = agent_options(&agents, "my-reviewer");
    let agent = option(&options, "agent");
    assert_eq!(agent.current_value, "my-reviewer");
    assert_eq!(agent.option_type, "select");
    assert_eq!(values(agent), ["default", "my-reviewer", "my-writer"]);
    assert_eq!(agent.options[2].description, None);
    let wire = serde_json::to_value(agent).unwrap();
    assert!(
        wire.get("category").is_none(),
        "the agent option has no category"
    );
    assert_eq!(
        wire["options"][0],
        json!({"value": "default", "name": "Default", "description": "Standard Claude Code agent"})
    );
    assert_eq!(wire["description"], "Main-thread agent persona");
}

#[test]
fn agent_flag_settings_clears_the_flag_layer_for_default() {
    assert_eq!(agent_flag_settings("default"), json!({"agent": null}));
    assert_eq!(
        agent_flag_settings("my-reviewer"),
        json!({"agent": "my-reviewer"})
    );
}

// ---- model-resolution.test.ts (settingsEffortForModel / effortFlagSettings) ----

fn sonnet() -> ModelInfo {
    info("sonnet", "Sonnet", Some("claude-sonnet-5"), None)
}

#[test]
fn settings_effort_prefers_the_per_model_entry_keyed_by_the_resolved_model() {
    let model_settings = json!({"claude-sonnet-5": {"effortLevel": "low"}});
    assert_eq!(
        settings_effort_for_model(
            Some(&model_settings),
            Some("high"),
            Some(&sonnet()),
            "sonnet"
        ),
        Some("low".into())
    );
}

#[test]
fn settings_effort_falls_back_to_the_row_value_then_the_raw_id() {
    let by_value = json!({"sonnet": {"effortLevel": "medium"}});
    assert_eq!(
        settings_effort_for_model(Some(&by_value), None, Some(&sonnet()), "sonnet"),
        Some("medium".into())
    );
    let by_raw = json!({"claude-x": {"effortLevel": "xhigh"}});
    assert_eq!(
        settings_effort_for_model(Some(&by_raw), None, None, "claude-x"),
        Some("xhigh".into())
    );
}

#[test]
fn settings_effort_matches_equivalent_1m_spellings() {
    let model_settings = json!({"claude-sonnet-5-1m": {"effortLevel": "low"}});
    let mut model = sonnet();
    model.value = "sonnet[1m]".into();
    model.resolved_model = Some("claude-sonnet-5[1m]".into());
    assert_eq!(
        settings_effort_for_model(
            Some(&model_settings),
            Some("high"),
            Some(&model),
            "sonnet[1m]"
        ),
        Some("low".into())
    );
}

#[test]
fn settings_effort_falls_back_to_the_top_level_effort() {
    assert_eq!(
        settings_effort_for_model(None, Some("high"), Some(&sonnet()), "sonnet"),
        Some("high".into())
    );
    let other = json!({"claude-opus-4-8": {"effortLevel": "low"}});
    assert_eq!(
        settings_effort_for_model(Some(&other), Some("high"), Some(&sonnet()), "sonnet"),
        Some("high".into())
    );
    assert_eq!(
        settings_effort_for_model(None, None, Some(&sonnet()), "sonnet"),
        None
    );
    assert_eq!(
        settings_effort_for_model(Some(&json!({})), None, None, "x"),
        None
    );
}

#[test]
fn effort_flag_settings_carries_ultracode_only_when_requested() {
    assert_eq!(
        effort_flag_settings(Some("high"), true),
        json!({"effortLevel": "high", "ultracode": true})
    );
    assert_eq!(
        effort_flag_settings(Some("default"), true),
        json!({"effortLevel": null, "ultracode": true})
    );
    assert_eq!(
        effort_flag_settings(Some("high"), false),
        json!({"effortLevel": "high"})
    );
    assert_eq!(
        effort_flag_settings(None, false),
        json!({"effortLevel": null})
    );
}

// ---- mergeEffortSettings (session-effort.ts:62-80) ----

#[test]
fn merge_effort_settings_returns_base_without_an_override() {
    let base = json!({"effortLevel": "high", "modelSettings": {"a": {"effortLevel": "low"}}});
    assert_eq!(merge_effort_settings(&base, None), base);
}

#[test]
fn merge_effort_settings_shadows_per_model_keys_by_canonical_spelling() {
    let base = json!({
        "effortLevel": "high",
        "modelSettings": {
            "claude-sonnet-5-1m": {"effortLevel": "low"},
            "claude-opus-4-8": {"effortLevel": "medium"},
        }
    });
    let over = json!({
        "effortLevel": "max",
        "modelSettings": {"claude-sonnet-5[1m]": {"effortLevel": "xhigh"}}
    });
    assert_eq!(
        merge_effort_settings(&base, Some(&over)),
        json!({
            "effortLevel": "max",
            "modelSettings": {
                "claude-opus-4-8": {"effortLevel": "medium"},
                "claude-sonnet-5[1m]": {"effortLevel": "xhigh"},
            }
        })
    );
}

#[test]
fn merge_effort_settings_keeps_base_models_when_the_override_has_none() {
    let base = json!({"modelSettings": {"m": {"effortLevel": "low"}}});
    let over = json!({"ultracode": true});
    assert_eq!(
        merge_effort_settings(&base, Some(&over)),
        json!({"ultracode": true, "modelSettings": {"m": {"effortLevel": "low"}}})
    );
}

#[test]
fn merge_effort_settings_has_no_model_settings_when_neither_tier_has_any() {
    let merged = merge_effort_settings(
        &json!({"effortLevel": "low"}),
        Some(&json!({"ultracode": true})),
    );
    assert_eq!(merged, json!({"effortLevel": "low", "ultracode": true}));
}

// ---- appliedEffortLevel bookkeeping ----

#[test]
fn applied_effort_bookkeeping_follows_upstream() {
    assert_eq!(applied_effort_after_pick("high"), Some("high".into()));
    assert_eq!(applied_effort_after_pick("default"), None);
    assert_eq!(
        applied_effort_after_sync(true, Some("medium")),
        Some("medium".into())
    );
    assert_eq!(applied_effort_after_sync(true, Some("default")), None);
    assert_eq!(applied_effort_after_sync(false, Some("medium")), None);
    assert_eq!(applied_effort_after_sync(true, None), None);
}

// ---- creation-time seeding (acp-agent.ts:11553-11584) ----

#[test]
fn fast_mode_state_is_seeded_from_the_initialize_answer() {
    let off_blocked = fast_mode_option_state_from_initialize(
        &json!({"fast_mode_state": "off", "fast_mode_disabled_reason": "free"}),
        true,
        true,
    );
    assert_eq!(
        off_blocked,
        FastModeOptionState {
            supported: true,
            enabled: false,
            use_boolean_option: true,
            disabled_reason: Some("free".into()),
        }
    );
    // A fresh SDK session reports `sdk_opt_in_required`, which normalizes away.
    let fresh = fast_mode_option_state_from_initialize(
        &json!({"fast_mode_state": "off", "fast_mode_disabled_reason": "sdk_opt_in_required"}),
        true,
        false,
    );
    assert_eq!(fresh.disabled_reason, None);
    // `on` and `cooldown` read as enabled and drop the reason.
    for state in ["on", "cooldown"] {
        let seeded = fast_mode_option_state_from_initialize(
            &json!({"fast_mode_state": state, "fast_mode_disabled_reason": "free"}),
            false,
            false,
        );
        assert!(seeded.enabled, "{state}");
        assert_eq!(seeded.disabled_reason, None);
        assert!(!seeded.supported);
    }
    // No reported state: not enabled.
    assert!(!fast_mode_option_state_from_initialize(&json!({}), true, true).enabled);
}

#[test]
fn requested_agent_is_adopted_only_when_the_picker_surfaces_it() {
    let agents = [AgentInfo {
        name: "my-reviewer".into(),
        description: String::new(),
    }];
    assert_eq!(
        resolve_current_agent(Some("my-reviewer"), &agents),
        "my-reviewer"
    );
    assert_eq!(resolve_current_agent(Some("Explore"), &agents), "default");
    assert_eq!(resolve_current_agent(Some(""), &agents), "default");
    assert_eq!(resolve_current_agent(None, &agents), "default");
    assert_eq!(resolve_current_agent(Some("my-reviewer"), &[]), "default");
}

// ---- session-config-options.test.ts: effort option across a model switch ----
// The rebuilt option is a pure function of (model infos, new model, seed).

fn effort_infos() -> Vec<ModelInfo> {
    vec![
        info("opus", "Opus", None, Some(&["low", "medium", "high"])),
        info("haiku", "Haiku", None, None),
        info("narrow", "Narrow", None, Some(&["low", "medium"])),
    ]
}

#[test]
fn effort_option_is_dropped_for_a_model_without_effort_support() {
    let infos = effort_infos();
    assert!(
        claude_agent_acp_rs::session_config::build_effort_config_option(
            &infos,
            "haiku",
            Some("high"),
            false
        )
        .is_none()
    );
}

#[test]
fn effort_option_appears_for_a_model_that_supports_effort() {
    let infos = effort_infos();
    let opt = claude_agent_acp_rs::session_config::build_effort_config_option(
        &infos, "opus", None, false,
    )
    .expect("effort option");
    assert_eq!(values(&opt), ["default", "low", "medium", "high"]);
    assert_eq!(opt.current_value, "default");
}

#[test]
fn effort_value_is_clamped_to_the_new_models_levels() {
    let infos = effort_infos();
    let opt = claude_agent_acp_rs::session_config::build_effort_config_option(
        &infos,
        "narrow",
        Some("high"),
        false,
    )
    .expect("effort option");
    assert_eq!(opt.current_value, "default");
    // With recommended values the clamp lands on the recommendation.
    let rec = claude_agent_acp_rs::session_config::build_effort_config_option(
        &infos,
        "narrow",
        Some("high"),
        true,
    )
    .expect("effort option");
    assert_eq!(rec.current_value, "medium");
}

#[test]
fn effort_value_is_preserved_when_the_new_model_supports_it() {
    let infos = effort_infos();
    let opt = claude_agent_acp_rs::session_config::build_effort_config_option(
        &infos,
        "narrow",
        Some("low"),
        false,
    )
    .expect("effort option");
    assert_eq!(opt.current_value, "low");
}
