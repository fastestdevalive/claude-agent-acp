//! Where Claude Code keeps the transcripts of a working directory (upstream
//! `session-index/project-dirs.ts`).
//!
//! The CLI stores the transcripts of `cwd` in `<config>/projects/<encoded cwd>/`.
//! The encoding replaces every character that is not ASCII alphanumeric with
//! `-` (once per UTF-16 unit, as the SDK's `replace(/[^a-zA-Z0-9]/g, "-")`).
//! A name longer than [`MAX_SANITIZED_LENGTH`] is cut and gets a hash suffix; the
//! CLI (Bun) and the SDK (Node) compute different hashes, so a long name is
//! matched by its prefix. The encoding is lossy, so a directory name is never
//! decoded.
//!
//! Upstream branches on `process.platform`; here the platform is a parameter
//! ([`Platform`]) so the rules are plain string logic and tests choose the host.

use std::path::PathBuf;

use crate::env_snapshot::EnvSnapshot;

/// The SDK cuts an encoded path at this length and appends a hash.
pub const MAX_SANITIZED_LENGTH: usize = 200;

/// The host rules that `process.platform` selects upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// `darwin`: paths are NFC, compared ignoring case.
    Darwin,
    /// `win32`: names and paths are compared ignoring case.
    Win32,
    /// Every other platform: case-sensitive.
    Other,
}

impl Platform {
    /// The platform this binary runs on.
    pub fn host() -> Self {
        match std::env::consts::OS {
            "macos" => Self::Darwin,
            "windows" => Self::Win32,
            _ => Self::Other,
        }
    }

    /// Paths compare ignoring case (`CASE_INSENSITIVE_PATHS`).
    pub fn case_insensitive_paths(self) -> bool {
        matches!(self, Self::Darwin | Self::Win32)
    }
}

/// Whether `value` is a Claude session id (a UUID, any hex case). Only such ids
/// name a transcript, and only such ids are safe to use as a file name.
pub fn is_session_id(value: &str) -> bool {
    crate::session_store::is_session_id(value)
}

/// `<config>/projects`, normalized NFC like `projectsRoot`.
pub fn projects_root(env: &EnvSnapshot) -> PathBuf {
    let root = crate::session_store::projects_root_with_env(env);
    match root.to_str() {
        Some(text) => PathBuf::from(super::nfc::nfc(text)),
        None => root,
    }
}

/// `sanitize`: every UTF-16 unit that is not ASCII alphanumeric becomes `-`.
pub fn sanitize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            for _ in 0..c.len_utf16() {
                out.push('-');
            }
        }
    }
    out
}

/// The SDK's 32-bit string hash, rendered as `Math.abs(hash).toString(36)`.
pub fn hash_base36(value: &str) -> String {
    let mut hash: i32 = 0;
    for unit in value.encode_utf16() {
        hash = hash
            .wrapping_shl(5)
            .wrapping_sub(hash)
            .wrapping_add(i32::from(unit));
    }
    let mut n = i64::from(hash).unsigned_abs();
    if n == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::new();
    while n > 0 {
        digits.push(char::from_digit((n % 36) as u32, 36).unwrap_or('0'));
        n /= 36;
    }
    digits.iter().rev().collect()
}

/// NFC on darwin, as the SDK normalizes paths there; other platforms pass the
/// value through (`normalizePath`).
pub fn normalize_path(value: &str, platform: Platform) -> String {
    if platform == Platform::Darwin {
        super::nfc::nfc(value)
    } else {
        value.to_string()
    }
}

