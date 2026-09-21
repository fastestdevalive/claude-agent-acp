//! Phase-2 integration tests (2.T1, 2.T4, 2.T5, 2.T6).
//!
//! These verify the capture toolchain and the committed fixtures, not any one
//! crate's library. They locate the fork root from the manifest dir and shell
//! out where the scenario requires it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The fork root (parent of `rust/`).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("repo root resolves")
}

/// 2.T1 — `capture.sh` run twice yields byte-identical `text-only.frames.jsonl`.
///
/// The normalised frames carry no volatile data (ids/uuids/timestamps are
/// replaced by placeholders), so a second capture must be byte-for-byte equal.
#[test]
fn capture_is_deterministic_across_runs() {
    let root = repo_root();
    let fixture = root.join("porting/fixtures/text-only.frames.jsonl");

    let first = std::fs::read_to_string(&fixture).expect("first text-only fixture exists");

    let status = Command::new("bash")
        .arg("porting/capture.sh")
        .arg("text-only")
        .current_dir(&root)
        .status()
        .expect("spawn capture.sh");
    assert!(
        status.success(),
        "capture.sh text-only must exit 0 (is Node + the fake built?)"
    );

    let second = std::fs::read_to_string(&fixture).expect("second text-only fixture exists");
    assert_eq!(
        first, second,
        "capture.sh run twice must yield byte-identical normalised frames (2.T1)"
    );
}

/// 2.T4 — the union of `porting/fixtures/*.argv.json` covers every exercised
/// B2 row (the flags/env the port must reproduce in phase 3).
#[test]
fn argv_union_covers_b2_rows() {
    let root = repo_root();
    let fixtures = root.join("porting/fixtures");
    let mut argv_flags: Vec<String> = Vec::new();
    let mut env_seen: Vec<String> = Vec::new();

    for entry in std::fs::read_dir(&fixtures).expect("fixtures dir") {
        let path = entry.expect("entry").path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".argv.json") {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for flag in value["argv"].as_array().unwrap() {
            if let Some(s) = flag.as_str() {
                argv_flags.push(s.to_string());
            }
        }
        for (k, _v) in value["env"].as_object().unwrap() {
            env_seen.push(k.clone());
        }
    }

    let joined = argv_flags.join("\n");
    let has = |needle: &str| {
        argv_flags
            .iter()
            .any(|f| f == needle || f.starts_with(needle))
    };

    assert!(has("--output-format"), "B2: --output-format\n{joined}");
    assert!(has("stream-json"), "B2: stream-json\n{joined}");
    assert!(has("--verbose"), "B2: --verbose\n{joined}");
    assert!(has("--input-format"), "B2: --input-format\n{joined}");
    assert!(
        has("--replay-user-messages"),
        "B2: --replay-user-messages\n{joined}"
    );
    assert!(
        has("--include-partial-messages"),
        "B2: --include-partial-messages\n{joined}"
    );
    assert!(
        has("--permission-prompt-tool"),
        "B2: --permission-prompt-tool\n{joined}"
    );
    assert!(
        argv_flags
            .iter()
            .any(|f| f.starts_with("--setting-sources=")),
        "B2: --setting-sources=user,project,local\n{joined}"
    );
    assert!(
        argv_flags.iter().any(|f| f.starts_with("--session-id=")),
        "B2: --session-id=<uuid> (non-resume session)\n{joined}"
    );
    assert!(
        argv_flags
            .iter()
            .any(|f| f.starts_with("--resume=") || f == "--resume"),
        "B2: --resume <id> (resume/load)\n{joined}"
    );
    assert!(has("--permission-mode"), "B2: --permission-mode\n{joined}");

    assert!(
        env_seen.iter().any(|k| k == "CLAUDE_CODE_ENTRYPOINT"),
        "B2: env CLAUDE_CODE_ENTRYPOINT set"
    );
    assert!(
        env_seen
            .iter()
            .any(|k| k == "CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS"),
        "B2: env CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1"
    );
    assert!(
        !env_seen.iter().any(|k| k == "NODE_OPTIONS"),
        "B2: env NODE_OPTIONS must be deleted"
    );
}

/// 2.T5 — Node's text-only capture settles with `stopReason: end_turn`, not a
/// force-cancel timeout.
#[test]
fn text_only_settles_with_end_turn() {
    let root = repo_root();
    let fixture = root.join("porting/fixtures/text-only.frames.jsonl");
    let text = std::fs::read_to_string(&fixture).expect("text-only fixture exists");

    let last_line = text.lines().last().expect("at least one frame");
    let frame: serde_json::Value = serde_json::from_str(last_line).unwrap();
    assert_eq!(frame["frame"]["result"]["stopReason"], "end_turn");
}

/// 2.T6 — Regression: the G7 guard (upstream files untouched) prints nothing.
#[test]
fn g7_upstream_untouched() {
    let root = repo_root();
    let output = Command::new("git")
        .args([
            "diff",
            "--stat",
            "v0.70.0",
            "--",
            ".",
            ":!rust",
            ":!porting",
            ":!skills",
            ":!.vibekit",
            ":!.gitignore",
        ])
        .current_dir(&root)
        .output()
        .expect("run git diff");
    assert!(
        output.status.success(),
        "git diff --stat v0.70.0 must succeed"
    );
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        stdout.is_empty(),
        "G7: upstream files must be untouched, but git reports:\n{stdout}"
    );
}

/// 2.T7 — every file under `porting/fixtures/` is portable and secret-free:
/// no `/home/`, no token/secret/socket marker, and no value of any current
/// process env var whose name matches `TOKEN|KEY|SECRET|PASS`.
#[test]
fn fixtures_are_portable_and_secret_free() {
    let root = repo_root();
    let fixtures = root.join("porting/fixtures");
    let mut texts: Vec<(PathBuf, String)> = Vec::new();
    for entry in std::fs::read_dir(&fixtures).expect("fixtures dir") {
        let path = entry.expect("entry").path();
        if path.is_file() {
            texts.push((
                path.clone(),
                std::fs::read_to_string(&path).expect("read fixture"),
            ));
        }
    }
    assert!(!texts.is_empty(), "expected at least one fixture file");

    for (path, text) in &texts {
        for needle in ["/home/", "TOKEN", "SECRET", "AUTH_SOCK"] {
            assert!(
                !text.contains(needle),
                "fixture {} must not contain {needle}",
                path.display()
            );
        }
    }

    // Any env var whose name matches TOKEN|KEY|SECRET|PASS must not leak its
    // value into any fixture (live credentials).
    for (name, value) in std::env::vars() {
        if !name.to_uppercase().contains("TOKEN")
            && !name.to_uppercase().contains("KEY")
            && !name.to_uppercase().contains("SECRET")
            && !name.to_uppercase().contains("PASS")
        {
            continue;
        }
        if value.is_empty() {
            continue;
        }
        for (path, text) in &texts {
            assert!(
                !text.contains(&value),
                "fixture {} leaks value of env var {name}",
                path.display()
            );
        }
    }
}
