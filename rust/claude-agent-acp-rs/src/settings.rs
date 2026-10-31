//! Claude Code settings resolver (rust-parity-fixes Decision 3, R6/X4).
//!
//! Node's `SettingsManager` (`dist/settings.js`) resolves the effective
//! settings through the SDK's `resolveSettings` merge engine and then applies
//! the CLI's trust filter (`filterEscalatingDefaultMode`). This module ports
//! the same four tiers and the same filter:
//!
//! 1. user    — `<CLAUDE_CONFIG_DIR>/settings.json`, else `$HOME/.claude/settings.json`
//! 2. project — `<cwd>/.claude/settings.json`
//! 3. local   — `<cwd>/.claude/settings.local.json`
//! 4. managed — the platform path from [`crate::process::managed_settings_path`]
//!
//! Later tiers win; objects merge recursively, everything else replaces. A
//! missing or unparseable file is skipped (the SDK swallows read errors too).
//! File watching is not ported: the adapter resolves settings once per spawn.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// The merged settings fields the spawn path needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    /// `permissions.defaultMode` after [`filter_escalating_default_mode`].
    pub default_mode: Option<String>,
    /// The raw `permissions.defaultMode` value (for the invalid-mode log, O-64):
    /// keeps a non-string value distinguishable from an absent one.
    pub default_mode_value: Option<Value>,
    /// `permissions.disableBypassPermissionsMode == "disable"`.
    pub disable_bypass: bool,
    /// `model` (the only settings key that seeds `currentModelId`, R9).
    pub model: Option<String>,
    /// `effortLevel`.
    pub effort_level: Option<String>,
    /// `modelSettings` (per-model effort overrides, used in Phase 2).
    pub model_settings: Option<Value>,
    /// `ultracode` (session-effort.js ultracode opt-in).
    pub ultracode: bool,
    /// `availableModels` (F5b/O-43), unioned across tiers in source order.
    pub available_models: Option<Vec<String>>,
    /// `modelOverrides` (F5b/O-44): allowlist alias -> provider id.
    pub model_overrides: Option<HashMap<String, String>>,
    /// `deniedModels` from the managed tier only (F5b/O-46).
    pub managed_denied_models: Vec<String>,
    /// The full effective document (provenance-free).
    pub effective: Value,
}

/// A settings tier and the document it contributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `~/.claude/settings.json`.
    User,
    /// `<cwd>/.claude/settings.json`.
    Project,
    /// `<cwd>/.claude/settings.local.json`.
    Local,
    /// The managed (policy) file.
    Managed,
}

impl Source {
    /// The SDK's provenance name (`v4`).
    pub fn name(self) -> &'static str {
        match self {
            Source::User => "user",
            Source::Project => "project",
            Source::Local => "local",
            Source::Managed => "managed",
        }
    }
}

/// Default modes the CLI refuses from repo-committed tiers (`RZe`).
const ESCALATING_MODES: [&str; 3] = ["bypassPermissions", "auto", "acceptEdits"];
/// Modes only `local` (and `project`) may not escalate (`YZ`).
const LOCAL_ESCALATING_MODES: [&str; 2] = ["auto", "bypassPermissions"];

/// Recursive merge: objects merge key-wise, any other shape is replaced.
///
/// `availableModels` is the exception the SDK's merge engine documents: arrays
/// from every tier are unioned and deduped in source order (later tiers append
/// their new entries) instead of the later tier replacing the earlier one.
fn merge(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, value) in source {
                match target.get_mut(&key) {
                    Some(slot) => {
                        if key == "availableModels" {
                            merge_available_models(slot, value);
                        } else {
                            merge(slot, value);
                        }
                    }
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (target, source) => *target = source,
    }
}

/// Union two `availableModels` arrays in source order, dropping duplicates.
fn merge_available_models(target: &mut Value, source: Value) {
    match (&mut *target, source) {
        (Value::Array(existing), Value::Array(incoming)) => {
            let mut seen: Vec<Value> = existing.clone();
            for entry in incoming {
                if !seen.contains(&entry) {
                    seen.push(entry);
                }
            }
            *existing = seen;
        }
        (target, source) => *target = source,
    }
}

/// Read one settings file; absent or malformed documents contribute nothing.
fn read_settings(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(value @ Value::Object(_)) => Some(value),
        _ => None,
    }
}

