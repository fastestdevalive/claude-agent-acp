//! Session title generation and state machine (Decision 13 / D6).
//!
//! Ports `session-titles.ts`: collects rolling context (up to 1000 characters),
//! asks the CLI child process for a generated title via `generate_session_title`
//! control request, adopts stored custom titles, and notifies the client via
//! `session_info_update`.

pub const MAX_TITLE_LENGTH: usize = 256;
pub const MAX_TITLE_CONTEXT_LENGTH: usize = 1000;
pub const MIN_TITLE_CONTEXT_LENGTH: usize = 10;

/// Rolling text a title is generated from: `previous` plus `text`, keeping only
/// the trailing [`MAX_TITLE_CONTEXT_LENGTH`] characters.
pub fn append_title_context(previous: Option<&str>, text: &str) -> String {
    let mut next = String::new();
    if let Some(prev) = previous {
        next.push_str(prev);
    }
    next.push_str(text);
    let count = next.chars().count();
    if count > MAX_TITLE_CONTEXT_LENGTH {
        let skip = count - MAX_TITLE_CONTEXT_LENGTH;
        next.chars().skip(skip).collect()
    } else {
        next
    }
}

/// One session's title state: rolling text, whether settled, and the last title published.
#[derive(Debug, Default, Clone)]
pub struct SessionTitles {
    /// Last title pushed to the client via `session_info_update`.
    pub last_title: Option<String>,
    /// Rolling tail of this session's own user + assistant text, capped at 1000 chars.
    pub context: Option<String>,
    /// Set once this session has a title or has asked for one. Released by [`reset`],
    /// and when generation yields nothing.
    pub settled: bool,
    /// The transcript resolved for this session's cwd project directory. Once
    /// found it is reused, so a settled session's later turn ends read that one
    /// file instead of scanning every project directory.
    pub transcript: Option<std::path::PathBuf>,
}

/// Resolve and read the stored session info off the actor thread (the caller
/// runs this in `spawn_blocking`). With a cached transcript path no directory
/// scan happens at all; otherwise the lookup is restricted to the session's cwd
/// project directory, like Node's `getSessionInfo(id, {dir: session.cwd})`.
pub fn load_stored_info(
    projects_root: &std::path::Path,
    cwd: &str,
    session_id: &str,
    cached: Option<&std::path::Path>,
) -> (
    Option<std::path::PathBuf>,
    Option<crate::session_store::StoredSessionInfo>,
) {
    let path = match cached {
        Some(path) => Some(path.to_path_buf()),
        None => crate::session_store::find_transcript_for_dir(projects_root, cwd, session_id),
    };
    let info = path
        .as_deref()
        .and_then(crate::session_store::read_stored_session_info_at);
    (path, info)
}

impl SessionTitles {
    pub fn new() -> Self {
        Self::default()
    }

    /// Collect prompt text for the title. No-op once settled. An empty prompt
    /// still appends its `"\n\n"` separator, like Node's `onPrompt`.
    pub fn on_prompt(&mut self, text: &str) {
        if self.settled {
            return;
        }
        let piece = format!("\n{}\n", text);
        self.context = Some(append_title_context(self.context.as_deref(), &piece));
    }

    /// Collect assistant text for the title. No-op once settled.
    pub fn on_assistant_text(&mut self, text: &str) {
        if self.settled || text.is_empty() {
            return;
        }
        self.context = Some(append_title_context(self.context.as_deref(), text));
    }

    /// Drop the title state on conversation reset.
    pub fn reset(&mut self) {
        self.settled = false;
        self.context = None;
        self.last_title = None;
    }

    /// Whether to ask the CLI for a generated title: once per session, not cancelled,
    /// and enough collected text (at least 10 non-whitespace chars).
    pub fn can_request(&self, cancelled: bool) -> bool {
        if self.settled || cancelled {
            return false;
        }
        let trimmed_len = self
            .context
            .as_deref()
            .map(|s| s.trim().chars().count())
            .unwrap_or(0);
        trimmed_len >= MIN_TITLE_CONTEXT_LENGTH
    }

    /// Mark as settled when generation is requested.
    pub fn mark_settled(&mut self) {
        self.settled = true;
    }

