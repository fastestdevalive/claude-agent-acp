//! Concurrency safety audit test (CONC-2).
//!
//! Verifies, by scanning every `src/**/*.rs` file line-wise, that:
//! 1. Production code imports/uses no `std::sync::{Mutex, RwLock}` and no
//!    `tokio::sync::{Mutex, RwLock, broadcast}` (the workspace clippy rule).
//! 2. No lock guard binding (`let guard = ....lock()` / `.read()` / `.write()`)
//!    is held across an `.await` before the guard goes out of scope.
//!
//! The scanner is exercised on synthetic snippets too, so it cannot silently
//! stop detecting what it claims (a passing run over the real tree is only
//! meaningful if the checker fails on a bad input).

use std::fs;
use std::path::{Path, PathBuf};

fn find_rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(find_rs_files(&path));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    files
}

/// Strip `//` line comments and `/* ... */` block comments (multi-line state in
/// `in_block`). Best effort: a `//` inside a string literal is stripped too, but
/// that only ever removes text, never adds a violation.
fn strip_comments(line: &str, in_block: &mut bool) -> String {
    let mut out = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if *in_block {
            if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                *in_block = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            break;
        }
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            *in_block = true;
            i += 2;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn identifiers(line: &str) -> Vec<&str> {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|token| !token.is_empty())
        .collect()
}

/// The lock type names of `std::sync`/`tokio::sync` this crate bans. `MutexGuard`
/// and the `RwLock*Guard` types are included: a helper that returns a guard is a
/// lock in the tree even when the acquisition is not on one line.
const STD_LOCK_TOKENS: [&str; 5] = [
    "Mutex",
    "RwLock",
    "MutexGuard",
    "RwLockReadGuard",
    "RwLockWriteGuard",
];
const TOKIO_LOCK_TOKENS: [&str; 8] = [
    "Mutex",
    "RwLock",
    "broadcast",
    "MutexGuard",
    "RwLockReadGuard",
    "RwLockWriteGuard",
    "OwnedMutexGuard",
    "OwnedRwLockWriteGuard",
];

fn is_lock_token(crate_name: &str, token: &str) -> bool {
    match crate_name {
        "std" => STD_LOCK_TOKENS.contains(&token),
        "tokio" => TOKIO_LOCK_TOKENS.contains(&token),
        _ => false,
    }
}

/// Every banned lock type import/use, in any `use`/brace/glob form (including a
/// multi-line `use a::{ ... };` and `use a::sync::*;` followed by a bare lock
/// type). Test modules are NOT skipped: the rule is a property of the whole
/// crate source.
fn banned_lock_types(file: &Path, content: &str) -> Vec<String> {
    let mut violations = Vec::new();
    let mut in_block_comment = false;
    // The crate of an open multi-line `use` statement.
    let mut open_use: Option<&'static str> = None;
    // A glob import of `std::sync::*` / `tokio::sync::*`: a bare lock type later
    // is that crate's.
    let mut glob_std_sync = false;
    let mut glob_tokio_sync = false;

    for (idx, raw) in content.lines().enumerate() {
        let line = strip_comments(raw, &mut in_block_comment);
        let tokens = identifiers(&line);
        let bare_lock = tokens
            .iter()
            .any(|token| is_lock_token("std", token) || is_lock_token("tokio", token));

        if let Some(crate_name) = open_use {
            let banned = tokens.iter().any(|token| is_lock_token(crate_name, token));
            if banned {
                violations.push(format!(
                    "{}:{}: banned lock type in a `use {crate_name}::sync` block",
                    file.display(),
                    idx + 1
                ));
            }
            if line.contains('*') {
                match crate_name {
                    "std" => glob_std_sync = true,
                    "tokio" => glob_tokio_sync = true,
                    _ => {}
                }
            }
            if line.contains(';') {
                open_use = None;
            }
            continue;
        }

        let std_line = line.contains("std::sync");
        let tokio_line = line.contains("tokio::sync");
        let std_banned = std_line && tokens.iter().any(|t| is_lock_token("std", t));
        let tokio_banned = tokio_line && tokens.iter().any(|t| is_lock_token("tokio", t));
        // A glob import makes a bare lock type later that crate's.
        let glob_banned =
            bare_lock && !std_line && !tokio_line && (glob_std_sync || glob_tokio_sync);
        if std_banned || tokio_banned || glob_banned {
            violations.push(format!(
                "{}:{}: banned lock type `{}`",
                file.display(),
                idx + 1,
                line.trim()
            ));
        }

        // A `use` that does not end on this line: keep scanning its braced list.
        if line.contains("use ") && !line.contains(';') {
            if std_line {
                open_use = Some("std");
            } else if tokio_line {
                open_use = Some("tokio");
            }
        }
        if line.contains("use ") && line.contains(';') {
            if std_line && line.contains('*') {
                glob_std_sync = true;
            }
            if tokio_line && line.contains('*') {
                glob_tokio_sync = true;
            }
        }
    }
    violations
}

