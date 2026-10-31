//! Port of `dist/goal-extension.js` (`src/goal-extension.ts`, upstream 0.94.0)
//! and the goal bookkeeping the ACP agent keeps per session (rust-air-gaps
//! G8 / Decision 16, rows M-93, U-07, S-21, N-15, N-52, Q-16).
//!
//! `/goal` is a Claude Code local command: the CLI sets or clears the
//! persistent goal of the conversation. The adapter mirrors it on the wire as
//! an AIR-only `session_info_update` (`_meta.jetbrains.air.goal`): a submitted
//! `/goal …` prompt publishes the new goal optimistically, the runtime's
//! `active_goal` frame refines it with the provider-neutral snapshot, and a
//! failed command rolls the optimistic update back. `_session/goal` is the
//! control method for the same thing: it steers (or prompts) `/goal
//! <objective>` / `/goal clear`.
//!
//! Everything here is pure (`inv_53_*`); the actor owns a [`GoalState`] and
//! decides WHEN to publish, this module only decides WHAT to publish.

use serde_json::{Map, Value};

use agent_client_protocol::Error;

/// The extension version advertised in `_meta.jetbrains.air.version`.
pub const GOAL_EXTENSION_VERSION: u64 = 1;

/// The control method advertised in `_meta.jetbrains.air.goal.controlMethod`.
pub const GOAL_CONTROL_METHOD: &str = "_session/goal";

/// The actions `_session/goal` accepts, in the advertised order.
pub const GOAL_ACTIONS: [&str; 2] = ["set", "clear"];

/// The one ACP shape of a goal (`GoalSnapshot`): the provider's own fields
/// (`condition`, `tokens_at_start`, …) never reach the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalSnapshot {
    /// The objective, trimmed (`toGoalSnapshot` / `goalUpdateFromPrompt`).
    pub objective: String,
    /// Always `"active"` in 0.94.0.
    pub status: &'static str,
    /// The runtime's iteration count, copied verbatim (`None` = `undefined`,
    /// which the upstream JSON omits).
    pub iterations: Option<Value>,
    /// `Some(_)` renders `lastReason` (the runtime snapshot always carries the
    /// key, `null` for an absent `last_reason`); `None` omits it (the
    /// optimistic snapshot does).
    pub last_reason: Option<Value>,
    /// The runtime's `set_at`, copied verbatim (`None` = `undefined`).
    pub created_at: Option<Value>,
}

impl GoalSnapshot {
    /// The optimistic snapshot published for a submitted `/goal <objective>`
    /// prompt (`goalUpdateFromPrompt`): objective, status, controlMethod only.
    #[must_use]
    pub fn optimistic(objective: &str) -> Self {
        Self {
            objective: objective.to_string(),
            status: "active",
            iterations: None,
            last_reason: None,
            created_at: None,
        }
    }

    /// The provider-neutral snapshot of an `active_goal` message
    /// (`toGoalSnapshot`), or `None` for the runtime's `value: null` clear.
    #[must_use]
    pub fn from_active_goal(message: &Value) -> Option<Self> {
        let value = message.get("value")?;
        if value.is_null() {
            return None;
        }
        let objective = value
            .get("condition")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        Some(Self {
            objective,
            status: "active",
            iterations: value.get("iterations").cloned(),
            // `value.last_reason ?? null`: the key is always present.
            last_reason: Some(value.get("last_reason").cloned().unwrap_or(Value::Null)),
            created_at: value.get("set_at").cloned(),
        })
    }

    /// The JSON object of the snapshot, in the upstream key order.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("objective".into(), Value::String(self.objective.clone()));
        map.insert("status".into(), Value::String(self.status.to_string()));
        if let Some(iterations) = &self.iterations {
            map.insert("iterations".into(), iterations.clone());
        }
        if let Some(last_reason) = &self.last_reason {
            map.insert("lastReason".into(), last_reason.clone());
        }
        if let Some(created_at) = &self.created_at {
            map.insert("createdAt".into(), created_at.clone());
        }
        map.insert(
            "controlMethod".into(),
            Value::String(GOAL_CONTROL_METHOD.to_string()),
        );
        Value::Object(map)
    }
}

