//! The recorder's driving script (item 1.2).
//!
//! A script is a JSON document describing an ordered list of ACP calls to make
//! against the agent under test, plus a permission reply policy.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

/// How the recorder answers `session/request_permission` requests.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionPolicy {
    /// Select the first permission option offered.
    Allow,
    /// Select the first reject-kind option, or cancel if none is offered.
    Deny,
    /// Select the first `allow_always`-kind option (the durable choice, item
    /// 3.9), falling back to the `allow_once` option when none is offered.
    #[serde(rename = "allow_always")]
    AllowAlways,
    /// Select the exact option whose `optionId` matches (Fix 1): lets a script
    /// choose a specific ExitPlanMode selection (e.g. `exit-plan-default` vs the
    /// elevated modes) regardless of its kind. Cancels when no offered option
    /// matches.
    #[serde(rename = "option_id")]
    OptionId(String),
}

/// One ACP call to issue.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "call", rename_all = "lowercase")]
pub enum Step {
    /// `initialize`
    #[serde(rename = "initialize")]
    Initialize {
        /// Optional `clientCapabilities._meta` to advertise (item 3.1): the
        /// AIR scripts declare `{"jetbrains":{"air":{...}}}` here so the agent
        /// under test treats this recorder run as an AIR client.
        #[serde(default)]
        client_capabilities_meta: Option<Value>,
        /// The WHOLE `clientCapabilities` object (F1, additive): `fs`,
        /// `terminal`, `session`, `auth`, `elicitation`, `_meta`, ... Existing
        /// scripts that only set `client_capabilities_meta` keep working; when
        /// both are present the explicit `_meta` is merged over the object's.
        #[serde(default)]
        client_capabilities: Option<Value>,
    },
    /// `session/new`
    #[serde(rename = "session/new")]
    NewSession {
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    /// `session/load` (resume an existing session).
    #[serde(rename = "session/load")]
    LoadSession {
        session_id: String,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    /// `session/prompt` with a text payload.
    #[serde(rename = "session/prompt")]
    Prompt {
        text: String,
        #[serde(default)]
        cancel_after_updates: Option<usize>,
        /// Send the prompt without awaiting its response (`no_wait`), so it can
        /// sit queued/active while the script drives a cancel or a later prompt
        /// (the phase-8 "queued prompt" scenario). `session/wait` then settles
        /// every in-flight prompt.
        #[serde(default)]
        no_wait: bool,
    },
    /// `session/steer` — inject a `_session/steering` message into the running
    /// turn (item 2.4). `after_updates` is counted RELATIVE to the last
    /// prompt's start, so the steer cannot race the prompt's streamed updates.
    #[serde(rename = "session/steer")]
    Steer {
        text: String,
        #[serde(default)]
        after_updates: Option<usize>,
    },
    /// `session/cancel` notification on the current session.
    #[serde(rename = "session/cancel")]
    Cancel,
    /// Await the responses of every `no_wait` prompt issued so far.
    #[serde(rename = "session/wait")]
    WaitPrompts,
    /// Reply to the next agent->client `elicitation/create` request (F1, the
    /// F23a corpus lane). Use after a `no_wait` prompt; the agent's request is
    /// queued by the client handler and answered here. `response` is the JSON
    /// `CreateElicitationResponse` object: `{"action":"accept","content":{...}}`,
    /// `{"action":"decline"}` or `{"action":"cancel"}`.
    #[serde(rename = "elicit_reply")]
    ElicitReply { response: Value },
    /// Any ACP request with arbitrary params (rust-air-gaps Decision 1). String
    /// values in `params` expand `$SESSION`, `$CWD`, `$CWD_SLUG`.
    #[serde(rename = "request")]
    Request {
        method: String,
        #[serde(default)]
        params: Value,
        /// Hold the request until an agent line containing this text arrived.
        #[serde(default)]
        after_recv_contains: Option<String>,
        /// Adopt `result.sessionId` as the current session.
        #[serde(default)]
        capture_session_id: bool,
        /// An error response is expected and is not a script failure.
        #[serde(default)]
        expect_error: bool,
    },
    /// Pause the script.
    #[serde(rename = "sleep")]
    Sleep { ms: u64 },
}

/// A file the recorder writes under `CLAUDE_CONFIG_DIR` before the agent is
/// spawned, so both adapters see an identical transcript store.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SeedFile {
    /// Destination relative to `CLAUDE_CONFIG_DIR` (`$CWD`/`$CWD_SLUG`/`$SESSION` expand).
    pub path: String,
    /// Source relative to `porting/corpus/` (same expansions apply to content).
    pub from: String,
    /// Explicit mtime in seconds since the epoch.
    #[serde(default)]
    pub mtime: Option<u64>,
}

/// A full recording script.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Script {
    /// Permission reply policy for the whole run.
    #[serde(default = "default_permission")]
    pub permission: PermissionPolicy,
    /// The ordered ACP calls to issue.
    pub steps: Vec<Step>,
    /// Files seeded under `CLAUDE_CONFIG_DIR` before the agent starts.
    #[serde(default)]
    pub seed_files: Vec<SeedFile>,
    /// Capture the first `initialize` control_request into `<name>.init.json`.
    #[serde(default)]
    pub capture_init: bool,
}

