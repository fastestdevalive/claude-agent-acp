//! Claude Code settings manager with file watching and debounced reload (F5a, O-60).
//!
//! Mirrors upstream `SettingsManager` in `src/settings.ts:24-242`.
//!
//! Watches the user/project/local/managed settings files for changes, re-resolves
//! settings with a 100 ms debounce, notifies `onChange`, and exposes effective
//! settings, managed denied models, and `apiKeyHelper` with precedence:
//! managed > `--settings` override > user/project/local (O-61, O-67).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
use tokio::sync::{mpsc, watch};

use crate::env_snapshot::EnvSnapshot;
use crate::settings::{self, Settings};

/// Callback invoked after settings are reloaded following a file change.
pub type OnChangeCallback = Arc<dyn Fn() + Send + Sync + 'static>;

/// Options passed when constructing a [`SettingsManager`].
#[derive(Default, Clone)]
pub struct SettingsManagerOptions {
    /// Notification hook triggered on debounced reload.
    pub on_change: Option<OnChangeCallback>,
    /// Logger for diagnostics.
    pub logger: Option<Arc<crate::logger::Logger>>,
    /// Environment snapshot for user config directory resolution.
    pub env: Option<EnvSnapshot>,
    /// Injected managed settings documents (Finding 3).
    /// When Some(docs), these documents are used instead of reading platform-managed sources.
    pub managed_docs: Option<Vec<Value>>,
    /// Explicit `managed-settings.json` path to read the managed file tier from
    /// on every resolve (Finding 2 test seam). Production leaves this `None`
    /// and resolves [`crate::process::managed_settings_path`].
    pub managed_file: Option<PathBuf>,
    /// Watcher poll interval (default 100 ms).
    pub poll_interval: Option<Duration>,
}

#[derive(Clone)]
struct State {
    cwd: PathBuf,
    settings: Settings,
}

enum ControlMsg {
    SetCwd(PathBuf),
    Dispose,
}

/// Manages settings for a given working directory, watching relevant files for changes.
pub struct SettingsManager {
    state_rx: watch::Receiver<State>,
    cmd_tx: mpsc::UnboundedSender<ControlMsg>,
}

impl SettingsManager {
    /// Create a new `SettingsManager` initialized with the settings for `cwd`
    /// and spawn a background watcher task.
    pub fn new(cwd: PathBuf, options: Option<SettingsManagerOptions>) -> Self {
        let opts = options.unwrap_or_default();
        let env = opts.env.unwrap_or_else(EnvSnapshot::from_process);
        // Precedence: explicit documents > explicit managed file > the env
        // snapshot's injected documents > production sources.
        let managed_file = opts.managed_file;
        let managed_docs = opts.managed_docs.or_else(|| {
            if managed_file.is_none() {
                env.managed_docs().map(|d| d.to_vec())
            } else {
                None
            }
        });
        let settings =
            resolve_settings(&cwd, &env, managed_docs.as_deref(), managed_file.as_deref());

        let (state_tx, state_rx) = watch::channel(State {
            cwd: cwd.clone(),
            settings,
        });
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<ControlMsg>();

        let on_change = opts.on_change;
        let poll_interval = opts.poll_interval.unwrap_or(DEFAULT_WATCHER_POLL_INTERVAL);

        tokio::spawn(async move {
            run_watcher(WatcherConfig {
                state_tx,
                cmd_rx,
                current_cwd: cwd,
                env,
                managed_docs,
                managed_file,
                on_change,
                poll_interval,
            })
            .await;
        });

        Self { state_rx, cmd_tx }
    }

    /// The current merged effective settings.
    pub fn get_settings(&self) -> Settings {
        self.state_rx.borrow().settings.clone()
    }

    /// Resolve the effective `apiKeyHelper` with precedence:
    /// managed > `--settings` override > user/project/local (O-61, O-67).
    pub fn get_api_key_helper(&self, settings_override: Option<&Value>) -> Option<String> {
        self.state_rx
            .borrow()
            .settings
            .get_api_key_helper(settings_override)
    }

    /// `deniedModels` from the managed tier only.
    pub fn get_managed_denied_models(&self) -> Vec<String> {
        self.state_rx
            .borrow()
            .settings
            .managed_denied_models
            .clone()
    }