/// What a prompt does to the goal (`goalUpdateFromPrompt`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptGoal {
    /// Not a state-changing `/goal` command: no publish.
    Unchanged,
    /// `/goal clear`: publish `goal: null`.
    Clear,
    /// `/goal <objective>`: publish the optimistic snapshot.
    Set(Box<GoalSnapshot>),
}

/// JS `\s` for the `/goal` regex.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
    )
}

/// Port of `goalUpdateFromPrompt`: only a prompt that IS a `/goal` command
/// changes the goal (`/^\/goal(?:\s+([\s\S]*))?$/`), and only with a non-empty
/// argument. A leading space, an inner mention, or a bare `/goal` does not.
#[must_use]
pub fn goal_update_from_prompt(prompt: &str) -> PromptGoal {
    let Some(rest) = prompt.strip_prefix("/goal") else {
        return PromptGoal::Unchanged;
    };
    // The optional group requires one or more whitespace characters; a bare
    // `/goal` (or `/goalx`) matches only with no argument.
    let argument = if rest.is_empty() {
        ""
    } else if rest.starts_with(is_js_space) {
        rest
    } else {
        return PromptGoal::Unchanged;
    };
    let argument = argument.trim_matches(is_js_space);
    if argument.is_empty() {
        return PromptGoal::Unchanged;
    }
    if argument == "clear" {
        PromptGoal::Clear
    } else {
        PromptGoal::Set(Box::new(GoalSnapshot::optimistic(argument)))
    }
}

/// One validated `_session/goal` request (`GoalRequest` / `parseGoalRequest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalRequest {
    pub session_id: String,
    pub action: GoalAction,
    /// Trimmed, non-empty, for `action == Set`.
    pub objective: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalAction {
    Set,
    Clear,
}

impl GoalRequest {
    /// The `/goal …` command the request turns into (`goal()`).
    #[must_use]
    pub fn command_text(&self) -> String {
        match self.action {
            GoalAction::Set => format!("/goal {}", self.objective.as_deref().unwrap_or_default()),
            GoalAction::Clear => "/goal clear".to_string(),
        }
    }
}

/// Port of `parseGoalRequest` (`goal-extension.ts:52-69`).
///
/// Every rejection is `RequestError.invalidParams(undefined, "<text>")`: code
/// `-32602`, message `"Invalid params: <text>"`, and no `data`. JS arrays are
/// objects (`typeof [] === "object"`), so an array falls through to the
/// `sessionId` check instead of failing the object check.
pub fn parse_goal_request(params: &Value) -> Result<GoalRequest, Error> {
    let invalid = |text: &str| Error::new(-32602, format!("Invalid params: {text}"));
    if params.is_null() || params.is_string() || params.is_number() || params.is_boolean() {
        return Err(invalid("goal params must be an object"));
    }
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("");
    if session_id.is_empty() {
        return Err(invalid("goal params require a non-empty sessionId"));
    }
    let action = match params.get("action").and_then(Value::as_str) {
        Some("set") => GoalAction::Set,
        Some("clear") => GoalAction::Clear,
        _ => return Err(invalid("goal action must be \"set\" or \"clear\"")),
    };
    if action == GoalAction::Set {
        let objective = params
            .get("objective")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if objective.is_empty() {
            return Err(invalid(
                "goal action \"set\" requires a non-empty objective",
            ));
        }
        return Ok(GoalRequest {
            session_id: session_id.to_string(),
            action,
            objective: Some(objective.to_string()),
        });
    }
    Ok(GoalRequest {
        session_id: session_id.to_string(),
        action,
        objective: None,
    })
}

/// The `session/update` body that carries a goal (or its removal) for an AIR
/// client (`publishGoal`, `acp-agent.ts:4199-4212`): `withAirMeta(undefined,
/// "goal", goal)` — `version: 1` next to the key, `goal: null` for a clear.
#[must_use]
pub fn goal_update_value(goal: Option<&GoalSnapshot>) -> Value {
    let goal = match goal {
        Some(snapshot) => snapshot.to_json(),
        None => Value::Null,
    };
    serde_json::json!({
        "sessionUpdate": "session_info_update",
        "_meta": {
            "jetbrains": {
                "air": {
                    "version": GOAL_EXTENSION_VERSION,
                    "goal": goal,
                }
            }
        }
    })
}

