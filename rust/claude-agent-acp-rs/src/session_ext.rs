//! Per-session extension state owned by the session actor (rust-air-gaps
//! Decision 7).
//!
//! One struct for the actor's AIR / working-directory state, so later items
//! (goal, file-change report, failure context) add fields here instead of new
//! parameters. It lives inside the actor task: no `Mutex`. It is read by the
//! actor loop itself; the frame handlers do not take it until an item needs it.

use std::path::PathBuf;

use crate::air::AirClient;
use crate::session_config::SessionConfigState;
use crate::usage::UsageState;

/// Actor-owned extension state: the parsed AIR client declaration, working
/// directory, usage state, and config state.
#[derive(Debug)]
pub struct SessionExt {
    /// The parsed AIR client declaration, if the client is AIR.
    pub air: Option<AirClient>,
    /// The session's working directory.
    pub cwd: PathBuf,
    /// Context window and usage update state (rust-parity-fixes Decision 9).
    pub usage: UsageState,
    /// Session configuration state (rust-parity-fixes Decision 10).
    pub config: SessionConfigState,
    /// Pending hook callbacks (rust-parity-fixes Decision 12).
    pub hooks: crate::hooks::HookRegistry,
    /// Task state for task plan updates (Decision 12).
    pub task_state: Vec<(String, crate::hooks::TaskStateEntry)>,
    /// Last published task plan JSON representation (for AIR client deduplication).
    pub last_published_task_plan: Option<String>,
    /// Whether the auto mode fallback warning has already been emitted in this session.
    pub auto_mode_fallback_warning_shown: bool,
    /// Environment snapshot for hermetic reads.
    pub env: crate::env_snapshot::EnvSnapshot,
    /// The root session's latest PostCompact `compact_summary` (acp-agent.ts:11350-11360
    /// `contextCompaction.recordSummary`); consumed by the compaction update path (F20).
    pub last_compaction_summary: Option<String>,
}

impl SessionExt {
    /// Build the extension state for one session actor.
    pub fn new(
        air: Option<AirClient>,
        cwd: PathBuf,
        env: crate::env_snapshot::EnvSnapshot,
    ) -> Self {
        Self {
            air,
            cwd,
            usage: UsageState::default(),
            config: SessionConfigState::default(),
            hooks: crate::hooks::HookRegistry::new(),
            task_state: Vec::new(),
            last_published_task_plan: None,
            auto_mode_fallback_warning_shown: false,
            env,
            last_compaction_summary: None,
        }
    }

    /// Whether the client declared the AIR capability `name`.
    pub fn air_supports(&self, name: &str) -> bool {
        self.air.as_ref().is_some_and(|air| air.supports(name))
    }
}
