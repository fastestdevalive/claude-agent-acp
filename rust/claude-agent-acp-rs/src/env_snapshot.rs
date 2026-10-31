use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

/// A snapshot of environment variables used by the adapter.
///
/// In production, this is captured once from the real process environment
/// (`EnvSnapshot::from_process()`). In tests and in-process scenarios, a hermetic
/// snapshot (`EnvSnapshot::empty()` or configured snapshot) isolates the adapter
/// from host process environment pollution and managed document injection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvSnapshot {
    vars: Arc<HashMap<String, String>>,
    managed_docs: Option<Arc<Vec<Value>>>,
}

impl EnvSnapshot {
    /// Capture snapshot from current process environment.
    /// `managed_docs` is `None`, so platform managed settings sources are resolved normally.
    pub fn from_process() -> Self {
        Self {
            vars: Arc::new(std::env::vars().collect()),
            managed_docs: None,
        }
    }

    /// Empty, hermetic environment snapshot with empty managed documents.
    pub fn empty() -> Self {
        Self {
            vars: Arc::new(HashMap::new()),
            managed_docs: Some(Arc::new(Vec::new())),
        }
    }

    /// Create from key-value pairs (hermetic by default with empty managed documents).
    pub fn from_pairs<I, K, V>(iter: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            vars: Arc::new(
                iter.into_iter()
                    .map(|(k, v)| (k.into(), v.into()))
                    .collect(),
            ),
            managed_docs: Some(Arc::new(Vec::new())),
        }
    }

    /// Injected managed settings documents (Finding 3).
    pub fn with_managed_docs(mut self, docs: Vec<Value>) -> Self {
        self.managed_docs = Some(Arc::new(docs));
        self
    }

    /// Return the injected managed settings documents, if any.
    pub fn managed_docs(&self) -> Option<&[Value]> {
        self.managed_docs.as_deref().map(Vec::as_slice)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(|s| s.as_str())
    }

    pub fn get_os(&self, key: &str) -> Option<OsString> {
        self.vars.get(key).map(OsString::from)
    }

    pub fn get_path(&self, key: &str) -> Option<PathBuf> {
        self.vars.get(key).map(PathBuf::from)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.vars.contains_key(key)
    }

    /// Whether `key` is set to a non-empty value.
    ///
    /// Node tests the remote-login environment with `process.env.NO_BROWSER`
    /// etc., where an empty string is falsy; this mirrors that truthiness
    /// without ever exposing the value.
    pub fn flag(&self, key: &str) -> bool {
        self.get(key).is_some_and(|value| !value.is_empty())
    }

    pub fn vars(&self) -> &HashMap<String, String> {
        &self.vars
    }

    pub fn with_var(&self, key: impl Into<String>, val: impl Into<String>) -> Self {
        let mut map = (*self.vars).clone();
        map.insert(key.into(), val.into());
        Self {
            vars: Arc::new(map),
            managed_docs: self.managed_docs.clone(),
        }
    }

    /// Merge entries over the snapshot, later entries winning (FA3-10).
    ///
    /// The managed-policy tier mutates `process.env` before any SDK call in
    /// Node (`index.ts:51`); the port folds the same entries into the one
    /// snapshot every adapter reader uses, so provider defaults,
    /// `ANTHROPIC_MODEL`, `CLAUDE_CONFIG_DIR` and the v2 opt-in all see them.
    #[must_use]
    pub fn with_vars<I, K, V>(&self, entries: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut map = (*self.vars).clone();
        for (key, value) in entries {
            map.insert(key.into(), value.into());
        }
        Self {
            vars: Arc::new(map),
            managed_docs: self.managed_docs.clone(),
        }
    }
}

impl Default for EnvSnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

impl<K: Into<String>, V: Into<String>> FromIterator<(K, V)> for EnvSnapshot {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        Self::from_pairs(iter)
    }
}

#[cfg(test)]
mod tests {
    use super::EnvSnapshot;

    /// FA3-10 — managed-policy env is folded into the adapter's snapshot, so
    /// every reader (spawn resolution, providers, the v2 opt-in) sees it and
    /// it overrides the host value, like Node's `process.env` mutation.
    #[test]
    fn fa3_10_policy_env_reaches_adapter_readers() {
        let host = EnvSnapshot::from_pairs([
            ("ANTHROPIC_BASE_URL", "https://host.example"),
            ("CLAUDE_AGENT_ACP_EXPERIMENTAL_V2", "0"),
        ])
        .with_managed_docs(Vec::new());
        let merged = host.with_vars([
            (
                "ANTHROPIC_BASE_URL".to_string(),
                "https://gateway.example".to_string(),
            ),
            ("ANTHROPIC_MODEL".to_string(), "claude-sonnet".to_string()),
            (
                "CLAUDE_CODE_EXECUTABLE".to_string(),
                "/opt/policy/claude".to_string(),
            ),
            (
                "CLAUDE_AGENT_ACP_EXPERIMENTAL_V2".to_string(),
                "1".to_string(),
            ),
        ]);

        assert_eq!(
            merged.get("ANTHROPIC_BASE_URL"),
            Some("https://gateway.example")
        );
        assert_eq!(merged.get("ANTHROPIC_MODEL"), Some("claude-sonnet"));
        assert_eq!(
            crate::agent::ServeOptions::from_snapshot(merged.clone())
                .claude_path
                .as_deref(),
            Some(std::path::Path::new("/opt/policy/claude"))
        );
        assert!(crate::v2::serve::is_experimental_v2_enabled(&merged));
    }
}