fn sanitized_length(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

/// The project directory name of `cwd`, exactly as the SDK computes it.
pub fn encode_project_path(cwd: &str, platform: Platform) -> String {
    let normalized = normalize_path(cwd, platform);
    let sanitized = sanitize(&normalized);
    if sanitized_length(&sanitized) <= MAX_SANITIZED_LENGTH {
        return sanitized;
    }
    format!(
        "{}-{}",
        &sanitized[..MAX_SANITIZED_LENGTH],
        hash_base36(&normalized)
    )
}

fn same_name(a: &str, b: &str, platform: Platform) -> bool {
    if platform == Platform::Win32 {
        a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

/// Whether `dir_name` is the project directory of `cwd`: the exact encoding,
/// or, for a long path, a directory with the same cut prefix and any hash
/// suffix.
pub fn project_dir_matches(dir_name: &str, cwd: &str, platform: Platform) -> bool {
    if same_name(dir_name, &encode_project_path(cwd, platform), platform) {
        return true;
    }
    let sanitized = sanitize(&normalize_path(cwd, platform));
    if sanitized_length(&sanitized) <= MAX_SANITIZED_LENGTH {
        return false;
    }
    let prefix = format!("{}-", &sanitized[..MAX_SANITIZED_LENGTH]);
    if platform == Platform::Win32 {
        dir_name.to_lowercase().starts_with(&prefix.to_lowercase())
    } else {
        dir_name.starts_with(&prefix)
    }
}

/// Whether a transcript's `cwd` belongs to `project_path`, compared the way the
/// SDK does: the full sanitized forms, ignoring case where the file system does.
pub fn same_project_path(cwd: &str, project_path: &str, platform: Platform) -> bool {
    let a = sanitize(&normalize_path(cwd, platform));
    let b = sanitize(&normalize_path(project_path, platform));
    if platform.case_insensitive_paths() {
        a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

/// Whether `dir_name` is the exact project directory of `cwd`, as the SDK finds
/// it by `dir`: the exact encoding, on macOS in any case (the file system
/// ignores it).
pub fn is_exact_project_dir(dir_name: &str, cwd: &str, platform: Platform) -> bool {
    let exact = encode_project_path(cwd, platform);
    if same_name(dir_name, &exact, platform) {
        return true;
    }
    platform == Platform::Darwin && dir_name.to_lowercase() == exact.to_lowercase()
}

/// A real path as Node's `fs.realpath` reports it: `std::fs::canonicalize`
/// returns a verbatim `\\?\C:\...` path on Windows, where Node returns
/// `C:\...` (`\\?\UNC\server\share` is `\\server\share`). Pure string logic:
/// a non-Windows path has no such prefix.
fn strip_verbatim(path: String) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match path.strip_prefix(r"\\?\") {
        Some(rest) => rest.to_string(),
        None => path,
    }
}

/// The real path of `cwd`, like the SDK resolves it before encoding. Falls back
/// to `cwd` when it does not exist.
pub fn canonical_path(cwd: &str, platform: Platform) -> String {
    match std::fs::canonicalize(cwd) {
        Ok(real) => real_path_string(&real.to_string_lossy(), platform),
        Err(_) => normalize_path(cwd, platform),
    }
}

/// A path `std::fs::canonicalize` returned, as `canonical_path` compares it: the
/// Windows verbatim prefix (`\\?\`) is gone, then normalised.
pub fn real_path_string(real: &str, platform: Platform) -> String {
    normalize_path(&strip_verbatim(real.to_string()), platform)
}

/// The names of the project directories of `cwd` among `root_entries`.
pub fn project_dirs_of(cwd: &str, root_entries: &[String], platform: Platform) -> Vec<String> {
    let exact = encode_project_path(cwd, platform);
    let length = sanitized_length(&sanitize(&normalize_path(cwd, platform)));
    if length <= MAX_SANITIZED_LENGTH {
        return root_entries
            .iter()
            .filter(|name| same_name(name, &exact, platform))
            .cloned()
            .collect();
    }
    root_entries
        .iter()
        .filter(|name| project_dir_matches(name, cwd, platform))
        .cloned()
        .collect()
}

/// `path.dirname` of a POSIX or (with `win32`) a Windows path.
pub fn dirname(value: &str, win32: bool) -> String {
    let is_sep = |c: char| c == '/' || (win32 && c == '\\');
    let trimmed = value.trim_end_matches(is_sep);
    if trimmed.is_empty() {
        // Only separators (or empty): the root, else ".".
        return if value.is_empty() {
            ".".into()
        } else {
            value[..1].to_string()
        };
    }
    match trimmed.rfind(is_sep) {
        None => {
            if win32 && trimmed.len() == 2 && trimmed.ends_with(':') && value.len() > 2 {
                // `C:\`: the drive root is its own parent.
                return value[..3].to_string();
            }
            if win32 && trimmed.len() >= 2 && trimmed.as_bytes()[1] == b':' {
                return trimmed[..2].to_string();
            }
            ".".into()
        }
        Some(at) => {
            let head = trimmed[..at].trim_end_matches(is_sep);
            if head.is_empty() {
                value[..1].to_string()
            } else if win32 && head.len() == 2 && head.ends_with(':') {
                format!("{head}{}", &trimmed[at..=at])
            } else {
                head.to_string()
            }
        }
    }
}

/// `value` and each of its ancestors, nearest first.
pub fn path_and_ancestors(value: &str, platform: Platform) -> Vec<String> {
    let win32 = platform == Platform::Win32;
    let mut result = Vec::new();
    let mut current = value.to_string();
    loop {
        result.push(current.clone());
        let parent = dirname(&current, win32);
        if parent == current {
            return result;
        }
        current = parent;
    }
}

/// Whether `value` is an absolute path (`path.isAbsolute`).
pub fn is_absolute(value: &str, platform: Platform) -> bool {
    if value.starts_with('/') {
        return true;
    }
    if platform == Platform::Win32 {
        let bytes = value.as_bytes();
        return value.starts_with('\\')
            || (bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && (bytes[2] == b'\\' || bytes[2] == b'/'));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: Platform = Platform::Other;

    /// project directory encoding > matches the SDK encoding and never decodes
    #[test]
    fn encoding_matches_the_sdk_and_never_decodes() {
        assert_eq!(
            encode_project_path("/Users/me/my.project", P),
            "-Users-me-my-project"
        );
        // `/a/b-c` and `/a/b/c` share a directory.
        assert_eq!(
            encode_project_path("/a/b-c", P),
            encode_project_path("/a/b/c", P)
        );
        // One dash per UTF-16 unit: an astral character is two.
        assert_eq!(encode_project_path("/a\u{1f600}", P), "-a--");
        assert_eq!(encode_project_path("/\u{e9}", P), "--");
    }

    /// project directory encoding > matches a long path by its cut prefix, whatever the hash
    #[test]
    fn a_long_path_is_matched_by_its_cut_prefix() {
        let long = format!("/{}", "a".repeat(250));
        let encoded = encode_project_path(&long, P);
        assert!(encoded.starts_with(&format!("-{}-", "a".repeat(199))));
        assert_eq!(
            encoded.len(),
            MAX_SANITIZED_LENGTH + 1 + hash_base36(&long).len()
        );
        let other_hash = format!("-{}-zzz", "a".repeat(199));
        assert!(project_dir_matches(&encoded, &long, P));
        assert!(project_dir_matches(&other_hash, &long, P));
        assert!(!project_dir_matches("-aaa", &long, P));
        // A short path is matched by its exact encoding only.
        assert!(!project_dir_matches("-a-b-extra", "/a/b", P));
        assert_eq!(
            project_dirs_of(&long, &[encoded.clone(), other_hash.clone(), "x".into()], P),
            vec![encoded, other_hash]
        );
    }

    /// project directory encoding > lists a path and its ancestors
    #[test]
    fn lists_a_path_and_its_ancestors() {
        assert_eq!(
            path_and_ancestors("/a/b/c", P),
            vec!["/a/b/c", "/a/b", "/a", "/"]
        );
        assert_eq!(path_and_ancestors("/", P), vec!["/"]);
        assert_eq!(
            path_and_ancestors("C:\\a\\b", Platform::Win32),
            vec!["C:\\a\\b", "C:\\a", "C:\\"]
        );
    }

    /// project directory encoding > accepts only UUID session ids
    #[test]
    fn accepts_only_uuid_session_ids() {
        assert!(is_session_id("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"));
        assert!(is_session_id("AAAAAAAA-aaaa-4aaa-8aaa-aaaaaaaaaaaa"));
        assert!(!is_session_id("../etc/passwd"));
        assert!(!is_session_id("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaa"));
    }

    /// canonicalPath > reports a real path as Node's fs.realpath does, without a Windows verbatim prefix
    #[test]
    fn strips_the_windows_verbatim_prefix() {
        assert_eq!(strip_verbatim(r"\\?\C:\repo".into()), r"C:\repo");
        assert_eq!(
            strip_verbatim(r"\\?\UNC\server\share".into()),
            r"\\server\share"
        );
        assert_eq!(strip_verbatim(r"C:\repo".into()), r"C:\repo");
        assert_eq!(strip_verbatim("/Users/me/repo".into()), "/Users/me/repo");
        // The project directory is Node's `C--repo`, not `\\?\C:...`.
        assert_eq!(
            encode_project_path(&strip_verbatim(r"\\?\C:\repo".into()), Platform::Win32),
            "C--repo"
        );
    }

    #[test]
    fn case_rules_follow_the_platform() {
        assert!(same_project_path("/A/b", "/a/B", Platform::Darwin));
        assert!(same_project_path("/A/b", "/a/B", Platform::Win32));
        assert!(!same_project_path("/A/b", "/a/B", P));
        assert!(is_exact_project_dir(
            "-Users-Me",
            "/users/me",
            Platform::Darwin
        ));
        assert!(!is_exact_project_dir("-Users-Me", "/users/me", P));
        assert!(project_dir_matches(
            "-USERS-ME",
            "/users/me",
            Platform::Win32
        ));
        assert!(!project_dir_matches(
            "-USERS-ME",
            "/users/me",
            Platform::Darwin
        ));
    }

    /// session list > a canonicalised Windows path compares without its verbatim prefix
    #[test]
    fn a_real_path_loses_its_verbatim_prefix() {
        assert_eq!(
            real_path_string(r"\\?\C:\Users\me\repo", Platform::Win32),
            normalize_path(r"C:\Users\me\repo", Platform::Win32)
        );
        assert_eq!(
            real_path_string(r"\\?\UNC\server\share\repo", Platform::Win32),
            normalize_path(r"\\server\share\repo", Platform::Win32)
        );
        assert_eq!(real_path_string("/tmp/repo", Platform::Other), "/tmp/repo");
    }

    /// NFC normalisation > darwin composes a decomposed path before encoding it
    #[test]
    fn darwin_composes_a_decomposed_path() {
        let decomposed = "/Users/me/re\u{301}po";
        let composed = "/Users/me/r\u{e9}po";
        assert_eq!(normalize_path(decomposed, Platform::Darwin), composed);
        assert_eq!(normalize_path(decomposed, Platform::Other), decomposed);
        // The encoding is lossy and counts UTF-16 units: the composed letter
        // becomes one dash, the decomposed letter keeps its base and adds one.
        assert_eq!(
            encode_project_path(decomposed, Platform::Other),
            "-Users-me-re-po"
        );
        assert_eq!(
            encode_project_path(decomposed, Platform::Darwin),
            "-Users-me-r-po"
        );
        assert_eq!(
            encode_project_path(composed, Platform::Darwin),
            "-Users-me-r-po"
        );
        // A directory name the CLI wrote from the composed path is found.
        assert!(project_dir_matches(
            "-Users-me-r-po",
            decomposed,
            Platform::Darwin
        ));
        assert!(!project_dir_matches(
            "-Users-me-r-po",
            decomposed,
            Platform::Other
        ));
        assert!(same_project_path(decomposed, composed, Platform::Darwin));
        assert!(!same_project_path(decomposed, composed, Platform::Other));
        assert_eq!(
            project_dirs_of(decomposed, &["-Users-me-r-po".into()], Platform::Darwin),
            vec!["-Users-me-r-po".to_string()]
        );
    }

    /// NFC normalisation > the projects root is composed on every platform
    #[test]
    fn the_projects_root_is_composed() {
        let env = EnvSnapshot::from_pairs([("CLAUDE_CONFIG_DIR", "/tmp/re\u{301}po")]);
        assert_eq!(
            projects_root(&env),
            PathBuf::from("/tmp/r\u{e9}po/projects")
        );
        let env = EnvSnapshot::from_pairs([("CLAUDE_CONFIG_DIR", "/tmp/plain")]);
        assert_eq!(projects_root(&env), PathBuf::from("/tmp/plain/projects"));
    }
}
