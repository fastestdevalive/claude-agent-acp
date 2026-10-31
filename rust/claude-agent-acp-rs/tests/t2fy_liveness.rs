//! Turn 2 fix round lane Y: the liveness of a live-registry record comes from
//! `kill(pid, 0)` ([`claude_agent_acp_rs::process::pid_alive`]), as upstream's
//! `live-registry.ts:66-72` does, not from a `ps` spawn per record. A real child
//! process is alive until it is killed and reaped; a record of a reused pid (its
//! `procStart` differs) does not count.
//!
//! Every process this file starts is killed and reaped before the test ends.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use claude_agent_acp_rs::process::pid_alive;
use claude_agent_acp_rs::session_index::live_registry::{LiveRegistryDeps, LiveSessionRegistry};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "acp-t2fy-live-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs_f64()
        * 1000.0
}

/// A child process that is killed and reaped when the test ends, also when it
/// panics.
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// process > a pid is alive while its process runs, and dead once it is reaped
#[cfg(unix)]
#[test]
fn a_pid_is_alive_while_its_process_runs_and_dead_once_reaped() {
    let child = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("a child process");
    let mut child = Child(child);
    let pid = child.0.id();
    assert_eq!(
        pid_alive(pid),
        Some(true),
        "a running child is alive (pid {pid})"
    );
    child.0.kill().expect("kill");
    child.0.wait().expect("reap");
    // Reaped: the pid is gone, so a record of it must not count.
    assert_eq!(pid_alive(pid), Some(false), "a reaped child is dead");
}

/// process > a pid that cannot exist is dead, and a pid above the range is not
/// mistaken for a process group
#[cfg(unix)]
#[test]
fn an_impossible_pid_is_dead() {
    // Above `i32::MAX` no pid exists; a cast to a negative pid would name a
    // process group instead.
    assert_eq!(pid_alive(u32::MAX), Some(false));
    // A pid that is far beyond the machine's limit.
    assert_eq!(pid_alive(4_000_000_000), Some(false));
}

/// live registry > a record of this process is live, a reused pid is not
/// (unix only: Windows has no `kill(pid, 0)` liveness yet, V-12)
#[cfg(unix)]
#[test]
fn a_record_of_this_process_is_live_and_a_reused_pid_is_not() {
    let dir = temp_dir("registry");
    let pid = std::process::id();
    let now = now_ms();
    let record = |session: &str, proc_start: Option<&str>| {
        let mut value = serde_json::json!({
            "pid": pid, "sessionId": session, "kind": "interactive", "entrypoint": "cli",
            "status": "idle", "updatedAt": now, "startedAt": now,
        });
        if let Some(start) = proc_start {
            value["procStart"] = serde_json::Value::String(start.to_string());
        }
        value.to_string()
    };
    // One record of this very process: live.
    std::fs::write(dir.join(format!("{pid}.json")), record("mine", None)).expect("record");
    let mut registry = LiveSessionRegistry::new(LiveRegistryDeps::system(dir.clone()));
    let snapshot = registry.snapshot();
    assert!(
        snapshot.contains_key("mine"),
        "a live pid counts, without a `ps` liveness spawn: {snapshot:?}"
    );
    // The same pid with a start time that cannot be this process's: a reused pid.
    std::fs::write(
        dir.join(format!("{pid}.json")),
        record("reused", Some("Thu Jan  1 00:00:00 1970")),
    )
    .expect("record");
    let mut registry = LiveSessionRegistry::new(LiveRegistryDeps::system(dir.clone()));
    let snapshot = registry.snapshot();
    assert!(
        !snapshot.contains_key("reused"),
        "a procStart that differs is a reused pid: {snapshot:?}"
    );
    // A pid that is not there: dead, and the record is not read as live.
    // Above `i32::MAX` no pid can exist on any host (the same pid is dead for
    // Node's `process.kill` too), so the test depends on no machine's pid range.
    std::fs::remove_file(dir.join(format!("{pid}.json"))).expect("remove");
    let impossible = u32::try_from(i32::MAX).expect("i32::MAX fits") + 1;
    std::fs::write(
        dir.join(format!("{impossible}.json")),
        serde_json::json!({
            "pid": impossible, "sessionId": "gone", "updatedAt": now,
        })
        .to_string(),
    )
    .expect("record");
    let mut registry = LiveSessionRegistry::new(LiveRegistryDeps::system(dir.clone()));
    assert!(
        !registry.snapshot().contains_key("gone"),
        "a dead pid does not count"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
