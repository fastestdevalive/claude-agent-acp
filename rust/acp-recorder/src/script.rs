//! The recorder's driving script (item 1.2).
//!
//! A script is a JSON document describing an ordered list of ACP calls to make
//! against the agent under test, plus a permission reply policy.

use std::path::PathBuf;

use serde::Deserialize;

/// How the recorder answers `session/request_permission` requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionPolicy {
    /// Select the first permission option offered.
    Allow,
    /// Select the first reject-kind option, or cancel if none is offered.
    Deny,
}

/// One ACP call to issue.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "call", rename_all = "lowercase")]
pub enum Step {
    /// `initialize`
    #[serde(rename = "initialize")]
    Initialize,
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
    /// `session/cancel` notification on the current session.
    #[serde(rename = "session/cancel")]
    Cancel,
    /// Await the responses of every `no_wait` prompt issued so far.
    #[serde(rename = "session/wait")]
    WaitPrompts,
}

/// A full recording script.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Script {
    /// Permission reply policy for the whole run.
    #[serde(default = "default_permission")]
    pub permission: PermissionPolicy,
    /// The ordered ACP calls to issue.
    pub steps: Vec<Step>,
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
        assert_eq!(script.steps[0], Step::Initialize);
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
    fn rejects_unknown_call() {
        assert!(Script::parse(r#"{"steps": [{"call": "session/bogus"}]}"#).is_err());
    }
}
