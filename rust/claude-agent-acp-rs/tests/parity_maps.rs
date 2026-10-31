//! Parity-map machinery tests (plan-parity-infra item 1.9).
//!
//! Hermetic: they use the committed snapshots + maps only, never the upstream
//! export, the network or the host config. The scripts are located through
//! `CARGO_MANIFEST_DIR`, and mutations are made on temp copies selected through
//! the checkers' documented env overrides.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves")
}

fn scripts_dir() -> PathBuf {
    repo_root().join("rust/scripts/parity")
}

fn parity_dir() -> PathBuf {
    repo_root().join("porting/parity")
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("parity-maps-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Run `bash <script> <args...>` with optional env overrides; return
/// (success, combined stdout+stderr).
fn run(script: &Path, args: &[&str], envs: &[(&str, &str)]) -> (bool, String) {
    let mut cmd = Command::new("bash");
    cmd.arg(script);
    for a in args {
        cmd.arg(a);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn parity checker");
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[test]
fn inv_70_parity_selftests() {
    let scripts = scripts_dir();
    for script in ["test-map-check", "module-map-check"] {
        let (ok, text) = run(&scripts.join(script), &["--selftest"], &[]);
        assert!(ok, "{script} --selftest failed:\n{text}");
        let last = text.lines().rev().find(|l| !l.trim().is_empty());
        assert_eq!(
            last.map(str::trim),
            Some("SELFTEST: PASS"),
            "{script}:\n{text}"
        );
    }
}

#[test]
fn inv_71_parity_maps_pass_offline() {
    let scripts = scripts_dir();
    for script in ["test-map-check", "module-map-check"] {
        let (ok, text) = run(&scripts.join(script), &["--map"], &[]);
        assert!(ok, "{script} --map failed:\n{text}");
    }

    // PARITY_FORCE_OFFLINE=1 makes fetch-upstream.sh exit 3 without touching
    // git, so the SKIP path is exercised deterministically (a warm
    // rust/target/parity cache must not make this test host-dependent).
    let (ok, text) = run(
        &scripts.join("gate-parity.sh"),
        &[],
        &[("PARITY_FORCE_OFFLINE", "1")],
    );
    assert!(ok, "gate-parity.sh failed:\n{text}");
    assert!(
        text.contains("PARITY REAL-EXPORT SKIPPED"),
        "missing SKIP banner:\n{text}"
    );
}

#[test]
fn inv_72_unmapped_test_fails() {
    let dir = temp_dir("unmapped-test");
    let map_src = parity_dir().join("TEST-MAP.md");
    let snap_src = parity_dir().join("upstream-tests.v0.94.0.tsv");
    let map = dir.join("TEST-MAP.md");
    let snap = dir.join("upstream-tests.tsv");

    let text = fs::read_to_string(&map_src).expect("read TEST-MAP");
    let mut kept = Vec::new();
    let mut removed = false;
    for line in text.lines() {
        if !removed && line.starts_with("src/") && line.contains(" | ") {
            removed = true;
            continue;
        }
        kept.push(line);
    }
    assert!(removed, "TEST-MAP has no explicit data row to delete");
    fs::write(&map, kept.join("\n")).expect("write mutated map");
    fs::copy(&snap_src, &snap).expect("copy snapshot");

    let (ok, text) = run(
        &scripts_dir().join("test-map-check"),
        &["--map"],
        &[
            ("TEST_MAP_FILE", map.to_str().expect("utf8")),
            ("TEST_SNAPSHOT_FILE", snap.to_str().expect("utf8")),
        ],
    );
    assert!(!ok, "a map with a deleted row must FAIL:\n{text}");
    assert!(text.contains("UNMAPPED"), "expected UNMAPPED:\n{text}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn inv_73_unmapped_module_fails() {
    let dir = temp_dir("unmapped-module");
    let map_src = parity_dir().join("MODULE-MAP.md");
    let snap_src = parity_dir().join("upstream-modules.v0.94.0.tsv");
    let map = dir.join("MODULE-MAP.md");
    let snap = dir.join("upstream-modules.tsv");

    let text = fs::read_to_string(&map_src).expect("read MODULE-MAP");
    let mut kept = Vec::new();
    let mut removed = false;
    for line in text.lines() {
        if !removed && line.starts_with("| tools.ts ") {
            removed = true;
            continue;
        }
        kept.push(line);
    }
    assert!(removed, "MODULE-MAP has no tools.ts row to delete");
    fs::write(&map, kept.join("\n")).expect("write mutated map");
    fs::copy(&snap_src, &snap).expect("copy snapshot");

    let (ok, text) = run(
        &scripts_dir().join("module-map-check"),
        &["--map"],
        &[
            ("MODULE_MAP_FILE", map.to_str().expect("utf8")),
            ("MODULE_SNAPSHOT_FILE", snap.to_str().expect("utf8")),
        ],
    );
    assert!(!ok, "a module map with a deleted row must FAIL:\n{text}");
    assert!(
        text.contains("UNMAPPED tools.ts"),
        "expected UNMAPPED:\n{text}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn inv_74_broken_ported_target_fails() {
    let dir = temp_dir("broken-ported");
    let map_src = parity_dir().join("TEST-MAP.md");
    let snap_src = parity_dir().join("upstream-tests.v0.94.0.tsv");
    let map = dir.join("TEST-MAP.md");
    let snap = dir.join("upstream-tests.tsv");

    let text = fs::read_to_string(&map_src).expect("read TEST-MAP");
    let mut replaced = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        // Pick a plain per-case row (no .each/%s or loop-generated title) so
        // the only fault is the broken ported: target.
        let plain = line.starts_with("src/")
            && line.contains("missing:T-UNCLASSIFIED")
            && !line.contains('%')
            && !line.contains("[dynamic");
        if !replaced && plain {
            lines.push(line.replacen(
                "missing:T-UNCLASSIFIED",
                "ported:tests/agent.rs::no_such_fn",
                1,
            ));
            replaced = true;
        } else {
            lines.push(line.to_string());
        }
    }
    assert!(replaced, "TEST-MAP has no seed row to rewrite");
    fs::write(&map, lines.join("\n")).expect("write mutated map");
    fs::copy(&snap_src, &snap).expect("copy snapshot");

    let (ok, text) = run(
        &scripts_dir().join("test-map-check"),
        &["--map"],
        &[
            ("TEST_MAP_FILE", map.to_str().expect("utf8")),
            ("TEST_SNAPSHOT_FILE", snap.to_str().expect("utf8")),
        ],
    );
    assert!(!ok, "a broken ported: target must FAIL:\n{text}");
    assert!(text.contains("BAD-PORTED"), "expected BAD-PORTED:\n{text}");

    let _ = fs::remove_dir_all(&dir);
}
