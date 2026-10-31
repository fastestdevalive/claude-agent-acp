//! Turn 2 fix round 2, lane G2: `Forget` removes only the session the archive
//! closed.

use super::*;
use crate::session::Command;

/// _session/archive > a session loaded under the id since is not forgotten with the closed one
#[test]
fn t2gb_forget_names_the_closed_instance_only() {
    let (closed, _closed_rx) = mpsc::unbounded_channel::<Command>();
    let (reloaded, _reloaded_rx) = mpsc::unbounded_channel::<Command>();
    let husk = closed.downgrade();
    assert!(is_registered_instance(&closed, &husk));
    assert!(
        !is_registered_instance(&reloaded, &husk),
        "a new session under the same id is another instance"
    );
}

/// _session/archive > a husk whose channel is gone matches nothing
#[test]
fn t2gb_forget_of_a_dropped_instance_matches_nothing() {
    let (closed, _closed_rx) = mpsc::unbounded_channel::<Command>();
    let (live, _live_rx) = mpsc::unbounded_channel::<Command>();
    let husk = closed.downgrade();
    drop(closed);
    assert!(!is_registered_instance(&live, &husk));
}
