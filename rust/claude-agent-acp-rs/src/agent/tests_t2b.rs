//! Turn 2 lane B, carried item: a session whose start returned but that never
//! reached the dispose list is closed when its recreate is dropped.

use super::*;
use crate::session::Command;
use std::time::Duration;

#[tokio::test]
async fn t2b_unwired_session_guard_closes_the_actor_when_dropped_armed() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    drop(UnwiredSessionGuard(Some(tx)));
    let command = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("the guard acted");
    assert!(matches!(command, Some(Command::Close { .. })));
}

/// The recreate task is aborted while the replay or `seed_task_state` still runs:
/// dropping the future drops the armed guard, which closes the actor (and so its
/// `claude` child).
#[tokio::test(start_paused = true)]
async fn t2b_aborting_a_task_that_holds_an_armed_guard_closes_the_actor() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let _guard = UnwiredSessionGuard(Some(tx));
        // The replay: far longer than the connection's dispose bound.
        tokio::time::sleep(Duration::from_secs(600)).await;
    });
    tokio::time::advance(Duration::from_secs(7)).await;
    assert!(
        rx.try_recv().is_err(),
        "nothing is closed while the replay runs"
    );
    task.abort();
    let command = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the abort closed the actor");
    assert!(matches!(command, Some(Command::Close { .. })));
}

#[tokio::test]
async fn t2b_unwired_session_guard_sends_nothing_once_disarmed() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut guard = UnwiredSessionGuard(Some(tx));
    guard.disarm();
    drop(guard);
    let command = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("the channel closed");
    assert!(command.is_none());
}
