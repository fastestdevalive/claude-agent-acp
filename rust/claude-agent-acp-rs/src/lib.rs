// Decision D8: the Windows Job Object FFI needs a scoped `#[allow(unsafe_code)]`
// on the private `windows_job` module in `process.rs`. `forbid` cannot coexist
// with an inner module `allow` (E0453), so Windows keeps the crate-level lint at
// `deny`; every other target keeps `forbid(unsafe_code)` unchanged (D12/R33).
#![cfg_attr(not(windows), forbid(unsafe_code))]
#![cfg_attr(windows, deny(unsafe_code))]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod acp_subagents;
pub mod agent;
pub mod air;
pub mod async_tasks;
pub mod auth_status;
pub mod client_caps;
pub mod codec;
pub mod context_compaction;
pub mod context_compaction_meta;
pub mod control;
pub mod diff;
pub mod dispatch;
pub mod elicitation;
pub mod env_snapshot;
pub mod event_loop_yield;
pub mod file_change_audit;
pub mod fork_session;
pub mod gateway_models;
pub mod goal;
pub mod hide_claude_auth;
pub mod hooks;
pub mod logger;
pub mod map;
pub mod mcp_command;
pub mod message_ids;
pub mod meta_filter;
pub mod native_subagents;
pub mod notices;
pub mod permission;
pub mod permission_effects;
pub mod permission_mode;
pub mod permission_options;
pub mod permission_response;
pub mod process;
pub mod prompt_suggestions;
pub mod quota;
pub mod resumed_session;
pub mod sdk_options;
pub mod session;
pub mod session_config;
pub mod session_ext;
pub mod session_failure;
pub mod session_index;
pub mod session_methods;
pub mod session_model;
pub mod session_store;
pub mod session_timing;
pub mod session_titles;
pub mod settings;
pub mod settings_manager;
pub mod subagent_history;
pub mod subagent_replay;
pub mod system_prompt;
pub mod tool_meta;
pub mod tool_reporters_compaction;
pub mod tools;
pub mod transcript;
pub mod turn;
pub mod turn_events;
pub mod unfinished;
pub mod usage;
pub mod usage_markdown;
pub mod v2;

#[cfg(test)]
mod tests_t6a2;

#[cfg(test)]
mod tests_t7c;

// Public library surface mirroring upstream `src/lib.ts` (FX-4 / O-83, Q-19).
pub use agent::serve as run_acp;
pub use agent::{serve, serve_handle, ClaudeAcpAgent, ServeHandle, ServeOptions};
pub use auth_status::{AuthAccount as AuthStatusAccount, AuthStatus, AUTH_STATUS_UPDATE_METHOD};
pub use map::map_user_frame as to_acp_notifications;
pub use settings_manager::{SettingsManager, SettingsManagerOptions};
pub use tools::{plan_entries, to_display_path, tool_info_from_tool_use, ClaudePlanEntry};

/// Composed `<crate>+upstream.<npm-pin>` version string, e.g. `0.1.0+upstream.0.94.0`.
///
/// Injected by `build.rs` from `porting/UPSTREAM_VERSION` (Decision 2). Used by
/// `agent_info()` (`agent.rs`) and the `--version` flag (`bin/claude-agent-acp-rs.rs`).
pub const VERSION: &str = env!("ACP_VERSION_STRING");

/// The mirrored npm `@agentclientprotocol/claude-agent-acp` version this crate
/// tracks, read from `porting/UPSTREAM_VERSION` by `build.rs`.
pub const UPSTREAM_VERSION: &str = env!("UPSTREAM_NPM_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
