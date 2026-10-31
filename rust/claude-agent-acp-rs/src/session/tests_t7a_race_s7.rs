//! CONC-2 scenario 7: the child exits mid-turn at a random point of its script.
//!
//! A clean exit settles the in-flight turn `end_turn` (the iterator ends,
//! `acp-agent.ts:5764-5800`); an abnormal exit fails it with the process-died
//! error. Either way the prompt settles, nothing hangs, and the actor tears down
//! cleanly (in-process there is no child; `tests/conc_race_child_exit.rs` covers
//! the process-count side against a real fake CLI).

use super::tests_t7a_race_common::*;
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t7a_race_7_child_exit_mid_turn() {
    run_race("child-exit-mid-turn", |seed| async move {
        let mut rng = Rng::new(seed);
        let mut actor = Actor::spawn();
        let reply_rx = actor.prompt("p1");
        // The exit lands at a random point of the turn's script.
        let script = TurnScript::new("p1").deltas(rng.below(50));
        for frame in script.frames() {
            actor.feed(frame.clone());
            if rng.one_in(3) {
                jitter(&mut rng, 2).await;
            }
        }
        let abnormal = rng.one_in(2);
        actor.end_stream(Some(abnormal));
        let settled = settle(reply_rx, Duration::from_secs(5)).await?;
        match settled {
            Ok(reply) => {
                if abnormal {
                    return Err(format!(
                        "an abnormal exit settled the turn {} instead of failing it",
                        reply.stop_reason
                    ));
                }
                if reply.stop_reason != "end_turn" {
                    return Err(format!(
                        "a clean exit settled {} instead of end_turn",
                        reply.stop_reason
                    ));
                }
            }
            Err(failure) => {
                if !abnormal {
                    return Err(format!("a clean exit failed the turn: {:?}", failure.error));
                }
                if !matches!(&failure.error, SessionError::PromptFailed { .. }) {
                    return Err(format!(
                        "an abnormal exit did not fail the turn: {:?}",
                        failure.error
                    ));
                }
            }
        }
        actor.shutdown().await?;
        // The actor ended (a closed session accepts no command).
        if actor.tx.send(Command::Cancel).is_ok() {
            return Err("the actor outlived its stream and close".into());
        }
        Ok(())
    })
    .await;
}
