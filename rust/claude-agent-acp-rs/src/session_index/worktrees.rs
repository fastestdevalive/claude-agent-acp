//! The git worktrees of a working directory, read from the repository files
//! (upstream `session-index/worktrees.ts`).
//!
//! The SDK `listSessions({ dir })` includes the sessions of every worktree of
//! the repository and runs `git worktree list` for that on each call. This
//! module reads the same list without spawning git: the main worktree is the
//! parent of the common `.git` directory, and each linked worktree is named by
//! `<common-dir>/worktrees/<name>/gitdir`. A worktree whose directory no longer
//! exists is dropped. Nothing is cached: `git worktree move` rewrites a `gitdir`
//! file without touching the `worktrees` directory.

use std::path::{Component, Path, PathBuf};

use super::project_dirs::{normalize_path, Platform};

/// `path.resolve(value)`: absolute and lexically normalized.
pub fn resolve(base: &Path, value: &Path) -> PathBuf {
    let joined = if value.is_absolute() {
        value.to_path_buf()
    } else {
        base.join(value)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

fn cwd_resolve(value: &str) -> PathBuf {
    let base = std::env::current_dir().unwrap_or_default();
    resolve(&base, Path::new(value))
}

/// The git dir of the repository that contains `cwd`, or `None`.
fn find_git_dir(cwd: &str) -> Option<PathBuf> {
    let mut current = cwd_resolve(cwd);
    loop {
        let candidate = current.join(".git");
        if let Ok(meta) = std::fs::metadata(&candidate) {
            if meta.is_dir() {
                return Some(candidate);
            }
            if meta.is_file() {
                // An unreadable `.git` file is not a repository.
                let text = std::fs::read_to_string(&candidate).ok()?;
                return text.lines().find_map(|line| {
                    let value = line.strip_prefix("gitdir:")?.trim();
                    (!value.is_empty()).then(|| resolve(&current, Path::new(value)))
                });
            }
        }
        let parent = current.parent()?.to_path_buf();
        current = parent;
    }
}

fn common_dir_of(git_dir: &Path) -> PathBuf {
    if let Ok(text) = std::fs::read_to_string(git_dir.join("commondir")) {
        let value = text.trim();
        if !value.is_empty() {
            return resolve(git_dir, Path::new(value));
        }
    }
    git_dir.to_path_buf()
}

fn linked_worktrees(common_dir: &Path) -> Vec<PathBuf> {
    let worktrees_dir = common_dir.join("worktrees");
    let Ok(read) = std::fs::read_dir(&worktrees_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
        .iter()
        .filter_map(|name| {
            let text = std::fs::read_to_string(worktrees_dir.join(name).join("gitdir")).ok()?;
            let git_file = text.trim();
            if git_file.is_empty() {
                return None;
            }
            let resolved = resolve(&worktrees_dir.join(name), Path::new(git_file));
            resolved.parent().map(Path::to_path_buf)
        })
        .collect()
}

/// The root of the worktree that contains `cwd`: the directory of its `.git` (a
/// directory in the main worktree, a file in a linked one).
fn worktree_root_of(cwd: &str) -> Option<PathBuf> {
    let mut current = cwd_resolve(cwd);
    loop {
        if std::fs::metadata(current.join(".git")).is_ok() {
            return Some(current);
        }
        let parent = current.parent()?.to_path_buf();
        current = parent;
    }
}

fn existing_dirs(candidates: Vec<PathBuf>, platform: Platform) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for candidate in candidates {
        if candidate.is_dir() {
            let value = normalize_path(&candidate.to_string_lossy(), platform);
            if !out.contains(&value) {
                out.push(value);
            }
        }
    }
    out
}

/// The same subdirectory as `cwd` in every existing worktree of its repository
/// (the main one and the linked ones), as the Codex TUI expands a cwd: for
/// `/repo/packages/a`, `/wt1/packages/a` if it exists. For a cwd at a worktree
/// root, the worktree roots. Empty outside a repository.
pub fn worktree_counterparts(cwd: &str, platform: Platform) -> Vec<String> {
    let Some(root) = worktree_root_of(cwd) else {
        return Vec::new();
    };
    let resolved = cwd_resolve(cwd);
    let relative = resolved
        .strip_prefix(&root)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let candidates = repository_worktrees_paths(cwd)
        .into_iter()
        .map(|worktree| {
            if relative.as_os_str().is_empty() {
                worktree
            } else {
                worktree.join(&relative)
            }
        })
        .collect();
    existing_dirs(candidates, platform)
}

/// Whether `cwd` lies in a linked worktree: its git dir is
/// `<common-dir>/worktrees/<name>`. False in the main worktree, in a submodule
/// and outside a repository.
pub fn is_in_linked_worktree(cwd: &str) -> bool {
    let Some(git_dir) = find_git_dir(cwd) else {
        return false;
    };
    let common = common_dir_of(&git_dir);
    let parent = git_dir.parent().map(Path::to_path_buf).unwrap_or_default();
    resolve(Path::new("/"), &parent) == resolve(&common, Path::new("worktrees"))
}

fn repository_worktrees_paths(cwd: &str) -> Vec<PathBuf> {
    let Some(git_dir) = find_git_dir(cwd) else {
        return Vec::new();
    };
    let common = common_dir_of(&git_dir);
    let mut candidates = Vec::new();
    if common.file_name().is_some_and(|name| name == ".git") {
        if let Some(parent) = common.parent() {
            candidates.push(parent.to_path_buf());
        }
    }
    candidates.extend(linked_worktrees(&common));
    candidates
}

/// Every existing worktree of the repository that contains `cwd`: the main
/// worktree and the linked ones. Empty outside a repository.
pub fn repository_worktrees(cwd: &str, platform: Platform) -> Vec<String> {
    existing_dirs(repository_worktrees_paths(cwd), platform)
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: Platform = Platform::Other;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acp-t2b-wt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    /// A main worktree `main`, linked worktrees `wt1` and `gone` (directory
    /// missing) and the `.git/worktrees/<name>/gitdir` files that name them.
    fn repo(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = temp_dir(tag);
        let main = base.join("main");
        let wt1 = base.join("wt1");
        std::fs::create_dir_all(main.join(".git/worktrees/wt1")).expect("git dir");
        std::fs::create_dir_all(main.join(".git/worktrees/gone")).expect("git dir");
        std::fs::create_dir_all(main.join("pkg/a")).expect("pkg");
        std::fs::create_dir_all(wt1.join("pkg/a")).expect("wt1");
        std::fs::write(
            main.join(".git/worktrees/wt1/gitdir"),
            format!("{}\n", wt1.join(".git").display()),
        )
        .expect("gitdir");
        std::fs::write(main.join(".git/worktrees/wt1/commondir"), "../..\n").expect("commondir");
        std::fs::write(
            main.join(".git/worktrees/gone/gitdir"),
            format!("{}\n", base.join("gone/.git").display()),
        )
        .expect("gitdir");
        std::fs::write(
            wt1.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/wt1").display()),
        )
        .expect("dot git");
        (base, main, wt1)
    }

    #[test]
    fn lists_the_main_and_the_existing_linked_worktrees() {
        let (base, main, wt1) = repo("list");
        let from_linked = repository_worktrees(&wt1.to_string_lossy(), P);
        let from_main = repository_worktrees(&main.join("pkg/a").to_string_lossy(), P);
        let expected = vec![
            main.to_string_lossy().into_owned(),
            wt1.to_string_lossy().into_owned(),
        ];
        assert_eq!(from_linked, expected, "the missing worktree is dropped");
        assert_eq!(from_main, expected);
        assert!(repository_worktrees(&base.to_string_lossy(), P).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn expands_a_subdirectory_into_every_worktree() {
        let (base, main, wt1) = repo("counterparts");
        let counterparts = worktree_counterparts(&main.join("pkg/a").to_string_lossy(), P);
        assert_eq!(
            counterparts,
            vec![
                main.join("pkg/a").to_string_lossy().into_owned(),
                wt1.join("pkg/a").to_string_lossy().into_owned()
            ]
        );
        // A subdirectory missing in a worktree is left out.
        std::fs::create_dir_all(main.join("only-main")).expect("dir");
        assert_eq!(
            worktree_counterparts(&main.join("only-main").to_string_lossy(), P),
            vec![main.join("only-main").to_string_lossy().into_owned()]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn tells_a_linked_worktree_from_the_main_one() {
        let (base, main, wt1) = repo("linked");
        assert!(is_in_linked_worktree(&wt1.to_string_lossy()));
        assert!(is_in_linked_worktree(&wt1.join("pkg/a").to_string_lossy()));
        assert!(!is_in_linked_worktree(&main.to_string_lossy()));
        assert!(!is_in_linked_worktree(&base.to_string_lossy()));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// worktrees after git worktree move > follows a moved linked worktree: the
    /// gitdir file is rewritten, the directory keeps its mtime.
    #[test]
    fn t8d_follows_a_moved_linked_worktree() {
        let (base, main, wt1) = repo("moved");
        let from_main = main.to_string_lossy().into_owned();
        assert_eq!(
            repository_worktrees(&from_main, P),
            vec![from_main.clone(), wt1.to_string_lossy().into_owned()]
        );
        let moved = base.join("wt2");
        std::fs::rename(&wt1, &moved).expect("move");
        std::fs::write(
            main.join(".git/worktrees/wt1/gitdir"),
            format!("{}\n", moved.join(".git").display()),
        )
        .expect("gitdir");
        assert_eq!(
            repository_worktrees(&from_main, P),
            vec![from_main, moved.to_string_lossy().into_owned()]
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