/// Port of the SDK's `filterEscalatingDefaultMode` (`wZe`).
///
/// Drops an escalating `permissions.defaultMode` when the tier that defined it
/// is not trusted: `project` for any escalating mode, `project`/`local` for
/// `auto`/`bypassPermissions`.
pub fn filter_escalating_default_mode(effective: Value, sources: &[(Source, Value)]) -> Value {
    let Some(mode) = effective
        .get("permissions")
        .and_then(|p| p.get("defaultMode"))
        .and_then(Value::as_str)
        .map(String::from)
    else {
        return effective;
    };
    if !ESCALATING_MODES.contains(&mode.as_str()) {
        return effective;
    }
    let untrusted: &[Source] = if LOCAL_ESCALATING_MODES.contains(&mode.as_str()) {
        &[Source::Project, Source::Local]
    } else {
        &[Source::Project]
    };
    let winner = sources.iter().rev().find(|(_, doc)| {
        doc.get("permissions")
            .and_then(|p| p.get("defaultMode"))
            .is_some()
    });
    let Some((source, _)) = winner else {
        return effective;
    };
    if !untrusted.contains(source) {
        return effective;
    }
    let mut effective = effective;
    if let Some(permissions) = effective
        .get_mut("permissions")
        .and_then(Value::as_object_mut)
    {
        permissions.remove("defaultMode");
    }
    effective
}

/// Resolve the settings for `cwd` with explicit tier locations (testable).
pub fn resolve(
    cwd: &Path,
    config_dir: Option<&Path>,
    home: Option<&Path>,
    managed: Option<&Path>,
) -> Settings {
    let user_dir = config_dir
        .map(PathBuf::from)
        .or_else(|| home.map(|h| h.join(".claude")));
    let mut sources: Vec<(Source, Value)> = Vec::new();
    let mut push = |source: Source, path: Option<PathBuf>| {
        if let Some(doc) = path.as_deref().and_then(read_settings) {
            sources.push((source, doc));
        }
    };
    push(Source::User, user_dir.map(|dir| dir.join("settings.json")));
    push(
        Source::Project,
        Some(cwd.join(".claude").join("settings.json")),
    );
    push(
        Source::Local,
        Some(cwd.join(".claude").join("settings.local.json")),
    );
    push(Source::Managed, managed.map(PathBuf::from));

    let mut effective = Value::Object(Map::new());
    for (_, doc) in &sources {
        merge(&mut effective, doc.clone());
    }
    let effective = filter_escalating_default_mode(effective, &sources);
    let permission = effective.get("permissions");
    let available_models = effective
        .get("availableModels")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        });
    let model_overrides = effective
        .get("modelOverrides")
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect()
        });
    // The CLI reads `deniedModels` from managed settings only; the merged
    // document must not let a user/project tier deny a model.
    let managed_denied_models = sources
        .iter()
        .filter(|(source, _)| *source == Source::Managed)
        .filter_map(|(_, doc)| doc.get("deniedModels"))
        .filter_map(Value::as_array)
        .flat_map(|entries| entries.iter().filter_map(Value::as_str))
        .map(String::from)
        .collect();
    Settings {
        default_mode: permission
            .and_then(|p| p.get("defaultMode"))
            .and_then(Value::as_str)
            .map(String::from),
        default_mode_value: permission.and_then(|p| p.get("defaultMode")).cloned(),
        disable_bypass: permission
            .and_then(|p| p.get("disableBypassPermissionsMode"))
            .and_then(Value::as_str)
            == Some("disable"),
        model: effective
            .get("model")
            .and_then(Value::as_str)
            .map(String::from),
        effort_level: effective
            .get("effortLevel")
            .and_then(Value::as_str)
            .map(String::from),
        model_settings: effective.get("modelSettings").cloned(),
        ultracode: effective
            .get("ultracode")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        available_models,
        model_overrides,
        managed_denied_models,
        effective,
    }
}

/// Resolve the settings for `cwd` from the environment snapshot.
pub fn load(cwd: &Path, env: &crate::env_snapshot::EnvSnapshot) -> Settings {
    let config_dir = env.get_path("CLAUDE_CONFIG_DIR");
    let home = env.get_path("HOME");
    let managed = crate::process::managed_settings_path();
    resolve(cwd, config_dir.as_deref(), home.as_deref(), Some(&managed))
}

