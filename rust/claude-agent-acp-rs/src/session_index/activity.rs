//! The `activity` and `cost` of a session list row (upstream
//! `session-index/activity.ts`).
//!
//! A session that this connection runs reports the SDK's own state, and `error`
//! when it is idle and its last turn failed with an error (not a cancel).
//! Another session reports what the CLI registry and the transcript tell:
//!
//! - no live process holds it: `idle`, or `error` when its last turn ended with
//!   an API error;
//! - an interactive CLI that is not driven by an SDK holds it, and its registry
//!   status is newer than the transcript: the registry status (an idle one is
//!   `error` after such a turn);
//! - otherwise the transcript tail: a finished turn is `idle` (`error` after an
//!   API error), an unfinished turn written in the last 10 minutes is `running`,
//!   and anything else has no known state.
//!
//! A user interrupt is no error. A new turn clears it. The SDK-driven CLIs
//! (`entrypoint: sdk-*`, which includes the ones this adapter starts) keep `busy`
//! in the registry long after a turn ends, so their registry status is not used.

use super::js_text::to_iso_string;
use super::live_registry::LiveRecord;
use super::transcript_scan::{TranscriptFacts, TurnState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityState {
    Running,
    Idle,
    RequiresAction,
    Error,
}

impl ActivityState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Idle => "idle",
            Self::RequiresAction => "requires_action",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionActivity {
    pub state: Option<ActivityState>,
    pub last_turn_ended_at: Option<String>,
}

/// What this connection knows about a session it runs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OwnSessionState {
    pub state: Option<ActivityState>,
    /// Epoch ms of the end of the last turn.
    pub last_turn_ended_at: Option<f64>,
    /// `total_cost_usd` of the last result.
    pub cost_usd: Option<f64>,
    /// The last turn failed with an error (not a cancel).
    pub last_turn_failed: bool,
    /// The query of the session failed here at this time (epoch ms) and no longer
    /// runs: the session is reported as any other one, `error` until its
    /// transcript shows a prompt or a turn end after it.
    pub query_failed_at: Option<f64>,
    /// The query resumed a stored conversation (load, resume, fork): the session
    /// has a transcript, unlike a new one before its first turn.
    pub resumed_from_history: bool,
    /// Prompts of the session sent and not settled yet (the connection's own
    /// bookkeeping; not on the wire).
    pub prompts_in_flight: usize,
}

const RECENT_UNFINISHED_TURN_MS: f64 = 10.0 * 60.0 * 1000.0;

fn registry_state(status: Option<&str>) -> Option<ActivityState> {
    match status? {
        "busy" | "shell" => Some(ActivityState::Running),
        "waiting" => Some(ActivityState::RequiresAction),
        "idle" => Some(ActivityState::Idle),
        _ => None,
    }
}

fn uses_registry_status(record: &LiveRecord, transcript_mtime_ms: f64) -> bool {
    record.kind.as_deref() == Some("interactive")
        && !record
            .entrypoint
            .as_deref()
            .unwrap_or("")
            .starts_with("sdk-")
        && record
            .status_updated_at
            .is_some_and(|at| at > transcript_mtime_ms)
}

fn iso(ms: Option<f64>) -> Option<String> {
    ms.filter(|v| v.is_finite())
        .map(|v| to_iso_string(v as i64))
}

pub struct ActivityInput<'a> {
    pub own: Option<&'a OwnSessionState>,
    pub live: Option<&'a LiveRecord>,
    pub facts: &'a TranscriptFacts,
    pub transcript_mtime_ms: f64,
    pub now: f64,
}

pub fn derive_activity(input: &ActivityInput<'_>) -> Option<SessionActivity> {
    let facts = input.facts;
    let failed_at = input.own.and_then(|o| o.query_failed_at);
    let own = if failed_at.is_none() { input.own } else { None };
    let state: Option<ActivityState>;
    // A finished turn that ended with an API error, or this connection's query of
    // the session failed and nothing was written since.
    let failed = (facts.turn_state == Some(TurnState::Finished) && facts.last_turn_error)
        || failed_at.is_some_and(|at| {
            let after = |written: Option<i64>| written.is_some_and(|v| v as f64 > at);
            !after(facts.last_prompt_at) && !after(facts.last_turn_ended_at)
        });
    if let Some(own) = own {
        let mut own_state = own.state.unwrap_or(ActivityState::Idle);
        if own_state == ActivityState::Idle && own.last_turn_failed {
            own_state = ActivityState::Error;
        }
        state = Some(own_state);
    } else if let Some(live) = input.live {
        if uses_registry_status(live, input.transcript_mtime_ms) {
            // A busy or waiting CLI wins; an idle one shows how its turn ended.
            let mut registry = registry_state(live.status.as_deref());
            if registry == Some(ActivityState::Idle) && failed {
                registry = Some(ActivityState::Error);
            }
            state = registry;
        } else if failed {
            state = Some(ActivityState::Error);
        } else if facts.turn_state == Some(TurnState::Finished) {
            state = Some(ActivityState::Idle);
        } else if facts.turn_state == Some(TurnState::Unfinished)
            && input.now - input.transcript_mtime_ms < RECENT_UNFINISHED_TURN_MS
        {
            state = Some(ActivityState::Running);
        } else {
            state = None;
        }
    } else {
        state = Some(if failed {
            ActivityState::Error
        } else {
            ActivityState::Idle
        });
    }
    let last_turn_ended_at = iso(own
        .and_then(|o| o.last_turn_ended_at)
        .or(facts.last_turn_ended_at.map(|v| v as f64)));
    if state.is_none() && last_turn_ended_at.is_none() {
        return None;
    }
    Some(SessionActivity {
        state,
        last_turn_ended_at,
    })
}