fn default_permission() -> PermissionPolicy {
    PermissionPolicy::Allow
}

impl Script {
    /// Parse a script from its JSON serialization.
    pub fn parse(input: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ordered_calls_and_policy() {
        let script = Script::parse(
            r#"{
                "permission": "deny",
                "steps": [
                    {"call": "initialize"},
                    {"call": "session/new", "cwd": "/tmp"},
                    {"call": "session/prompt", "text": "hello"},
                    {"call": "session/cancel"}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(script.permission, PermissionPolicy::Deny);
        assert_eq!(script.steps.len(), 4);
        assert_eq!(
            script.steps[0],
            Step::Initialize {
                client_capabilities_meta: None,
                client_capabilities: None
            }
        );
        assert_eq!(
            script.steps[1],
            Step::NewSession {
                cwd: Some(PathBuf::from("/tmp"))
            }
        );
        assert_eq!(
            script.steps[2],
            Step::Prompt {
                text: "hello".to_string(),
                cancel_after_updates: None,
                no_wait: false
            }
        );
        assert_eq!(script.steps[3], Step::Cancel);
    }

    #[test]
    fn defaults_permission_to_allow() {
        let script = Script::parse(r#"{"steps": [{"call": "initialize"}]}"#).unwrap();
        assert_eq!(script.permission, PermissionPolicy::Allow);
    }

    #[test]
    fn parses_cancel_after_updates() {
        let script = Script::parse(
            r#"{"steps": [{"call": "session/prompt", "text": "x", "cancel_after_updates": 5}]}"#,
        )
        .unwrap();
        assert_eq!(
            script.steps[0],
            Step::Prompt {
                text: "x".to_string(),
                cancel_after_updates: Some(5),
                no_wait: false
            }
        );
    }

    #[test]
    fn parses_no_wait_and_wait_prompts() {
        let script = Script::parse(
            r#"{"steps": [
                {"call": "session/prompt", "text": "a", "no_wait": true},
                {"call": "session/prompt", "text": "b"},
                {"call": "session/wait"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            script.steps[0],
            Step::Prompt {
                text: "a".to_string(),
                cancel_after_updates: None,
                no_wait: true
            }
        );
        assert_eq!(
            script.steps[1],
            Step::Prompt {
                text: "b".to_string(),
                cancel_after_updates: None,
                no_wait: false
            }
        );
        assert_eq!(script.steps[2], Step::WaitPrompts);
    }

    #[test]
    fn parses_load_session_step() {
        let script = Script::parse(
            r#"{"steps":[{"call": "session/load", "session_id": "sess-123", "cwd": "/tmp"}]}"#,
        )
        .unwrap();
        assert_eq!(
            script.steps[0],
            Step::LoadSession {
                session_id: "sess-123".to_string(),
                cwd: Some(PathBuf::from("/tmp"))
            }
        );
    }

    #[test]
    fn parses_steer_step() {
        let script = Script::parse(
            r#"{"steps": [
                {"call": "session/prompt", "text": "write a long essay", "no_wait": true},
                {"call": "session/steer", "text": "actually be brief", "after_updates": 2},
                {"call": "session/steer", "text": "no count"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            script.steps[1],
            Step::Steer {
                text: "actually be brief".to_string(),
                after_updates: Some(2)
            }
        );
        assert_eq!(
            script.steps[2],
            Step::Steer {
                text: "no count".to_string(),
                after_updates: None
            }
        );
    }

    #[test]
    fn rejects_unknown_call() {
        assert!(Script::parse(r#"{"steps": [{"call": "session/bogus"}]}"#).is_err());
    }

    #[test]
    fn parses_initialize_with_client_meta() {
        let script = Script::parse(
            r#"{"steps": [{"call": "initialize", "client_capabilities_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["asyncTasks"]}}}}]}"#,
        )
        .unwrap();
        assert_eq!(
            script.steps[0],
            Step::Initialize {
                client_capabilities_meta: Some(serde_json::json!({
                    "jetbrains": {"air": {"version": 1, "capabilities": ["asyncTasks"]}}
                })),
                client_capabilities: None
            }
        );
    }

    /// F1: the whole `clientCapabilities` object is carried, additively (the
    /// meta-only form above keeps working unchanged).
    #[test]
    fn parses_initialize_with_whole_client_capabilities() {
        let script = Script::parse(
            r#"{"steps": [{"call": "initialize", "client_capabilities": {
                "fs": {"readTextFile": true, "writeTextFile": true},
                "terminal": true,
                "elicitation": {"form": {}, "url": {}},
                "_meta": {"jetbrains": {"air": {"version": 1}}}
            }}]}"#,
        )
        .unwrap();
        let Step::Initialize {
            client_capabilities,
            ..
        } = &script.steps[0]
        else {
            panic!("expected an initialize step");
        };
        let caps = client_capabilities.as_ref().expect("whole object present");
        assert_eq!(caps["terminal"], serde_json::json!(true));
        assert_eq!(
            caps["fs"]["readTextFile"],
            serde_json::json!(true),
            "fs capability carried verbatim"
        );
        assert_eq!(caps["elicitation"]["form"], serde_json::json!({}));
        assert_eq!(caps["_meta"]["jetbrains"]["air"]["version"], 1);
    }

    /// F1: both capability forms may be set; the explicit `_meta` is merged over
    /// the whole object's `_meta` (existing AIR scripts stay valid).
    #[test]
    fn parses_initialize_with_both_capability_forms() {
        let script = Script::parse(
            r#"{"steps": [{"call": "initialize",
                "client_capabilities": {"terminal": true, "_meta": {"jetbrains": {"air": {"version": 1}}}},
                "client_capabilities_meta": {"jetbrains": {"air": {"capabilities": ["asyncTasks"]}}}
            }]}"#,
        )
        .unwrap();
        let Step::Initialize {
            client_capabilities_meta,
            client_capabilities,
        } = &script.steps[0]
        else {
            panic!("expected an initialize step");
        };
        assert!(client_capabilities.is_some());
        assert!(client_capabilities_meta.is_some());
    }

    /// F1: `elicit_reply` carries the JSON `CreateElicitationResponse` for the
    /// F23a corpus lane (accept with content, decline, cancel).
    #[test]
    fn parses_elicit_reply_actions() {
        let script = Script::parse(
            r#"{"steps": [
                {"call": "elicit_reply", "response": {"action": "accept", "content": {"choice": "a"}}},
                {"call": "elicit_reply", "response": {"action": "decline"}},
                {"call": "elicit_reply", "response": {"action": "cancel"}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            script.steps[0],
            Step::ElicitReply {
                response: serde_json::json!({"action": "accept", "content": {"choice": "a"}})
            }
        );
        assert_eq!(
            script.steps[1],
            Step::ElicitReply {
                response: serde_json::json!({"action": "decline"})
            }
        );
        assert_eq!(
            script.steps[2],
            Step::ElicitReply {
                response: serde_json::json!({"action": "cancel"})
            }
        );
    }

    #[test]
    fn parses_request_sleep_seed_and_capture_init() {
        let script = Script::parse(
            r#"{"capture_init": true,
                "seed_files": [{"path": "projects/$CWD_SLUG/a.jsonl", "from": "seeds/a.jsonl", "mtime": 5}],
                "steps": [
                  {"call": "request", "method": "session/new", "params": {"cwd": "$CWD"}, "capture_session_id": true},
                  {"call": "request", "method": "m", "after_recv_contains": "x", "expect_error": true},
                  {"call": "sleep", "ms": 10}
                ]}"#,
        )
        .unwrap();
        assert!(script.capture_init);
        assert_eq!(script.seed_files[0].mtime, Some(5));
        assert!(matches!(
            &script.steps[0],
            Step::Request {
                capture_session_id: true,
                ..
            }
        ));
        assert!(matches!(
            &script.steps[1],
            Step::Request {
                expect_error: true,
                after_recv_contains: Some(_),
                ..
            }
        ));
        assert_eq!(script.steps[2], Step::Sleep { ms: 10 });
    }

    #[test]
    fn parses_permission_allow_always() {
        let script =
            Script::parse(r#"{"permission": "allow_always", "steps": [{"call": "initialize"}]}"#)
                .unwrap();
        assert_eq!(script.permission, PermissionPolicy::AllowAlways);
    }

    #[test]
    fn parses_permission_option_id() {
        let script = Script::parse(
            r#"{"permission": {"option_id": "exit-plan-default"}, "steps": [{"call": "initialize"}]}"#,
        )
        .unwrap();
        assert_eq!(
            script.permission,
            PermissionPolicy::OptionId("exit-plan-default".to_string())
        );
    }
}