/// Whether a file can hold a lock guard at all: a lock type name, or a glob
/// import of a sync module. A `.read()`/`.write()` on a non-lock receiver (a
/// plain method named `read`) is not a guard.
fn file_may_hold_guards(content: &str) -> bool {
    let mut in_block_comment = false;
    for raw in content.lines() {
        let line = strip_comments(raw, &mut in_block_comment);
        let tokens = identifiers(&line);
        // A lock type name, or a glob import of a sync module (whose bare lock
        // types the type scanner reports; this scan only needs to run).
        if tokens
            .iter()
            .any(|token| STD_LOCK_TOKENS.contains(token) || TOKIO_LOCK_TOKENS.contains(token))
        {
            return true;
        }
        if line.contains("use ")
            && line.contains('*')
            && (line.contains("std::sync") || line.contains("tokio::sync"))
        {
            return true;
        }
    }
    false
}

/// A call that returns a guard through a helper (`self.guard()`, `read_lock(&s)`,
/// `MutexGuard::new`): its name ends in `guard`/`lock` (or is exactly one), so the
/// binding is a guard even though no `.lock()` appears on its line.
fn calls_guard_helper(line: &str) -> bool {
    identifiers(line).into_iter().any(|token| {
        (token == "guard"
            || token == "lock"
            || token.ends_with("_lock")
            || token.ends_with("_guard")
            || token.ends_with("Guard"))
            && line.contains(&format!("{token}("))
    })
}

/// Lock guard bindings held across `.await` in the same block. Simple brace
/// tracking: the guard dies when its block closes (or on an explicit `drop(`).
///
/// The acquisition and the await may share a line (`let g = m.lock(); work().await;`),
/// which the old one-line rule skipped; a tokio `.lock().await` is the
/// acquisition itself, not a guard held across an await.
fn lock_across_await(file: &Path, content: &str) -> Vec<String> {
    if !file_may_hold_guards(content) {
        return Vec::new();
    }
    let mut violations = Vec::new();
    let mut in_block_comment = false;
    let mut depth: i32 = 0;
    let mut guard: Option<(usize, i32)> = None;

    for (idx, raw) in content.lines().enumerate() {
        let line = strip_comments(raw, &mut in_block_comment);

        // A closing brace on an earlier line may have ended the guard's block.
        if let Some((_, bind_depth)) = guard {
            if depth < bind_depth {
                guard = None;
            }
        }

        let acquires =
            line.contains(".lock()") || line.contains(".read()") || line.contains(".write()");
        let binding = line.contains("let ") && (acquires || calls_guard_helper(&line));
        let own_await = line.contains(".lock().await")
            || line.contains(".read().await")
            || line.contains(".write().await");
        // A guard acquired on this line and an `.await` on it (not the lock's own
        // await) is held across that await.
        if binding && line.contains(".await") && !own_await {
            violations.push(format!(
                "{}:{}: guard bound on the same line is held across .await",
                file.display(),
                idx + 1
            ));
        }
        if line.contains(".await") && !binding {
            if let Some((bind_line, _)) = guard {
                violations.push(format!(
                    "{}:{}: guard bound at line {bind_line} is held across .await",
                    file.display(),
                    idx + 1
                ));
            }
        }

        for ch in line.chars() {
            if ch == '{' {
                depth += 1;
            } else if ch == '}' {
                depth -= 1;
            }
        }

        if binding {
            // A `.lock().await` acquires the guard as part of this line; the
            // guard is live from here on.
            guard = Some((idx + 1, depth));
        }
        if line.contains("drop(") {
            guard = None;
        }
    }
    violations
}

#[test]
fn test_no_banned_lock_types_in_src() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src_dir = manifest_dir.join("src");
    let files = find_rs_files(&src_dir);
    assert!(!files.is_empty(), "src/ should contain rust source files");

    let mut violations = Vec::new();
    for file in &files {
        let content = fs::read_to_string(file).expect("read file");
        violations.extend(banned_lock_types(file, &content));
    }

    assert!(
        violations.is_empty(),
        "Found banned std/tokio lock types in production source:\n{}",
        violations.join("\n")
    );
}