/// The cost of a session this connection runs: the `total_cost_usd` the SDK gave
/// in its last result. Only a positive amount; none for any other session.
pub fn select_cost(own: Option<&OwnSessionState>) -> Option<f64> {
    own?.cost_usd
        .filter(|amount| amount.is_finite() && *amount > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_767_229_200_000.0; // 2026-01-01T01:00:00Z

    fn facts() -> TranscriptFacts {
        TranscriptFacts {
            has_messages: true,
            ..TranscriptFacts::default()
        }
    }

    fn live(status: &str, entrypoint: &str) -> LiveRecord {
        LiveRecord {
            pid: 1,
            session_id: "s".into(),
            cwd: None,
            kind: Some("interactive".into()),
            entrypoint: Some(entrypoint.into()),
            status: Some(status.into()),
            status_updated_at: Some(NOW),
        }
    }

    fn state_of(
        own: Option<&OwnSessionState>,
        live: Option<&LiveRecord>,
        facts: &TranscriptFacts,
        mtime: f64,
    ) -> Option<ActivityState> {
        derive_activity(&ActivityInput {
            own,
            live,
            facts,
            transcript_mtime_ms: mtime,
            now: NOW,
        })
        .and_then(|a| a.state)
    }

    /// activity > reports the SDK state of a session this connection runs
    #[test]
    fn reports_the_sdk_state_of_a_session_that_runs_here() {
        let own = OwnSessionState {
            state: Some(ActivityState::RequiresAction),
            last_turn_ended_at: Some(NOW - 1000.0),
            ..OwnSessionState::default()
        };
        let mut f = facts();
        f.last_turn_ended_at = Some(1);
        let activity = derive_activity(&ActivityInput {
            own: Some(&own),
            live: None,
            facts: &f,
            transcript_mtime_ms: NOW,
            now: NOW,
        });
        assert_eq!(
            activity,
            Some(SessionActivity {
                state: Some(ActivityState::RequiresAction),
                last_turn_ended_at: Some(to_iso_string((NOW - 1000.0) as i64)),
            })
        );
        let idle = derive_activity(&ActivityInput {
            own: Some(&OwnSessionState::default()),
            live: None,
            facts: &facts(),
            transcript_mtime_ms: NOW,
            now: NOW,
        });
        assert_eq!(
            idle,
            Some(SessionActivity {
                state: Some(ActivityState::Idle),
                last_turn_ended_at: None
            })
        );
    }

    /// activity > is error after a turn that ended with an API error, until a new turn
    #[test]
    fn is_error_after_an_api_error_until_a_new_turn() {
        let mut error_facts = facts();
        error_facts.turn_state = Some(TurnState::Finished);
        error_facts.last_turn_error = true;
        assert_eq!(
            state_of(None, None, &error_facts, NOW),
            Some(ActivityState::Error)
        );
        // A busy or waiting CLI wins; an idle one shows the error.
        let held = NOW - 1.0;
        assert_eq!(
            state_of(None, Some(&live("busy", "cli")), &error_facts, held),
            Some(ActivityState::Running)
        );
        assert_eq!(
            state_of(None, Some(&live("waiting", "cli")), &error_facts, held),
            Some(ActivityState::RequiresAction)
        );
        assert_eq!(
            state_of(None, Some(&live("idle", "cli")), &error_facts, held),
            Some(ActivityState::Error)
        );
        // A session that runs here: its own failed turn, while idle.
        let failed_idle = OwnSessionState {
            state: Some(ActivityState::Idle),
            last_turn_failed: true,
            ..OwnSessionState::default()
        };
        assert_eq!(
            state_of(Some(&failed_idle), None, &facts(), NOW),
            Some(ActivityState::Error)
        );
        let failed_running = OwnSessionState {
            state: Some(ActivityState::Running),
            last_turn_failed: true,
            ..OwnSessionState::default()
        };
        assert_eq!(
            state_of(Some(&failed_running), None, &facts(), NOW),
            Some(ActivityState::Running)
        );
        let idle = OwnSessionState {
            state: Some(ActivityState::Idle),
            ..OwnSessionState::default()
        };
        assert_eq!(
            state_of(Some(&idle), None, &error_facts, NOW),
            Some(ActivityState::Idle)
        );
        // A query that failed here: error until the transcript changes, and a busy
        // CLI still wins.
        let mut unfinished = facts();
        unfinished.turn_state = Some(TurnState::Unfinished);
        let failed_here = OwnSessionState {
            query_failed_at: Some(NOW),
            ..OwnSessionState::default()
        };
        assert_eq!(
            state_of(Some(&failed_here), None, &unfinished, NOW - 1.0),
            Some(ActivityState::Error)
        );
        assert_eq!(
            state_of(Some(&failed_here), None, &unfinished, NOW + 60_000.0),
            Some(ActivityState::Error)
        );
        let mut resumed = facts();
        resumed.turn_state = Some(TurnState::Finished);
        resumed.last_turn_ended_at = Some(NOW as i64 + 2000);
        assert_eq!(
            state_of(Some(&failed_here), None, &resumed, NOW + 2000.0),
            Some(ActivityState::Idle)
        );
        assert_eq!(
            state_of(
                Some(&failed_here),
                Some(&live("busy", "cli")),
                &unfinished,
                NOW - 1.0
            ),
            Some(ActivityState::Running)
        );
    }

    /// activity > is idle when no live process holds the session
    #[test]
    fn is_idle_when_no_live_process_holds_the_session() {
        let mut unfinished = facts();
        unfinished.turn_state = Some(TurnState::Unfinished);
        assert_eq!(
            derive_activity(&ActivityInput {
                own: None,
                live: None,
                facts: &unfinished,
                transcript_mtime_ms: NOW,
                now: NOW
            }),
            Some(SessionActivity {
                state: Some(ActivityState::Idle),
                last_turn_ended_at: None
            })
        );
    }

    /// activity > uses the registry status of an interactive CLI newer than the transcript
    #[test]
    fn uses_the_registry_status_newer_than_the_transcript() {
        let waiting = live("waiting", "cli");
        assert_eq!(
            state_of(None, Some(&waiting), &facts(), NOW - 1.0),
            Some(ActivityState::RequiresAction)
        );
        assert_eq!(
            state_of(None, Some(&live("busy", "cli")), &facts(), NOW - 1.0),
            Some(ActivityState::Running)
        );
        // Older than the transcript: the tail decides.
        let mut finished = facts();
        finished.turn_state = Some(TurnState::Finished);
        assert_eq!(
            state_of(None, Some(&waiting), &finished, NOW + 1.0),
            Some(ActivityState::Idle)
        );
    }

    /// activity > reads the tail for an SDK-driven CLI, whose registry stays busy
    #[test]
    fn reads_the_tail_for_an_sdk_driven_cli() {
        let busy = live("busy", "sdk-ts");
        let mut finished = facts();
        finished.turn_state = Some(TurnState::Finished);
        let mut unfinished = facts();
        unfinished.turn_state = Some(TurnState::Unfinished);
        assert_eq!(
            state_of(None, Some(&busy), &finished, NOW - 60_000.0),
            Some(ActivityState::Idle)
        );
        assert_eq!(
            state_of(None, Some(&busy), &unfinished, NOW - 60_000.0),
            Some(ActivityState::Running)
        );
        // An unfinished turn that has not written for 10 minutes has no known state.
        assert_eq!(
            derive_activity(&ActivityInput {
                own: None,
                live: Some(&busy),
                facts: &unfinished,
                transcript_mtime_ms: NOW - 11.0 * 60_000.0,
                now: NOW
            }),
            None
        );
    }

    /// cost > is the SDK's amount of a session that runs here, and only a positive one
    #[test]
    fn cost_is_the_sdks_positive_amount_of_a_session_that_runs_here() {
        let cost = |amount| OwnSessionState {
            cost_usd: Some(amount),
            ..OwnSessionState::default()
        };
        assert_eq!(select_cost(Some(&cost(2.0))), Some(2.0));
        assert_eq!(select_cost(Some(&cost(0.0))), None);
        assert_eq!(select_cost(Some(&cost(f64::NAN))), None);
        assert_eq!(select_cost(Some(&OwnSessionState::default())), None);
        assert_eq!(select_cost(None), None);
    }
}
