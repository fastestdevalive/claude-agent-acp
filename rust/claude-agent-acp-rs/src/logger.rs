//! The adapter's file logger (`index.ts:64-84`).
//!
//! When `CLAUDE_AGENT_LOGS` names a directory the adapter appends every
//! diagnostic line to `<dir>/agent.log` as
//! `{ISO-8601 timestamp} pid={pid} {message}`. The directory is created on
//! first use. Without the variable the logger is disabled: `log` is a no-op
//! and `error` only writes to stderr. The variable is read ONLY through
//! [`crate::env_snapshot::EnvSnapshot`] (never `std::env` directly), so tests
//! stay hermetic.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::env_snapshot::EnvSnapshot;

/// The environment variable that selects the log directory (`index.ts:73`).
pub const LOGS_ENV: &str = "CLAUDE_AGENT_LOGS";

/// The log file name inside the directory (`index.ts:76`).
pub const LOG_FILE_NAME: &str = "agent.log";

/// Append-only diagnostic logger.
#[derive(Debug, Clone)]
pub struct Logger {
    /// `None` when `CLAUDE_AGENT_LOGS` is unset/empty: every write is a no-op.
    file: Option<PathBuf>,
    /// The adapter's own pid, captured once (`process.pid`).
    pid: u32,
}

impl Logger {
    /// A logger that never writes a file (diagnostics still go to stderr).
    pub fn disabled() -> Self {
        Self {
            file: None,
            pid: std::process::id(),
        }
    }

    /// Build the logger from an environment snapshot (`index.ts:73-84`).
    ///
    /// `CLAUDE_AGENT_LOGS` selects the directory; `mkdirSync(recursive)` is
    /// best-effort so a transient filesystem failure never stops the adapter.
    pub fn from_env(env: &EnvSnapshot) -> Self {
        match env.get(LOGS_ENV) {
            Some(dir) if !dir.is_empty() => Self::to_dir(Path::new(dir)),
            _ => Self::disabled(),
        }
    }

    /// Log into `<dir>/agent.log`, creating `dir` if needed.
    pub fn to_dir(dir: &Path) -> Self {
        let _ = std::fs::create_dir_all(dir);
        Self {
            file: Some(dir.join(LOG_FILE_NAME)),
            pid: std::process::id(),
        }
    }

    /// Log into an explicit file (unit tests / embedders).
    pub fn to_file(path: impl Into<PathBuf>) -> Self {
        Self {
            file: Some(path.into()),
            pid: std::process::id(),
        }
    }

    /// Whether file logging is enabled.
    pub fn enabled(&self) -> bool {
        self.file.is_some()
    }

    /// Append one diagnostic line, if enabled.
    pub fn log(&self, message: &str) {
        let Some(path) = &self.file else {
            return;
        };
        let line = Self::line(SystemTime::now(), self.pid, message);
        // `appendFileSync`: open per write so an external rotation is picked up
        // and a failure is swallowed (diagnostics must never break serving).
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            use std::io::Write;
            let _ = file.write_all(line.as_bytes());
        }
    }

    /// Report an error: always to stderr, and to the file when enabled
    /// (`index.ts:78-83`).
    pub fn error(&self, message: &str) {
        eprintln!("{message}");
        self.log(message);
    }

    /// Render one log line exactly like the Node logger:
    /// `${new Date().toISOString()} pid=${process.pid} ${rendered}\n`.
    pub fn line(now: SystemTime, pid: u32, message: &str) -> String {
        format!("{} pid={} {}\n", iso_timestamp(now), pid, message)
    }
}

impl Default for Logger {
    fn default() -> Self {
        Self::disabled()
    }
}

/// `Date.prototype.toISOString()` for a [`SystemTime`]: UTC, millisecond
/// precision, `YYYY-MM-DDTHH:MM:SS.sssZ`.
pub fn iso_timestamp(now: SystemTime) -> String {
    let duration = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = duration.as_secs();
    let millis = duration.subsec_millis();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Days-since-epoch -> (year, month, day), Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_file(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acp-logger-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join(LOG_FILE_NAME)
    }

    #[test]
    fn inv_logger_iso_timestamp_matches_to_iso_string() {
        assert_eq!(
            iso_timestamp(UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z",
            "epoch"
        );
        assert_eq!(
            iso_timestamp(UNIX_EPOCH + Duration::from_millis(1_000)),
            "1970-01-01T00:00:01.000Z",
            "one second"
        );
        // 2000-02-29T00:00:00Z (a leap day) = 951782400 s.
        assert_eq!(
            iso_timestamp(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00.000Z",
            "leap day"
        );
    }

    #[test]
    fn inv_logger_line_has_pid_and_timestamp() {
        let line = Logger::line(UNIX_EPOCH + Duration::from_millis(42), 4242, "hello");
        assert_eq!(line, "1970-01-01T00:00:00.042Z pid=4242 hello\n");
    }

    #[test]
    fn inv_logger_writes_agent_log_only_when_enabled() {
        // Disabled logger: no file, no error.
        let disabled = Logger::disabled();
        assert!(!disabled.enabled());
        disabled.log("not written");

        // Enabled: appends one `{iso} pid={pid} {message}` line per call.
        let path = temp_file("write");
        let logger = Logger::to_file(&path);
        assert!(logger.enabled());
        logger.log("Claude ACP started");
        logger.log("second");

        let content = std::fs::read_to_string(&path).expect("read agent.log");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "one line per log call: {content:?}");
        assert!(
            lines[0].contains(" pid=") && lines[0].ends_with(" Claude ACP started"),
            "line shape: {}",
            lines[0]
        );
        let pid_marker = format!(" pid={} ", std::process::id());
        assert!(
            lines[0].contains(&pid_marker),
            "pid from the process: {}",
            lines[0]
        );
        assert!(lines[1].ends_with(" second"));

        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn inv_logger_env_selects_directory() {
        let dir = std::env::temp_dir().join(format!(
            "acp-logger-env-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let unset = EnvSnapshot::empty();
        assert!(!Logger::from_env(&unset).enabled(), "unset -> disabled");

        let empty = EnvSnapshot::from_pairs([(LOGS_ENV, "")]);
        assert!(!Logger::from_env(&empty).enabled(), "empty -> disabled");

        let env = EnvSnapshot::from_pairs([(LOGS_ENV, dir.to_string_lossy().into_owned())]);
        let logger = Logger::from_env(&env);
        assert!(logger.enabled(), "set -> enabled");
        logger.log("Claude ACP started");
        assert!(dir.join(LOG_FILE_NAME).exists(), "agent.log created in dir");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inv_logger_error_also_writes_file() {
        let path = temp_file("error");
        let logger = Logger::to_file(&path);
        logger.error("boom");
        let content = std::fs::read_to_string(&path).expect("read agent.log");
        assert!(content.ends_with(" boom\n"), "error logged: {content:?}");
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }
}
