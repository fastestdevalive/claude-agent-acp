//! Actor-owned state of the Turn 2 hazard lane (T2A), held by
//! [`crate::session_ext::SessionExt::t2a`]: the consumer-local sets and books
//! Node keeps as locals of its consume loop, plus the effects a frame handler
//! asks the run loop to carry out after the frame was dispatched.

use std::collections::HashSet;

use super::failures::SessionFailures;
use crate::session_titles::ExplicitPublish;

/// Something a frame handler needs the run loop to do once the frame is
/// handled (the handler has no command channel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// `syncModelAfterExternalSwitch`: the SDK moved the session to this model
    /// by itself (a refusal fallback), so reconcile the model state.
    SyncModel(String),
    /// An accepted ExitPlanMode plan continues in a fresh context: start the
    /// restart for the plan in [`T2aState::restart_plan`].
    ExitPlanRestart,
}

/// `MAX_TASK_RUN_IDS` (`acp-agent.ts:468`).
const MAX_TASK_RUN_IDS: usize = 1024;

/// The tasks whose latest run a session remembers (`session.taskRunIds`), in
/// insertion order like the JS `Map`, oldest dropped past the cap.
#[derive(Debug, Default)]
pub struct TaskRuns {
    runs: Vec<(String, String)>,
}

impl TaskRuns {
    /// `isLateTaskRun` (`acp-agent.ts:469-487`): record `run_id` as the latest
    /// run of `task_id` and tell whether it names a run that opened before the
    /// latest one seen. A resumed task keeps its `task_id` and gets a new
    /// `run_id` (SDK 0.3.292+), and a queued `task_notification` of the finished
    /// run can arrive after the resume: applied, it would close the new run. Run
    /// ids of one task sort in run order by plain string comparison. Frames
    /// without one (older CLIs) are never late.
    pub fn is_late(&mut self, task_id: &str, run_id: Option<&str>) -> bool {
        let Some(run_id) = run_id else {
            return false;
        };
        let latest = self
            .runs
            .iter()
            .find(|(task, _)| task == task_id)
            .map(|(_, run)| run.as_str());
        if latest.is_some_and(|latest| run_id < latest) {
            return true;
        }
        self.runs.retain(|(task, _)| task != task_id);
        self.runs.push((task_id.to_string(), run_id.to_string()));
        if self.runs.len() > MAX_TASK_RUN_IDS {
            self.runs.remove(0);
        }
        false
    }
}

/// The display terminal of a command that went to the background
/// (`Session.backgroundTerminals`): it keeps running until its task ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundTerminal {
    pub tool_call_id: String,
    pub parent_tool_use_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N-33 — a frame of an earlier run is late; the latest run and runs of
    /// other tasks are not; frames without a run id never are.
    #[test]
    fn t2a_task_runs_flag_only_earlier_runs() {
        let mut runs = TaskRuns::default();
        assert!(!runs.is_late("t1", None));
        assert!(!runs.is_late("t1", Some("run-02")));
        assert!(runs.is_late("t1", Some("run-01")));
        assert!(!runs.is_late("t1", Some("run-02")), "the latest run itself");
        assert!(!runs.is_late("t1", Some("run-03")), "a newer run advances");
        assert!(runs.is_late("t1", Some("run-02")));
        assert!(!runs.is_late("t2", Some("run-01")), "per task");
        assert!(!runs.is_late("t1", None));
    }

    /// N-33 — the book remembers at most 1024 tasks, oldest dropped.
    #[test]
    fn t2a_task_runs_are_capped() {
        let mut runs = TaskRuns::default();
        for index in 0..1030 {
            assert!(!runs.is_late(&format!("t{index}"), Some("run-05")));
        }
        // The oldest task was forgotten, so its earlier run no longer reads late.
        assert!(!runs.is_late("t0", Some("run-01")));
        assert!(runs.is_late("t1029", Some("run-01")));
    }
}

/// A client title change that waits for the title generation in flight
/// (`ExplicitTitle.apply`: `await this.generation`).
pub struct ParkedExplicit {
    pub publish: ExplicitPublish,
    pub persist: super::ExplicitPersist,
    pub reply: tokio::sync::oneshot::Sender<Result<(), String>>,
}

impl std::fmt::Debug for ParkedExplicit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParkedExplicit")
            .field("publish", &self.publish)
            .finish_non_exhaustive()
    }
}

/// A clear-context restart waiting to start: the accepted plan and the pending
/// turn taken out of the old machine (`pendingExitPlanContextReset`, the turn
/// of `continuePlanInFreshContext`).
pub struct RestartPlan {
    pub reset: super::exit_plan::ContextReset,
    pub turn: crate::turn::Turn,
    /// The prompts that were queued behind it, in order.
    pub queued: Vec<crate::turn::Turn>,
}

impl std::fmt::Debug for RestartPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RestartPlan")
            .field("reset", &self.reset)
            .finish_non_exhaustive()
    }
}

/// What the last user message's Bash results say about backgrounded commands,
/// read while that message's tool results are rendered
/// (`runningTerminal` / `backgroundedToolCalls`, `acp-agent.ts:8013-8057`).
#[derive(Debug, Default)]
pub struct BackgroundFacts {
    /// The tool call whose display terminal keeps running after its result.
    pub running_terminal: Option<String>,
    /// The Bash tool calls whose command went to the background.
    pub backgrounded: HashSet<String>,
}

/// The T2A actor state.
#[derive(Debug, Default)]
pub struct T2aState {
    /// Client title changes waiting for a title generation to end.
    pub parked_explicit: Vec<ParkedExplicit>,
    /// The latest run of every task (`taskRunIds`).
    pub task_runs: TaskRuns,
    /// The terminals of backgrounded commands, by task id.
    pub background_terminals: std::collections::HashMap<String, BackgroundTerminal>,
    /// The AIR session-failure lifecycle (`SessionFailureController`).
    pub failures: SessionFailures,
    /// The pending ExitPlanMode interruption and clear-context reset.
    pub exit_plan: super::exit_plan::ExitPlanState,
    /// The background facts of the user message being rendered.
    pub background: BackgroundFacts,
    /// The restart [`Effect::ExitPlanRestart`] starts.
    pub restart_plan: Option<RestartPlan>,
    /// `pendingWorkerShutdown`: the worker announced its shutdown; the signal
    /// stays armed until the transport actually ends (`acp-agent.ts:6576`).
    pub pending_worker_shutdown: bool,
    /// `noticedToolUses`: the tool calls an informational notice was already
    /// shown for.
    pub noticed_tool_uses: HashSet<String>,
    /// Effects for the run loop, drained after every dispatched frame.
    pub effects: Vec<Effect>,
}
