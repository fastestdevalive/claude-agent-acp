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

/// Every banned lock type import/use, in any `use`/brace form (including a
/// multi-line `use a::{ ... };`). Test modules are NOT skipped: the rule is a
/// property of the whole crate source.
fn banned_lock_types(file: &Path, content: &str) -> Vec<String> {
    let mut violations = Vec::new();
    let mut in_block_comment = false;
    // The crate of an open multi-line `use` statement.
    let mut open_use: Option<&'static str> = None;

    for (idx, raw) in content.lines().enumerate() {
        let line = strip_comments(raw, &mut in_block_comment);
        let tokens = identifiers(&line);
        let has = |token: &str| tokens.contains(&token);

        if let Some(crate_name) = open_use {
            let banned = match crate_name {
                "std" => has("Mutex") || has("RwLock"),
                "tokio" => has("Mutex") || has("RwLock") || has("broadcast"),
                _ => false,
            };
            if banned {
                violations.push(format!(
                    "{}:{}: banned lock type in a `use {crate_name}::sync` block",
                    file.display(),
                    idx + 1
                ));
            }
            if line.contains(';') {
                open_use = None;
            }
            continue;
        }

        let std_banned = line.contains("std::sync") && (has("Mutex") || has("RwLock"));
        let tokio_banned =
            line.contains("tokio::sync") && (has("Mutex") || has("RwLock") || has("broadcast"));
        if std_banned || tokio_banned {
            violations.push(format!(
                "{}:{}: banned lock type `{}`",
                file.display(),
                idx + 1,
                line.trim()
            ));
        }

        // A `use` that does not end on this line: keep scanning its braced list.
        if line.contains("use ") && !line.contains(';') {
            if line.contains("std::sync") {
                open_use = Some("std");
            } else if line.contains("tokio::sync") {
                open_use = Some("tokio");
            }
        }
    }

    violations
}

/// Lock guard bindings held across `.await` in the same block. Simple brace
/// tracking: the guard dies when its block closes (or on an explicit `drop(`).
fn lock_across_await(file: &Path, content: &str) -> Vec<String> {
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

        let binding = line.contains("let ")
            && (line.contains(".lock()") || line.contains(".read()") || line.contains(".write()"));
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
async fn bad() {
    let guard = state.lock().await;
    do_work().await;
}
"#;
    assert!(
        !lock_across_await(file, bad_tokio).is_empty(),
        "the checker must flag a tokio guard held across a later .await"
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
}
