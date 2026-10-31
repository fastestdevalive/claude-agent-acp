//! Turn 1 lane B (F5b): the model-picker logic ported from
//! `session-model.ts` / `model-presentation.test.ts`. The session.rs half of
//! O-74 (hooking the `providers/set` listing into session creation) is Turn 5.

use claude_agent_acp_rs::session_config::{build_model_config_option, ModelInfo};
use claude_agent_acp_rs::session_model::{
    get_available_models, narrow_models_for_gateway, narrow_to_gateway_models, normalize_model_id,
};
use serde_json::json;

fn info(value: &str, display: &str, description: &str, resolved: Option<&str>) -> ModelInfo {
    ModelInfo {
        value: value.into(),
        display_name: display.into(),
        description: description.into(),
        resolved_model: resolved.map(String::from),
        ..Default::default()
    }
}

/// `model-presentation.test.ts` "adds standard-family versions and preserves
/// model identity and recommendation": the picker shows `Opus 5`, leaves
/// non-matching and custom rows alone, and the recommendation points at the
/// concrete row that the default resolves to.
#[test]
fn versioned_names_and_recommendation_through_the_model_option() {
    let opus = info(
        "opus[1m]",
        "Opus (1M context)",
        "Opus 5 with 1M context \u{b7} Best for everyday, complex tasks",
        Some("claude-opus-5[1m]"),
    );
    let mut default_row = opus.clone();
    default_row.value = "default".into();
    default_row.display_name = "Default (recommended)".into();
    let models = vec![
        default_row,
        opus.clone(),
        info("sonnet[1m]", "Sonnet 5 (1M context)", "", None),
        info("custom", "Custom Opus (1M context)", "", None),
    ];
    let state = get_available_models(&models, &models, None, None, false, None, false);
    let names: Vec<&str> = state
        .available_models
        .iter()
        .map(|m| m.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "Default (recommended)",
            "Opus 5",
            "Sonnet 5 (1M context)",
            "Custom Opus (1M context)"
        ]
    );
    // Not switched: the SDK already runs the default.
    assert!(state.skip_set_model);
    let option = build_model_config_option(
        &state.available_models,
        &state.current_model_id,
        &models,
        true,
    );
    assert_eq!(option.current_value, "opus[1m]");
    assert_eq!(
        option._meta,
        Some(json!({"jetbrains": {"air": {"version": 1, "recommendedValue": "opus[1m]"}}}))
    );
    let row = option
        .options
        .iter()
        .find(|o| o.value == "opus[1m]")
        .expect("opus row");
    assert_eq!(row.name, "Opus 5");
    assert_eq!(row.description.as_deref(), Some(opus.description.as_str()));
    // The caller's input is untouched.
    assert_eq!(opus.display_name, "Opus (1M context)");
}

#[test]
fn normalize_model_id_strips_a_bare_date_and_survives_multibyte_ids() {
    assert_eq!(
        normalize_model_id("claude-haiku-4-5-20251001"),
        "claude-haiku-4-5"
    );
    assert_eq!(normalize_model_id("-20260101"), "");
    // Eight bytes of a multibyte tail must not split a character.
    assert_eq!(
        normalize_model_id("modèle-é\u{e9}\u{e9}\u{e9}"),
        "modèle-éééé"
    );
}

#[test]
fn gateway_narrowing_keeps_listed_models_and_leaves_none_when_nothing_matches() {
    let sdk = vec![
        info("default", "Default", "", Some("claude-sonnet-5")),
        info("sonnet", "Sonnet", "", Some("claude-sonnet-5")),
        info("opus", "Opus", "", Some("claude-opus-5")),
    ];
    let narrowed = narrow_to_gateway_models(&sdk, &["claude-sonnet-5-20260101".into()], None);
    let values: Vec<&str> = narrowed.iter().map(|m| m.value.as_str()).collect();
    assert_eq!(values, ["default", "sonnet"]);
    assert!(narrow_to_gateway_models(&sdk, &["claude-other-1".into()], None).is_empty());
    // No listing yet: the models pass through and the picker is not narrowed.
    let (same, narrowed_flag) = narrow_models_for_gateway(sdk.clone(), None, None);
    assert_eq!(same, sdk);
    assert!(!narrowed_flag);
    let (none, narrowed_flag) =
        narrow_models_for_gateway(sdk, Some(&["claude-other-1".to_string()]), None);
    assert!(none.is_empty());
    assert!(narrowed_flag);
}

