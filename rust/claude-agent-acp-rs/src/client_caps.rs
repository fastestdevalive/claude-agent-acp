//! The full parse of `initialize.clientCapabilities` (ROADMAP F2a).
//!
//! The Node adapter reads the client's capability choices once, at
//! `initialize`, into `ClientCapabilities` (`tool-calls/client-capabilities.ts`)
//! and keeps them on the connection/session for every later report. This module
//! is the Rust port of that parse: it turns the raw `clientCapabilities` JSON
//! into the booleans and small enums that the tool-call, notice, config and
//! session code consults.
//!
//! The parse is deliberately lenient, exactly like Node: every capability is
//! read from wire data that was never validated by a schema, so a missing,
//! `null`, wrong-typed or array value means "not advertised" rather than an
//! error. The parsed object is stored on [`crate::session_ext::SessionExt`] so
//! later packages can read it without re-parsing the handshake.
//!
//! A capability that Rust advertises but does not implement yet stays inert;
//! each such behaviour is a separate PARITY row.

use serde_json::Value;

use crate::air::AirClient;

/// The form of native subagent sessions a client gets (`acp-subagents.ts`).
///
/// `Rfd` is the ACP subagents RFD (a non-AIR client that declared
/// `clientCapabilities.subagents`); `Air` is the earlier AIR draft, which AIR
/// gets whichever capability it declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentForm {
    Rfd,
    Air,
}

/// The client choices read once from `initialize.clientCapabilities`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientCapabilities {
    /// The client renders a terminal from `_meta.terminal_info`,
    /// `terminal_output` and `terminal_exit` (`terminal_output` or
    /// `terminal_output_delta`, or a v2 client).
    pub terminal_output: bool,
    /// The client appends `_meta.terminal_output_delta` instead of
    /// `terminal_output` chunks.
    pub terminal_output_delta: bool,
    /// The client accepts an exact git patch in a diff (`jetbrains.air`
    /// `diffPatch`).
    pub diff_patch: bool,
    /// AIR renders `rawInput` itself and needs no display copy of the input.
    pub raw_input_rendering: bool,
    /// AIR reads the ExitPlanMode plan from `rawInput.planFilePath`.
    pub plan_file: bool,
    /// AIR gets `recommendedValue` entries on select config options.
    pub recommended_value: bool,
    /// The client advertised boolean session config options
    /// (`session.configOptions.boolean`).
    pub boolean_config: bool,
    /// The client can present live `notice` updates
    /// (`session.notices` is an object).
    pub notices: bool,
    /// The client accepts ID-addressed compaction updates
    /// (`session.compaction` is an object).
    pub compaction: bool,
    /// The form of native subagent sessions, when the client gets them.
    pub subagent_form: Option<SubagentForm>,
    /// The client wants the transcript of a native subagent
    /// (`_meta["subagent-transcript"]`).
    pub subagent_transcript: bool,
    /// The client supports form-based elicitation (`elicitation.form` truthy).
    pub elicitation_form: bool,
    /// The client supports URL-based elicitation (`elicitation.url` truthy).
    pub elicitation_url: bool,
    /// AIR wants the `_meta.jetbrains.air.loadReplayEnd` marker after a
    /// complete history replay.
    pub load_replay_end: bool,
    /// The client supports `terminal` auth methods (`auth.terminal === true`).
    pub auth_terminal: bool,
    /// The client supports the `_meta["terminal-auth"]` login command
    /// (`clientCapabilities._meta["terminal-auth"] === true`).
    pub meta_terminal_auth: bool,
    /// The client supports the custom-gateway auth methods
    /// (`auth._meta.gateway === true`).
    pub auth_gateway: bool,
    /// The client declared `_meta.jetbrains.air` (an AIR client).
    pub air_client: bool,
    /// The client speaks ACP v2. Rust does not serve v2 yet (G10), so this is
    /// always `false`; it exists so the tool-call capability object has the
    /// same shape as Node's.
    pub v2: bool,
}

/// Read a nested key from a JSON value, returning `None` for any missing link.
fn nested<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in keys {
        current = current.get(*key)?;
    }
    Some(current)
}

/// JavaScript truthiness for the values that appear in capability JSON.
///
/// Node reads `elicitation.form`/`url` with `if (capability)` and coerces with
/// `!!`; an empty object is truthy, `null`, `false`, `0` and `""` are not.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(boolean)) => *boolean,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// Whether `value` is a JSON object (Node's
/// `typeof value === "object" && value !== null && !Array.isArray(value)`).
fn is_object(value: Option<&Value>) -> bool {
    value.is_some_and(Value::is_object)
}

