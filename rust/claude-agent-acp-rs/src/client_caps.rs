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

use serde_json::{json, Value};

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
    /// AIR session failure extension (`jetbrains.air` `sessionFailure`).
    pub session_failure: bool,
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
            session_failure: air.as_ref().is_some_and(|a| a.supports("sessionFailure")),
            v2: false,
        }
    }

    /// Whether the client can present live notices.
    pub fn supports_notices(&self) -> bool {
        self.notices
    }

    /// Whether a tool call gets a display terminal: a command, for a client
    /// that renders terminals (`hasTerminal`, `renderer.ts:173-175`).
    pub fn has_terminal(&self, is_command: bool) -> bool {
        is_command && self.terminal_output
    }

    /// The `_meta` fragment announcing a command's display terminal on its
    /// first report (`renderer.ts:246`): `terminal_info` under the tool_use id.
    pub fn terminal_info_meta(&self, is_command: bool, tool_use_id: &str) -> Option<Value> {
        self.has_terminal(is_command)
            .then(|| json!({ "terminal_info": { "terminal_id": tool_use_id } }))
    }

    /// The `_meta` that ends the display terminal of a command whose tool call
    /// gets no result (`terminalExit`, `renderer.ts:684-692`). The exit code is
    /// unknown: `null`, or `1` for AIR, which always gets a number.
    pub fn terminal_exit_meta(&self, is_command: bool, tool_use_id: &str) -> Option<Value> {
        self.has_terminal(is_command).then(|| {
            json!({ "terminal_exit": {
                "terminal_id": tool_use_id,
                "exit_code": if self.air_client { json!(1) } else { Value::Null },
                "signal": Value::Null,
            }})
        })
    }

    /// The terminal reports of a finished command (`renderer.ts:391-418`).
    ///
    /// The terminal was announced under the tool_use id, so the output keys off
    /// that id (the result's `tool_use_id` when the tool_use carries none; a
    /// non-string or empty id is no id). `None` means nothing can match the
    /// output (or the client renders no terminal): the caller renders the
    /// output as a code block instead.
    pub fn terminal_result(
        &self,
        tool_use_id: Option<&str>,
        result_tool_use_id: Option<&str>,
        command: &CommandOutcome,
        is_error: bool,
    ) -> Option<TerminalResult> {
        let id = [tool_use_id, result_tool_use_id]
            .into_iter()
            .flatten()
            .find(|id| !id.is_empty())?;
        if !self.terminal_output {
            return None;
        }
        let data = json!({ "terminal_id": id, "data": command.output });
        let output_meta = if self.terminal_output_delta {
            json!({ "terminal_output_delta": data })
        } else {
            json!({ "terminal_output": data })
        };
        // AIR keeps the numbers it always got: 1 for a failed or interrupted
        // command, else the exit code, or 0 when unknown.
        let exit_code = if self.air_client {
            json!(if is_error || command.interrupted {
                1
            } else {
                command.exit_code.unwrap_or(0)
            })
        } else {
            command.exit_code.map_or(Value::Null, |code| json!(code))
        };
        Some(TerminalResult {
            // A client that is not AIR gets the terminal content again, like
            // upstream: it replaces a denial text that an earlier update showed.
            content: (!self.air_client).then(|| json!({ "type": "terminal", "terminalId": id })),
            output_meta,
            exit_meta: json!({ "terminal_exit": {
                "terminal_id": id,
                "exit_code": exit_code,
                "signal": Value::Null,
            }}),
        })
    }
}

/// What a finished command reported (`ToolResultFacts.command`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutcome {
    pub output: String,
    pub exit_code: Option<i64>,
    pub interrupted: bool,
}

/// The terminal reports of one finished command: the terminal `content` block
/// (non-AIR clients only), the `terminal_output(_delta)` meta sent as its own
/// report first, and the `terminal_exit` meta carried by the status report.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalResult {
    pub content: Option<Value>,
    pub output_meta: Value,
    pub exit_meta: Value,
}