/// The update to put on the wire for one published goal: `None` for a non-AIR
/// client (`publishGoal` returns before `client.sessionUpdate`; the goal is an
/// AIR extension only).
#[must_use]
pub fn goal_publish(air: bool, goal: Option<&GoalSnapshot>) -> Option<Value> {
    if !air {
        return None;
    }
    Some(goal_update_value(goal))
}

/// A submitted `/goal …` command whose runtime update has not arrived yet.
///
/// While it is pending, an `active_goal` frame that does not match its
/// expectation is a stale update for the PREVIOUS goal and is suppressed
/// (`publishRuntimeGoal`, `acp-agent.ts:4269-4284`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingGoal {
    /// The prompt uuid of the command; the user echo carrying it marks the
    /// command as started.
    pub command_uuid: String,
    /// The goal the command expects (`None` for `/goal clear`).
    pub expected: Option<GoalSnapshot>,
    /// The goal published before the command (rollback target).
    pub previous: Option<GoalSnapshot>,
    /// Whether the CLI echoed the command (a result can now roll back).
    pub started: bool,
}

/// Per-session goal bookkeeping (`session.lastPublishedGoal` +
/// `session.pendingGoalUpdate`), owned by the session actor.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GoalState {
    /// The last goal published to the client (`None` for a clear / never).
    pub last_published: Option<GoalSnapshot>,
    /// The optimistic update awaiting its runtime confirmation.
    pub pending: Option<PendingGoal>,
}

impl GoalState {
    /// Record a submitted prompt and return the optimistic goal to publish
    /// (`publishGoalFromPrompt`). `None` means the prompt does not change the
    /// goal.
    pub fn note_prompt(
        &mut self,
        prompt: &str,
        command_uuid: &str,
    ) -> Option<Option<GoalSnapshot>> {
        let goal = match goal_update_from_prompt(prompt) {
            PromptGoal::Unchanged => return None,
            PromptGoal::Clear => None,
            PromptGoal::Set(snapshot) => Some(*snapshot),
        };
        self.pending = Some(PendingGoal {
            command_uuid: command_uuid.to_string(),
            expected: goal.clone(),
            previous: self.last_published.clone(),
            started: false,
        });
        self.last_published = goal.clone();
        Some(goal)
    }

    /// Mark the pending command started when the CLI echoes it
    /// (`acp-agent.ts:7728-7730`).
    pub fn note_started(&mut self, uuid: &str) {
        if let Some(pending) = &mut self.pending {
            if pending.command_uuid == uuid {
                pending.started = true;
            }
        }
    }

    /// Apply a runtime `active_goal` snapshot and return what to publish, or
    /// `None` when the update is stale and must be suppressed
    /// (`publishRuntimeGoal`). A matching update clears the pending state.
    pub fn note_runtime(&mut self, goal: Option<GoalSnapshot>) -> Option<Option<GoalSnapshot>> {
        if let Some(pending) = &self.pending {
            let matches = match (&pending.expected, &goal) {
                (None, None) => true,
                (Some(expected), Some(runtime)) => runtime.objective == expected.objective,
                _ => false,
            };
            if !matches {
                return None;
            }
            self.pending = None;
        }
        self.last_published = goal.clone();
        Some(goal)
    }

    /// Clear the pending state at the submitted command's own result, and
    /// return the goal to publish when that result failed (`is_error` or a
    /// `refusal`): the optimistic update is rolled back to the previously
    /// published goal (`acp-agent.ts:6853-6870`).
    pub fn note_result(&mut self, failed: bool) -> Option<Option<GoalSnapshot>> {
        if !self.pending.as_ref().is_some_and(|pending| pending.started) {
            return None;
        }
        let pending = self.pending.take()?;
        if failed {
            self.last_published = pending.previous.clone();
            Some(pending.previous)
        } else {
            None
        }
    }
}

