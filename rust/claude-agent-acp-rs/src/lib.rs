#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod agent;
pub mod air;
pub mod async_tasks;
pub mod auth_status;
pub mod codec;
pub mod control;
pub mod dispatch;
pub mod env_snapshot;
pub mod hooks;
pub mod map;
pub mod meta_filter;
pub mod permission;
pub mod permission_mode;
pub mod permission_options;
pub mod process;
pub mod quota;
pub mod resumed_session;
pub mod sdk_options;
pub mod session;
pub mod session_config;
pub mod session_ext;
pub mod session_methods;
pub mod session_store;
pub mod settings;
pub mod system_prompt;
pub mod tools;
pub mod transcript;
pub mod turn;
pub mod unfinished;
pub mod usage;

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