    /// Returns the trimmed context string if any.
    pub fn context_trimmed(&self) -> String {
        self.context
            .as_deref()
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    /// Whether publishing `title` is needed (true if differing from `last_title`).
    pub fn publish_needed(&self, title: &str) -> bool {
        self.last_title.as_deref() != Some(title)
    }

    /// Adopt a stored custom title (or aiTitle) and latch.
    pub fn adopt_custom_title(
        &mut self,
        custom_title: &str,
        last_modified: Option<&str>,
    ) -> Option<(String, Option<String>)> {
        self.settled = true;
        self.context = None;
        let sanitized = crate::session_store::sanitize_title(custom_title);
        if self.publish_needed(&sanitized) {
            self.last_title = Some(sanitized.clone());
            Some((sanitized, last_modified.map(ToString::to_string)))
        } else {
            None
        }
    }

    /// Called when the background `generate_session_title` control response arrives.
    /// If title is Some, sanitize and publish, and keep settled.
    /// If title is None, release the settled latch (so a later turn retries) and fall back to summary.
    pub fn on_title_generated(
        &mut self,
        title: Option<&str>,
        fallback: Option<(&str, Option<&str>)>,
    ) -> Option<(String, Option<String>)> {
        if let Some(raw) = title {
            if !raw.trim().is_empty() {
                let sanitized = crate::session_store::sanitize_title(raw);
                self.context = None;
                self.settled = true;
                if self.publish_needed(&sanitized) {
                    self.last_title = Some(sanitized.clone());
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let now = crate::session_store::iso8601(now_ms);
                    return Some((sanitized, Some(now)));
                }
                return None;
            }
        }
        // Generation yielded nothing: release the latch.
        self.settled = false;
        if let Some((fb_title, fb_mod)) = fallback {
            let sanitized = crate::session_store::sanitize_title(fb_title);
            if self.publish_needed(&sanitized) {
                self.last_title = Some(sanitized.clone());
                return Some((sanitized, fb_mod.map(ToString::to_string)));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inv_80_context_rolls_to_1000_chars() {
        let first = "a".repeat(600);
        let second = "b".repeat(600);
        let combined = append_title_context(Some(&first), &second);
        assert_eq!(combined.chars().count(), 1000);
        assert_eq!(&combined[..400], &"a".repeat(400));
        assert_eq!(&combined[400..], &"b".repeat(600));
    }

    #[test]
    fn inv_80_generation_requires_10_chars_and_not_cancelled() {
        let mut titles = SessionTitles::new();
        assert!(!titles.can_request(false));
        titles.on_prompt("short");
        assert!(!titles.can_request(false));
        titles.on_prompt("now long enough");
        assert!(titles.can_request(false));
        assert!(!titles.can_request(true)); // cancelled
    }

    #[test]
    fn inv_80_adopts_custom_title_and_latches() {
        let mut titles = SessionTitles::new();
        titles.on_prompt("some context here that is long enough");
        assert!(titles.can_request(false));

        let res = titles.adopt_custom_title("Renamed Project", Some("2026-01-01T00:00:00Z"));
        assert_eq!(
            res,
            Some((
                "Renamed Project".to_string(),
                Some("2026-01-01T00:00:00Z".to_string())
            ))
        );
        assert!(titles.settled);
        assert!(titles.context.is_none());
        assert!(!titles.can_request(false));
    }

    #[test]
    fn inv_80_publishes_once_per_title() {
        let mut titles = SessionTitles::new();
        let first = titles.adopt_custom_title("Same Title", None);
        assert!(first.is_some());
        let second = titles.adopt_custom_title("Same Title", None);
        assert!(second.is_none());
    }

    #[test]
    fn inv_80_reset_releases_latch() {
        let mut titles = SessionTitles::new();
        let _ = titles.adopt_custom_title("Settled", None);
        assert!(titles.settled);
        assert_eq!(titles.last_title.as_deref(), Some("Settled"));

        titles.reset();
        assert!(!titles.settled);
        assert!(titles.last_title.is_none());
        assert!(titles.context.is_none());
    }

    /// Review G14 finding 2 — the turn-end lookup is restricted to the cwd
    /// project directory and, once a transcript has been resolved, reuses it:
    /// a settled session's later turn end performs no directory scan (an empty
    /// root finds the cached file's info all the same).
    #[test]
    fn inv_80_settled_title_uses_cached_transcript_no_scan() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let base = std::env::temp_dir().join(format!(
            "inv80-title-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let root = base.join("projects");
        let work = base.join("work");
        let empty_root = base.join("empty-projects");
        std::fs::create_dir_all(&work).expect("cwd");
        std::fs::create_dir_all(&empty_root).expect("empty root");
        let real_work = std::fs::canonicalize(&work).expect("canonical cwd");
        let slug = crate::session_store::project_slug(&real_work.to_string_lossy());
        let project = root.join(slug);
        std::fs::create_dir_all(&project).expect("project dir");
        let transcript = project.join(format!("{id}.jsonl"));
        std::fs::write(
            &transcript,
            "{\"type\":\"summary\",\"summary\":\"Disk title\"}\n",
        )
        .expect("transcript");

        let mut titles = SessionTitles::new();
        titles.settled = true;
        let (cached, info) = load_stored_info(&root, &real_work.to_string_lossy(), id, None);
        assert_eq!(cached.as_deref(), Some(transcript.as_path()));
        assert_eq!(
            info.as_ref().and_then(|i| i.summary.as_deref()),
            Some("Disk title")
        );
        titles.transcript = cached.clone();

        // A second read with the cached path ignores the scan root entirely: an
        // empty projects root would find nothing if it scanned.
        let (cached_again, info_again) = load_stored_info(
            &empty_root,
            &real_work.to_string_lossy(),
            id,
            titles.transcript.as_deref(),
        );
        assert_eq!(cached_again, cached, "the cached transcript is reused");
        assert!(
            info_again.is_some(),
            "a settled title must not depend on a filesystem scan"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn inv_80_null_title_falls_back_to_summary() {
        let mut titles = SessionTitles::new();
        titles.mark_settled();
        let res = titles.on_title_generated(
            None,
            Some(("Initial prompt summary", Some("2026-01-01T12:00:00Z"))),
        );
        assert_eq!(
            res,
            Some((
                "Initial prompt summary".to_string(),
                Some("2026-01-01T12:00:00Z".to_string())
            ))
        );
        // Latch is released when title generation is null
        assert!(!titles.settled);
    }
}
