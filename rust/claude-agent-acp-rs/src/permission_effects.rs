//! Port of `permissions/effects.ts` (Q-24): the tool-specific effects of an
//! ACP permission selection.
//!
//! `applyClaudePermissionSelection` (`permissions/effects.ts:191-223`) dispatches
//! on the tool name: Skill validates and generates a rule only for its exact and
//! prefix ids, WebFetch generates a domain rule, EnterPlanMode and the plain
//! tools use the common selection, ExitPlanMode maps its mode ids (and turns
//! "keep planning" and the clear-context choices into interrupting denies), and
//! an `mcp__*` or other tool may generate a local allow rule.
//!
//! The option ids and the option builder stay in `permission_options`; this
//! module owns the effect side (`dist/permissions/effects.js`).

use serde_json::{json, Value};

use crate::permission_options::{
    build_options, url_hostname, OptionsContext, OPTION_ALLOW_ONCE, OPTION_ALLOW_SKILL_EXACT,
    OPTION_ALLOW_SKILL_PREFIX, OPTION_ALLOW_WITH_UPDATES, OPTION_EXIT_PLAN_ACCEPT_EDITS,
    OPTION_EXIT_PLAN_AUTO, OPTION_EXIT_PLAN_BYPASS, OPTION_EXIT_PLAN_CLEAR_ACCEPT_EDITS,
    OPTION_EXIT_PLAN_CLEAR_AUTO, OPTION_EXIT_PLAN_CLEAR_BYPASS, OPTION_EXIT_PLAN_DEFAULT,
    OPTION_REJECT,
};

/// The result of applying a permission selection (the `control_response`
/// `response.response` body, minus the framing).
#[derive(Debug, Clone, PartialEq)]
pub enum SelectionOutcome {
    Allow {
        updated_permissions: Option<Vec<Value>>,
        permanent: bool,
    },
    Deny {
        message: String,
        interrupt: bool,
    },
}

/// Apply a chosen option id to produce the selection effect, port of
/// `applyClaudePermissionSelection` + helpers (`dist/permissions/effects.js`).
/// Returns `Err` with a tool-independent reason for an unknown option id or an
/// invalid input.
pub fn apply_selection(option_id: &str, ctx: &OptionsContext) -> Result<SelectionOutcome, String> {
    match ctx.tool_name.as_str() {
        "Skill" => apply_skill_selection(option_id, ctx),
        "WebFetch" => {
            apply_generated_durable_selection(option_id, ctx, || web_fetch_permission_update(ctx))
        }
        "Read"
        | "Bash"
        | "PowerShell"
        | "Glob"
        | "Grep"
        | "Edit"
        | "Write"
        | "NotebookEdit"
        | "EnterPlanMode"
        | "SandboxNetworkAccess" => apply_common_selection(option_id, ctx),
        "ExitPlanMode" => apply_exit_plan_mode_selection(option_id, ctx),
        _ => {
            if ctx.tool_name.starts_with("mcp__") {
                apply_common_selection(option_id, ctx)
            } else {
                apply_generated_durable_selection(option_id, ctx, || {
                    Ok(local_allow_rule_update(&ctx.tool_name, None))
                })
            }
        }
    }
}

/// `applySkillSelection` (`permissions/effects.ts:91-108`): `input.skill` is
/// validated only inside the exact and prefix branches; every other id
/// (allow-once, reject, ...) goes to the common selection untouched, so an
/// empty or missing skill still allows once (P-28).
fn apply_skill_selection(
    option_id: &str,
    ctx: &OptionsContext,
) -> Result<SelectionOutcome, String> {
    match option_id {
        OPTION_ALLOW_SKILL_EXACT => Ok(SelectionOutcome::Allow {
            updated_permissions: Some(local_allow_rule_update("Skill", Some(skill_name(ctx)?))),
            permanent: true,
        }),
        OPTION_ALLOW_SKILL_PREFIX => {
            let skill = skill_name(ctx)?;
            let space_index = skill
                .find(' ')
                .ok_or_else(|| "Skill prefix permission requires arguments".to_string())?;
            if space_index == 0 {
                return Err("Skill prefix permission requires arguments".to_string());
            }
            Ok(SelectionOutcome::Allow {
                updated_permissions: Some(local_allow_rule_update(
                    "Skill",
                    Some(format!("{}:*", &skill[..space_index])),
                )),
                permanent: true,
            })
        }
        _ => apply_common_selection(option_id, ctx),
    }
}

fn skill_name(ctx: &OptionsContext) -> Result<String, String> {
    let value = ctx
        .input
        .get("skill")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if value.is_empty() || value.encode_utf16().count() > 1_000 {
        return Err("Invalid Skill permission input".to_string());
    }
    Ok(value)
}