    /// The current working directory.
    pub fn get_cwd(&self) -> PathBuf {
        self.state_rx.borrow().cwd.clone()
    }

    /// Update working directory and immediately reload settings.
    pub fn set_cwd(&self, cwd: PathBuf) {
        let _ = self.cmd_tx.send(ControlMsg::SetCwd(cwd));
    }

    /// Dispose file watchers and clean up background tasks.
    pub fn dispose(&self) {
        let _ = self.cmd_tx.send(ControlMsg::Dispose);
    }
}

impl Drop for SettingsManager {
    fn drop(&mut self) {
        self.dispose();
    }
}

/// Resolve settings from the configured managed tier (Finding 2/3):
/// injected documents, an explicit managed file, or the production sources.
fn resolve_settings(
    cwd: &Path,
    env: &EnvSnapshot,
    managed_docs: Option<&[Value]>,
    managed_file: Option<&Path>,
) -> Settings {
    match (managed_docs, managed_file) {
        (Some(docs), _) => settings::load_with_managed(cwd, env, docs.to_vec()),
        (None, Some(file)) => settings::load_with_managed_file(cwd, env, file),
        (None, None) => settings::load(cwd, env),
    }
}

/// Compute paths that contribute to settings for `cwd`.
fn watched_paths_for(cwd: &Path, env: &EnvSnapshot, managed_file: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let user_dir = env
        .get_path("CLAUDE_CONFIG_DIR")
        .or_else(|| env.get_path("HOME").map(|h| h.join(".claude")));
    if let Some(dir) = user_dir {
        paths.push(dir.join("settings.json"));
    }
    paths.push(cwd.join(".claude").join("settings.json"));
    paths.push(cwd.join(".claude").join("settings.local.json"));
    paths.push(match managed_file {
        Some(path) => path.to_path_buf(),
        None => crate::process::managed_settings_path(),
    });
    paths
}

type FileSnapshot = Vec<(PathBuf, Option<(u64, Option<SystemTime>)>)>;

fn take_snapshot(paths: &[PathBuf]) -> FileSnapshot {
    paths
        .iter()
        .map(|path| {
            let meta = std::fs::metadata(path).ok();
            let state = meta.map(|m| (m.len(), m.modified().ok()));
            (path.clone(), state)
        })
        .collect()
}

/// Default polling interval for settings file modification detection.
///
/// The production default is 100 ms (the value before commit 5cace28); the
/// debounce test injects its own 20 ms interval via
/// `SettingsManagerOptions::poll_interval`, so the fast poll it needs never
/// changes the cost paid by every live session (item 5, G7a-fix4).
const DEFAULT_WATCHER_POLL_INTERVAL: Duration = Duration::from_millis(100);

struct WatcherConfig {
    state_tx: watch::Sender<State>,
    cmd_rx: mpsc::UnboundedReceiver<ControlMsg>,
    current_cwd: PathBuf,
    env: EnvSnapshot,
    managed_docs: Option<Vec<Value>>,
    managed_file: Option<PathBuf>,
    on_change: Option<OnChangeCallback>,
    poll_interval: Duration,
}