#[test]
fn an_empty_picker_leaves_no_model_option_and_keeps_the_sdk_model() {
    let sdk = vec![info("claude-sonnet-5", "Sonnet", "", None)];
    let state = get_available_models(&[], &sdk, None, None, false, None, true);
    assert!(state.available_models.is_empty());
    assert_eq!(state.current_model_id, "claude-sonnet-5");
    let resumed = get_available_models(&[], &sdk, None, None, true, Some("claude-x"), true);
    assert_eq!(resumed.current_model_id, "claude-x");
}

// ---- acp-agent-settings.test.ts: model resolution at session creation ----

/// "resolves model aliases like opus[1m] to the correct model": the `[1m]`
/// context hint lands on the `-1m` row, and the SDK is told via `setModel`.
#[test]
fn settings_alias_with_context_hint_resolves_to_the_long_context_row() {
    let models = vec![
        info("claude-opus-4-6", "Claude Opus 4.6", "Base", None),
        info(
            "claude-opus-4-6-1m",
            "Claude Opus 4.6 (1M)",
            "Long context",
            None,
        ),
    ];
    let settings_model = json!("opus[1m]");
    let state = get_available_models(
        &models,
        &models,
        Some(&settings_model),
        None,
        false,
        None,
        false,
    );
    assert_eq!(state.current_model_id, "claude-opus-4-6-1m");
    assert!(
        !state.skip_set_model,
        "an alias needs the setModel round-trip"
    );
}

/// "skips the initial setModel when the resolved value matches the SDK's model
/// list verbatim" (the launcher bakes the model into ANTHROPIC_MODEL).
#[test]
fn verbatim_env_model_skips_the_initial_set_model() {
    let models = vec![
        info("default", "Default", "", None),
        info("claude-sonnet-4-6", "Claude Sonnet 4.6", "", None),
        info("claude-opus-4-6", "Claude Opus 4.6", "", None),
    ];
    let state = get_available_models(
        &models,
        &models,
        None,
        Some("claude-opus-4-6"),
        false,
        None,
        false,
    );
    assert_eq!(state.current_model_id, "claude-opus-4-6");
    assert!(state.skip_set_model);
}

/// "still calls setModel when the allowlist synthesizes a value the SDK has not
/// surfaced": the user's literal id is not in the SDK's list.
#[test]
fn allowlist_synthesized_value_still_calls_set_model() {
    use claude_agent_acp_rs::session_model::apply_available_models_allowlist;
    let sdk = vec![
        info("default", "Default", "", None),
        info("haiku", "Haiku", "Fast", None),
    ];
    let allowed =
        apply_available_models_allowlist(&sdk, &["claude-haiku-4-5".into()], None, &[], None);
    let settings_model = json!("claude-haiku-4-5");
    let state = get_available_models(
        &allowed,
        &sdk,
        Some(&settings_model),
        None,
        false,
        None,
        false,
    );
    assert_eq!(state.current_model_id, "claude-haiku-4-5");
    assert!(!state.skip_set_model);
}

// ---- session-config-options.test.ts: model value resolution ----

fn config_options_models() -> Vec<ModelInfo> {
    vec![
        info("claude-opus-4-5", "Claude Opus", "Most capable", None),
        info("claude-sonnet-4-6", "Claude Sonnet", "Balanced", None),
    ]
}

/// "resolves model alias 'opus'/'sonnet' to full model ID", "resolves display
/// name to model ID", "still works with exact model ID".
#[test]
fn set_config_model_value_resolves_aliases_display_names_and_exact_ids() {
    use claude_agent_acp_rs::session_model::resolve_model_preference;
    let models = config_options_models();
    for (value, expected) in [
        ("opus", "claude-opus-4-5"),
        ("sonnet", "claude-sonnet-4-6"),
        ("Claude Sonnet", "claude-sonnet-4-6"),
        ("claude-sonnet-4-6", "claude-sonnet-4-6"),
    ] {
        let resolved = resolve_model_preference(&models, value)
            .unwrap_or_else(|| panic!("{value} must resolve"));
        assert_eq!(resolved.value, expected, "{value}");
    }
}

/// "throws for completely invalid model value": nothing resolves, so the
/// session layer reports `Invalid value for config option model: gpt-4`.
#[test]
fn set_config_model_value_that_matches_nothing_does_not_resolve() {
    use claude_agent_acp_rs::session_model::resolve_model_preference;
    assert!(resolve_model_preference(&config_options_models(), "gpt-4").is_none());
}
