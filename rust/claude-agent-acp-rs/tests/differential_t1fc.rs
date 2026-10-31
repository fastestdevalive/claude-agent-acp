//! Turn 1 fix round, lane C: the diff engine on the wire. Every frame comes
//! from the Node fixture (`bash porting/capture.sh <name>`), never from a
//! hand-written expectation.
//!
//! The git patch names the file by its absolute path, so each scenario works on
//! a file under a fixed directory that is NOT inside the worktree
//! (`/tmp/acp-parity-t1fc/<scenario>/`). The test writes the file's content
//! before the run, exactly as it was when the fixture was captured.

mod common;

use std::path::Path;

use common::{
    diff_frames, normalize_agent_info_version, normalize_root_paths, normalize_terminal_auth,
    parse_fixture_frames, pinned_upstream_version, repo_root, run_recorder, rust_agent_binary,
};

static NEXT_DIFF_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const FIXTURE_DIR: &str = "/tmp/acp-parity-t1fc";

/// Next to the fixture directory. It is never deleted: the OS advisory lock on
/// it (`File::try_lock`) is the lock, and the kernel releases that when the
/// holder dies, so a killed `cargo test` leaves nothing to clean up.
const FIXTURE_LOCK: &str = "/tmp/acp-parity-t1fc.lock";

/// How long a test waits for another run's lock.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

/// Holds the advisory lock on [`FIXTURE_LOCK`] until dropped (or the process dies).
///
/// LIMITATION: the Node-captured patches embed the absolute path
/// `/tmp/acp-parity-t1fc/<scenario>/<file>`, so the directory cannot move per
/// run or per worktree. Concurrent test runs (other worktrees, other `cargo
/// test` processes, the tests of this file) would seed and read the same
/// files, so they take turns: a test holds the lock from seeding until its
/// scenario has been diffed.
struct FixtureLock(std::fs::File);

impl FixtureLock {
    fn acquire() -> Self {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(FIXTURE_LOCK)
            .unwrap_or_else(|e| panic!("cannot open {FIXTURE_LOCK}: {e}"));
        let started = std::time::Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Self(file),
                Err(std::fs::TryLockError::WouldBlock) => {
                    assert!(
                        started.elapsed() < LOCK_WAIT,
                        "another live run holds {FIXTURE_LOCK}"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => panic!("cannot lock {FIXTURE_LOCK}: {e}"),
            }
        }
    }
}

impl Drop for FixtureLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Put the scenario's file in the state the Node capture saw (`None`: absent).
/// The returned lock must be held until the scenario has run.
#[must_use]
fn seed(scenario: &str, file: &str, content: Option<&str>) -> FixtureLock {
    let lock = FixtureLock::acquire();
    let dir = Path::new(FIXTURE_DIR).join(scenario);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join(file);
    match content {
        Some(text) => std::fs::write(&path, text).expect("seed fixture file"),
        None => {
            let _ = std::fs::remove_file(&path);
        }
    }
    lock
}

fn run_and_diff(name: &str) {
    let root = repo_root();
    let script = root.join(format!("porting/corpus/{name}.acp.json"));
    let transcript = root.join(format!("porting/corpus/{name}.transcript.jsonl"));
    let fixture = root.join(format!("porting/fixtures/{name}.frames.jsonl"));

    assert!(script.exists(), "missing script {script:?}");
    assert!(transcript.exists(), "missing transcript {transcript:?}");
    assert!(fixture.exists(), "missing fixture {fixture:?}");

    let tmp = std::env::temp_dir().join(format!(
        "acp-diff-t1fc-{name}-{}-{}",
        std::process::id(),
        NEXT_DIFF_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let out = tmp.join("out.jsonl");

    let agent_cmd = rust_agent_binary();
    assert!(
        agent_cmd.exists(),
        "rust agent binary missing: {}",
        agent_cmd.display()
    );
    let frames = run_recorder(&agent_cmd.to_string_lossy(), &script, &transcript, &out);
    let frames = normalize_terminal_auth(normalize_root_paths(frames, &root));
    let expected = normalize_terminal_auth(parse_fixture_frames(&fixture));
    let (frames, expected) =
        normalize_agent_info_version(frames, expected, &pinned_upstream_version());
    if let Err(diff) = diff_frames(&frames, &expected, &[]) {
        panic!("{name} differential failed:\n{diff}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// An Edit approval shows the exact patch of the change (T-106, Q-72); the
/// streamed snippet diff does not replace it; the hook's `structuredPatch`
/// diff does, because the file on disk still holds the old text.
#[test]
fn edit_diff_preview() {
    let _lock = seed(
        "edit-diff-preview",
        "a.txt",
        Some("one\ntwo\nthree\nfour\nfive\n"),
    );
    run_and_diff("edit-diff-preview");
}

/// A diffPatch client's Write tool call shows no input diff (T-40); the
/// approval carries the patch of the overwrite.
#[test]
fn write_diff_patch_client() {
    let _lock = seed("write-diff-patch-client", "b.txt", Some("alpha\nbeta\n"));
    run_and_diff("write-diff-patch-client");
}

/// The file on disk already holds the new text: the approval has no patch, and
/// the PostToolUse hook sends the exact patch of the written file (T-43).
#[test]
fn write_diff_hook_patch() {
    let _lock = seed("write-diff-hook-patch", "c.txt", Some("alpha\ngamma\n"));
    run_and_diff("write-diff-hook-patch");
}

/// A created file: the approval carries the creation patch, and the hook, which
/// cannot read the file back, sends the standard diff of the created file.
#[test]
fn write_create_patch_client() {
    let _lock = seed("write-create-patch-client", "new.txt", None);
    run_and_diff("write-create-patch-client");
}

/// An AIR client without `diffPatch` keeps the Write input diff and gets no
/// approval patch.
#[test]
fn write_air_no_diff_patch() {
    let _lock = seed("write-air-no-diff-patch", "d.txt", Some("alpha\nbeta\n"));
    run_and_diff("write-air-no-diff-patch");
}
