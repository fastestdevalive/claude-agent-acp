//! Session lifecycle phase timer (`session-timing.ts`).
//!
//! One [`SessionTiming`] is created per session operation (`create`, `load`,
//! `resume`, `fork`) and reports each phase as
//!
//! ```text
//! [session/<operation>] sessionId=<id> phase=<name> durationMs=<n> totalMs=<n><detail>
//! ```
//!
//! `durationMs` is the time since the previous phase (or construction), and
//! `totalMs` is the time since construction. Both are rounded to whole
//! milliseconds. Without a logger every call is a no-op.

use std::sync::Arc;
use std::time::Instant;

use crate::logger::Logger;

/// A phase timer for one session operation.
#[derive(Debug, Clone)]
pub struct SessionTiming {
    logger: Option<Arc<Logger>>,
    operation: String,
    session_id: String,
    started_at: Instant,
    phase_started_at: Instant,
}

impl SessionTiming {
    /// Start a timer for `operation` on `session_id` at `Instant::now()`.
    pub fn new(logger: Option<Arc<Logger>>, operation: &str, session_id: &str) -> Self {
        Self::with_started_at(logger, operation, session_id, Instant::now())
    }

    /// Start a timer whose total already includes work done before it was
    /// constructed (`new SessionTiming(..., createStartedAt)`).
    pub fn with_started_at(
        logger: Option<Arc<Logger>>,
        operation: &str,
        session_id: &str,
        started_at: Instant,
    ) -> Self {
        Self {
            logger,
            operation: operation.to_string(),
            session_id: session_id.to_string(),
            started_at,
            phase_started_at: started_at,
        }
    }

    /// Report a phase at the current instant, with an optional `detail`
    /// appended verbatim (leading space included by the caller).
    pub fn phase(&mut self, name: &str, detail: &str) {
        self.phase_at(name, detail, Instant::now());
    }

    /// Report a phase at an explicit instant (deterministic unit tests).
    pub fn phase_at(&mut self, name: &str, detail: &str, now: Instant) {
        let duration_ms = elapsed_ms(self.phase_started_at, now);
        let total_ms = elapsed_ms(self.started_at, now);
        if let Some(logger) = &self.logger {
            logger.log(&format!(
                "[session/{}] sessionId={} phase={} durationMs={} totalMs={}{}",
                self.operation, self.session_id, name, duration_ms, total_ms, detail
            ));
        }
        self.phase_started_at = now;
    }
}

/// Milliseconds between two instants, rounded (`Math.round`).
fn elapsed_ms(from: Instant, to: Instant) -> u64 {
    let micros = to.saturating_duration_since(from).as_micros() as f64;
    (micros / 1000.0).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_log(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acp-timing-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("agent.log")
    }

    /// Mirrors `src/tests/session-timing.test.ts`: a start time supplied at
    /// construction is included in both durations.
    #[test]
    fn inv_timing_line_includes_preconstruction_work() {
        let path = temp_log("create");
        let logger = Arc::new(Logger::to_file(&path));
        let started = Instant::now();
        let now = started + Duration::from_millis(75);
        let mut timing =
            SessionTiming::with_started_at(Some(logger), "create", "session-id", started);
        timing.phase_at("validate-cwd", "", now);

        let content = std::fs::read_to_string(&path).expect("read agent.log");
        assert!(
            content.ends_with(
                "[session/create] sessionId=session-id phase=validate-cwd durationMs=75 totalMs=75\n"
            ),
            "exact line: {content:?}"
        );

        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn inv_timing_duration_is_since_previous_phase() {
        let path = temp_log("load");
        let logger = Arc::new(Logger::to_file(&path));
        let started = Instant::now();
        let mut timing = SessionTiming::with_started_at(Some(logger), "load", "s1", started);

        timing.phase_at("session-ready", "", started + Duration::from_millis(100));
        timing.phase_at("replay", "", started + Duration::from_millis(160));

        let content = std::fs::read_to_string(&path).expect("read agent.log");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "{content:?}");
        assert!(
            lines[0].ends_with("phase=session-ready durationMs=100 totalMs=100"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].ends_with("phase=replay durationMs=60 totalMs=160"),
            "{}",
            lines[1]
        );

        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn inv_timing_detail_is_appended_verbatim() {
        let path = temp_log("fork");
        let logger = Arc::new(Logger::to_file(&path));
        let started = Instant::now();
        let mut timing = SessionTiming::with_started_at(Some(logger), "fork", "s1", started);
        timing.phase_at(
            "sdk-fork",
            " resolution=latest",
            started + Duration::from_millis(5),
        );
        let content = std::fs::read_to_string(&path).expect("read agent.log");
        assert!(
            content.contains("phase=sdk-fork durationMs=5 totalMs=5 resolution=latest\n"),
            "{content:?}"
        );
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn inv_timing_without_logger_is_noop() {
        let mut timing = SessionTiming::new(None, "create", "s1");
        timing.phase("validate-cwd", "");
        timing.phase("settings", "");
        // No file, no panic.
    }
}
