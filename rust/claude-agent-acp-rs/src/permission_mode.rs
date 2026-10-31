//! Permission-mode resolution ported from the Node adapter's
//! `dist/permissions/modes.js` and `dist/session-mode.js`
//! `resolveInitialPermissionMode` (rust-parity-fixes Decision 3, R6).
//!
//! Node maps settings' user-facing spellings to camel-cased SDK wire values,
//! clamps `bypassPermissions` away when bypass is unavailable, and resolves the
//! initial mode as explicit > resumed > settings default. Invalid values are
//! reported through the adapter logger (`permissions/modes.ts:31-44`). All
//! functions here are pure (they take the resolved sources / capability flags
//! explicitly) so they are unit-testable without the OS.

use serde_json::Value;

use crate::logger::Logger;

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

/// Port of `permissions/modes.js:resolvePermissionMode` with the upstream's
/// error logging (`permissions/modes.ts:31-44`).
///
/// The raw settings value is kept as a [`Value`] so a non-string
/// `permissions.defaultMode` is reported exactly like Node (a plain `None`
/// would silently treat it as absent). Every rejected value is logged and the
/// result falls back to `default`.
pub fn resolve_permission_mode_logged(
    default_mode: Option<&Value>,
    logger: Option<&Logger>,
    allow_bypass: bool,
) -> String {
    let Some(value) = default_mode else {
        return "default".to_string();
    };
    let Some(raw) = value.as_str() else {
        log_error(
            logger,
            "Ignoring permissions.defaultMode from settings: expected a string.",
        );
        return "default".to_string();
    };
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        log_error(
            logger,
            "Ignoring permissions.defaultMode from settings: expected a non-empty string.",
        );
        return "default".to_string();
    }
    let mapped = match normalized.as_str() {
        "auto" => "auto",
        "default" => "default",
        "manual" => "default",
        "acceptedits" => "acceptEdits",
        "dontask" => "dontAsk",
        "plan" => "plan",
        "bypasspermissions" | "bypass" => "bypassPermissions",
        _ => {
            log_error(
                logger,
                &format!("Ignoring permissions.defaultMode from settings: unknown value '{raw}'."),
            );
            return "default".to_string();
        }
    };
    if mapped == "bypassPermissions" && !allow_bypass {
        log_error(
            logger,
            "Ignoring permission mode bypassPermissions: not available in this session.",
        );
        return "default".to_string();
    }
    mapped.to_string()
}

fn log_error(logger: Option<&Logger>, message: &str) {
    if let Some(logger) = logger {
        logger.error(message);
    }
}

/// Port of `session-mode.js:resolveInitialPermissionMode` with logging: an
/// explicit mode wins, then a resumed transcript's mode, then the settings
/// default. A resumed mode that cannot resolve now is reported once with the
/// transcript-specific message and the settings default is used.
pub fn resolve_initial_permission_mode_logged(
    explicit: Option<&Value>,
    resumed: Option<&Value>,
    default_mode: Option<&Value>,
    logger: Option<&Logger>,
    allow_bypass: bool,
) -> String {
    if let Some(explicit) = explicit {
        return resolve_permission_mode_logged(Some(explicit), logger, allow_bypass);
    }
    if let Some(resumed) = resumed {
        let raw = resumed.as_str();
        if let Some(mapped) =
            raw.and_then(|raw| resolve_permission_mode_clean(Some(raw), allow_bypass))
        {
            return mapped;
        }
        let rendered = raw
            .map(str::to_string)
            .unwrap_or_else(|| resumed.to_string());
        log_error(
            logger,
            &format!(
                "Ignoring permission mode \"{rendered}\" of the resumed transcript: not available in this session."
            ),
        );
    }
    resolve_permission_mode_logged(default_mode, logger, allow_bypass)
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
    let explicit = explicit.map(Value::from);
    let resumed = resumed.map(Value::from);
    let default_mode = default_mode.map(Value::from);
    resolve_initial_permission_mode_logged(
        explicit.as_ref(),
        resumed.as_ref(),
        default_mode.as_ref(),
        None,
        allow_bypass,
    )
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

    /// O-64 / Q-25 — every rejected `permissions.defaultMode` is logged with the
    /// upstream message (`permissions/modes.ts:31-44`), and an accepted value is
    /// logged not at all.
    #[test]
    fn inv_57_invalid_permission_mode_is_logged() {
        use serde_json::json;

        let dir = std::env::temp_dir().join(format!(
            "acp-perm-log-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let log_path = dir.join("agent.log");
        let logger = Logger::to_file(&log_path);

        // Non-string.
        assert_eq!(
            resolve_permission_mode_logged(Some(&json!(5)), Some(&logger), true),
            "default"
        );
        // Empty / whitespace.
        assert_eq!(
            resolve_permission_mode_logged(Some(&json!("")), Some(&logger), true),
            "default"
        );
        assert_eq!(
            resolve_permission_mode_logged(Some(&json!("   ")), Some(&logger), true),
            "default"
        );
        // Unknown.
        assert_eq!(
            resolve_permission_mode_logged(Some(&json!("bogus")), Some(&logger), true),
            "default"
        );
        // Bypass clamp.
        assert_eq!(
            resolve_permission_mode_logged(Some(&json!("bypassPermissions")), Some(&logger), false),
            "default"
        );

        let content = std::fs::read_to_string(&log_path).expect("read agent.log");
        assert!(
            content.contains("Ignoring permissions.defaultMode from settings: expected a string."),
            "{content:?}"
        );
        assert!(
            content.contains(
                "Ignoring permissions.defaultMode from settings: expected a non-empty string."
            ),
            "{content:?}"
        );
        assert!(
            content
                .contains("Ignoring permissions.defaultMode from settings: unknown value 'bogus'."),
            "{content:?}"
        );
        assert!(
            content.contains(
                "Ignoring permission mode bypassPermissions: not available in this session."
            ),
            "{content:?}"
        );

        // An accepted mode is not logged.
        let before = content.len();
        assert_eq!(
            resolve_permission_mode_logged(Some(&json!("plan")), Some(&logger), true),
            "plan"
        );
        let after = std::fs::read_to_string(&log_path).expect("read agent.log");
        assert_eq!(after.len(), before, "valid modes log nothing");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// O-64 — an unresolvable resumed-transcript mode is reported once with the
    /// transcript-specific message, then the settings default is used
    /// (`session-mode.ts:resolveInitialPermissionMode`).
    #[test]
    fn inv_57_unresolvable_resumed_mode_is_logged() {
        use serde_json::json;

        let dir = std::env::temp_dir().join(format!(
            "acp-perm-resumed-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let log_path = dir.join("agent.log");
        let logger = Logger::to_file(&log_path);

        assert_eq!(
            resolve_initial_permission_mode_logged(
                None,
                Some(&json!("bypassPermissions")),
                Some(&json!("acceptEdits")),
                Some(&logger),
                false,
            ),
            "acceptEdits"
        );
        let content = std::fs::read_to_string(&log_path).expect("read agent.log");
        assert!(
            content.contains(
                "Ignoring permission mode \"bypassPermissions\" of the resumed transcript: not available in this session."
            ),
            "{content:?}"
        );

        // A resolvable resumed mode is silent.
        let before = content.len();
        assert_eq!(
            resolve_initial_permission_mode_logged(
                None,
                Some(&json!("plan")),
                Some(&json!("acceptEdits")),
                Some(&logger),
                true,
            ),
            "plan"
        );
        let after = std::fs::read_to_string(&log_path).expect("read agent.log");
        assert_eq!(after.len(), before, "resolvable resumed mode logs nothing");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
