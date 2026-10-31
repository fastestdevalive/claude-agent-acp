//! The clear-context restart of an accepted ExitPlanMode plan
//! (`src/clear-context-coordinator.ts` and `ExitPlanCoordinator` in
//! `src/exit-plan.ts`, 0.94.0): Claude's private conversation is replaced by a
//! fresh `claude` child while the public ACP turn stays pending, and the
//! accepted plan is sent to the fresh context as its first message.
//!
//! The session actor owns the lifecycle (it starts the restart, adopts the
//! fresh child, and ends it on cancel, close or failure); this module holds the
//! pieces that do not need the actor's state: the spawn options of the fresh
//! context (`restartParams`), the continuation message, the child bring-up and
//! the guards that keep a half-started child from outliving its session.

use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use super::{relay_stream, Command, CHILD_EXIT_WAIT};
use crate::control::{Control, InitializeOptions};
use crate::dispatch::{self, Route, StreamMsg};
use crate::process::{self, Process, SpawnOptions, Timings};
use crate::session_config::{DEFAULT_AGENT_ID, DEFAULT_MODEL_ID};

/// Aborts its task when dropped: a restart that is cancelled, or whose session
/// ends, takes the child it started down with it.
pub(crate) struct AbortOnDrop(pub tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// What the actor keeps to bring up a fresh child: the spawn and initialize
/// options of the session, and the way to stop the child `run` owns.
pub(crate) struct Restarter {
    pub spawn: SpawnOptions,
    pub timings: Timings,
    pub initialize: InitializeOptions,
    /// Stops the first child, owned by `run` (taken by the first restart).
    pub stop_first: Option<oneshot::Sender<()>>,
}

/// The child a restart replaces.
pub(crate) enum OldChild {
    /// The child `run` owns.
    First(oneshot::Sender<()>),
    /// A child an earlier restart started.
    Restarted(AbortOnDrop),
    /// Nothing to stop (no live child).
    None,
}

impl OldChild {
    /// Close the old child. Dropping the guard aborts the relay that owns the
    /// process, which kills it.
    fn stop(self) {
        match self {
            OldChild::First(stop) => {
                let _ = stop.send(());
            }
            OldChild::Restarted(guard) => drop(guard),
            OldChild::None => {}
        }
    }
}

/// What the fresh context keeps of the session's preferences
/// (`restartParams`): the model, agent and effort pins that differ from the
/// defaults, and the permission mode the accepted plan runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RestartParams {
    pub model: String,
    pub agent: String,
    pub effort: Option<String>,
    pub mode: String,
}

/// `restartParams` as spawn options: a fresh conversation (no resume, a new
/// SDK session id) that keeps the session's other options. The original
/// model, agent and effort are dropped and only the live ones that differ from
/// the defaults come back, so a session that returned to its defaults does not
/// get its creation-time preferences again.
pub(crate) fn restart_spawn(
    base: &SpawnOptions,
    params: &RestartParams,
    fresh_session_id: String,
) -> SpawnOptions {
    let mut spawn = base.clone();
    spawn.resume = None;
    spawn.session_id = Some(fresh_session_id);
    spawn.model = (params.model != DEFAULT_MODEL_ID).then(|| params.model.clone());
    spawn.agent = (params.agent != DEFAULT_AGENT_ID).then(|| params.agent.clone());
    spawn.effort = params.effort.clone();
    spawn.permission_mode = Some(params.mode.clone());
    spawn
}

/// `continuationMessage`: the accepted plan as the first message of the fresh
/// context, under the pending turn's prompt uuid so its echo activates it.
pub(crate) fn continuation_frame(session_id: &str, plan: &str, prompt_uuid: &str) -> Value {
    json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": format!("Implement the following plan:\n\n{plan}") }],
        },
        "session_id": session_id,
        "parent_tool_use_id": null,
        "origin": { "kind": "human" },
        "uuid": prompt_uuid,
    })
}

/// A fresh child that finished its handshake.
pub(crate) struct RestartedChild {
    /// The child's frames, behind the relay that appends its exit verdict.
    pub stream: mpsc::UnboundedReceiver<Value>,
    /// Session frames that arrived during the handshake, in order.
    pub buffered: Vec<Value>,
    /// Owns the relay and, through it, the process.
    pub guard: AbortOnDrop,
}

/// Why a restart ended, from the actor's point of view.
pub(crate) type RestartResult = Result<RestartedChild, String>;

/// The exit verdict of `process`: whether it ended abnormally, `None` when it
/// did not end in time (the verdict the relay appends to the stream).
pub(crate) async fn child_exit_verdict(mut process: Process, timings: Timings) -> Option<bool> {
    match tokio::time::timeout(CHILD_EXIT_WAIT, process.wait_with_stderr(&timings)).await {
        Ok(Ok(status)) => Some(!status.success()),
        _ => None,
    }
}

/// Everything the bring-up reads from the session.
pub(crate) struct BringUp {
    pub spawn: SpawnOptions,
    pub timings: Timings,
    pub initialize: InitializeOptions,
    pub control: Control,
    pub control_in_tx: mpsc::UnboundedSender<Value>,
    /// How long the whole bring-up may take before it fails.
    pub deadline: Duration,
}

