//! The experimental draft ACP v2 surface of the adapter (F38a).
//!
//! Upstream: `src/v2/` (`initialize.ts`, `session.ts`, `agent.ts`, `serve.ts`).

pub mod agent;
pub mod initialize;
pub mod permission;
pub mod prompt;
pub mod serve;
pub mod session;
pub mod session_update;
pub mod terminal;
pub mod tool_call;
pub mod tool_call_meta;

#[cfg(test)]
mod agent_tests;
#[cfg(test)]
mod e2e_tests;
#[cfg(test)]
mod prompt_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod translate_tests;

pub use agent::v2_fs_error;
pub use initialize::{
    reject_v2_client_fs, v1_initialize_request, v2_auth_method, v2_initialize_response,
    v2_session_capabilities, V1_PROTOCOL_VERSION, V2_FS_ERROR_MESSAGE, V2_PROTOCOL_VERSION,
};
pub use permission::{v1_permission_response, v2_permission_presentation, v2_permission_request};
pub use serve::{
    handle_v2_request_sync, is_experimental_v2_enabled, negotiate_initialize, route_first_frame,
    Negotiated, ProtocolMode, ProtocolVersion, RouteDecision, EXPERIMENTAL_V2_ENV,
    MAX_PROTOCOL_VERSION,
};
pub use session::{
    is_extension_method, is_final_tool_call_status, tool_call_status_for_aborted,
    v1_fork_session_request, v1_new_session_request, v1_restore_session_request,
    v1_set_session_config_option_request, v2_available_commands, v2_config_options,
    v2_new_session_response, v2_resume_session_response, validate_extension_method,
    V1RestoreResult,
};
pub use session_update::{
    v2_child_state_update, v2_session_update, v2_subagent_state, SESSION_PLAN_ID,
};
pub use terminal::{TerminalSplit, V2Terminals};
pub use tool_call::{is_v2_diff, v2_tool_call_content, v2_tool_call_patch, v2_tool_call_update};
pub use tool_call_meta::V2ToolCallMeta;
