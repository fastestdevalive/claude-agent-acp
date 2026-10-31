//! Hermeticity of the settings load (G7a-fix4 item 1).
//!
//! The adapter resolves managed policy documents through one channel:
//! `EnvSnapshot::managed_docs`. Tests inject an empty (or fake) document list,
//! so no test may consult the host managed-policy paths
//! (`/Library/Managed Preferences`, `/etc/claude-code`, the Windows policy
//! registry). [`ManagedSourceGuard`] turns a violation into a hard failure.

use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::process::{read_managed_sources, ManagedSourceGuard};
use claude_agent_acp_rs::settings;
use serde_json::json;

fn temp_cwd() -> std::path::PathBuf {
    let cwd = std::env::temp_dir().join(format!("acp-hermetic-managed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(&cwd).expect("create temp cwd");
    cwd
}

#[tokio::test]
async fn hermetic_snapshot_never_reads_host_managed_paths() {
    let _guard = ManagedSourceGuard::arm();
    let cwd = temp_cwd();

    // 1. `EnvSnapshot::empty()` injects an empty managed tier: the load must
    //    not fall back to the host policy paths (the guard would panic).
    let empty = settings::load(&cwd, &EnvSnapshot::empty());
    assert_eq!(
        empty.get_api_key_helper(None),
        None,
        "no host managed apiKeyHelper may leak into a hermetic load"
    );

    // 2. Explicit fake documents are the only managed source consulted, and
    //    they win the precedence (managed > user/project/local).
    let env = EnvSnapshot::empty()
        .with_managed_docs(vec![json!({"apiKeyHelper": "/fake/managed-helper"})]);
    let loaded = settings::load(&cwd, &env);
    assert_eq!(
        loaded.get_api_key_helper(None).as_deref(),
        Some("/fake/managed-helper"),
        "the injected managed document must be used"
    );

    // 3. The auth actor's `apiKeyHelper` read goes through the same snapshot.
    assert_eq!(
        claude_agent_acp_rs::auth_status::read_api_key_helper_with_env(&cwd, &env).await,
        Some("/fake/managed-helper".to_string()),
        "auth status must resolve apiKeyHelper through the injected snapshot"
    );
    assert_eq!(
        claude_agent_acp_rs::auth_status::read_api_key_helper_with_env(&cwd, &EnvSnapshot::empty())
            .await,
        None,
        "an empty snapshot must not fall back to the host managed tier"
    );

    // 4. The guard itself is effective: a direct host read panics.
    let tripped = std::panic::catch_unwind(read_managed_sources).is_err();
    assert!(
        tripped,
        "the managed-source guard must fail a host managed-policy read"
    );

    let _ = std::fs::remove_dir_all(&cwd);
}
