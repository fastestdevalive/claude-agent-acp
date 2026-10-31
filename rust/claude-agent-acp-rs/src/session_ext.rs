//! Per-session extension state owned by the session actor (rust-air-gaps
//! Decision 7).
//!
//! One struct for the actor's AIR / working-directory state, so later items
//! (goal, file-change report, failure context) add fields here instead of new
//! parameters. It lives inside the actor task: no `Mutex`. It is read by the
//! actor loop itself; the frame handlers do not take it until an item needs it.

use std::path::PathBuf;

use crate::air::AirClient;

/// Actor-owned extension state: the parsed AIR client declaration and the
/// session working directory.
#[derive(Debug)]
pub struct SessionExt {
    /// The parsed AIR client declaration, if the client is AIR.
    pub air: Option<AirClient>,
    /// The session's working directory.
    pub cwd: PathBuf,
}

impl SessionExt {
    /// Build the extension state for one session actor.
    pub fn new(air: Option<AirClient>, cwd: PathBuf) -> Self {
        Self { air, cwd }
    }

    /// Whether the client declared the AIR capability `name`.
    pub fn air_supports(&self, name: &str) -> bool {
        self.air.as_ref().is_some_and(|air| air.supports(name))
    }
}