/// Whether `value` is exactly the boolean `true` (Node's `=== true`).
fn is_true(value: Option<&Value>) -> bool {
    value.is_some_and(|v| v == &Value::Bool(true))
}

impl ClientCapabilities {
    /// Parse `initialize.clientCapabilities`.
    ///
    /// `None`/absent capabilities parse to the all-false default, matching
    /// Node's optional reads (`capabilities?.session?.notices`).
    pub fn from_value(capabilities: Option<&Value>) -> Self {
        let Some(capabilities) = capabilities else {
            return Self::default();
        };
        let air = AirClient::from_client_capabilities(capabilities);
        let terminal_output_delta =
            is_true(nested(capabilities, &["_meta", "terminal_output_delta"]));
        let terminal_output =
            terminal_output_delta || is_true(nested(capabilities, &["_meta", "terminal_output"]));

        let subagents_rfd = is_object(nested(capabilities, &["subagents"]));
        let subagents_air = air
            .as_ref()
            .is_some_and(|client| client.supports("nativeSubagentSessions"));
        let subagent_form = if subagents_rfd || subagents_air {
            if air.is_some() {
                Some(SubagentForm::Air)
            } else {
                Some(SubagentForm::Rfd)
            }
        } else {
            None
        };

        Self {
            terminal_output,
            terminal_output_delta,
            diff_patch: air.as_ref().is_some_and(|a| a.supports("diffPatch")),
            raw_input_rendering: air
                .as_ref()
                .is_some_and(|a| a.supports("rawInputRendering")),
            plan_file: air.as_ref().is_some_and(|a| a.supports("planFile")),
            recommended_value: air.as_ref().is_some_and(|a| a.supports("recommendedValue")),
            boolean_config: nested(capabilities, &["session", "configOptions", "boolean"])
                .is_some_and(|value| !value.is_null()),
            notices: is_object(nested(capabilities, &["session", "notices"])),
            compaction: is_object(nested(capabilities, &["session", "compaction"])),
            subagent_form,
            subagent_transcript: is_true(nested(capabilities, &["_meta", "subagent-transcript"])),
            elicitation_form: truthy(nested(capabilities, &["elicitation", "form"])),
            elicitation_url: truthy(nested(capabilities, &["elicitation", "url"])),
            load_replay_end: air.as_ref().is_some_and(|a| a.supports("loadReplayEnd")),
            auth_terminal: is_true(nested(capabilities, &["auth", "terminal"])),
            meta_terminal_auth: is_true(nested(capabilities, &["_meta", "terminal-auth"])),
            auth_gateway: is_true(nested(capabilities, &["auth", "_meta", "gateway"])),
            air_client: air.is_some(),
            v2: false,
        }
    }

