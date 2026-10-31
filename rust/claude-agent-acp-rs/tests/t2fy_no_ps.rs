//! Turn 2 fix round lane Y: the live registry must not spawn `ps` to learn
//! whether a pid is alive — that made every record dead where `ps` does not
//! exist (Windows, slim containers) and cost one spawn per record elsewhere.
//!
//! This binary has one test, because it replaces `PATH` for the whole process: a
//! fake `ps` writes a marker when it runs. The liveness check must not run it; the
//! batched start-time call only runs it for records that carry a `procStart`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use claude_agent_acp_rs::session_index::live_registry::{LiveRegistryDeps, LiveSessionRegistry};

/// Restores `PATH` when the test ends, also when it panics.
struct PathGuard {
    old: Option<std::ffi::OsString>,
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match &self.old {
            Some(old) => std::env::set_var("PATH", old),
            None => std::env::remove_var("PATH"),
        }
    }
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs_f64()
        * 1000.0
}

fn write_fake_ps(bin: &Path, marker: &Path) {
    let script = format!(
        "#!/bin/sh\nprintf x >> '{}'\nexit 1\n",
        marker.to_string_lossy()
    );
    std::fs::write(bin.join("ps"), script).expect("the fake ps");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(bin.join("ps"))
            .expect("the fake ps")
            .permissions()
            .mode();
        std::fs::set_permissions(
            bin.join("ps"),
            std::fs::Permissions::from_mode(mode | 0o755),
        )
        .expect("executable");
    }
}

/// live registry > finds a live record without a `ps` spawn (and without `ps` at all)
#[cfg(unix)]
#[test]
fn a_live_record_counts_without_spawning_ps() {
    let base = std::env::temp_dir().join(format!("acp-t2fy-nops-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let bin = base.join("bin");
    let registry = base.join("sessions");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(&registry).expect("registry");
    let marker = base.join("ps-ran");
    write_fake_ps(&bin, &marker);
    // The registry holds this very process, without a `procStart`, so the
    // batched start-time call has nothing to ask for.
    let pid = std::process::id();
    std::fs::write(
        registry.join(format!("{pid}.json")),
        serde_json::json!({
            "pid": pid, "sessionId": "mine", "kind": "interactive", "entrypoint": "cli",
            "status": "idle", "updatedAt": now_ms(), "startedAt": now_ms(),
        })
        .to_string(),
    )
    .expect("record");

    let _guard = PathGuard {
        old: std::env::var_os("PATH"),
    };
    std::env::set_var("PATH", &bin);
    let mut registry_state = LiveSessionRegistry::new(LiveRegistryDeps::system(registry.clone()));
    let snapshot = registry_state.snapshot();
    drop(_guard);

    assert!(
        snapshot.contains_key("mine"),
        "the live record counts although `ps` is missing: {snapshot:?}"
    );
    assert!(
        !PathBuf::from(&marker).exists(),
        "the liveness check spawned `ps`"
    );
    let _ = std::fs::remove_dir_all(&base);
}