fn local_allow_rule_update(tool_name: &str, rule_content: Option<String>) -> Vec<Value> {
    let rule = match rule_content {
        Some(content) => json!({ "toolName": tool_name, "ruleContent": content }),
        None => json!({ "toolName": tool_name }),
    };
    vec![json!({
        "type": "addRules",
        "rules": [rule],
        "behavior": "allow",
        "destination": "localSettings",
    })]
}

fn web_fetch_permission_update(ctx: &OptionsContext) -> Result<Vec<Value>, String> {
    let url = ctx
        .input
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "Invalid WebFetch permission input".to_string())?;
    let hostname =
        url_hostname(url).ok_or_else(|| "Invalid WebFetch permission input".to_string())?;
    Ok(local_allow_rule_update(
        "WebFetch",
        Some(format!("domain:{hostname}")),
    ))
}

fn apply_common_selection(
    option_id: &str,
    ctx: &OptionsContext,
) -> Result<SelectionOutcome, String> {
    match option_id {
        OPTION_ALLOW_ONCE => Ok(SelectionOutcome::Allow {
            updated_permissions: None,
            permanent: false,
        }),
        OPTION_ALLOW_WITH_UPDATES => {
            let updates = ctx
                .change_set
                .as_ref()
                .map(|cs| cs.updates.clone())
                .ok_or_else(|| "Invalid durable permission selection".to_string())?;
            Ok(SelectionOutcome::Allow {
                updated_permissions: Some(updates),
                permanent: true,
            })
        }
        OPTION_REJECT => Ok(SelectionOutcome::Deny {
            message: "User refused permission to run tool".to_string(),
            interrupt: false,
        }),
        _ => Err(format!("Unknown permission option: {option_id}")),
    }
}

/// Node `exitPlanMode` (`effects.js`): map an ExitPlanMode option id to the
/// session mode to activate, or `None` for ids without a mode.
pub fn exit_plan_mode(option_id: &str) -> Option<&'static str> {
    match option_id {
        OPTION_EXIT_PLAN_BYPASS => Some("bypassPermissions"),
        OPTION_EXIT_PLAN_AUTO => Some("auto"),
        OPTION_EXIT_PLAN_ACCEPT_EDITS => Some("acceptEdits"),
        OPTION_EXIT_PLAN_DEFAULT => Some("default"),
        _ => None,
    }
}

/// Node `exitPlanClearContextMode` (`effects.js`): the context-reset variants
/// map to the mode the fresh query should resume in.
pub fn exit_plan_clear_context_mode(option_id: &str) -> Option<&'static str> {
    match option_id {
        OPTION_EXIT_PLAN_CLEAR_AUTO => Some("auto"),
        OPTION_EXIT_PLAN_CLEAR_BYPASS => Some("bypassPermissions"),
        OPTION_EXIT_PLAN_CLEAR_ACCEPT_EDITS => Some("acceptEdits"),
        _ => None,
    }
}

/// Node `applyExitPlanModeSelection` (`effects.js`): selecting an elevated
/// mode or the default runs a session `setMode` (classified permanent except
/// for the default), "No, keep planning" interrupts the turn, and the
/// clear-context variants deny with an interrupt so the adapter can hand off
/// to a fresh query.
fn apply_exit_plan_mode_selection(
    option_id: &str,
    ctx: &OptionsContext,
) -> Result<SelectionOutcome, String> {
    if let Some(mode) = exit_plan_clear_context_mode(option_id) {
        // The adapter consumes this interrupt and continues the same ACP turn
        // on a fresh Claude query (Node `effects.js:101-106`).
        let _ = mode;
        return Ok(SelectionOutcome::Deny {
            message: "User accepted the plan and requested a fresh context".to_string(),
            interrupt: true,
        });
    }
    if let Some(mode) = exit_plan_mode(option_id) {
        let permanent = mode != "default";
        return Ok(SelectionOutcome::Allow {
            updated_permissions: Some(vec![json!({
                "type": "setMode",
                "mode": mode,
                "destination": "session",
            })]),
            permanent,
        });
    }
    if option_id == OPTION_REJECT {
        // A regular deny would let Claude keep planning; interrupt stops this
        // ACP turn (Node `effects.js:111-116`).
        return Ok(SelectionOutcome::Deny {
            message: "User chose to keep planning".to_string(),
            interrupt: true,
        });
    }
    apply_common_selection(option_id, ctx)
}

/// Node's `decodeClaudePermissionResponse` "was this option offered" guard
/// (`permissions/response.js:5-8`): a selected id that was not among the
/// offered options is rejected the way Node throws (`Permission option was not
/// offered: <id>`).
pub fn option_was_offered(ctx: &OptionsContext, option_id: &str) -> bool {
    build_options(ctx)
        .iter()
        .any(|option| option.get("optionId").and_then(Value::as_str) == Some(option_id))
}

