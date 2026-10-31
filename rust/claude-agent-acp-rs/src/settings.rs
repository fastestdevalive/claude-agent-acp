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

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// The merged settings fields the spawn path needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Settings {
    /// `permissions.defaultMode` after [`filter_escalating_default_mode`].
    pub default_mode: Option<String>,
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
fn merge(target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, value) in source {
                match target.get_mut(&key) {
                    Some(slot) => merge(slot, value),
                    None => {
                        target.insert(key, value);
                    }
                }
            }
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
    Settings {
        default_mode: permission
            .and_then(|p| p.get("defaultMode"))
            .and_then(Value::as_str)
            .map(String::from),
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
}