/// The first text block of a Claude `user` frame (`firstText` in
/// `acp-agent.ts:3951`): `""` when the first block is not text.
#[must_use]
pub fn first_prompt_text(frame: &Value) -> &str {
    frame
        .pointer("/message/content/0")
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// upstream `acp-agent.test.ts` "derives only state-changing goal slash
    /// commands".
    #[test]
    fn inv_53_goal_update_from_prompt() {
        assert_eq!(goal_update_from_prompt("/goal clear"), PromptGoal::Clear);
        assert_eq!(goal_update_from_prompt("/goal"), PromptGoal::Unchanged);
        assert_eq!(
            goal_update_from_prompt("explain /goal clear"),
            PromptGoal::Unchanged
        );
        assert_eq!(
            goal_update_from_prompt(" /goal Finish the migration"),
            PromptGoal::Unchanged
        );
        // `/goalx` is not the command; the argument is trimmed.
        assert_eq!(goal_update_from_prompt("/goalx"), PromptGoal::Unchanged);
        assert_eq!(
            goal_update_from_prompt("/goal   Finish the migration  "),
            PromptGoal::Set(Box::new(GoalSnapshot::optimistic("Finish the migration")))
        );
        // `/goal pause` is a goal, not a control action (upstream validates).
        assert_eq!(
            goal_update_from_prompt("/goal pause"),
            PromptGoal::Set(Box::new(GoalSnapshot::optimistic("pause")))
        );
        // A bare `/goal` followed only by whitespace sets nothing.
        assert_eq!(
            goal_update_from_prompt("/goal \t \n"),
            PromptGoal::Unchanged
        );
    }

    /// upstream "validates goal control requests" + the four `-32602` texts.
    #[test]
    fn inv_53_parse_goal_request() {
        assert_eq!(
            parse_goal_request(&serde_json::json!({
                "sessionId": "test-session", "action": "clear"
            }))
            .unwrap(),
            GoalRequest {
                session_id: "test-session".to_string(),
                action: GoalAction::Clear,
                objective: None,
            }
        );
        assert_eq!(
            parse_goal_request(&serde_json::json!({
                "sessionId": "test-session", "action": "set", "objective": "  Replacement  "
            }))
            .unwrap()
            .command_text(),
            "/goal Replacement"
        );
        assert_eq!(
            parse_goal_request(&serde_json::json!({
                "sessionId": "test-session", "action": "set", "objective": "  Replacement  "
            }))
            .unwrap()
            .objective
            .as_deref(),
            Some("Replacement")
        );

        let cases: [(&str, Value); 4] = [
            ("goal params must be an object", Value::Null),
            (
                "goal params require a non-empty sessionId",
                serde_json::json!({"action": "clear"}),
            ),
            (
                "goal action must be \"set\" or \"clear\"",
                serde_json::json!({"sessionId": "s", "action": "pause"}),
            ),
            (
                "goal action \"set\" requires a non-empty objective",
                serde_json::json!({"sessionId": "s", "action": "set", "objective": "   "}),
            ),
        ];
        for (message, params) in cases {
            let err = parse_goal_request(&params).unwrap_err();
            assert_eq!(i32::from(err.code), -32602, "{message}");
            assert_eq!(err.message, format!("Invalid params: {message}"));
            assert_eq!(err.data, None, "{message} must carry no data");
        }
        // JS arrays are objects: an array reaches the sessionId check.
        let err = parse_goal_request(&serde_json::json!([])).unwrap_err();
        assert_eq!(
            err.message,
            "Invalid params: goal params require a non-empty sessionId"
        );
        // A whitespace-only sessionId is non-empty in JS (`length > 0`).
        assert!(parse_goal_request(&serde_json::json!({
            "sessionId": "  ", "action": "clear"
        }))
        .is_ok());
    }

    /// upstream "maps the SDK goal shape without exposing provider fields".
    #[test]
    fn inv_53_to_goal_snapshot() {
        let message = serde_json::json!({
            "type": "active_goal",
            "value": {
                "condition": "Goal",
                "iterations": 1,
                "set_at": 1710000000000u64,
                "tokens_at_start": 99
            },
            "uuid": "u",
            "session_id": "test-session"
        });
        assert_eq!(
            GoalSnapshot::from_active_goal(&message).unwrap().to_json(),
            serde_json::json!({
                "objective": "Goal",
                "status": "active",
                "iterations": 1,
                "lastReason": null,
                "createdAt": 1710000000000u64,
                "controlMethod": "_session/goal"
            })
        );
        // The runtime's clear is `value: null`.
        assert_eq!(
            GoalSnapshot::from_active_goal(&serde_json::json!({"value": null})),
            None
        );
        // Objective trimmed; last_reason preserved when present.
        let full = GoalSnapshot::from_active_goal(&serde_json::json!({
            "value": {
                "condition": "  Finish the migration  ",
                "iterations": 3,
                "set_at": 1710000000123u64,
                "tokens_at_start": 42,
                "last_reason": "Tests still need work"
            }
        }))
        .unwrap();
        assert_eq!(full.objective, "Finish the migration");
        assert_eq!(full.to_json()["lastReason"], "Tests still need work");
    }

    /// The optimistic publish records the pending command and its rollback
    /// target; a non-goal prompt publishes nothing.
    #[test]
    fn inv_53_optimistic_publish_and_pending() {
        let mut state = GoalState::default();
        assert_eq!(state.note_prompt("hello", "u0"), None);
        assert_eq!(state.pending, None);

        let published = state.note_prompt("/goal   Finish  ", "u1").unwrap();
        assert_eq!(
            published.unwrap().to_json(),
            serde_json::json!({
                "objective": "Finish",
                "status": "active",
                "controlMethod": "_session/goal"
            })
        );
        assert_eq!(state.last_published.unwrap().objective, "Finish");
        let pending = state.pending.clone().unwrap();
        assert_eq!(pending.command_uuid, "u1");
        assert!(!pending.started);
        assert_eq!(pending.previous, None);
        assert_eq!(pending.expected.unwrap().objective, "Finish");
    }

    /// A runtime update that matches the pending command is published and
    /// clears the pending state.
    #[test]
    fn inv_53_runtime_goal_matches_pending() {
        let mut state = GoalState::default();
        state.note_prompt("/goal Original", "u1");
        let runtime = GoalSnapshot::from_active_goal(&serde_json::json!({
            "value": {
                "condition": "Original",
                "iterations": 1,
                "set_at": 1710000000000u64,
                "tokens_at_start": 0
            }
        }))
        .unwrap();
        let published = state.note_runtime(Some(runtime.clone())).unwrap();
        assert_eq!(published.as_ref(), Some(&runtime));
        assert_eq!(state.pending, None);
        // A second, unrelated runtime update now publishes unconditionally.
        assert!(state.note_runtime(None).is_some());
    }

    /// upstream "does not let a stale runtime update overwrite an optimistic
    /// goal replacement".
    #[test]
    fn inv_53_runtime_goal_suppresses_stale_update() {
        let mut state = GoalState::default();
        state.note_prompt("/goal Original", "u1");
        // Confirm the original goal with its runtime snapshot.
        let original = GoalSnapshot::from_active_goal(&serde_json::json!({
            "value": {
                "condition": "Original",
                "iterations": 1,
                "set_at": 1710000000000u64,
                "tokens_at_start": 0
            }
        }))
        .unwrap();
        state.note_runtime(Some(original.clone()));
        // A replacement is submitted optimistically.
        state.note_prompt("/goal Replacement", "u2");
        assert_eq!(
            state.last_published.as_ref().unwrap().objective,
            "Replacement"
        );
        // A late runtime update for the OLD goal is suppressed.
        let stale = GoalSnapshot::from_active_goal(&serde_json::json!({
            "value": {
                "condition": "Original",
                "iterations": 2,
                "set_at": 1710000000000u64,
                "tokens_at_start": 0
            }
        }))
        .unwrap();
        assert_eq!(state.note_runtime(Some(stale)), None);
        assert_eq!(state.pending.as_ref().unwrap().command_uuid, "u2");
        // The replacement's own runtime update is published and clears pending.
        let replacement = GoalSnapshot::from_active_goal(&serde_json::json!({
            "value": {
                "condition": "Replacement",
                "iterations": 1,
                "set_at": 1710000001000u64,
                "tokens_at_start": 0
            }
        }))
        .unwrap();
        assert_eq!(
            state.note_runtime(Some(replacement.clone())),
            Some(Some(replacement))
        );
        assert_eq!(state.pending, None);
    }

    /// A `/goal clear` command expects `goal: null`; a runtime snapshot that is
    /// not a clear is stale.
    #[test]
    fn inv_53_clear_pending_only_matches_clear() {
        let mut state = GoalState::default();
        state.note_prompt("/goal Original", "u1");
        state.note_prompt("/goal clear", "u2");
        let stale = GoalSnapshot::from_active_goal(&serde_json::json!({
            "value": {"condition": "Original", "iterations": 1}
        }))
        .unwrap();
        assert_eq!(state.note_runtime(Some(stale)), None);
        assert_eq!(state.note_runtime(None), Some(None));
        assert_eq!(state.pending, None);
    }

    /// upstream "rolls back an optimistic goal update when the goal command
    /// fails": only a STARTED command rolls back, to the previously published
    /// goal.
    #[test]
    fn inv_53_rollback_on_failed_result() {
        let mut state = GoalState::default();
        state.note_prompt("/goal Original", "u1");
        let original = GoalSnapshot::from_active_goal(&serde_json::json!({
            "value": {
                "condition": "Original",
                "iterations": 2,
                "set_at": 1710000000000u64,
                "tokens_at_start": 0
            }
        }))
        .unwrap();
        state.note_runtime(Some(original.clone()));
        state.note_started("u1");
        // The first command's result succeeds: pending cleared, no publish.
        assert_eq!(state.note_result(false), None);
        assert_eq!(state.pending, None);

        state.note_prompt("/goal Replacement", "u2");
        // Not started yet: a result cannot roll back.
        assert_eq!(state.note_result(true), None);
        assert_eq!(state.pending.as_ref().unwrap().command_uuid, "u2");
        state.note_started("u2");
        // Failed -> publish the previous goal.
        let rolled_back = state.note_result(true).unwrap();
        assert_eq!(rolled_back.as_ref(), Some(&original));
        assert_eq!(state.last_published.as_ref(), Some(&original));
        assert_eq!(state.pending, None);
    }

    /// A successful started command clears pending without publishing.
    #[test]
    fn inv_53_result_success_clears_pending() {
        let mut state = GoalState::default();
        state.note_prompt("/goal Keep working", "u1");
        state.note_started("u1");
        assert_eq!(state.note_result(false), None);
        assert_eq!(state.pending, None);
        assert_eq!(state.last_published.unwrap().objective, "Keep working");
    }

    /// The AIR update shape, including the clear.
    #[test]
    fn inv_53_goal_update_value() {
        assert_eq!(
            goal_update_value(None),
            serde_json::json!({
                "sessionUpdate": "session_info_update",
                "_meta": {"jetbrains": {"air": {"version": 1, "goal": null}}}
            })
        );
        assert_eq!(
            goal_update_value(Some(&GoalSnapshot::optimistic("ship it"))),
            serde_json::json!({
                "sessionUpdate": "session_info_update",
                "_meta": {
                    "jetbrains": {
                        "air": {
                            "version": 1,
                            "goal": {
                                "objective": "ship it",
                                "status": "active",
                                "controlMethod": "_session/goal"
                            }
                        }
                    }
                }
            })
        );
    }

    #[test]
    fn inv_53_goal_publish_air_only() {
        let goal = GoalSnapshot::optimistic("ship it");
        assert_eq!(goal_publish(false, Some(&goal)), None);
        assert_eq!(goal_publish(false, None), None);
        assert_eq!(
            goal_publish(true, None),
            Some(serde_json::json!({
                "sessionUpdate": "session_info_update",
                "_meta": {"jetbrains": {"air": {"version": 1, "goal": null}}}
            }))
        );
    }

    #[test]
    fn inv_53_first_prompt_text() {
        let text_frame = serde_json::json!({
            "message": {"content": [{"type": "text", "text": "/goal ship it"}]}
        });
        assert_eq!(first_prompt_text(&text_frame), "/goal ship it");
        let image_frame = serde_json::json!({
            "message": {"content": [{"type": "image", "source": {}}]}
        });
        assert_eq!(first_prompt_text(&image_frame), "");
        assert_eq!(first_prompt_text(&serde_json::json!({})), "");
    }
}
