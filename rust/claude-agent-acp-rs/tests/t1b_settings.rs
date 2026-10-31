//! Turn 1 lane B (F5a): `SettingsManager` lifecycle parity with
//! `src/settings.ts` — `setCwd` re-initializes without `onChange`, a deleted
//! settings file reloads, and `dispose` stops the watcher. Every wait is a
//! bounded poll; the tests use temp dirs and an injected env (never the host
//! `~/.claude` or host managed policy).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::settings_manager::{SettingsManager, SettingsManagerOptions};

fn temp_root(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "acp-t1b-settings-{}-{label}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn manager(root: &std::path::Path, cwd: PathBuf, counter: Arc<AtomicUsize>) -> SettingsManager {
    let env = EnvSnapshot::from_pairs([(
        "CLAUDE_CONFIG_DIR",
        root.join("config").to_string_lossy().into_owned(),
    )]);
    SettingsManager::new(
        cwd,
        Some(SettingsManagerOptions {
            on_change: Some(Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            })),
            env: Some(env),
            poll_interval: Some(Duration::from_millis(20)),
            ..Default::default()
        }),
    )
}

async fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(result.is_ok(), "timed out waiting for {what}");
}

/// `setCwd` reloads the project settings of the new cwd but, like upstream
/// (`dispose` + `initialize`), never fires `onChange`.
#[tokio::test]
async fn set_cwd_reloads_without_firing_on_change() {
    let root = temp_root("set-cwd");
    let (a, b) = (root.join("a"), root.join("b"));
    for (dir, model) in [(&a, "model-a"), (&b, "model-b")] {
        std::fs::create_dir_all(dir.join(".claude")).expect("mkdir");
        std::fs::write(
            dir.join(".claude/settings.json"),
            format!(r#"{{"model":"{model}"}}"#),
        )
        .expect("write settings");
    }
    let counter = Arc::new(AtomicUsize::new(0));
    let mgr = manager(&root, a.clone(), counter.clone());
    assert_eq!(mgr.get_settings().model.as_deref(), Some("model-a"));

    mgr.set_cwd(b.clone());
    wait_for("the new cwd's settings", || {
        mgr.get_settings().model.as_deref() == Some("model-b")
    })
    .await;
    assert_eq!(mgr.get_cwd(), b);
    // Give a (wrongly) scheduled callback time to land before asserting.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "setCwd must not fire onChange"
    );

    // The new cwd's files are now the watched ones.
    std::fs::write(b.join(".claude/settings.json"), r#"{"model":"model-b2"}"#).expect("write");
    wait_for("an edit under the new cwd", || {
        mgr.get_settings().model.as_deref() == Some("model-b2")
    })
    .await;
    assert!(counter.load(Ordering::SeqCst) >= 1);

    mgr.dispose();
    let _ = std::fs::remove_dir_all(&root);
}

/// Deleting a watched settings file reloads and drops its contribution.
#[tokio::test]
async fn deleting_a_settings_file_reloads_and_notifies() {
    let root = temp_root("delete");
    let cwd = root.join("cwd");
    std::fs::create_dir_all(cwd.join(".claude")).expect("mkdir");
    let file = cwd.join(".claude/settings.local.json");
    std::fs::write(&file, r#"{"model":"local-model"}"#).expect("write");
    let counter = Arc::new(AtomicUsize::new(0));
    let mgr = manager(&root, cwd, counter.clone());
    assert_eq!(mgr.get_settings().model.as_deref(), Some("local-model"));
    tokio::time::sleep(Duration::from_millis(80)).await;

    std::fs::remove_file(&file).expect("remove");
    wait_for("the deleted file's contribution to drop", || {
        mgr.get_settings().model.is_none()
    })
    .await;
    wait_for("onChange", || counter.load(Ordering::SeqCst) >= 1).await;

    mgr.dispose();
    let _ = std::fs::remove_dir_all(&root);
}

/// After `dispose` the watcher is gone: later edits neither reload nor notify.
#[tokio::test]
async fn dispose_stops_the_watcher() {
    let root = temp_root("dispose");
    let cwd = root.join("cwd");
    std::fs::create_dir_all(cwd.join(".claude")).expect("mkdir");
    let file = cwd.join(".claude/settings.json");
    std::fs::write(&file, r#"{"model":"before"}"#).expect("write");
    let counter = Arc::new(AtomicUsize::new(0));
    let mgr = manager(&root, cwd, counter.clone());
    tokio::time::sleep(Duration::from_millis(80)).await;

    mgr.dispose();
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::write(&file, r#"{"model":"after"}"#).expect("write");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(mgr.get_settings().model.as_deref(), Some("before"));
    assert_eq!(counter.load(Ordering::SeqCst), 0);

    let _ = std::fs::remove_dir_all(&root);
}
