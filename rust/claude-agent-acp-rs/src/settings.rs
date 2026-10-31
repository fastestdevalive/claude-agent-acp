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
    /// `apiKeyHelper` from the managed tier only (O-61).
    pub managed_api_key_helper: Option<String>,
    /// The full effective document (provenance-free).
    pub effective: Value,
}

impl Settings {
    /// Precedence: managed > --settings override > user/project/local (O-61, O-67).
    pub fn get_api_key_helper(&self, settings_override: Option<&Value>) -> Option<String> {
        let override_helper = settings_override
            .and_then(|v| v.get("apiKeyHelper"))
            .and_then(Value::as_str)
            .map(String::from);
        self.managed_api_key_helper
            .clone()
            .or(override_helper)
            .or_else(|| {
                self.effective
                    .get("apiKeyHelper")
                    .and_then(Value::as_str)
                    .map(String::from)
            })
    }
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

/// Recursive merge: objects merge key-wise, arrays concatenate and deduplicate
/// in source order (except `fallbackModel` which replaces, O-57), everything else replaces.
fn merge(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(target_map), Value::Object(source_map)) => {
            for (key, value) in source_map {
                match target_map.get_mut(&key) {
                    Some(slot) => merge_key(&key, slot, value),
                    None => {
                        target_map.insert(key, value);
                    }
                }
            }
        }
        (target, source) => *target = source,
    }
}