/// The managed-policy tier's `env` object (`managed-policy.ts:19-34`).
///
/// The Node adapter resolves the policy with
/// `resolveSettings({settingSources: []})` and copies `effective.env` into
/// `process.env` before any SDK call. This port reads the managed settings file
/// (the same tier [`resolve`] merges last) and returns its string-valued `env`
/// entries. A missing or malformed file yields an empty list, so a transient
/// read failure never stops the adapter from starting.
pub fn managed_policy_env(managed: Option<&Path>) -> Vec<(String, String)> {
    let Some(doc) = managed.and_then(read_settings) else {
        return Vec::new();
    };
    env_entries(&doc)
}

/// The managed-policy env applied at process entry (O-03), best-effort.
///
/// The tier is resolved before the first SDK call so the spawned `claude`
/// inherits it. A missing policy file is normal (no MDM/managed policy); an
/// unreadable one is reported to stderr and the adapter still starts
/// (`managed-policy.ts` catch). Never logs values.
pub fn apply_managed_policy_env(logger: &crate::logger::Logger) -> Vec<(String, String)> {
    apply_managed_policy_env_at(&crate::process::managed_settings_path(), logger)
}

/// [`apply_managed_policy_env`] against an explicit managed-settings path
/// (unit-testable without touching the host policy file).
pub fn apply_managed_policy_env_at(
    managed: &Path,
    logger: &crate::logger::Logger,
) -> Vec<(String, String)> {
    if !managed.exists() {
        return Vec::new();
    }
    match read_settings(managed) {
        Some(doc) => env_entries(&doc),
        None => {
            logger.error(
                "Failed to resolve managed policy settings: unreadable managed-settings.json",
            );
            Vec::new()
        }
    }
}

