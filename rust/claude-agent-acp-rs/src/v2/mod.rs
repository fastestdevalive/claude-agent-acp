//! The experimental draft ACP v2 surface of the adapter (F38a).
//!
//! Upstream: `src/v2/` (`initialize.ts`, `session.ts`, `agent.ts`, `serve.ts`).

pub mod initialize;
pub mod serve;
pub mod session;

#[cfg(test)]
mod tests;

pub use initialize::{
    reject_v2_client_fs, v1_initialize_request, v2_auth_method, v2_initialize_response,
    v2_session_capabilities, V1_PROTOCOL_VERSION, V2_FS_ERROR_MESSAGE, V2_PROTOCOL_VERSION,
};
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