fn merge_key(key: &str, slot: &mut Value, value: Value) {
    if key == "modelPicker" {
        *slot = value;
        return;
    }
    match (slot, value) {
        (Value::Object(target_map), Value::Object(source_map)) => {
            if key == "extraKnownMarketplaces" || key == "managedMcpServers" {
                for (k, v) in source_map {
                    target_map.insert(k, v);
                }
                return;
            }
            for (k, v) in source_map {
                match target_map.get_mut(&k) {
                    Some(sub_slot) => merge_key(&k, sub_slot, v),
                    None => {
                        target_map.insert(k, v);
                    }
                }
            }
        }
        (Value::Array(target_arr), Value::Array(source_arr)) if key != "fallbackModel" => {
            for entry in source_arr {
                if !target_arr.contains(&entry) {
                    target_arr.push(entry);
                }
            }
        }
        (slot, value) => *slot = value,
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
/// `managed: None` means no managed tier (hermetic, finding 11).
pub fn resolve(
    cwd: &Path,
    config_dir: Option<&Path>,
    home: Option<&Path>,
    managed: Option<&Path>,
) -> Settings {
    let mut managed_docs = Vec::new();
    if let Some(managed_path) = managed {
        if let Some(doc) = read_settings(managed_path) {
            managed_docs.push(doc);
        }
        if let Some(parent) = managed_path.parent() {
            for doc in crate::process::read_managed_settings_dir(&parent.join("managed-settings.d"))
            {
                managed_docs.push(doc);
            }
        }
    }
    resolve_with_docs(cwd, config_dir, home, managed_docs)
}

/// Resolve settings with explicit managed documents (hermetic when empty).
pub fn resolve_with_docs(
    cwd: &Path,
    config_dir: Option<&Path>,
    home: Option<&Path>,
    managed_docs: Vec<Value>,
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
    for doc in managed_docs {
        sources.push((Source::Managed, doc));
    }

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
    let managed_api_key_helper = sources
        .iter()
        .filter(|(source, _)| *source == Source::Managed)
        .filter_map(|(_, doc)| doc.get("apiKeyHelper").and_then(Value::as_str))
        .next_back()
        .map(String::from);
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
        managed_api_key_helper,
        effective,
    }
}

/// Resolve the settings for `cwd` from the environment snapshot.
/// If `env.managed_docs()` is injected (`Some`), those documents are used (hermetic for tests).
/// Otherwise, platform-managed sources are read.
pub fn load(cwd: &Path, env: &crate::env_snapshot::EnvSnapshot) -> Settings {
    match env.managed_docs() {
        Some(docs) => load_with_managed(cwd, env, docs.to_vec()),
        None => load_with_managed(cwd, env, crate::process::read_managed_sources()),
    }
}

/// Resolve the settings for `cwd` from the environment snapshot with explicitly
/// injected managed sources. Tests and isolated embedders pass empty/mock docs.
pub fn load_with_managed(
    cwd: &Path,
    env: &crate::env_snapshot::EnvSnapshot,
    managed_docs: Vec<Value>,
) -> Settings {
    let config_dir = env.get_path("CLAUDE_CONFIG_DIR");
    let home = env.get_path("HOME");
    resolve_with_docs(cwd, config_dir.as_deref(), home.as_deref(), managed_docs)
}

/// Resolve settings with the managed FILE tier read from an explicit
/// `managed-settings.json` path (plus `managed-settings.d` beside it).
///
/// Production resolves [`crate::process::managed_settings_path`]; this seam lets
/// the SettingsManager hot-reload test exercise the real per-resolve file read
/// against a temp directory (Finding 2), never the host `/Library` or `/etc`
/// policy path.
pub fn load_with_managed_file(
    cwd: &Path,
    env: &crate::env_snapshot::EnvSnapshot,
    managed_file: &Path,
) -> Settings {
    load_with_managed(
        cwd,
        env,
        crate::process::read_managed_file_sources_at(managed_file),
    )
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
pub fn apply_managed_policy_env(_logger: &crate::logger::Logger) -> Vec<(String, String)> {
    // In sdk.mjs applyManagedPolicyEnv / resolveSettings({settingSources: []}):
    // Winning managed sources (MDM plist, Windows registry, or managed file + .d)
    // are resolved, and their effective `env` entries are applied.
    let docs = crate::process::read_managed_sources();
    let mut entries = Vec::new();
    for doc in docs {
        entries.extend(env_entries(&doc));
    }
    entries
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

    /// O-57: arrays are concatenated and deduplicated across tiers, except fallbackModel which replaces.
    #[test]
    fn o57_array_merge_and_fallback_model() {
        let root = temp_root("array-merge");
        let cwd = root.join("cwd");
        let config = root.join("config");

        write(
            &config.join("settings.json"),
            json!({
                "customArray": ["item1", "item2"],
                "fallbackModel": ["fb-old"]
            }),
        );
        write(
            &cwd.join(".claude/settings.json"),
            json!({
                "customArray": ["item2", "item3"],
                "fallbackModel": ["fb-new"]
            }),
        );

        let settings = resolve(&cwd, Some(&config), None, None);
        assert_eq!(
            settings.effective["customArray"],
            json!(["item1", "item2", "item3"])
        );
        assert_eq!(settings.effective["fallbackModel"], json!(["fb-new"]));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// O-57: modelPicker is replaced whole; extraKnownMarketplaces / managedMcpServers are shallow merged.
    #[test]
    fn o57_model_picker_and_shallow_object_merges() {
        let root = temp_root("picker-shallow");
        let cwd = root.join("cwd");
        let config = root.join("config");

        write(
            &config.join("settings.json"),
            json!({
                "modelPicker": {
                    "options": [{"id": "m1"}],
                    "otherField": "keep?"
                },
                "extraKnownMarketplaces": {
                    "mp1": {"url": "u1", "args": ["a1"]}
                },
                "managedMcpServers": {
                    "srv1": {"cmd": "c1", "args": ["a1"]}
                }
            }),
        );
        write(
            &cwd.join(".claude/settings.json"),
            json!({
                "modelPicker": {
                    "options": [{"id": "m2"}]
                },
                "extraKnownMarketplaces": {
                    "mp1": {"url": "u2"},
                    "mp2": {"url": "u3"}
                },
                "managedMcpServers": {
                    "srv1": {"cmd": "c2"}
                }
            }),
        );

        let settings = resolve(&cwd, Some(&config), None, None);
        // modelPicker replaced whole (no deep merge)
        assert_eq!(
            settings.effective["modelPicker"],
            json!({"options": [{"id": "m2"}]})
        );
        // extraKnownMarketplaces shallow merged (mp1 replaced whole, mp2 added)
        assert_eq!(
            settings.effective["extraKnownMarketplaces"],
            json!({
                "mp1": {"url": "u2"},
                "mp2": {"url": "u3"}
            })
        );
        // managedMcpServers shallow merged (srv1 replaced whole, inner args not concatenated)
        assert_eq!(
            settings.effective["managedMcpServers"],
            json!({
                "srv1": {"cmd": "c2"}
            })
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// O-61: apiKeyHelper precedence: managed > --settings override > user/project/local.
    #[test]
    fn o61_api_key_helper_precedence() {
        let root = temp_root("api-key-helper");
        let cwd = root.join("cwd");
        let config = root.join("config");
        let managed = root.join("managed.json");

        write(
            &config.join("settings.json"),
            json!({"apiKeyHelper": "user-helper"}),
        );

        // Case 1: user helper only
        let settings = resolve(&cwd, Some(&config), None, None);
        assert_eq!(
            settings.get_api_key_helper(None),
            Some("user-helper".to_string())
        );

        // Case 2: --settings override beats user helper
        let override_val = json!({"apiKeyHelper": "override-helper"});
        assert_eq!(
            settings.get_api_key_helper(Some(&override_val)),
            Some("override-helper".to_string())
        );

        // Case 3: managed helper beats --settings override
        write(&managed, json!({"apiKeyHelper": "managed-helper"}));
        let settings_managed = resolve(&cwd, Some(&config), None, Some(&managed));
        assert_eq!(
            settings_managed.get_api_key_helper(Some(&override_val)),
            Some("managed-helper".to_string())
        );
        assert_eq!(
            settings_managed.get_api_key_helper(None),
            Some("managed-helper".to_string())
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// T8A — the local project helper wins over an unused user Central helper,
    /// and the label follows the resolved helper.
    #[test]
    fn t8a_local_project_helper_wins_over_a_user_central_helper() {
        let root = temp_root("t8a-central-helper");
        let cwd = root.join("cwd");
        let config = root.join("config");
        std::fs::create_dir_all(cwd.join(".claude")).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        write(
            &config.join("settings.json"),
            json!({"apiKeyHelper": "central proxy start"}),
        );
        write(
            &cwd.join(".claude/settings.json"),
            json!({"apiKeyHelper": "central proxy start"}),
        );
        write(
            &cwd.join(".claude/settings.local.json"),
            json!({"apiKeyHelper": "ordinary-helper"}),
        );
        let settings = resolve(&cwd, Some(&config), None, None);
        let helper = settings.get_api_key_helper(None);
        assert_eq!(helper.as_deref(), Some("ordinary-helper"));
        let status = crate::auth_status::from_account_info(
            &json!({"apiKeySource": "apiKeyHelper"}),
            helper.as_deref(),
        )
        .expect("a status");
        assert_eq!(status.kind, "api_key");
        assert_eq!(status.label, "Anthropic API key");
        assert_eq!(status.detail.as_deref(), Some("API key helper"));
        // A Central helper alone is the AIR gateway.
        let central = crate::auth_status::from_account_info(
            &json!({"apiKeySource": "apiKeyHelper"}),
            Some("central proxy start"),
        )
        .expect("a status");
        assert_eq!(central.kind, "gateway");
        assert_eq!(central.label, "JetBrains Air Gateway");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// T8A — a programmatic `settings.apiKeyHelper` override wins for every
    /// value, and an empty override stays a plain API key.
    #[test]
    fn t8a_programmatic_helper_override_wins() {
        let root = temp_root("t8a-helper-override");
        let cwd = root.join("cwd");
        let config = root.join("config");
        std::fs::create_dir_all(cwd.join(".claude")).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        write(
            &cwd.join(".claude/settings.json"),
            json!({"apiKeyHelper": "decentralized proxy start"}),
        );
        let settings = resolve(&cwd, Some(&config), None, None);
        for helper in ["central proxy start", "", "ordinary-helper"] {
            let override_val = json!({"apiKeyHelper": helper});
            let resolved = settings.get_api_key_helper(Some(&override_val));
            assert_eq!(resolved.as_deref(), Some(helper), "override {helper:?}");
            let status = crate::auth_status::from_account_info(
                &json!({"apiKeySource": "apiKeyHelper"}),
                resolved.as_deref(),
            )
            .expect("a status");
            if helper == "central proxy start" {
                assert_eq!(status.kind, "gateway");
                assert_eq!(status.label, "JetBrains Air Gateway");
            } else {
                assert_eq!(status.kind, "api_key");
                assert_eq!(status.label, "Anthropic API key");
                assert_eq!(status.detail.as_deref(), Some("API key helper"));
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Finding 1: An unreadable/unparseable MDM plist (or a `reg.exe` timeout)
    /// drops only the MDM tier; the managed FILE tier is still enforced
    /// (deniedModels, disableBypassPermissionsMode, apiKeyHelper, managed env).
    /// Exercises the real file reader on a temp path, so no host policy is read.
    #[test]
    fn test_unreadable_mdm_plist_drops_only_mdm_tier_file_tier_enforced() {
        let root = temp_root("mdm-unreadable-file-enforced");
        let cwd = root.join("cwd");
        let managed_file = root.join("managed-settings.json");

        write(
            &managed_file,
            json!({
                "deniedModels": ["forbidden-model-v1"],
                "permissions": {"disableBypassPermissionsMode": "disable"},
                "apiKeyHelper": "file-managed-helper",
                "env": {"MANAGED_VAR": "managed_val"}
            }),
        );

        // The OS/MDM source is unreadable, so it contributes no document (None);
        // only the file tier remains. This is the `Oc` winner list without `mdm`.
        let file_docs = crate::process::read_managed_file_sources_at(&managed_file);
        assert_eq!(file_docs.len(), 1, "the file tier is still readable");
        let settings = load_with_managed(
            &cwd,
            &crate::env_snapshot::EnvSnapshot::empty(),
            file_docs.clone(),
        );

        assert_eq!(
            settings.managed_denied_models,
            vec!["forbidden-model-v1".to_string()]
        );
        assert!(settings.disable_bypass);
        assert_eq!(
            settings.get_api_key_helper(None),
            Some("file-managed-helper".to_string())
        );

        // The managed `env` object is applied from the same file documents.
        let entries: Vec<(String, String)> = file_docs.iter().flat_map(env_entries).collect();
        assert!(
            entries.contains(&("MANAGED_VAR".to_string(), "managed_val".to_string())),
            "{entries:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Finding 2: `load_with_managed_file` re-reads the managed file on every
    /// resolve, so an edit after the first load is visible to the second.
    #[test]
    fn test_load_with_managed_file_rereads_after_edit() {
        let root = temp_root("managed-file-reread");
        let cwd = root.join("cwd");
        let managed_file = root.join("managed-settings.json");
        let env = crate::env_snapshot::EnvSnapshot::empty();

        write(&managed_file, json!({"model": "managed-v1"}));
        assert_eq!(
            load_with_managed_file(&cwd, &env, &managed_file)
                .model
                .as_deref(),
            Some("managed-v1")
        );

        write(&managed_file, json!({"model": "managed-v2"}));
        assert_eq!(
            load_with_managed_file(&cwd, &env, &managed_file)
                .model
                .as_deref(),
            Some("managed-v2")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Finding 3: Hermetic settings load: load() with hermetic EnvSnapshot uses injected
    /// documents and load_with_managed has real callers.
    #[test]
    fn test_hermetic_load_with_managed_docs() {
        let root = temp_root("hermetic-load");
        let cwd = root.join("cwd");

        let injected_doc = json!({
            "deniedModels": ["injected-denied-model"],
            "apiKeyHelper": "injected-helper"
        });

        // 1. load_with_managed direct call
        let env_empty = crate::env_snapshot::EnvSnapshot::empty();
        let settings = load_with_managed(&cwd, &env_empty, vec![injected_doc.clone()]);
        assert_eq!(
            settings.managed_denied_models,
            vec!["injected-denied-model".to_string()]
        );
        assert_eq!(
            settings.get_api_key_helper(None).as_deref(),
            Some("injected-helper")
        );

        // 2. load() via EnvSnapshot::with_managed_docs
        let env_injected =
            crate::env_snapshot::EnvSnapshot::empty().with_managed_docs(vec![injected_doc]);
        let settings2 = load(&cwd, &env_injected);
        assert_eq!(
            settings2.managed_denied_models,
            vec!["injected-denied-model".to_string()]
        );
        assert_eq!(
            settings2.get_api_key_helper(None).as_deref(),
            Some("injected-helper")
        );

        // 3. EnvSnapshot::from_pairs defaults to empty managed_docs (hermetic)
        let env_pairs = crate::env_snapshot::EnvSnapshot::from_pairs([(
            "CLAUDE_CONFIG_DIR",
            root.to_str().unwrap(),
        )]);
        // Finding 3: the injected tier is the ONLY one consulted — an empty
        // document set means `load` never touches /Library/... or /etc.
        assert!(
            env_pairs.managed_docs().is_some_and(|d| d.is_empty()),
            "from_pairs must inject an empty managed tier"
        );
        let settings_pairs = load(&cwd, &env_pairs);
        assert!(settings_pairs.managed_denied_models.is_empty());
        assert_eq!(settings_pairs.get_api_key_helper(None), None);

        let _ = std::fs::remove_dir_all(&root);
    }
}