fn apply_generated_durable_selection(
    option_id: &str,
    ctx: &OptionsContext,
    updates: impl FnOnce() -> Result<Vec<Value>, String>,
) -> Result<SelectionOutcome, String> {
    if option_id != OPTION_ALLOW_WITH_UPDATES {
        return apply_common_selection(option_id, ctx);
    }
    Ok(SelectionOutcome::Allow {
        updated_permissions: Some(updates()?),
        permanent: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_options::{normalize_change_set, ChangeSet};

    fn ctx(tool_name: &str, input: Value, change_set: Option<ChangeSet>) -> OptionsContext {
        OptionsContext {
            tool_name: tool_name.to_string(),
            display_name: None,
            input,
            cwd: "/workspace".into(),
            change_set,
            no_persistent_rule: false,
            default_to_no: false,
            available_modes: Vec::new(),
            auto_unavailable: false,
            pre_plan_mode: None,
            context_used_percent: None,
            home_dir: None,
        }
    }

    /// `permission-effects.test.ts` > "Claude permission response effects" >
    /// "applies the native Skill effect for %s" (`it.each` of
    /// `allow-skill-exact` and `allow-skill-prefix`), plus P-28: the skill is
    /// validated only inside those two branches, so `allow-once` with no skill
    /// still allows.
    #[test]
    fn t6a2_applies_the_native_skill_effect() {
        let skill = ctx("Skill", json!({"skill": "deploy prod"}), None);
        let exact = apply_selection(OPTION_ALLOW_SKILL_EXACT, &skill).unwrap();
        assert_eq!(
            exact,
            SelectionOutcome::Allow {
                updated_permissions: Some(vec![json!({
                    "type": "addRules",
                    "rules": [{"toolName": "Skill", "ruleContent": "deploy prod"}],
                    "behavior": "allow",
                    "destination": "localSettings",
                })]),
                permanent: true,
            }
        );
        let prefix = apply_selection(OPTION_ALLOW_SKILL_PREFIX, &skill).unwrap();
        assert_eq!(
            prefix,
            SelectionOutcome::Allow {
                updated_permissions: Some(vec![json!({
                    "type": "addRules",
                    "rules": [{"toolName": "Skill", "ruleContent": "deploy:*"}],
                    "behavior": "allow",
                    "destination": "localSettings",
                })]),
                permanent: true,
            }
        );

        // P-28: only the exact and prefix ids read `input.skill`; the common
        // ids allow once / reject without touching it.
        let missing = ctx("Skill", json!({}), None);
        assert_eq!(
            apply_selection(OPTION_ALLOW_ONCE, &missing).unwrap(),
            SelectionOutcome::Allow {
                updated_permissions: None,
                permanent: false,
            }
        );
        assert!(matches!(
            apply_selection(OPTION_REJECT, &missing).unwrap(),
            SelectionOutcome::Deny { .. }
        ));
        // The validated branches still refuse a missing skill.
        assert_eq!(
            apply_selection(OPTION_ALLOW_SKILL_EXACT, &missing),
            Err("Invalid Skill permission input".to_string())
        );
        assert_eq!(
            apply_selection(
                OPTION_ALLOW_SKILL_PREFIX,
                &ctx("Skill", json!({"skill": "solo"}), None)
            ),
            Err("Skill prefix permission requires arguments".to_string())
        );
    }

    /// `permission-effects.test.ts` > "applies a generated WebFetch effect and
    /// an exact MCP provider effect".
    #[test]
    fn t6a2_applies_a_generated_webfetch_effect_and_an_exact_mcp_provider_effect() {
        let web = ctx("WebFetch", json!({"url": "https://example.com/path"}), None);
        assert_eq!(
            apply_selection(OPTION_ALLOW_WITH_UPDATES, &web).unwrap(),
            SelectionOutcome::Allow {
                updated_permissions: Some(vec![json!({
                    "type": "addRules",
                    "rules": [{"toolName": "WebFetch", "ruleContent": "domain:example.com"}],
                    "behavior": "allow",
                    "destination": "localSettings",
                })]),
                permanent: true,
            }
        );

        let change_set = normalize_change_set(
            &json!([{
                "type": "addRules",
                "rules": [{"toolName": "mcp__demo__deploy"}],
                "behavior": "allow",
                "destination": "localSettings",
            }]),
            false,
        );
        let mcp = ctx(
            "mcp__demo__deploy",
            json!({"target": "staging"}),
            change_set.clone(),
        );
        assert_eq!(
            apply_selection(OPTION_ALLOW_WITH_UPDATES, &mcp).unwrap(),
            SelectionOutcome::Allow {
                updated_permissions: Some(change_set.unwrap().updates),
                permanent: true,
            }
        );
    }

    /// `permission-effects.test.ts` > "allows EnterPlanMode temporarily without
    /// changing mode before the tool runs".
    #[test]
    fn t6a2_allows_enter_plan_mode_temporarily() {
        assert_eq!(
            apply_selection(OPTION_ALLOW_ONCE, &ctx("EnterPlanMode", json!({}), None)).unwrap(),
            SelectionOutcome::Allow {
                updated_permissions: None,
                permanent: false,
            }
        );
    }

    /// `permission-effects.test.ts` > "applies the selected ExitPlanMode mode
    /// and maps rejection" (P-32: the reject is the interrupting deny with
    /// "User chose to keep planning").
    #[test]
    fn t6a2_applies_the_selected_exit_plan_mode_and_maps_rejection() {
        let mut exit = ctx("ExitPlanMode", json!({"plan": "Implement it"}), None);
        exit.available_modes = vec!["auto".into(), "default".into(), "acceptEdits".into()];
        assert_eq!(
            apply_selection(OPTION_EXIT_PLAN_AUTO, &exit).unwrap(),
            SelectionOutcome::Allow {
                updated_permissions: Some(vec![json!({
                    "type": "setMode",
                    "mode": "auto",
                    "destination": "session",
                })]),
                permanent: true,
            }
        );
        assert_eq!(
            apply_selection(OPTION_EXIT_PLAN_ACCEPT_EDITS, &exit).unwrap(),
            SelectionOutcome::Allow {
                updated_permissions: Some(vec![json!({
                    "type": "setMode",
                    "mode": "acceptEdits",
                    "destination": "session",
                })]),
                permanent: true,
            }
        );
        // The `default` setMode is classified temporary.
        assert_eq!(
            apply_selection(OPTION_EXIT_PLAN_DEFAULT, &exit).unwrap(),
            SelectionOutcome::Allow {
                updated_permissions: Some(vec![json!({
                    "type": "setMode",
                    "mode": "default",
                    "destination": "session",
                })]),
                permanent: false,
            }
        );
        assert_eq!(
            apply_selection(OPTION_REJECT, &exit).unwrap(),
            SelectionOutcome::Deny {
                message: "User chose to keep planning".to_string(),
                interrupt: true,
            }
        );
    }

    /// `permission-effects.test.ts` > "interrupts the old query when
    /// ExitPlanMode clears context".
    #[test]
    fn t6a2_interrupts_the_old_query_when_exit_plan_mode_clears_context() {
        let mut exit = ctx("ExitPlanMode", json!({"plan": "Implement it"}), None);
        exit.available_modes = vec!["auto".into()];
        assert_eq!(
            apply_selection(OPTION_EXIT_PLAN_CLEAR_AUTO, &exit).unwrap(),
            SelectionOutcome::Deny {
                message: "User accepted the plan and requested a fresh context".to_string(),
                interrupt: true,
            }
        );
        assert_eq!(
            exit_plan_clear_context_mode(OPTION_EXIT_PLAN_CLEAR_AUTO),
            Some("auto")
        );
        assert_eq!(
            exit_plan_clear_context_mode(OPTION_EXIT_PLAN_CLEAR_BYPASS),
            Some("bypassPermissions")
        );
        assert_eq!(
            exit_plan_clear_context_mode(OPTION_EXIT_PLAN_CLEAR_ACCEPT_EDITS),
            Some("acceptEdits")
        );
        assert_eq!(exit_plan_clear_context_mode(OPTION_REJECT), None);
    }

    /// `permission-effects.test.ts` > "maps one-time allow without remembered
    /// updates".
    #[test]
    fn t6a2_maps_one_time_allow_without_remembered_updates() {
        assert_eq!(
            apply_selection(
                OPTION_ALLOW_ONCE,
                &ctx("Bash", json!({"command": "pwd"}), None)
            )
            .unwrap(),
            SelectionOutcome::Allow {
                updated_permissions: None,
                permanent: false,
            }
        );
    }

    /// `permission-effects.test.ts` > "maps durable allow to the snapshotted
    /// provider effect".
    #[test]
    fn t6a2_maps_durable_allow_to_the_snapshotted_provider_effect() {
        let suggestions = json!([{
            "type": "addRules",
            "rules": [{"toolName": "Bash", "ruleContent": "npm test:*"}],
            "behavior": "allow",
            "destination": "session",
        }]);
        let change_set = normalize_change_set(&suggestions, false).unwrap();
        let bash = ctx(
            "Bash",
            json!({"command": "npm test"}),
            Some(change_set.clone()),
        );
        assert_eq!(
            apply_selection(OPTION_ALLOW_WITH_UPDATES, &bash).unwrap(),
            SelectionOutcome::Allow {
                updated_permissions: Some(change_set.updates.clone()),
                permanent: true,
            }
        );
        // The effect is the snapshot, not the live provider array.
        assert_eq!(change_set.updates, suggestions.as_array().unwrap().clone());
    }
}
