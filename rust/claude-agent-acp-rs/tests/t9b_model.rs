//! T9B — the `model-resolution.test.ts` cases for `resolveModelPreference`,
//! `matchResumedModel`, `applyAvailableModelsAllowlist` and `isDeniedModel`.
//! Each test mirrors the upstream `LIVE_SHAPED_MODELS` fixture and expected
//! values, so the Rust port proves the same picker resolution.

use std::collections::HashMap;

use claude_agent_acp_rs::session_model::{
    apply_available_models_allowlist, is_denied_model, match_resumed_model,
    resolve_model_preference, ModelInfo,
};

/// Mirrors a real `supportedModels()` response: alias rows carry
/// `resolvedModel`, and "Sonnet 5" has no `major.minor` dot unlike older
/// "claude-opus-4-6"-style ids.
fn live_shaped_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            value: "default".into(),
            display_name: "Default (recommended)".into(),
            description: "Use the default model (currently Opus 4.8 (1M context))".into(),
            resolved_model: Some("claude-opus-4-8[1m]".into()),
            ..Default::default()
        },
        ModelInfo {
            value: "opus[1m]".into(),
            display_name: "Opus".into(),
            description: "Opus 4.8 with 1M context · Best for everyday, complex tasks".into(),
            resolved_model: Some("claude-opus-4-8[1m]".into()),
            ..Default::default()
        },
        ModelInfo {
            value: "sonnet".into(),
            display_name: "Sonnet".into(),
            description: "Sonnet 5 · Efficient for routine tasks".into(),
            resolved_model: Some("claude-sonnet-5".into()),
            ..Default::default()
        },
        ModelInfo {
            value: "haiku".into(),
            display_name: "Haiku".into(),
            description: "Haiku 4.5 · Fastest for quick answers".into(),
            resolved_model: Some("claude-haiku-4-5-20251001".into()),
            ..Default::default()
        },
    ]
}

fn sonnet_default_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            value: "default".into(),
            display_name: "Default (recommended)".into(),
            description: "Use the default model (currently Sonnet 5)".into(),
            resolved_model: Some("claude-sonnet-5".into()),
            ..Default::default()
        },
        ModelInfo {
            value: "opus".into(),
            display_name: "Opus".into(),
            description: "Opus 4.6 · Most capable for complex work".into(),
            resolved_model: Some("claude-opus-4-6[1m]".into()),
            ..Default::default()
        },
        ModelInfo {
            value: "sonnet".into(),
            display_name: "Sonnet".into(),
            description: "Sonnet 5 · Efficient for routine tasks".into(),
            resolved_model: Some("claude-sonnet-5".into()),
            ..Default::default()
        },
        ModelInfo {
            value: "sonnet[1m]".into(),
            display_name: "Sonnet".into(),
            description: "Sonnet 5 with 1M context".into(),
            resolved_model: Some("claude-sonnet-5[1m]".into()),
            ..Default::default()
        },
    ]
}

/// resolveModelPreference — an exact `resolvedModel`, a case-insensitive
/// preference, plain aliases, a retired dated id, an alias's context-hint
/// suffix, and a preference that matches nothing.
#[test]
fn t9b_resolve_preference_resolved_model_tiers() {
    let models = live_shaped_models();
    assert_eq!(
        resolve_model_preference(&models, "claude-sonnet-5")
            .expect("sonnet")
            .value,
        "sonnet"
    );
    assert_eq!(
        resolve_model_preference(&models, "claude-opus-4-8[1m]")
            .expect("opus 1m")
            .value,
        "opus[1m]"
    );
    assert_eq!(
        resolve_model_preference(&models, "claude-haiku-4-5-20251001")
            .expect("haiku")
            .value,
        "haiku"
    );
    assert_eq!(
        resolve_model_preference(&models, "CLAUDE-SONNET-5")
            .expect("case-insensitive")
            .value,
        "sonnet"
    );
    assert_eq!(
        resolve_model_preference(&models, "sonnet")
            .expect("alias")
            .value,
        "sonnet"
    );
    assert_eq!(
        resolve_model_preference(&models, "opus[1m]")
            .expect("alias with hint")
            .value,
        "opus[1m]"
    );
    assert!(
        resolve_model_preference(&models, "claude-sonnet-4-6").is_none(),
        "a retired dated id must not drift onto another generation"
    );
    // "opus[1m]"'s value has no dotted version; its "[1m]" hint must not read
    // as version "1" and block the match against the description's real 4.8.
    assert_eq!(
        resolve_model_preference(&models, "claude-opus-4-8")
            .expect("hint stripped")
            .value,
        "opus[1m]"
    );
    assert!(resolve_model_preference(&models, "claude-gpt-99").is_none());
}

