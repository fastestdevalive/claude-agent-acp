//! Permission-mode resolution ported from the Node adapter's
//! `dist/permissions/modes.js` and `dist/session-mode.js`
//! `resolveInitialPermissionMode` (rust-parity-fixes Decision 3, R6).
//!
//! Node maps settings' user-facing spellings to camel-cased SDK wire values,
//! clamps `bypassPermissions` away when bypass is unavailable, and resolves the
//! initial mode as explicit > resumed > settings default. All functions here are
//! pure (they take the resolved sources / capability flags explicitly) so they
//! are unit-testable without the OS.

/// Whether bypass permissions is ever possible: not when root, unless in a
/// sandbox. Mirrors `ALLOW_BYPASS = !IS_ROOT || !!process.env.IS_SANDBOX`.
pub fn allow_bypass(is_root: bool, is_sandbox: bool) -> bool {
    !is_root || is_sandbox
}

/// Truthiness check mirroring Node's `!!process.env.IS_SANDBOX`:
/// empty string is falsy, non-empty is truthy, absent is falsy.
pub fn is_sandbox_env(val: Option<&str>) -> bool {
    val.map(|v| !v.is_empty()).unwrap_or(false)
}

/// Port of `permissions/modes.js:resolvePermissionMode`.
///
/// Returns `None` when the mode is rejected (empty / unknown / bypass-clamped),
/// `Some(mode)` on a clean resolution. Unknown / empty / non-string settings
/// modes reject to `default`; `bypassPermissions` clamps to `default` when
/// bypass is unavailable.
pub fn resolve_permission_mode_clean(
    default_mode: Option<&str>,
    allow_bypass: bool,
) -> Option<String> {
    let Some(raw) = default_mode else {
        return Some("default".to_string());
    };
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    let mapped = match normalized.as_str() {
        "auto" => "auto",
        "default" => "default",
        "manual" => "default",
        "acceptedits" => "acceptEdits",
        "dontask" => "dontAsk",
        "plan" => "plan",
        "bypasspermissions" | "bypass" => "bypassPermissions",
        _ => return None,
    };
    if mapped == "bypassPermissions" && !allow_bypass {
        return None;
    }
    Some(mapped.to_string())
}

/// The mode a new session starts in, where an invalid settings value falls back
/// to `default` (the caller-facing [[`resolve_permission_mode`]]).
pub fn resolve_permission_mode(default_mode: Option<&str>, allow_bypass: bool) -> String {
    resolve_permission_mode_clean(default_mode, allow_bypass)
        .unwrap_or_else(|| "default".to_string())
}

/// Port of `session-mode.js:resolveInitialPermissionMode`: an explicit mode
/// wins, then a resumed transcript's mode (Phase 1 passes `resumed=None`), then
/// the settings default.
pub fn resolve_initial_permission_mode(
    explicit: Option<&str>,
    resumed: Option<&str>,
    default_mode: Option<&str>,
    allow_bypass: bool,
) -> String {
    if let Some(explicit) = explicit {
        return resolve_permission_mode(Some(explicit), allow_bypass);
    }
    if let Some(resumed) = resumed {
        // Node rejects an unresolvable resumed mode and falls through to the
        // settings default (never the clamped `default`).
        if resolve_permission_mode_clean(Some(resumed), allow_bypass).is_some() {
            return resolve_permission_mode(Some(resumed), allow_bypass);
        }
    }
    resolve_permission_mode(default_mode, allow_bypass)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inv_57_permission_mode_aliases() {
        // Missing -> default.
        assert_eq!(resolve_permission_mode(None, true), "default");
        // Empty / whitespace / unknown -> default.
        assert_eq!(resolve_permission_mode(Some(""), true), "default");
        assert_eq!(resolve_permission_mode(Some("   "), true), "default");
        assert_eq!(resolve_permission_mode(Some("bogus"), true), "default");
        // Exact and case-insensitive aliases.
        assert_eq!(
            resolve_permission_mode(Some("acceptEdits"), true),
            "acceptEdits"
        );
        assert_eq!(
            resolve_permission_mode(Some("ACCEPTEDITS"), true),
            "acceptEdits"
        );
        assert_eq!(resolve_permission_mode(Some("manual"), true), "default");
        assert_eq!(resolve_permission_mode(Some("dontAsk"), true), "dontAsk");
        assert_eq!(resolve_permission_mode(Some("plan"), true), "plan");
        assert_eq!(
            resolve_permission_mode(Some("bypass"), true),
            "bypassPermissions"
        );
        assert_eq!(
            resolve_permission_mode(Some("BypassPermissions"), true),
            "bypassPermissions"
        );
        // Trippy whitespace is not trimmed mid-string.
        assert_eq!(resolve_permission_mode(Some("plan "), true), "plan");
    }

    #[test]
    fn inv_57_bypass_clamp() {
        // bypassPermissions clamps to default when bypass is unavailable.
        assert_eq!(
            resolve_permission_mode(Some("bypassPermissions"), false),
            "default"
        );
        // allow_bypass: not root always true.
        assert!(allow_bypass(false, false));
        // root + sandbox -> allowed.
        assert!(allow_bypass(true, true));
        // root, no sandbox -> denied.
        assert!(!allow_bypass(true, false));
        // IS_SANDBOX truthiness (Node !!process.env.IS_SANDBOX parity):
        assert!(!is_sandbox_env(None));
        assert!(!is_sandbox_env(Some("")));
        assert!(is_sandbox_env(Some("1")));
        assert!(is_sandbox_env(Some("0"))); // Non-empty string is truthy in JS
        assert!(is_sandbox_env(Some("true")));
    }

    #[test]
    fn inv_57_resolve_initial_mode_order() {
        // Explicit wins over resumed and default.
        assert_eq!(
            resolve_initial_permission_mode(
                Some("plan"),
                Some("default"),
                Some("acceptEdits"),
                true
            ),
            "plan"
        );
        // Resumed wins over default when resolvable.
        assert_eq!(
            resolve_initial_permission_mode(None, Some("plan"), Some("acceptEdits"), true),
            "plan"
        );
        // Default when nothing else.
        assert_eq!(
            resolve_initial_permission_mode(None, None, Some("acceptEdits"), true),
            "acceptEdits"
        );
        // Resumed bypass unavailable -> falls to default setting.
        assert_eq!(
            resolve_initial_permission_mode(
                None,
                Some("bypassPermissions"),
                Some("acceptEdits"),
                false
            ),
            "acceptEdits"
        );
        // Resumed bypass available -> keeps it.
        assert_eq!(
            resolve_initial_permission_mode(
                None,
                Some("bypassPermissions"),
                Some("acceptEdits"),
                true
            ),
            "bypassPermissions"
        );
    }
}