/// Background watcher loop polling with 100 ms debounce.
async fn run_watcher(mut cfg: WatcherConfig) {
    let mut watched_paths =
        watched_paths_for(&cfg.current_cwd, &cfg.env, cfg.managed_file.as_deref());
    let mut last_snapshot = take_snapshot(&watched_paths);
    let mut debounce_deadline: Option<Instant> = None;

    let reload = |cwd: &Path| -> Settings {
        resolve_settings(
            cwd,
            &cfg.env,
            cfg.managed_docs.as_deref(),
            cfg.managed_file.as_deref(),
        )
    };

    loop {
        tokio::select! {
            cmd = cfg.cmd_rx.recv() => {
                match cmd {
                    Some(ControlMsg::SetCwd(new_cwd)) => {
                        if new_cwd != cfg.current_cwd {
                            cfg.current_cwd = new_cwd;
                            let new_settings = reload(&cfg.current_cwd);
                            let _ = cfg.state_tx.send(State {
                                cwd: cfg.current_cwd.clone(),
                                settings: new_settings,
                            });
                            if let Some(cb) = &cfg.on_change {
                                cb();
                            }
                            watched_paths = watched_paths_for(
                                &cfg.current_cwd,
                                &cfg.env,
                                cfg.managed_file.as_deref(),
                            );
                            last_snapshot = take_snapshot(&watched_paths);
                            debounce_deadline = None;
                        }
                    }
                    Some(ControlMsg::Dispose) | None => break,
                }
            }
            _ = tokio::time::sleep(cfg.poll_interval) => {}
        }

        let current_snapshot = take_snapshot(&watched_paths);
        if current_snapshot != last_snapshot {
            last_snapshot = current_snapshot;
            // Any detected file change (creation, modification, deletion) sets/resets the 100 ms debounce timer.
            debounce_deadline = Some(Instant::now() + Duration::from_millis(100));
        }

        if let Some(deadline) = debounce_deadline {
            if Instant::now() >= deadline {
                debounce_deadline = None;
                let new_settings = reload(&cfg.current_cwd);
                let _ = cfg.state_tx.send(State {
                    cwd: cfg.current_cwd.clone(),
                    settings: new_settings,
                });
                if let Some(cb) = &cfg.on_change {
                    cb();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acp-settings-mgr-{}-{label}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn test_settings_manager_debounce_and_on_change() {
        let root = temp_dir("debounce");
        let cwd = root.join("cwd");
        let claude_dir = cwd.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);

        let env =
            EnvSnapshot::from_pairs([("CLAUDE_CONFIG_DIR", root.join("config").to_str().unwrap())]);
        let mgr = SettingsManager::new(
            cwd.clone(),
            Some(SettingsManagerOptions {
                on_change: Some(Arc::new(move || {
                    counter_clone.fetch_add(1, Ordering::SeqCst);
                })),
                logger: None,
                env: Some(env),
                poll_interval: Some(Duration::from_millis(20)),
                ..Default::default()
            }),
        );

        assert_eq!(counter.load(Ordering::SeqCst), 0);

        // Let background watcher start and record initial snapshot
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Write to settings.json
        let settings_file = claude_dir.join("settings.json");
        std::fs::write(&settings_file, r#"{"model":"claude-3-5-sonnet"}"#).unwrap();

        // Wait for debounce (100 ms) + polling margin with a deadline
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(1) {
            if mgr.get_settings().model.as_deref() == Some("claude-3-5-sonnet") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(
            mgr.get_settings().model.as_deref(),
            Some("claude-3-5-sonnet")
        );

        // Rapid double edit, spaced so the watcher OBSERVES each write while the
        // 100 ms debounce is still armed (Finding 4): poll interval is 20 ms, the
        // writes are 60 ms apart (< 100 ms). With the debounce, both writes
        // coalesce into exactly 1 reload (total 2). Without the debounce, each
        // observed write reloads immediately (total 3) — mutation-verified.
        std::fs::write(&settings_file, r#"{"model":"claude-3-5-haiku"}"#).unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        std::fs::write(&settings_file, r#"{"model":"claude-3-5-opus"}"#).unwrap();

        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(1) {
            if mgr.get_settings().model.as_deref() == Some("claude-3-5-opus") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Wait for debounce window and polling margin to settle
        tokio::time::sleep(Duration::from_millis(200)).await;
        let c = counter.load(Ordering::SeqCst);
        assert_eq!(
            c, 2,
            "expected exactly 2 reloads (1 initial write + 1 debounced double edit): got {c}"
        );
        assert_eq!(mgr.get_settings().model.as_deref(), Some("claude-3-5-opus"));

        mgr.dispose();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_settings_manager_api_key_helper_and_set_cwd() {
        let root = temp_dir("helper");
        let cwd1 = root.join("cwd1");
        let cwd2 = root.join("cwd2");
        std::fs::create_dir_all(cwd1.join(".claude")).unwrap();
        std::fs::create_dir_all(cwd2.join(".claude")).unwrap();

        std::fs::write(
            cwd1.join(".claude/settings.json"),
            r#"{"apiKeyHelper":"helper-cwd1"}"#,
        )
        .unwrap();
        std::fs::write(
            cwd2.join(".claude/settings.json"),
            r#"{"apiKeyHelper":"helper-cwd2"}"#,
        )
        .unwrap();

        let env =
            EnvSnapshot::from_pairs([("CLAUDE_CONFIG_DIR", root.join("config").to_str().unwrap())]);
        let mgr = SettingsManager::new(
            cwd1.clone(),
            Some(SettingsManagerOptions {
                on_change: None,
                logger: None,
                env: Some(env),
                ..Default::default()
            }),
        );

        assert_eq!(mgr.get_cwd(), cwd1);
        assert_eq!(mgr.get_api_key_helper(None).as_deref(), Some("helper-cwd1"));

        // Override beats user/project helper
        let override_val = json!({"apiKeyHelper": "helper-override"});
        assert_eq!(
            mgr.get_api_key_helper(Some(&override_val)).as_deref(),
            Some("helper-override")
        );

        // set_cwd switches to cwd2
        mgr.set_cwd(cwd2.clone());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(mgr.get_cwd(), cwd2);
        assert_eq!(mgr.get_api_key_helper(None).as_deref(), Some("helper-cwd2"));

        mgr.dispose();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Finding 2: Hot reload of settings after initial load: editing a watched settings file
    /// fires on_change and returns the updated effective settings.
    #[tokio::test]
    async fn test_settings_manager_hot_reload() {
        let root = temp_dir("hot-reload");
        let cwd = root.join("cwd");
        let claude_dir = cwd.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();

        let settings_file = claude_dir.join("settings.json");
        std::fs::write(&settings_file, r#"{"model":"claude-initial"}"#).unwrap();

        let change_count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&change_count);

        let env =
            EnvSnapshot::from_pairs([("CLAUDE_CONFIG_DIR", root.join("config").to_str().unwrap())]);
        let mgr = SettingsManager::new(
            cwd.clone(),
            Some(SettingsManagerOptions {
                on_change: Some(Arc::new(move || {
                    count_clone.fetch_add(1, Ordering::SeqCst);
                })),
                logger: None,
                env: Some(env),
                ..Default::default()
            }),
        );

        assert_eq!(mgr.get_settings().model.as_deref(), Some("claude-initial"));

        // Wait for watcher to initialize
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Edit settings file: live hot reload (O-60)
        std::fs::write(&settings_file, r#"{"model":"claude-updated"}"#).unwrap();

        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(1) {
            if mgr.get_settings().model.as_deref() == Some("claude-updated") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(mgr.get_settings().model.as_deref(), Some("claude-updated"));
        assert!(change_count.load(Ordering::SeqCst) >= 1);

        mgr.dispose();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Finding 2: the managed FILE tier is re-read on every resolve, so editing
    /// `managed-settings.json` after the first load fires `on_change` and returns
    /// the new managed content. Uses the explicit-path seam: no host
    /// /Library/... or /etc/claude-code policy is ever read.
    #[tokio::test]
    async fn test_settings_manager_managed_file_hot_reload() {
        let root = temp_dir("managed-hot-reload");
        let cwd = root.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let managed = root.join("managed").join("managed-settings.json");
        std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
        std::fs::write(&managed, r#"{"model":"managed-initial"}"#).unwrap();

        let change_count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&change_count);

        let env =
            EnvSnapshot::from_pairs([("CLAUDE_CONFIG_DIR", root.join("config").to_str().unwrap())]);
        let mgr = SettingsManager::new(
            cwd.clone(),
            Some(SettingsManagerOptions {
                on_change: Some(Arc::new(move || {
                    count_clone.fetch_add(1, Ordering::SeqCst);
                })),
                logger: None,
                env: Some(env),
                managed_file: Some(managed.clone()),
                poll_interval: Some(Duration::from_millis(20)),
                ..Default::default()
            }),
        );

        assert_eq!(
            mgr.get_settings().model.as_deref(),
            Some("managed-initial"),
            "managed tier applies at construction"
        );

        // Let the watcher record its initial snapshot.
        tokio::time::sleep(Duration::from_millis(60)).await;

        std::fs::write(&managed, r#"{"model":"managed-updated"}"#).unwrap();

        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(1) {
            if mgr.get_settings().model.as_deref() == Some("managed-updated") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(
            mgr.get_settings().model.as_deref(),
            Some("managed-updated"),
            "the managed file must be re-read, not frozen by the OS-source cache"
        );
        assert!(
            change_count.load(Ordering::SeqCst) >= 1,
            "on_change fires for a managed-file edit"
        );

        mgr.dispose();
        let _ = std::fs::remove_dir_all(&root);
    }
}