/// resolveModelPreference — the `-1m` id-suffix form and the `[1m]` alias
/// spelling both land on the 1M row, never the bare 200k sibling.
#[test]
fn t9b_resolve_preference_hint_spellings() {
    let models = vec![
        ModelInfo {
            value: "sonnet".into(),
            display_name: "Sonnet".into(),
            description: "Sonnet 5 · Efficient for routine tasks".into(),
            resolved_model: Some("claude-sonnet-5".into()),
            ..Default::default()
        },
        ModelInfo {
            value: "sonnet[1m]".into(),
            display_name: "Sonnet".into(),
            description: "Sonnet 5 with 1M context".into(),
            resolved_model: Some("claude-sonnet-5[1m]".into()),
            ..Default::default()
        },
    ];
    assert_eq!(
        resolve_model_preference(&models, "claude-sonnet-5-1m")
            .expect("id suffix")
            .value,
        "sonnet[1m]"
    );
    assert_eq!(
        resolve_model_preference(&live_shaped_models(), "opus-1m")
            .expect("alias suffix")
            .value,
        "opus[1m]"
    );

    // Rows without resolvedModel cannot use the exact tier; the hinted
    // preference must not be absorbed by the bare sibling.
    let bare = vec![
        ModelInfo {
            value: "sonnet".into(),
            display_name: "Sonnet".into(),
            ..Default::default()
        },
        ModelInfo {
            value: "sonnet[1m]".into(),
            display_name: "Sonnet".into(),
            ..Default::default()
        },
    ];
    assert_eq!(
        resolve_model_preference(&bare, "claude-sonnet-5[1m]")
            .expect("bracket hint")
            .value,
        "sonnet[1m]"
    );
    assert_eq!(
        resolve_model_preference(&bare, "claude-sonnet-5-1m")
            .expect("suffix hint")
            .value,
        "sonnet[1m]"
    );
    assert_eq!(
        resolve_model_preference(&bare, "claude-sonnet-5")
            .expect("bare")
            .value,
        "sonnet"
    );
}

/// matchResumedModel — issue #845's shape: the default resolves to Sonnet and
/// the session was switched to Opus before the restart. Exact named-row
/// matches beat the hint-stripped Default tier, and an identical
/// `resolvedModel` keeps Default.
#[test]
fn t9b_match_resumed_model_default_and_hints() {
    let models = sonnet_default_models();
    assert_eq!(
        match_resumed_model(&models, "claude-opus-4-6").value,
        "opus"
    );
    assert_eq!(
        match_resumed_model(&models, "claude-opus-4-6[1m]").value,
        "opus"
    );
    assert_eq!(
        match_resumed_model(&models, "claude-sonnet-5").value,
        "default"
    );
    assert_eq!(
        match_resumed_model(&models, "claude-sonnet-5[1m]").value,
        "sonnet[1m]"
    );
    assert_eq!(
        match_resumed_model(&models, "claude-sonnet-5-1m").value,
        "sonnet[1m]"
    );

    let live = live_shaped_models();
    assert_eq!(
        match_resumed_model(&live, "claude-opus-4-8-1m").value,
        "default"
    );
    assert_eq!(
        match_resumed_model(&live, "Claude-Opus-4-8[1M]").value,
        "default"
    );
    assert_eq!(
        match_resumed_model(&live, "claude-opus-4-8[1m]").value,
        "default",
        "a named row sharing Default's resolvedModel keeps Default"
    );
    assert_eq!(
        match_resumed_model(&live, "claude-opus-4-8").value,
        "default",
        "the transcript drops the [1m] hint"
    );
    assert_eq!(
        match_resumed_model(&live, "claude-sonnet-5").value,
        "sonnet"
    );
    assert_eq!(
        match_resumed_model(&live, "claude-haiku-4-5-20251001").value,
        "haiku"
    );
}

/// matchResumedModel — a live model with no picker counterpart is tracked
/// verbatim.
#[test]
fn t9b_match_resumed_model_tracks_unknown_verbatim() {
    let result = match_resumed_model(&live_shaped_models(), "claude-gpt-99");
    assert_eq!(result.value, "claude-gpt-99");
    assert_eq!(result.display_name, "claude-gpt-99");
}