/// The string-valued entries of a settings document's `env` object.
fn env_entries(doc: &Value) -> Vec<(String, String)> {
    let Some(env) = doc.get("env").and_then(Value::as_object) else {
        return Vec::new();
    };
    env.iter()
        .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acp-settings-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    fn write(path: &Path, value: Value) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, value.to_string()).expect("write settings");
    }

    #[test]
    fn inv_57_settings_merge_and_escalation_filter() {
        let root = temp_root("merge");
        let cwd = root.join("cwd");
        let config = root.join("config");
        let managed = root.join("managed.json");

        write(
            &config.join("settings.json"),
            json!({"model": "user-model", "permissions": {"defaultMode": "plan"}, "effortLevel": "low"}),
        );
        write(
            &cwd.join(".claude/settings.json"),
            json!({"model": "project-model", "permissions": {"defaultMode": "acceptEdits"}}),
        );
        write(
            &cwd.join(".claude/settings.local.json"),
            json!({"permissions": {"disableBypassPermissionsMode": "disable"}}),
        );

        let settings = resolve(&cwd, Some(&config), None, Some(&managed));
        // Project wins for `model`; the escalating project defaultMode is dropped.
        assert_eq!(settings.model.as_deref(), Some("project-model"));
        assert_eq!(settings.default_mode, None);
        assert!(settings.disable_bypass);
        assert_eq!(settings.effort_level.as_deref(), Some("low"));

        // A managed defaultMode is trusted, even the escalating kind.
        write(
            &managed,
            json!({"permissions": {"defaultMode": "bypassPermissions"}}),
        );
        let settings = resolve(&cwd, Some(&config), None, Some(&managed));
        assert_eq!(settings.default_mode.as_deref(), Some("bypassPermissions"));

        // A user-tier acceptEdits is trusted too (only project/local escalate).
        let managed_user = root.join("no-managed.json");
        write(
            &config.join("settings.json"),
            json!({"permissions": {"defaultMode": "acceptEdits"}}),
        );
        let _ = std::fs::remove_file(cwd.join(".claude/settings.json"));
        let settings = resolve(&cwd, Some(&config), None, Some(&managed_user));
        assert_eq!(settings.default_mode.as_deref(), Some("acceptEdits"));

        // `auto` from local is escalating (project + local are untrusted).
        write(
            &cwd.join(".claude/settings.local.json"),
            json!({"permissions": {"defaultMode": "auto"}}),
        );
        let settings = resolve(&cwd, Some(&config), None, Some(&managed_user));
        assert_eq!(settings.default_mode, None);

        // Deep merge keeps sibling keys from lower tiers.
        write(
            &config.join("settings.json"),
            json!({"permissions": {"allow": ["Read"]}, "model": "m1"}),
        );
        write(
            &cwd.join(".claude/settings.local.json"),
            json!({"permissions": {"deny": ["Bash"]}}),
        );
        let settings = resolve(&cwd, Some(&config), None, Some(&managed_user));
        assert_eq!(
            settings.effective["permissions"],
            json!({"allow": ["Read"], "deny": ["Bash"]})
        );
        assert_eq!(settings.model.as_deref(), Some("m1"));

        // Malformed files and missing tiers are skipped without error.
        std::fs::write(cwd.join(".claude/settings.local.json"), "{not json").unwrap();
        let settings = resolve(&cwd, Some(&config), None, Some(&root.join("absent.json")));
        assert_eq!(settings.model.as_deref(), Some("m1"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn inv_57_ultracode_setting() {
        let root = temp_root("ultracode");
        let cwd = root.join("cwd");
        write(
            &cwd.join(".claude/settings.local.json"),
            json!({"ultracode": true}),
        );
        let settings = resolve(&cwd, None, None, None);
        assert!(settings.ultracode);

        let settings_empty = resolve(&root, None, None, None);
        assert!(!settings_empty.ultracode);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// O-64 — the raw `permissions.defaultMode` survives resolution so a
    /// non-string value can be reported (and is not confused with absence).
    #[test]
    fn inv_57_default_mode_value_keeps_non_string() {
        let root = temp_root("raw-mode");
        let cwd = root.join("cwd");
        write(
            &cwd.join(".claude/settings.local.json"),
            json!({"permissions": {"defaultMode": 7}}),
        );
        let settings = resolve(&cwd, None, None, None);
        assert_eq!(settings.default_mode, None);
        assert_eq!(settings.default_mode_value, Some(json!(7)));

        // An escalating mode dropped by the trust filter is absent, not raw.
        write(
            &cwd.join(".claude/settings.local.json"),
            json!({"permissions": {"defaultMode": "auto"}}),
        );
        let settings = resolve(&cwd, None, None, None);
        assert_eq!(settings.default_mode_value, None);

        let _ = std::fs::remove_dir_all(&root);
    }

    // F5b/O-43: `availableModels` unions and dedupes across tiers in source
    // order; an absent key means no allowlist, an empty array stays empty.
    #[test]
    fn f5b_available_models_union_across_tiers() {
        let root = temp_root("available-models");
        let cwd = root.join("cwd");
        let config = root.join("config");

        write(
            &config.join("settings.json"),
            json!({"availableModels": ["claude-haiku-4-5", "claude-opus-4-7[1m]"]}),
        );
        write(
            &cwd.join(".claude/settings.json"),
            json!({"availableModels": ["claude-opus-4-7[1m]", "claude-sonnet-4-6[1m]"]}),
        );
        let settings = resolve(&cwd, Some(&config), None, None);
        assert_eq!(
            settings.available_models,
            Some(vec![
                "claude-haiku-4-5".to_string(),
                "claude-opus-4-7[1m]".to_string(),
                "claude-sonnet-4-6[1m]".to_string()
            ])
        );

        // A later tier's empty array unions to the same list (nothing new).
        write(
            &cwd.join(".claude/settings.local.json"),
            json!({"availableModels": []}),
        );
        let settings = resolve(&cwd, Some(&config), None, None);
        assert_eq!(settings.available_models.as_ref().map(Vec::len), Some(3));

        // Absent everywhere: only the user tier contributes (two entries).
        let bare = root.join("bare");
        let settings = resolve(&bare, Some(&config), None, None);
        assert_eq!(settings.available_models.as_ref().map(Vec::len), Some(2));
        let settings = resolve(&bare, None, None, None);
        assert!(settings.available_models.is_none());

        // Explicitly empty allowlist: Default only downstream.
        write(
            &config.join("settings.json"),
            json!({"availableModels": []}),
        );
        let settings = resolve(&bare, Some(&config), None, None);
        assert_eq!(settings.available_models, Some(Vec::new()));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// O-03 — the managed tier's `env` object is returned for the spawn
    /// environment; a missing or malformed file yields nothing (best-effort).
    #[test]
    fn inv_managed_policy_env_from_managed_tier() {
        let root = temp_root("policy-env");
        let managed = root.join("managed-settings.json");
        write(
            &managed,
            json!({"env": {"POLICY_FLAG": "on", "EMPTY": "", "NUM": 1}}),
        );
        let mut env = managed_policy_env(Some(&managed));
        env.sort();
        assert_eq!(
            env,
            vec![
                ("EMPTY".to_string(), String::new()),
                ("POLICY_FLAG".to_string(), "on".to_string()),
            ],
            "string-valued env entries only"
        );

        // Missing file -> no policy env.
        assert!(managed_policy_env(Some(&root.join("absent.json"))).is_empty());

        // Malformed file -> no policy env, no panic.
        std::fs::write(&managed, "{not json").unwrap();
        assert!(managed_policy_env(Some(&managed)).is_empty());

        // No managed path at all -> no policy env.
        assert!(managed_policy_env(None).is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// O-03 — the entry-point application reads the managed tier and reports a
    /// malformed/unreadable policy file to the logger without aborting.
    #[test]
    fn inv_managed_policy_env_applied_and_failure_logged() {
        let root = temp_root("policy-apply");
        let managed = root.join("managed-settings.json");
        write(&managed, json!({"env": {"POLICY_FLAG": "on", "NUM": 1}}));
        let log_path = root.join("agent.log");
        let logger = crate::logger::Logger::to_file(&log_path);

        assert_eq!(
            apply_managed_policy_env_at(&managed, &logger),
            vec![("POLICY_FLAG".to_string(), "on".to_string())],
            "policy env applied"
        );
        assert!(!log_path.exists(), "a successful policy read logs nothing");

        // Malformed policy: no env, but an error line (not the values).
        std::fs::write(&managed, "{not json").unwrap();
        assert!(apply_managed_policy_env_at(&managed, &logger).is_empty());
        let content = std::fs::read_to_string(&log_path).expect("read agent.log");
        assert!(
            content.contains("Failed to resolve managed policy settings"),
            "{content:?}"
        );
        assert!(
            !content.contains("POLICY_FLAG"),
            "never log policy values: {content:?}"
        );

        // Missing file: nothing, no error line.
        let before = content.len();
        assert!(apply_managed_policy_env_at(&root.join("absent.json"), &logger).is_empty());
        let after = std::fs::read_to_string(&log_path).expect("read agent.log");
        assert_eq!(after.len(), before, "a missing policy file is silent");

        let _ = std::fs::remove_dir_all(&root);
    }

    // F5b/O-44: `modelOverrides` merges per key, later tiers winning.
    #[test]
    fn f5b_model_overrides_merge() {
        let root = temp_root("model-overrides");
        let cwd = root.join("cwd");
        let config = root.join("config");
        write(
            &config.join("settings.json"),
            json!({"modelOverrides": {"claude-opus-4-6": "us.anthropic.claude-opus-4-6-v1", "haiku": "h1"}}),
        );
        write(
            &cwd.join(".claude/settings.json"),
            json!({"modelOverrides": {"claude-opus-4-6": "eu.anthropic.claude-opus-4-6-v1"}}),
        );
        let settings = resolve(&cwd, Some(&config), None, None);
        let overrides = settings.model_overrides.expect("overrides");
        assert_eq!(
            overrides.get("claude-opus-4-6").map(String::as_str),
            Some("eu.anthropic.claude-opus-4-6-v1")
        );
        assert_eq!(overrides.get("haiku").map(String::as_str), Some("h1"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // F5b/O-46: `deniedModels` is honored from the managed tier only.
    #[test]
    fn f5b_denied_models_managed_only() {
        let root = temp_root("denied-models");
        let cwd = root.join("cwd");
        let config = root.join("config");
        let managed = root.join("managed.json");

        write(
            &config.join("settings.json"),
            json!({"deniedModels": ["opus"]}),
        );
        let settings = resolve(&cwd, Some(&config), None, Some(&managed));
        assert!(settings.managed_denied_models.is_empty());

        write(&managed, json!({"deniedModels": ["opus", "claude-opus-5"]}));
        let settings = resolve(&cwd, Some(&config), None, Some(&managed));
        assert_eq!(
            settings.managed_denied_models,
            ["opus".to_string(), "claude-opus-5".to_string()]
        );
        // The merged effective document carries the managed tier's key; the
        // managed-only extraction is what `deniedModels` policy reads.
        assert_eq!(
            settings.effective["deniedModels"],
            json!(["opus", "claude-opus-5"])
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // F5b/O-35: a non-string `model` is visible to the caller as a diagnostic
    // while `Settings.model` stays `None`.
    #[test]
    fn f5b_non_string_model_is_ignored() {
        let root = temp_root("model-non-string");
        let cwd = root.join("cwd");
        write(&cwd.join(".claude/settings.json"), json!({"model": 42}));
        let settings = resolve(&cwd, None, None, None);
        assert_eq!(settings.model, None);
        assert_eq!(settings.effective["model"], json!(42));
        let _ = std::fs::remove_dir_all(&root);
    }
}
