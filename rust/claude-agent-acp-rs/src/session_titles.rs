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

/// The client-set title of a session (`ExplicitTitle`,
/// `session-index/explicit-title.ts`), as [`SessionTitles`] guards it against
/// generated titles: once a client named the session, or while a rename is in
/// flight, no generated title is adopted, and a rename waits for a generation in
/// flight, whose persisted title would otherwise land after it.
#[derive(Debug, Default, Clone)]
pub struct ExplicitTitle {
    /// Set once a client named the session. Released by [`Self::reset`].
    named: bool,
    /// Renames in flight.
    pending: u32,
    /// Renames started so far, to tell that one ran between two reads.
    started: u64,
    /// A title generation is in flight.
    generating: bool,
}

impl ExplicitTitle {
    /// Whether a generated title must not be adopted.
    pub fn active(&self) -> bool {
        self.named || self.pending > 0
    }

    /// Whether a client named the session and no rename is in flight: a title
    /// stored since by someone else (a `/rename`) may still be adopted.
    pub fn settled_by_client(&self) -> bool {
        self.named && self.pending == 0
    }

    /// Changes with every rename that starts.
    pub fn renames(&self) -> u64 {
        self.started
    }

    pub fn reset(&mut self) {
        self.named = false;
    }

    /// A title generation starts (`track`).
    pub fn begin_generation(&mut self) {
        self.generating = true;
    }

    /// The title generation in flight ended.
    pub fn end_generation(&mut self) {
        self.generating = false;
    }

    /// Whether a rename has to wait for a generation (`await this.generation`).
    pub fn generation_in_flight(&self) -> bool {
        self.generating
    }

    /// A rename starts (`apply`: `pending++`, `started++`).
    fn begin_rename(&mut self) {
        self.pending += 1;
        self.started += 1;
    }

    /// A rename ended. `named` tells whether it left the session named; `None`
    /// is a failure. Returns whether the title state has to be restored: the
    /// rename failed or named nothing, and no other rename named the session.
    fn end_rename(&mut self, named: Option<bool>) -> bool {
        let named_now = named == Some(true);
        if named_now {
            self.named = true;
        }
        let restore = !named_now && !self.named && self.pending == 1;
        self.pending -= 1;
        restore
    }
}

/// Makes the title to publish from what an explicit change stored.
pub type TitleFromStored = Box<dyn FnOnce(Option<&str>) -> String + Send>;

/// How the title a client chose is published once it is stored
/// (`setExplicitTitle`'s `publish`).
pub enum ExplicitPublish {
    /// Nothing is published.
    None,
    /// This title.
    Title(String),
    /// The title that follows from what was stored.
    FromStored(TitleFromStored),
}

impl std::fmt::Debug for ExplicitPublish {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Title(title) => f.debug_tuple("Title").field(title).finish(),
            Self::FromStored(_) => f.write_str("FromStored(..)"),
        }
    }
}

/// How a stored title is shown to the client (`ClaudeAcpAgent.clientTitle`): an
/// AIR client gets the title of an archived session without the archive prefix.
#[derive(Clone)]
pub struct ClientTitle(pub std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>);

impl std::fmt::Debug for ClientTitle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientTitle(..)")
    }
}

/// One session's title state: rolling text, whether settled, and the last title published.
#[derive(Debug, Default, Clone)]
pub struct SessionTitles {
    /// The title a client set, which no generated title replaces.
    pub explicit: ExplicitTitle,
    /// The title last stored through an explicit change, as stored (with the
    /// archive prefix of an archived session): the title the CLI holds
    /// (`persistedTitle`).
    pub persisted_title: Option<String>,
    /// Client title changes in flight (`explicitChanges`).
    explicit_changes: u32,
    /// The title state before the first of the changes in flight.
    before_explicit: Option<(bool, Option<String>)>,
    /// How a stored title reads on the client (see [`ClientTitle`]).
    pub client_title: Option<ClientTitle>,
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

    /// Drop the title state on conversation reset (`reset`): a fresh transcript
    /// mounts, so the cached title no longer describes the session and the next
    /// turn end re-evaluates, with the client-set title forgotten too.
    pub fn reset(&mut self) {
        self.settled = false;
        self.explicit.reset();
        self.context = None;
        self.last_title = None;
        self.persisted_title = None;
    }