/// applyAvailableModelsAllowlist — a canonical resolvedModel id resolves to the
/// alias row with its display metadata, and an alias entry is checked against
/// the row it resolves to for deniedModels.
#[test]
fn t9b_allowlist_resolved_model_and_alias_denial() {
    let models = live_shaped_models();
    let out =
        apply_available_models_allowlist(&models, &["claude-sonnet-5".into()], None, &[], None);
    let entry = out
        .iter()
        .find(|m| m.value == "claude-sonnet-5")
        .expect("the resolvedModel entry is surfaced");
    assert_eq!(entry.display_name, "Sonnet");

    let out = apply_available_models_allowlist(
        &models,
        &["sonnet".into()],
        None,
        &["claude-sonnet-5".into()],
        None,
    );
    assert_eq!(
        out.iter().map(|m| m.value.as_str()).collect::<Vec<_>>(),
        ["default"],
        "an alias entry is denied when the row it resolves to is denied"
    );
}

/// applyAvailableModelsAllowlist — the override target is the value while the
/// alias's display metadata is kept.
#[test]
fn t9b_allowlist_override_keeps_display_metadata() {
    let sdk = vec![
        ModelInfo {
            value: "default".into(),
            display_name: "Default".into(),
            description: "Default model".into(),
            ..Default::default()
        },
        ModelInfo {
            value: "opus".into(),
            display_name: "Opus".into(),
            description: "Claude Opus 4.6".into(),
            supports_effort: Some(true),
            supported_effort_levels: Some(vec!["low".into(), "high".into()]),
            ..Default::default()
        },
    ];
    let target = "arn:aws:bedrock:us-west-2:111122223333:inference-profile/custom-7f3a".to_string();
    let mut overrides = HashMap::new();
    overrides.insert("claude-opus-4-6".to_string(), target.clone());
    let out = apply_available_models_allowlist(
        &sdk,
        &["claude-opus-4-6".into()],
        Some(&overrides),
        &[],
        None,
    );
    let entry = out
        .iter()
        .find(|m| m.value == target)
        .expect("override target");
    assert_eq!(entry.display_name, "Opus");
    assert_eq!(entry.description, "Claude Opus 4.6");
    assert_eq!(entry.supports_effort, Some(true));
    assert_eq!(
        entry.supported_effort_levels,
        Some(vec!["low".to_string(), "high".to_string()])
    );
}

/// isDeniedModel — every spelling of a denied version and its later minors; an
/// OpenRouter/dotted entry reads as the dashed SDK id; a minor entry blocks only
/// that version; a family alias blocks the family and settings-dependent
/// aliases are ignored.
#[test]
fn t9b_is_denied_model_spellings_and_minors() {
    let denied = vec!["claude-opus-5".to_string()];
    for spelling in [
        "claude-opus-5",
        "claude-opus-5-20260101",
        "claude-opus-5[1m]",
        "us.anthropic.claude-opus-5-v1:0",
        "claude-opus-5-5",
        "anthropic/claude-opus-5.5:batch[1m]",
        "anthropic/us.anthropic.claude-opus-5-v1:0",
    ] {
        assert!(is_denied_model(spelling, &denied), "{spelling}");
    }
    assert!(!is_denied_model("claude-opus-4-8", &denied));

    assert!(is_denied_model(
        "claude-opus-4-5",
        &["claude-opus-4.5".to_string()]
    ));
    assert!(is_denied_model(
        "claude-opus-5-5",
        &["anthropic/claude-opus-5.5".to_string()]
    ));
    assert!(!is_denied_model(
        "claude-opus-5",
        &["anthropic/claude-opus-5.5".to_string()]
    ));

    assert!(!is_denied_model(
        "claude-opus-5",
        &["claude-opus-5-5".to_string()]
    ));
    assert!(is_denied_model(
        "claude-opus-5-5-fast",
        &["claude-opus-5-5".to_string()]
    ));

    assert!(is_denied_model(
        "claude-haiku-4-5-20251001",
        &["haiku".to_string()]
    ));
    assert!(is_denied_model("haiku", &["haiku".to_string()]));
    assert!(!is_denied_model("claude-sonnet-5", &["haiku".to_string()]));
    let aliases = vec!["default".to_string(), "best".into(), "opusplan".into()];
    assert!(!is_denied_model("claude-opus-5", &aliases));
}