#[test]
fn test_no_lock_across_await_in_src() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src_dir = manifest_dir.join("src");
    let files = find_rs_files(&src_dir);

    let mut violations = Vec::new();
    for file in &files {
        let content = fs::read_to_string(file).expect("read file");
        violations.extend(lock_across_await(file, &content));
    }

    assert!(
        violations.is_empty(),
        "Found lock guards held across .await:\n{}",
        violations.join("\n")
    );
}

#[test]
fn checker_flags_banned_lock_types() {
    let file = Path::new("synthetic.rs");
    for (snippet, reason) in [
        ("use std::sync::Mutex;", "std::sync::Mutex import"),
        ("use std::sync::{Arc, Mutex};", "braced std::sync import"),
        ("use std::sync::RwLock;", "std::sync::RwLock import"),
        ("use tokio::sync::Mutex;", "tokio::sync::Mutex import"),
        (
            "use tokio::sync::{mpsc, RwLock};",
            "braced tokio::sync import",
        ),
        (
            "use tokio::sync::broadcast;",
            "tokio::sync::broadcast import",
        ),
        ("let state = std::sync::Mutex::new(0);", "qualified Mutex"),
        (
            "use tokio::sync::{\n    RwLock,\n};",
            "multi-line braced import",
        ),
        (
            "use std::sync::*;\nfn f() { let m = Mutex::new(0); }",
            "glob import then a bare Mutex",
        ),
        (
            "use tokio::sync::*;\nfn f() { let m = RwLock::new(0); }",
            "glob import then a bare RwLock",
        ),
        (
            "fn guard(&self) -> std::sync::MutexGuard<'_, u8> { todo!() }",
            "a helper returning a MutexGuard",
        ),
        (
            "fn guard(&self) -> std::sync::RwLockReadGuard<'_, u8> { todo!() }",
            "a helper returning an RwLockReadGuard",
        ),
    ] {
        assert!(
            !banned_lock_types(file, snippet).is_empty(),
            "the checker must flag {reason}: {snippet:?}"
        );
    }
}

#[test]
fn checker_flags_lock_guard_across_await() {
    let file = Path::new("synthetic.rs");
    let bad = r#"
use std::sync::Mutex;
async fn bad() {
    let guard = state.lock();
    do_work().await;
    drop(guard);
}
"#;
    assert!(
        !lock_across_await(file, bad).is_empty(),
        "the checker must flag a guard held across .await"
    );

    let bad_tokio = r#"
use tokio::sync::Mutex;
async fn bad() {
    let guard = state.lock().await;
    do_work().await;
}
"#;
    assert!(
        !lock_across_await(file, bad_tokio).is_empty(),
        "the checker must flag a tokio guard held across a later .await"
    );

    // The acquisition and the await share one line: the old rule skipped it.
    let same_line = r#"
use std::sync::Mutex;
async fn bad() {
    let guard = state.lock(); do_work().await;
}
"#;
    assert!(
        !lock_across_await(file, same_line).is_empty(),
        "the checker must flag a guard acquired and awaited on the same line"
    );

    // A guard obtained through a helper, with the lock type elsewhere in the file.
    let helper = r#"
fn guard(&self) -> MutexGuard<'_, u8> { self.inner.lock() }
async fn bad() {
    let g = state.guard();
    do_work().await;
}
"#;
    assert!(
        !lock_across_await(file, helper).is_empty(),
        "the checker must flag a guard obtained through a helper"
    );
}

#[test]
fn checker_accepts_clean_snippets() {
    let file = Path::new("synthetic.rs");
    let clean = r#"
use tokio::sync::mpsc;
use std::sync::atomic::AtomicBool;

async fn fine() {
    {
        let guard = state.lock();
        touch(guard);
    }
    do_work().await;
}

// std::sync::Mutex is mentioned only in a comment.
async fn also_fine() {
    {
        let value = state.read();
        consume(value);
    }
    do_work().await;
}
"#;
    assert!(
        banned_lock_types(file, clean).is_empty(),
        "clean imports must pass"
    );
    assert!(
        lock_across_await(file, clean).is_empty(),
        "a guard dropped before .await must pass"
    );

    // A plain method named `read` on a non-lock type is not a guard (the old
    // scanner armed a guard on any `let x = foo.read()`).
    let plain_read = r#"
fn snapshot(&self) -> Snapshot {
    let snapshot = by_session(self.read());
    snapshot
}
async fn later(&self) {
    let snapshot = by_session(self.read());
    do_work().await;
    consume(snapshot);
}
"#;
    assert!(
        lock_across_await(file, plain_read).is_empty(),
        "a plain `read()` method must not arm a lock guard"
    );
}