    /// Whether the client can present live notices.
    pub fn supports_notices(&self) -> bool {
        self.notices
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn absent_or_malformed_capabilities_are_all_false() {
        let empty = ClientCapabilities::from_value(None);
        assert_eq!(empty, ClientCapabilities::default());
        let empty = ClientCapabilities::from_value(Some(&json!(null)));
        assert_eq!(empty, ClientCapabilities::default());
        let empty = ClientCapabilities::from_value(Some(&json!("terminal")));
        assert_eq!(empty, ClientCapabilities::default());
    }

    #[test]
    fn terminal_output_reads_both_meta_spellings() {
        let plain = ClientCapabilities::from_value(Some(&json!({})));
        assert!(!plain.terminal_output);
        assert!(!plain.terminal_output_delta);

        let output = ClientCapabilities::from_value(Some(&json!({
            "_meta": {"terminal_output": true}
        })));
        assert!(output.terminal_output);
        assert!(!output.terminal_output_delta);

        let delta = ClientCapabilities::from_value(Some(&json!({
            "_meta": {"terminal_output_delta": true}
        })));
        assert!(delta.terminal_output);
        assert!(delta.terminal_output_delta);
    }

    #[test]
    fn air_capabilities_are_read_from_the_declared_list() {
        let caps = ClientCapabilities::from_value(Some(&json!({
            "_meta": {"jetbrains": {"air": {
                "version": 1,
                "capabilities": [
                    "diffPatch",
                    "rawInputRendering",
                    "planFile",
                    "recommendedValue",
                    "loadReplayEnd",
                    "nativeSubagentSessions"
                ]
            }}}
        })));
        assert!(caps.air_client);
        assert!(caps.diff_patch);
        assert!(caps.raw_input_rendering);
        assert!(caps.plan_file);
        assert!(caps.recommended_value);
        assert!(caps.load_replay_end);
        assert_eq!(caps.subagent_form, Some(SubagentForm::Air));
    }

    #[test]
    fn air_float_version_is_accepted_like_number_is_integer() {
        // Number.isInteger(1.0) is true; `as_u64` alone would reject the
        // float spelling and drop every AIR capability.
        let caps = ClientCapabilities::from_value(Some(&json!({
            "_meta": {"jetbrains": {"air": {
                "version": 1.0,
                "capabilities": ["diffPatch"]
            }}}
        })));
        assert!(caps.air_client);
        assert!(caps.diff_patch);
    }

    #[test]
    fn air_absent_or_malformed_declares_nothing() {
        let caps = ClientCapabilities::from_value(Some(&json!({
            "_meta": {"jetbrains": {"air": []}}
        })));
        assert!(!caps.air_client);
        assert!(!caps.diff_patch);
        assert_eq!(caps.subagent_form, None);
    }

    #[test]
    fn notices_and_compaction_require_objects() {
        let caps = ClientCapabilities::from_value(Some(&json!({
            "session": {"notices": {}, "compaction": {}}
        })));
        assert!(caps.notices);
        assert!(caps.compaction);

        for value in [json!(null), json!([]), json!("yes"), json!(true)] {
            let caps = ClientCapabilities::from_value(Some(&json!({
                "session": {"notices": value, "compaction": value}
            })));
            assert!(!caps.notices);
            assert!(!caps.compaction);
        }
    }

    #[test]
    fn boolean_config_reads_the_non_null_presence() {
        let caps = ClientCapabilities::from_value(Some(&json!({
            "session": {"configOptions": {"boolean": {}}}
        })));
        assert!(caps.boolean_config);
        // `!= null`: a `false` payload still opts in.
        let caps = ClientCapabilities::from_value(Some(&json!({
            "session": {"configOptions": {"boolean": false}}
        })));
        assert!(caps.boolean_config);
        let caps = ClientCapabilities::from_value(Some(&json!({
            "session": {"configOptions": {"boolean": null}}
        })));
        assert!(!caps.boolean_config);
        let caps = ClientCapabilities::from_value(Some(&json!({
            "session": {"configOptions": {}}
        })));
        assert!(!caps.boolean_config);
    }

    #[test]
    fn subagents_follow_the_rfd_or_air_form() {
        let rfd = ClientCapabilities::from_value(Some(&json!({"subagents": {}})));
        assert_eq!(rfd.subagent_form, Some(SubagentForm::Rfd));

        let air = ClientCapabilities::from_value(Some(&json!({
            "_meta": {"jetbrains": {"air": {
                "version": 1,
                "capabilities": ["nativeSubagentSessions"]
            }}}
        })));
        assert_eq!(air.subagent_form, Some(SubagentForm::Air));

        let none = ClientCapabilities::from_value(Some(&json!({"subagents": []})));
        assert_eq!(none.subagent_form, None);
    }

    #[test]
    fn subagent_transcript_reads_the_root_meta_flag() {
        let caps = ClientCapabilities::from_value(Some(&json!({
            "_meta": {"subagent-transcript": true}
        })));
        assert!(caps.subagent_transcript);
        let caps = ClientCapabilities::from_value(Some(&json!({
            "_meta": {"subagent-transcript": "yes"}
        })));
        assert!(!caps.subagent_transcript);
    }

    #[test]
    fn elicitation_form_and_url_are_truthy() {
        let caps = ClientCapabilities::from_value(Some(&json!({
            "elicitation": {"form": {}, "url": {}}
        })));
        assert!(caps.elicitation_form);
        assert!(caps.elicitation_url);

        let caps = ClientCapabilities::from_value(Some(&json!({
            "elicitation": {"form": null, "url": false}
        })));
        assert!(!caps.elicitation_form);
        assert!(!caps.elicitation_url);
    }

    #[test]
    fn auth_capabilities_are_strict_true() {
        let caps = ClientCapabilities::from_value(Some(&json!({
            "auth": {"terminal": true},
            "_meta": {"terminal-auth": true}
        })));
        assert!(caps.auth_terminal);
        assert!(caps.meta_terminal_auth);
        assert!(!caps.auth_gateway);

        let caps = ClientCapabilities::from_value(Some(&json!({
            "auth": {"terminal": "true", "_meta": {"gateway": true}},
            "_meta": {"terminal-auth": 1}
        })));
        assert!(!caps.auth_terminal);
        assert!(!caps.meta_terminal_auth);
        assert!(caps.auth_gateway);
    }
}
