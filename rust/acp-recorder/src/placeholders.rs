//! Placeholder expansion and corpus seeding (rust-air-gaps Decisions 1 and 3).
//!
//! Script strings may use `$SESSION` (the current session id), `$CWD` (the
//! canonical working directory) and `$CWD_SLUG` (the Claude project-dir slug of
//! `$CWD`: every non-alphanumeric char becomes `-`).

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde_json::Value;

use crate::script::SeedFile;

/// Values substituted for the placeholders.
#[derive(Debug, Clone)]
pub struct Placeholders {
    pub session: Option<String>,
    pub cwd: String,
    pub cwd_slug: String,
}

/// The Claude project-dir slug of `path`.
pub fn cwd_slug(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The canonical current directory (falls back to the plain one, then `/`).
pub fn canonical_cwd() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
    cwd.canonicalize().unwrap_or(cwd)
}

impl Placeholders {
    /// Placeholders for the current process directory and an optional session.
    pub fn new(session: Option<String>) -> Self {
        let cwd = canonical_cwd().to_string_lossy().into_owned();
        let cwd_slug = cwd_slug(&cwd);
        Self {
            session,
            cwd,
            cwd_slug,
        }
    }

    /// Expand the placeholders inside one string.
    pub fn expand_str(&self, text: &str) -> String {
        // `$CWD_SLUG` first: `$CWD` is its prefix.
        let mut out = text.replace("$CWD_SLUG", &self.cwd_slug);
        out = out.replace("$CWD", &self.cwd);
        if let Some(session) = &self.session {
            out = out.replace("$SESSION", session);
        }
        out
    }

    /// Expand the placeholders in every string value (not keys) of `value`.
    pub fn expand(&self, value: &Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.expand_str(s)),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.expand(v)).collect()),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), self.expand(v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// Write every seed file under `config_dir`. `corpus_dir` holds the sources.
pub fn seed(
    files: &[SeedFile],
    corpus_dir: &Path,
    config_dir: &Path,
    ph: &Placeholders,
) -> std::io::Result<()> {
    for file in files {
        let dest_rel = ph.expand_str(&file.path);
        if dest_rel.starts_with('/') || dest_rel.split('/').any(|part| part == "..") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("seed path `{dest_rel}` must stay under CLAUDE_CONFIG_DIR"),
            ));
        }
        let dest = config_dir.join(&dest_rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = ph.expand_str(&std::fs::read_to_string(corpus_dir.join(&file.from))?);
        std::fs::write(&dest, content)?;
        if let Some(secs) = file.mtime {
            let handle = std::fs::OpenOptions::new().write(true).open(&dest)?;
            handle.set_modified(UNIX_EPOCH + Duration::from_secs(secs))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_in_values_and_longest_name_first() {
        let ph = Placeholders {
            session: Some("S1".into()),
            cwd: "/a/b".into(),
            cwd_slug: "-a-b".into(),
        };
        let value = serde_json::json!({"k": ["$SESSION", "$CWD/x", "p/$CWD_SLUG/q.jsonl"], "n": 3});
        let out = ph.expand(&value);
        assert_eq!(
            out,
            serde_json::json!({"k": ["S1", "/a/b/x", "p/-a-b/q.jsonl"], "n": 3})
        );
    }

    #[test]
    fn slug_replaces_non_alphanumerics() {
        assert_eq!(cwd_slug("/Users/a.b/c_d"), "-Users-a-b-c-d");
    }

    #[test]
    fn seed_writes_expanded_files_with_mtime_and_rejects_escape() {
        let tmp = std::env::temp_dir().join(format!("acp-seed-{}", std::process::id()));
        let corpus = tmp.join("corpus");
        let config = tmp.join("config");
        std::fs::create_dir_all(&corpus).unwrap();
        std::fs::write(corpus.join("a.jsonl"), "{\"cwd\":\"$CWD\"}\n").unwrap();
        let ph = Placeholders {
            session: None,
            cwd: "/w".into(),
            cwd_slug: "-w".into(),
        };
        let good = SeedFile {
            path: "projects/$CWD_SLUG/a.jsonl".into(),
            from: "a.jsonl".into(),
            mtime: Some(1000),
        };
        seed(&[good], &corpus, &config, &ph).unwrap();
        let dest = config.join("projects/-w/a.jsonl");
        assert_eq!(
            std::fs::read_to_string(&dest).unwrap(),
            "{\"cwd\":\"/w\"}\n"
        );
        let modified = std::fs::metadata(&dest).unwrap().modified().unwrap();
        assert_eq!(modified, UNIX_EPOCH + Duration::from_secs(1000));
        let bad = SeedFile {
            path: "../escape".into(),
            from: "a.jsonl".into(),
            mtime: None,
        };
        assert!(seed(&[bad], &corpus, &config, &ph).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
