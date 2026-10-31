//! The client-set title of a session (`_session/rename`), as `SessionTitles`
//! guards it against generated titles (upstream `session-index/explicit-title.ts`):
//! once a client named the session, or while a rename is in flight, no generated
//! title is adopted, and a rename waits for a generation in flight, whose
//! persisted title would otherwise land after it.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::watch;

/// Marks a title generation in flight until it is dropped.
pub struct GenerationGuard {
    in_flight: Arc<watch::Sender<usize>>,
}

impl Drop for GenerationGuard {
    fn drop(&mut self) {
        self.in_flight
            .send_modify(|count| *count = count.saturating_sub(1));
    }
}

pub struct ExplicitTitle {
    /// Set once a client named the session. Released by [`reset`](Self::reset).
    named: AtomicBool,
    /// Renames in flight.
    pending: AtomicUsize,
    /// Renames started so far, to tell that one ran between two reads.
    started: AtomicUsize,
    /// The title generations in flight.
    generations: Arc<watch::Sender<usize>>,
}

impl Default for ExplicitTitle {
    fn default() -> Self {
        Self::new()
    }
}

impl ExplicitTitle {
    pub fn new() -> Self {
        Self {
            named: AtomicBool::new(false),
            pending: AtomicUsize::new(0),
            started: AtomicUsize::new(0),
            generations: Arc::new(watch::channel(0).0),
        }
    }

    /// Whether a generated title must not be adopted.
    pub fn active(&self) -> bool {
        self.named.load(Ordering::SeqCst) || self.pending.load(Ordering::SeqCst) > 0
    }

    /// Whether a client named the session and no rename is in flight: a title
    /// stored since by someone else (a `/rename`) may still be adopted.
    pub fn settled_by_client(&self) -> bool {
        self.named.load(Ordering::SeqCst) && self.pending.load(Ordering::SeqCst) == 0
    }

    /// Changes with every rename that starts.
    pub fn renames(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.named.store(false, Ordering::SeqCst);
    }

    /// Tracks a title generation until the returned guard is dropped.
    pub fn track(&self) -> GenerationGuard {
        self.generations.send_modify(|count| *count += 1);
        GenerationGuard {
            in_flight: self.generations.clone(),
        }
    }

    /// Persists a client title after the generations in flight. `persist` tells
    /// whether the session is named now. When it fails or names nothing (an
    /// archive of an archived session), and no other rename named the session,
    /// `restore` puts the title state back, so a later turn may still generate a
    /// title.
    pub async fn apply<E, Fut>(
        &self,
        persist: impl FnOnce() -> Fut,
        restore: impl FnOnce(),
    ) -> Result<(), E>
    where
        Fut: Future<Output = Result<bool, E>>,
    {
        self.pending.fetch_add(1, Ordering::SeqCst);
        self.started.fetch_add(1, Ordering::SeqCst);
        let mut idle = self.generations.subscribe();
        let _ = idle.wait_for(|count| *count == 0).await;
        let result = persist().await;
        let outcome = match result {
            Ok(true) => {
                self.named.store(true, Ordering::SeqCst);
                Ok(())
            }
            Ok(false) => {
                if !self.named.load(Ordering::SeqCst) && self.pending.load(Ordering::SeqCst) == 1 {
                    restore();
                }
                Ok(())
            }
            Err(error) => {
                if !self.named.load(Ordering::SeqCst) && self.pending.load(Ordering::SeqCst) == 1 {
                    restore();
                }
                Err(error)
            }
        };
        self.pending.fetch_sub(1, Ordering::SeqCst);
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn a_named_session_refuses_generated_titles_until_reset() {
        let title = ExplicitTitle::new();
        assert!(!title.active());
        title
            .apply(|| async { Ok::<bool, ()>(true) }, || {})
            .await
            .expect("applied");
        assert!(title.active());
        assert!(title.settled_by_client());
        assert_eq!(title.renames(), 1);
        title.reset();
        assert!(!title.active());
    }

    #[tokio::test]
    async fn a_rename_that_names_nothing_restores_the_title_state() {
        let title = ExplicitTitle::new();
        let restored = AtomicBool::new(false);
        title
            .apply(
                || async { Ok::<bool, ()>(false) },
                || restored.store(true, Ordering::SeqCst),
            )
            .await
            .expect("applied");
        assert!(restored.load(Ordering::SeqCst));
        assert!(!title.active());
        let restored = AtomicBool::new(false);
        let failed = title
            .apply(
                || async { Err::<bool, &str>("boom") },
                || restored.store(true, Ordering::SeqCst),
            )
            .await;
        assert_eq!(failed, Err("boom"));
        assert!(restored.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_rename_waits_for_a_generation_in_flight() {
        let title = Arc::new(ExplicitTitle::new());
        let generation = title.track();
        let ran = Arc::new(AtomicBool::new(false));
        let task = {
            let (title, ran) = (title.clone(), ran.clone());
            tokio::spawn(async move {
                title
                    .apply(
                        || async {
                            ran.store(true, Ordering::SeqCst);
                            Ok::<bool, ()>(true)
                        },
                        || {},
                    )
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            !ran.load(Ordering::SeqCst),
            "persist waits for the generation"
        );
        assert!(
            title.active(),
            "the rename in flight blocks generated titles"
        );
        drop(generation);
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("finished")
            .expect("joined")
            .expect("applied");
        assert!(ran.load(Ordering::SeqCst));
    }
}