    /// The title last published, as the client shows it (`shownTitle`).
    pub fn shown_title(&self) -> Option<&str> {
        self.last_title.as_deref()
    }

    /// The title the CLI holds (`storedTitle`).
    pub fn stored_title(&self) -> Option<&str> {
        self.persisted_title.as_deref()
    }

    /// Records a title the CLI took, as soon as it took it (`rememberStoredTitle`).
    pub fn remember_stored_title(&mut self, title: &str) {
        self.persisted_title = Some(title.to_string());
    }

    /// How `raw` reads on the client, before it is sanitised.
    fn client_text(&self, raw: &str) -> String {
        match &self.client_title {
            Some(map) => (map.0)(raw),
            None => raw.to_string(),
        }
    }

    /// A client title change starts (`setExplicitTitle`, first half): the title
    /// is settled for good and the collected text goes. The state before the first
    /// of several overlapping changes is kept for one that fails or names nothing.
    pub fn begin_explicit_change(&mut self) {
        if self.explicit_changes == 0 {
            self.before_explicit = Some((self.settled, self.context.clone()));
        }
        self.explicit_changes += 1;
        self.settled = true;
        self.context = None;
        self.explicit.begin_rename();
    }

    /// A client title change ended (`setExplicitTitle`, second half). `stored` is
    /// what `persist` returned, `None` for a failure; `publishes` whether a title
    /// is to be published. A change that failed, stored nothing and publishes
    /// nothing leaves the title state as it was.
    pub fn end_explicit_change(&mut self, stored: Option<Option<&str>>, publishes: bool) {
        let named = stored.map(|stored| stored.is_some() || publishes);
        if let Some(Some(stored)) = stored {
            self.persisted_title = Some(stored.to_string());
        }
        if self.explicit.end_rename(named) {
            if let Some((settled, context)) = self.before_explicit.clone() {
                self.settled = settled;
                if self.context.is_none() {
                    self.context = context;
                }
            }
        }
        self.explicit_changes -= 1;
        if self.explicit_changes == 0 {
            self.before_explicit = None;
        }
    }

    /// The `session_info_update` that publishes a title a client chose
    /// (`setExplicitTitle`): the title as given, with no `updatedAt`.
    pub fn publish_explicit(&mut self, title: String) -> serde_json::Value {
        self.last_title = Some(title.clone());
        serde_json::json!({ "sessionUpdate": "session_info_update", "title": title })
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
        self.publish_stored(custom_title, last_modified)
    }

    /// `publish`: notify the client of a title, unless it is the one last sent.
    /// The title reads as the client shows it, sanitised.
    pub fn publish_stored(
        &mut self,
        raw_title: &str,
        last_modified: Option<&str>,
    ) -> Option<(String, Option<String>)> {
        let sanitized = crate::session_store::sanitize_title(&self.client_text(raw_title));
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
        self.explicit.end_generation();
        // A title the client set meanwhile wins (`this.explicit.active`).
        if self.explicit.active() {
            return None;
        }
        if let Some(raw) = title.filter(|raw| !raw.is_empty()) {
            self.context = None;
            self.settled = true;
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let now = crate::session_store::iso8601(now_ms);
            return self.publish_stored(raw, Some(&now));
        }
        // Generation yielded nothing: release the latch.
        self.settled = false;
        fallback.and_then(|(fb_title, fb_mod)| self.publish_stored(fb_title, fb_mod))
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

    /// B-78 — a slash command's arguments are part of the collected context; an
    /// argument-less one is too short to ask for a title.
    #[test]
    fn t2a_prompt_arguments_are_in_the_title_context() {
        let mut titles = SessionTitles::new();
        titles.on_prompt("/investigate-alert kafka lag on prod-3");
        assert!(titles.can_request(false));
        assert!(titles
            .context_trimmed()
            .contains("/investigate-alert kafka lag on prod-3"));
        let mut bare = SessionTitles::new();
        bare.on_prompt("/compact");
        assert!(!bare.can_request(false), "too little text to title");
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
