//! Per-session extension state owned by the session actor (rust-air-gaps
//! Decision 7).
//!
//! One struct for the actor's AIR / working-directory state, so later items
//! (goal, file-change report, failure context) add fields here instead of new
//! parameters. It lives inside the actor task: no `Mutex`. It is read by the
//! actor loop itself; the frame handlers do not take it until an item needs it.

use std::path::PathBuf;

use serde_json::Value;

use crate::air::AirClient;
use crate::client_caps::ClientCapabilities;
use crate::session_config::SessionConfigState;
use crate::usage::UsageState;

/// Actor-owned extension state: the parsed AIR client declaration, the full
/// client-capability parse, working directory, usage state, and config state.
#[derive(Debug)]
pub struct SessionExt {
    /// The parsed AIR client declaration, if the client is AIR.
    pub air: Option<AirClient>,
    /// The full `initialize.clientCapabilities` parse (ROADMAP F2a), stored so
    /// later packages (notices, config options, tool calls) read it once.
    pub caps: ClientCapabilities,
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
    /// The adapter logger (`CLAUDE_AGENT_LOGS`); `None` when logging is off.
    pub logger: Option<std::sync::Arc<crate::logger::Logger>>,
    /// Serialized init `plugin_errors` last logged, so a repeated init frame is
    /// not logged twice (`acp-agent.ts:1083,5999-6001`).
    pub logged_plugin_errors: Option<String>,
    /// The full slash-command list, seeded from the initialize handshake and
    /// replaced by every `commands_changed` frame (D5 / Decision 9). Node keeps
    /// it in `session.query.supportedCommands()`; the ACP advertisement is
    /// built from it plus the terminal-bound names below.
    pub commands: Vec<Value>,
    /// Names the CLI tagged terminal-bound on `system/init`; filtered out of
    /// the advertisement (D5 / S-04, `acp-agent.ts:5976-5998`). `None` until a
    /// first init frame latches a list (Node's `session.terminalSlashCommands`
    /// is `undefined` before that, so an empty list still counts as a change).
    pub terminal_slash_commands: Option<Vec<String>>,
    /// Per-turn `/mcp` state (rust-air-gaps G12, Decision 21): the parsed
    /// command, the CLI text/message id of the first output shape, and the
    /// produced replacement. Actor-owned, like the rest of this struct.
    pub mcp: crate::mcp_command::McpTurns,
    /// Goal bookkeeping (rust-air-gaps G8, Decision 16): the last published
    /// AIR goal and the optimistic update awaiting its runtime confirmation.
    pub goal: crate::goal::GoalState,
    /// Session title state machine (Decision 13 / D6).
    pub titles: crate::session_titles::SessionTitles,
    /// Failure context recorded from assistant frames for the active turn (G9).
    pub failure_ctx: crate::session_failure::AssistantFailureCtx,
    /// Turn 2 hazard-lane state: AIR failure lifecycle, notice books, run-loop effects.
    pub t2a: crate::session::state::T2aState,
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
            caps: ClientCapabilities::default(),
            cwd,
            usage: UsageState::default(),
            config: SessionConfigState::default(),
            hooks: crate::hooks::HookRegistry::new(),
            task_state: Vec::new(),
            last_published_task_plan: None,
            auto_mode_fallback_warning_shown: false,
            env,
            last_compaction_summary: None,
            logger: None,
            logged_plugin_errors: None,
            commands: Vec::new(),
            terminal_slash_commands: None,
            mcp: crate::mcp_command::McpTurns::default(),
            goal: crate::goal::GoalState::default(),
            titles: crate::session_titles::SessionTitles::new(),
            failure_ctx: crate::session_failure::AssistantFailureCtx::default(),
            t2a: crate::session::state::T2aState::default(),
        }
    }

    /// Attach the adapter logger (N-05 plugin-error logging).
    #[must_use]
    pub fn with_logger(mut self, logger: Option<std::sync::Arc<crate::logger::Logger>>) -> Self {
        self.logger = logger;
        self
    }

    /// Whether the client declared the AIR capability `name`.
    pub fn air_supports(&self, name: &str) -> bool {
        self.air.as_ref().is_some_and(|air| air.supports(name))
    }

    /// Set the full client-capability parse (ROADMAP F2a).
    #[must_use]
    pub fn with_caps(mut self, caps: ClientCapabilities) -> Self {
        self.caps = caps;
        self
    }

    /// Whether the client can present live notices.
    pub fn supports_notices(&self) -> bool {
        self.caps.supports_notices()
    }
}