/// Start a fresh child (the live model, agent and effort travel as its flags,
/// so no model switch follows), move the control channel onto it, stop the old
/// child and run the handshake. The child's control traffic is routed to the control
/// task while the handshake waits (the actor does not read the stream yet);
/// session frames that arrive meanwhile are buffered for it.
pub(crate) async fn bring_up(bring: BringUp, old: OldChild) -> RestartResult {
    let BringUp {
        spawn,
        timings,
        initialize,
        control,
        control_in_tx,
        deadline,
    } = bring;
    let mut process = process::spawn(&spawn)
        .await
        .map_err(|error| error.to_string())?;
    let stdin = process
        .take_stdin()
        .ok_or_else(|| "child stdin unavailable".to_string())?;
    let mut lines = std::mem::replace(&mut process.lines, mpsc::unbounded_channel().1);
    // From here the control channel writes to the fresh child, so the old one
    // can go without a write ever reaching its closed stdin.
    control.rebind(stdin).map_err(|error| error.to_string())?;
    old.stop();

    let handshake = async {
        control
            .initialize(&initialize)
            .await
            .map_err(|error| format!("initialize handshake: {error}"))?;
        Ok::<(), String>(())
    };
    tokio::pin!(handshake);
    let mut buffered = Vec::new();
    let timeout = tokio::time::sleep(deadline);
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            outcome = &mut handshake => {
                outcome?;
                break;
            }
            line = lines.recv() => match line {
                Some(line) => {
                    if dispatch::route(&StreamMsg::new(line.clone()), &dispatch::TurnView::default())
                        == Route::Control
                    {
                        let _ = control_in_tx.send(line);
                    } else {
                        buffered.push(line);
                    }
                }
                None => return Err("the fresh Claude process ended during startup".to_string()),
            },
            _ = &mut timeout => {
                return Err("the fresh Claude context did not start in time".to_string());
            }
        }
    }

    let (relay_tx, stream) = mpsc::unbounded_channel::<Value>();
    let relay = tokio::spawn(relay_stream(
        lines,
        relay_tx,
        child_exit_verdict(process, timings),
    ));
    Ok(RestartedChild {
        stream,
        buffered,
        guard: AbortOnDrop(relay),
    })
}

/// Run a bring-up as a task that reports to the actor. The returned guard
/// aborts it (and with it a child that is still starting).
pub(crate) fn spawn_restart(
    id: u64,
    bring: BringUp,
    old: OldChild,
    command_tx: mpsc::UnboundedSender<Command>,
) -> AbortOnDrop {
    AbortOnDrop(tokio::spawn(async move {
        let result = bring_up(bring, old).await;
        let _ = command_tx.send(Command::RestartDone { id, result });
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SpawnOptions {
        SpawnOptions {
            resume: Some("old-session".into()),
            session_id: None,
            model: Some("claude-opus".into()),
            agent: Some("reviewer".into()),
            effort: Some("high".into()),
            permission_mode: Some("plan".into()),
            max_turns: Some(7),
            ..SpawnOptions::default()
        }
    }

    /// B-74 — the fresh context is a new conversation in the plan's mode that
    /// keeps the session's other options; creation-time model, agent and effort
    /// are dropped, and only the live pins that differ from the defaults return.
    #[test]
    fn t3a_restart_spawn_is_a_fresh_conversation_with_the_live_pins() {
        let live = RestartParams {
            model: "claude-sonnet".into(),
            agent: DEFAULT_AGENT_ID.into(),
            effort: Some("low".into()),
            mode: "acceptEdits".into(),
        };
        let spawn = restart_spawn(&base(), &live, "fresh-id".into());
        assert_eq!(spawn.resume, None);
        assert_eq!(spawn.session_id.as_deref(), Some("fresh-id"));
        assert_eq!(spawn.model.as_deref(), Some("claude-sonnet"));
        assert_eq!(spawn.agent, None, "the default agent is not pinned");
        assert_eq!(spawn.effort.as_deref(), Some("low"));
        assert_eq!(spawn.permission_mode.as_deref(), Some("acceptEdits"));
        assert_eq!(spawn.max_turns, Some(7), "other options are kept");

        let defaults = RestartParams {
            model: DEFAULT_MODEL_ID.into(),
            agent: DEFAULT_AGENT_ID.into(),
            effort: None,
            mode: "auto".into(),
        };
        let spawn = restart_spawn(&base(), &defaults, "fresh-id".into());
        assert_eq!(spawn.model, None, "a session back on its default model");
        assert_eq!(spawn.effort, None);
    }

    /// B-74 — the continuation carries the plan under the pending turn's uuid.
    #[test]
    fn t3a_continuation_frame_carries_the_plan_under_the_prompt_uuid() {
        let frame = continuation_frame("sess", "ship it", "uuid-1");
        assert_eq!(frame["type"], "user");
        assert_eq!(frame["uuid"], "uuid-1");
        assert_eq!(frame["origin"], json!({"kind": "human"}));
        assert_eq!(
            frame["message"]["content"][0]["text"],
            "Implement the following plan:\n\nship it"
        );
        assert_eq!(frame["session_id"], "sess");
        assert!(frame["parent_tool_use_id"].is_null());
    }
}