/// The `authMethods` of the `initialize` response (`acp-agent.ts:2844-2947`):
/// the terminal login methods for a client that declared `auth.terminal` or
/// `_meta["terminal-auth"]`, then the two custom-gateway methods for a client
/// that declared `auth._meta.gateway`.
///
/// The remote branch (`NO_BROWSER`/`SSH_*`/`CLAUDE_CODE_REMOTE`) offers the TUI
/// login instead of the browser redirect; the claude.ai login is dropped under
/// `--hide-claude-auth`. `executable`/`argv` are the process entrypoint used by
/// `_meta["terminal-auth"]` (`process.execPath`, `process.argv.slice(1)`).
pub fn auth_methods(
    caps: &ClientCapabilities,
    env: &crate::env_snapshot::EnvSnapshot,
    executable: &str,
    argv: &[String],
    hide_claude_auth: bool,
) -> Vec<Value> {
    let mut methods = terminal_auth_methods(caps, env, executable, argv, hide_claude_auth);
    if caps.auth_gateway {
        methods.extend(gateway_auth_methods());
    }
    methods
}

fn terminal_auth_methods(
    caps: &ClientCapabilities,
    env: &crate::env_snapshot::EnvSnapshot,
    executable: &str,
    argv: &[String],
    hide_claude_auth: bool,
) -> Vec<Value> {
    let supports_terminal = caps.auth_terminal;
    let supports_meta = caps.meta_terminal_auth;
    if !supports_terminal && !supports_meta {
        return Vec::new();
    }
    let is_remote = env.flag("NO_BROWSER")
        || env.flag("SSH_CONNECTION")
        || env.flag("SSH_CLIENT")
        || env.flag("SSH_TTY")
        || env.flag("CLAUDE_CODE_REMOTE");

    let terminal_auth = |extra: &[&str], label: &str| -> Value {
        let args: Vec<String> = argv
            .iter()
            .cloned()
            .chain(extra.iter().map(|arg| (*arg).to_string()))
            .collect();
        json!({ "command": executable, "args": args, "label": label })
    };

    if is_remote {
        let mut method = json!({
            "description": "Run `claude /login` in the terminal",
            "name": "Log in with Claude",
            "id": "claude-login",
            "type": "terminal",
            "args": ["--cli"],
        });
        if supports_meta {
            method["_meta"] = json!({
                "terminal-auth": terminal_auth(&["--cli"], "Claude Login"),
            });
        }
        return if hide_claude_auth {
            Vec::new()
        } else {
            vec![method]
        };
    }

    let mut claude_login = json!({
        "description": "Use Claude subscription ",
        "name": "Claude Subscription",
        "id": "claude-ai-login",
        "type": "terminal",
        "args": ["--cli", "auth", "login", "--claudeai"],
    });
    let mut console_login = json!({
        "description": "Use Anthropic Console (API usage billing)",
        "name": "Anthropic Console",
        "id": "console-login",
        "type": "terminal",
        "args": ["--cli", "auth", "login", "--console"],
    });
    if supports_meta {
        claude_login["_meta"] = json!({
            "terminal-auth": terminal_auth(
                &["--cli", "auth", "login", "--claudeai"],
                "Claude Login",
            ),
        });
        console_login["_meta"] = json!({
            "terminal-auth": terminal_auth(
                &["--cli", "auth", "login", "--console"],
                "Anthropic Console Login",
            ),
        });
    }
    let mut methods = Vec::new();
    if !hide_claude_auth {
        methods.push(claude_login);
    }
    methods.push(console_login);
    methods
}

fn gateway_auth_methods() -> Vec<Value> {
    ["anthropic", "bedrock"]
        .into_iter()
        .map(|protocol| {
            json!({
                "id": if protocol == "anthropic" { "gateway" } else { "gateway-bedrock" },
                "name": "Custom model gateway",
                "description": "Use a custom gateway to authenticate and access models",
                "_meta": {"gateway": {"protocol": protocol}},
            })
        })
        .collect()
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
                    "sessionFailure",
                    "nativeSubagentSessions"
                ]
            }}}
        })));
        assert!(caps.air_client);
        assert!(caps.session_failure);
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
