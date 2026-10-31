use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

/// A snapshot of environment variables used by the adapter.
///
/// In production, this is captured once from the real process environment
/// (`EnvSnapshot::from_process()`). In tests and in-process scenarios, a hermetic
/// snapshot (`EnvSnapshot::empty()` or configured snapshot) isolates the adapter
/// from host process environment pollution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvSnapshot {
    vars: Arc<HashMap<String, String>>,
}

impl EnvSnapshot {
    /// Capture snapshot from current process environment.
    pub fn from_process() -> Self {
        Self {
            vars: Arc::new(std::env::vars().collect()),
        }
    }

    /// Empty, hermetic environment snapshot.
    pub fn empty() -> Self {
        Self {
            vars: Arc::new(HashMap::new()),
        }
    }

    /// Create from key-value pairs.
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
        }
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
