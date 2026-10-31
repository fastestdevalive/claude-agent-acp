//! Turn 3 lane B: the gateway `authenticate` status, the probe context, the
//! epoch rules, the probe timeout warning and the per-session account kinds of the
//! `--hide-claude-auth` guard (ports of `auth-status.test.ts` cases).

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

fn status(kind: &str, label: &str) -> AuthStatus {
    AuthStatus::new(kind, label)
}

fn pro(email: &str) -> AuthStatus {
    let mut s = status("account", "Claude Pro");
    s.account = Some(AuthAccount {
        plan: Some("pro".into()),
        email: Some(email.into()),
        organization: None,
    });
    s
}

fn api_key() -> AuthStatus {
    let mut s = status("api_key", "Anthropic API key");
    s.detail = Some("ANTHROPIC_API_KEY".into());
    s
}

/// A probe that answers with `outcome` and counts its runs and records requests.
fn counting(
    outcome: ProbeOutcome,
) -> (
    ProbeFn,
    Arc<AtomicUsize>,
    mpsc::UnboundedReceiver<ProbeRequest>,
) {
    let runs = Arc::new(AtomicUsize::new(0));
    let (seen_tx, seen_rx) = mpsc::unbounded_channel();
    let counter = runs.clone();
    let probe: ProbeFn = Box::new(move |request| {
        counter.fetch_add(1, Ordering::SeqCst);
        let _ = seen_tx.send(request);
        let outcome = outcome.clone();
        Box::pin(async move { outcome })
    });
    (probe, runs, seen_rx)
}

fn actor(probe: ProbeFn, config: AuthActorConfig) -> (AuthHandle, mpsc::UnboundedReceiver<Value>) {
    let (push_tx, mut push_rx) = mpsc::unbounded_channel::<AuthPush>();
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        while let Some(item) = push_rx.recv().await {
            match item {
                AuthPush::Status(v) => {
                    let _ = out_tx.send(v);
                }
                AuthPush::Barrier(done) => {
                    let _ = done.send(());
                }
            }
        }
    });
    (
        spawn_auth_actor_with(
            push_tx,
            probe,
            PathBuf::from("/a"),
            crate::env_snapshot::EnvSnapshot::empty(),
            config,
        ),
        out_rx,
    )
}

async fn next_push(rx: &mut mpsc::UnboundedReceiver<Value>) -> Option<Value> {
    tokio::time::timeout(Duration::from_millis(300), rx.recv())
        .await
        .ok()
        .flatten()
}

fn temp_log(tag: &str) -> (Arc<crate::logger::Logger>, PathBuf) {
    let dir = std::env::temp_dir().join(format!("t3b-auth-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let logger = crate::logger::Logger::to_dir(&dir);
    let file = dir.join("agent.log");
    (Arc::new(logger), file)
}

fn context(cwd: &str) -> SessionAuthContext {
    SessionAuthContext {
        cwd: PathBuf::from(cwd),
        ..SessionAuthContext::default()
    }
}

/// authenticate > the gateway replaces whatever the CLI store reported, with the host as detail
#[tokio::test]
async fn t3b_authenticate_reports_the_gateway_host() {
    let (probe, runs, _seen) = counting(ProbeOutcome::Status(pro("a@b.c")));
    let (handle, mut pushes) = actor(probe, AuthActorConfig::default());
    handle.send(AuthCmd::Authenticate {
        base_url: Some("https://gateway.example:8443/v1".into()),
    });
    let push = next_push(&mut pushes).await.expect("a push");
    assert_eq!(push["kind"], "gateway");
    assert_eq!(push["label"], "Custom model gateway");
    assert_eq!(push["detail"], "gateway.example:8443");
    // The CLI store is bypassed: probes are not run while the gateway holds the auth.
    handle.probe(PathBuf::from("/a"), true);
    assert_eq!(next_push(&mut pushes).await, None);
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    // No payload: the gateway without a host.
    let (probe, _runs, _seen) = counting(ProbeOutcome::LoggedOut);
    let (handle, mut pushes) = actor(probe, AuthActorConfig::default());
    handle.send(AuthCmd::Authenticate { base_url: None });
    let push = next_push(&mut pushes).await.expect("a push");
    assert_eq!(push["kind"], "gateway");
    assert!(push.get("detail").is_none());
}

/// authenticate bumps the epoch: a probe in flight is stale
#[tokio::test]
async fn t3b_authenticate_discards_an_older_probe() {
    let probe: ProbeFn = Box::new(|_| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_millis(60)).await;
            ProbeOutcome::Status(pro("a@b.c"))
        })
    });
    let (handle, mut pushes) = actor(probe, AuthActorConfig::default());
    handle.probe(PathBuf::from("/a"), false);
    handle.send(AuthCmd::Authenticate {
        base_url: Some("https://g.example".into()),
    });
    let push = next_push(&mut pushes).await.expect("gateway");
    assert_eq!(push["kind"], "gateway");
    assert_eq!(
        next_push(&mut pushes).await,
        None,
        "the stale probe never publishes"
    );
}

/// B-86 / B-87: a change of cwd, helper or launch env re-probes in that context; an equal context does not
#[tokio::test]
async fn t3b_context_changes_reprobe_in_the_new_context() {
    let (probe, runs, mut seen) = counting(ProbeOutcome::Status(api_key()));
    let (handle, mut pushes) = actor(probe, AuthActorConfig::default());
    // Same as the initial context: nothing to re-probe.
    handle.use_context(context("/a")).await;
    assert_eq!(next_push(&mut pushes).await, None);
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    // Another cwd.
    handle.use_context(context("/b")).await;
    assert!(next_push(&mut pushes).await.is_some());
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(seen.recv().await.expect("request").cwd, PathBuf::from("/b"));
    // The same context again.
    handle.use_context(context("/b")).await;
    // A new apiKeyHelper.
    let mut with_helper = context("/b");
    with_helper.api_key_helper = Some("/bin/helper".into());
    handle.use_context(with_helper.clone()).await;
    // A launch env, then the same env.
    let mut with_env = with_helper.clone();
    with_env.launch_env = Some(vec![("X".into(), "1".into())]);
    with_env.settings_override = Some(json!({ "model": "haiku" }));
    handle.use_context(with_env.clone()).await;
    handle.use_context(with_env.clone()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        3,
        "cwd, helper and env each probed once"
    );
    seen.recv().await.expect("helper probe");
    let last = seen.recv().await.expect("env probe");
    assert_eq!(
        last.launch_env,
        Some(vec![("X".to_string(), "1".to_string())])
    );
    assert_eq!(last.settings_override, Some(json!({ "model": "haiku" })));
}

/// O-66: the epoch bump of a context change discards a probe that read the old context
#[tokio::test]
async fn t3b_a_context_change_discards_the_probe_of_the_old_context() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let probe: ProbeFn = Box::new(move |request| {
        let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if first {
                tokio::time::sleep(Duration::from_millis(80)).await;
                ProbeOutcome::Status(pro("old@b.c"))
            } else {
                assert_eq!(request.cwd, PathBuf::from("/b"));
                ProbeOutcome::Status(pro("new@b.c"))
            }
        })
    });
    let (handle, mut pushes) = actor(probe, AuthActorConfig::default());
    handle.probe(PathBuf::from("/a"), false);
    handle.use_context(context("/b")).await;
    let push = next_push(&mut pushes)
        .await
        .expect("the new context's read");
    assert_eq!(push["account"]["email"], "new@b.c");
    assert_eq!(
        next_push(&mut pushes).await,
        None,
        "the old read is dropped"
    );
}

/// B-82: a probe timeout pushes nothing, keeps the last state and warns once
#[tokio::test]
async fn t3b_probe_timeout_keeps_the_state_and_warns_once() {
    let (logger, file) = temp_log("timeout");
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let probe: ProbeFn = Box::new(move |_| {
        let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if first {
                ProbeOutcome::Status(pro("a@b.c"))
            } else {
                ProbeOutcome::TimedOut
            }
        })
    });
    let (handle, mut pushes) = actor(
        probe,
        AuthActorConfig {
            hide_claude_auth: false,
            logger: Some(logger),
        },
    );
    handle.probe(PathBuf::from("/a"), false);
    assert!(next_push(&mut pushes).await.is_some());
    for _ in 0..3 {
        handle.probe(PathBuf::from("/a"), true);
        assert_eq!(next_push(&mut pushes).await, None);
    }
    let log = std::fs::read_to_string(&file).unwrap_or_default();
    assert_eq!(
        log.matches("claude auth status did not answer within 5 s; the identity is not refreshed")
            .count(),
        1,
        "{log}"
    );
}

/// U-62: the session account is published unless a provider override or a gateway holds the identity
#[tokio::test]
async fn t3b_session_account_is_not_published_under_a_provider_override_or_gateway() {
    let account =
        json!({ "apiProvider": "firstParty", "subscriptionType": "max", "email": "u@x.y" });
    let created = |epoch: Option<u64>| AuthCmd::SessionCreated {
        session_id: Some("s1".into()),
        cwd: PathBuf::from("/a"),
        account: Some(account.clone()),
        api_key_helper: None,
        epoch,
    };
    // Plain: published.
    let (probe, _r, _s) = counting(ProbeOutcome::LoggedOut);
    let (handle, mut pushes) = actor(probe, AuthActorConfig::default());
    handle.send(created(None));
    assert_eq!(
        next_push(&mut pushes).await.expect("push")["account"]["email"],
        "u@x.y"
    );
    // A client provider override keeps the agent-owned probe state.
    let (probe, _r, _s) = counting(ProbeOutcome::LoggedOut);
    let (handle, mut pushes) = actor(probe, AuthActorConfig::default());
    handle.send(AuthCmd::SetRouting {
        provider_override: true,
        routing_active: true,
    });
    handle.send(created(None));
    assert_eq!(next_push(&mut pushes).await, None);
    // The legacy gateway owns the identity.
    let (probe, _r, _s) = counting(ProbeOutcome::LoggedOut);
    let (handle, mut pushes) = actor(probe, AuthActorConfig::default());
    handle.send(AuthCmd::Authenticate {
        base_url: Some("https://g.example".into()),
    });
    assert_eq!(
        next_push(&mut pushes).await.expect("gateway")["kind"],
        "gateway"
    );
    handle.send(created(None));
    assert_eq!(next_push(&mut pushes).await, None);
    // An account read under an older epoch is not published.
    let (probe, _r, _s) = counting(ProbeOutcome::LoggedOut);
    let (handle, mut pushes) = actor(probe, AuthActorConfig::default());
    let epoch = handle.use_context(context("/a")).await;
    handle.bump_epoch();
    handle.send(created(Some(epoch)));
    assert_eq!(next_push(&mut pushes).await, None);
}

/// B-91 / V-16: under the flag a probe that finds another kind of identity flags the session; the flag is set once and stays until the session is recreated
#[tokio::test]
async fn t3b_a_probe_of_another_kind_flags_the_session_for_the_next_turn() {
    let config = AuthActorConfig {
        hide_claude_auth: true,
        logger: None,
    };
    // The session started on an API key; the store now holds a subscription.
    let (probe, _runs, _seen) = counting(ProbeOutcome::Status(pro("a@b.c")));
    let (handle, mut pushes) = actor(probe, config.clone());
    handle.send(AuthCmd::SessionCreated {
        session_id: Some("s1".into()),
        cwd: PathBuf::from("/a"),
        account: Some(json!({ "apiKeySource": "ANTHROPIC_API_KEY" })),
        api_key_helper: None,
        epoch: None,
    });
    assert!(next_push(&mut pushes).await.is_some());
    assert!(!handle.needs_respawn("s1").await);
    handle.probe(PathBuf::from("/a"), true);
    assert!(next_push(&mut pushes).await.is_some());
    assert!(handle.needs_respawn("s1").await, "flagged, not interrupted");
    // The recreate registers the session again: the flag is gone.
    handle.send(AuthCmd::SessionCreated {
        session_id: Some("s1".into()),
        cwd: PathBuf::from("/a"),
        account: Some(json!({ "apiKeySource": "ANTHROPIC_API_KEY" })),
        api_key_helper: None,
        epoch: None,
    });
    assert!(!handle.needs_respawn("s1").await);
    // The same kind never flags.
    let (probe, _runs, _seen) = counting(ProbeOutcome::Status(api_key()));
    let (handle, _pushes) = actor(probe, config.clone());
    handle.send(AuthCmd::SessionCreated {
        session_id: Some("s2".into()),
        cwd: PathBuf::from("/a"),
        account: Some(json!({ "apiKeySource": "ANTHROPIC_API_KEY" })),
        api_key_helper: None,
        epoch: None,
    });
    handle.probe(PathBuf::from("/a"), true);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!handle.needs_respawn("s2").await);
    // Without the flag nothing is ever flagged, by a probe or a sign-out.
    let (probe, _runs, _seen) = counting(ProbeOutcome::Status(pro("a@b.c")));
    let (handle, _pushes) = actor(probe, AuthActorConfig::default());
    handle.send(AuthCmd::SessionCreated {
        session_id: Some("s3".into()),
        cwd: PathBuf::from("/a"),
        account: Some(json!({ "apiKeySource": "ANTHROPIC_API_KEY" })),
        api_key_helper: None,
        epoch: None,
    });
    handle.probe(PathBuf::from("/a"), true);
    handle.send(AuthCmd::MarkRespawn {
        session_id: "s3".into(),
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!handle.needs_respawn("s3").await);
    // A routing override switches the guard off: no flag.
    let (probe, _runs, _seen) = counting(ProbeOutcome::Status(pro("a@b.c")));
    let (handle, _pushes) = actor(probe, config);
    handle.send(AuthCmd::SetRouting {
        provider_override: true,
        routing_active: true,
    });
    handle.send(AuthCmd::SessionCreated {
        session_id: Some("s4".into()),
        cwd: PathBuf::from("/a"),
        account: Some(json!({ "apiKeySource": "ANTHROPIC_API_KEY" })),
        api_key_helper: None,
        epoch: None,
    });
    handle.probe(PathBuf::from("/a"), true);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!handle.needs_respawn("s4").await);
}

/// V-16: a sign-out flags the session for the recreate; forgetting it clears the flag
#[tokio::test]
async fn t3b_a_sign_out_flags_the_session_until_it_is_forgotten() {
    let (probe, _runs, _seen) = counting(ProbeOutcome::LoggedOut);
    let (handle, _pushes) = actor(
        probe,
        AuthActorConfig {
            hide_claude_auth: true,
            logger: None,
        },
    );
    // The flag lives on a session this connection runs (upstream keeps it on the
    // `Session` object).
    handle.send(AuthCmd::SessionCreated {
        session_id: Some("s1".into()),
        cwd: PathBuf::from("/a"),
        account: Some(json!({ "apiKeySource": "ANTHROPIC_API_KEY" })),
        api_key_helper: None,
        epoch: None,
    });
    handle.send(AuthCmd::MarkRespawn {
        session_id: "s1".into(),
    });
    assert!(handle.needs_respawn("s1").await);
    handle.send(AuthCmd::ForgetSession {
        session_id: "s1".into(),
    });
    assert!(!handle.needs_respawn("s1").await);
}

/// U-61: the vendor bag rides the payload and survives a merge of the same identity
#[test]
fn t3b_the_vendor_bag_is_part_of_the_payload() {
    let mut first = pro("a@b.c");
    first.vendor = Some(Box::new(json!({ "claudeCode": { "x": 1 } })));
    assert_eq!(
        first.to_json()["vendor"],
        json!({ "claudeCode": { "x": 1 } })
    );
    // A richer read of the same login keeps the vendor it lacks.
    let merged = merge(Some(&first), &pro("a@b.c"));
    assert_eq!(merged.vendor, first.vendor);
    // A different vendor bag is a different payload (every rendered field counts).
    let mut other = pro("a@b.c");
    other.vendor = Some(Box::new(json!({ "claudeCode": { "x": 2 } })));
    assert!(!same_status(Some(&first), &other));
    assert!(same_status(Some(&first), &first.clone()));
    assert!(pro("a@b.c").to_json().get("vendor").is_none());
}

/// `authorization.test.ts` > "reports the identity the respawned session
/// logged back in with": each `SessionCreated` publishes its account, so a
/// recreate reports the new login without a probe.
#[tokio::test]
async fn t8c_a_recreated_session_publishes_the_new_identity() {
    let (probe, _runs, _seen) = counting(ProbeOutcome::LoggedOut);
    let (handle, mut pushes) = actor(probe, AuthActorConfig::default());
    handle.send(AuthCmd::SessionCreated {
        session_id: Some("s1".into()),
        cwd: PathBuf::from("/a"),
        account: Some(json!({ "apiKeySource": "ANTHROPIC_API_KEY" })),
        api_key_helper: None,
        epoch: None,
    });
    let first = next_push(&mut pushes).await.expect("first identity");
    assert_eq!(first["kind"], "api_key");
    assert_eq!(first["detail"], "ANTHROPIC_API_KEY");
    // The respawned session signed in with a helper key.
    handle.send(AuthCmd::SessionCreated {
        session_id: Some("s1".into()),
        cwd: PathBuf::from("/a"),
        account: Some(json!({ "apiKeySource": "apiKeyHelper" })),
        api_key_helper: Some("my-helper".into()),
        epoch: None,
    });
    let second = next_push(&mut pushes).await.expect("second identity");
    assert_eq!(second["kind"], "api_key");
    assert_eq!(second["detail"], "API key helper");
}

/// `authorization.test.ts` > "never marks a session whose account named no
/// identity": an OAuth token carries no identity signal, so there is no kind
/// to compare and the probe never flags the session.
#[tokio::test]
async fn t8c_an_account_without_an_identity_is_never_marked() {
    let config = AuthActorConfig {
        hide_claude_auth: true,
        logger: None,
    };
    let (probe, _runs, _seen) = counting(ProbeOutcome::Status(pro("a@b.c")));
    let (handle, mut pushes) = actor(probe, config);
    handle.send(AuthCmd::SessionCreated {
        session_id: Some("s1".into()),
        cwd: PathBuf::from("/a"),
        account: Some(json!({ "tokenSource": "CLAUDE_CODE_OAUTH_TOKEN" })),
        api_key_helper: None,
        epoch: None,
    });
    // The account carries no identity signal, so nothing is published.
    assert_eq!(next_push(&mut pushes).await, None);
    handle.probe(PathBuf::from("/a"), true);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!handle.needs_respawn("s1").await);
}

/// B-83 — one in-flight probe serves every reader: a second read that does not
/// ask to be fresh joins the running one instead of spawning another process.
#[tokio::test]
async fn t9b_probe_is_shared_between_readers() {
    let (probe, runs, _seen) = counting(ProbeOutcome::Status(pro("a@b.c")));
    let (handle, mut pushes) = actor(probe, AuthActorConfig::default());
    handle.probe(PathBuf::from("/a"), false);
    handle.probe(PathBuf::from("/a"), false);
    assert_eq!(
        next_push(&mut pushes).await.expect("the shared read")["account"]["email"],
        "a@b.c"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "two readers, one CLI process"
    );
    assert_eq!(next_push(&mut pushes).await, None);
}

/// B-84 — a probe that started before `logout` is discarded even when it lands
/// after the logout's own read; only the fresh read publishes.
#[tokio::test]
async fn t9b_stale_probe_landing_after_logout_is_discarded() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let probe: ProbeFn = Box::new(move |_request| {
        let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if first {
                tokio::time::sleep(Duration::from_millis(80)).await;
                ProbeOutcome::Status(pro("stale@b.c"))
            } else {
                tokio::time::sleep(Duration::from_millis(10)).await;
                ProbeOutcome::LoggedOut
            }
        })
    });
    let (handle, mut pushes) = actor(probe, AuthActorConfig::default());
    handle.probe(PathBuf::from("/a"), false);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    handle.send(AuthCmd::Logout {
        cwd: PathBuf::from("/a"),
        reply: reply_tx,
    });
    reply_rx.await.expect("the post-logout read finished");
    assert_eq!(
        next_push(&mut pushes).await,
        Some(not_logged_in().to_json()),
        "the fresh read reports logged out"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        next_push(&mut pushes).await,
        None,
        "the pre-logout read never resurrects the identity"
    );
}

/// U-62 — a client provider override does not suppress the agent-owned probe.
#[tokio::test]
async fn t9b_probes_under_a_provider_override() {
    let (probe, runs, _seen) = counting(ProbeOutcome::Status(pro("a@b.c")));
    let (handle, mut pushes) = actor(probe, AuthActorConfig::default());
    handle.send(AuthCmd::SetRouting {
        provider_override: true,
        routing_active: true,
    });
    handle.probe(PathBuf::from("/a"), false);
    assert_eq!(
        next_push(&mut pushes).await.expect("the probe")["kind"],
        "account"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

/// U-62 — a session account with no identity signal keeps the probed API key;
/// a richer account upgrades the identity.
#[tokio::test]
async fn t9b_session_account_upgrades_and_uninformative_account_keeps_key() {
    let key = api_key();
    let (probe, _runs, _seen) = counting(ProbeOutcome::Status(key.clone()));
    let (handle, mut pushes) = actor(probe, AuthActorConfig::default());
    handle.probe(PathBuf::from("/a"), false);
    assert_eq!(
        next_push(&mut pushes).await.expect("the key")["kind"],
        "api_key"
    );
    for account in [json!({"apiProvider": "firstParty"}), json!({})] {
        handle.send(AuthCmd::SessionCreated {
            session_id: Some("s1".into()),
            cwd: PathBuf::from("/a"),
            account: Some(account.clone()),
            api_key_helper: None,
            epoch: None,
        });
        assert_eq!(
            next_push(&mut pushes).await,
            None,
            "an uninformative account keeps the probed key: {account}"
        );
    }
    handle.send(AuthCmd::SessionCreated {
        session_id: Some("s1".into()),
        cwd: PathBuf::from("/a"),
        account: Some(
            json!({"apiProvider": "firstParty", "subscriptionType": "pro",
            "email": "u@x.com"}),
        ),
        api_key_helper: None,
        epoch: None,
    });
    let upgraded = next_push(&mut pushes).await.expect("the account");
    assert_eq!(upgraded["kind"], "account");
    assert_eq!(upgraded["account"]["email"], "u@x.com");
}

/// The effective pre-session helper turns both the probe and the session
/// account into the gateway identity, so the second read publishes nothing and
/// no helper command reaches the payload.
#[tokio::test]
async fn t9b_central_settings_push_suppresses_the_duplicate() {
    let gateway = status("gateway", "JetBrains Air Gateway");
    let (probe, _runs, _seen) = counting(ProbeOutcome::Status(gateway.clone()));
    let (handle, mut pushes) = actor(probe, AuthActorConfig::default());
    handle.probe(PathBuf::from("/a"), false);
    let first = next_push(&mut pushes).await.expect("the gateway");
    assert_eq!(first, gateway.to_json());
    assert!(!first.to_string().contains("proxy"));
    handle.send(AuthCmd::SessionCreated {
        session_id: Some("s1".into()),
        cwd: PathBuf::from("/a"),
        account: Some(json!({"apiProvider": "firstParty", "apiKeySource": "apiKeyHelper"})),
        api_key_helper: Some("central proxy start --return-key".into()),
        epoch: None,
    });
    assert_eq!(
        next_push(&mut pushes).await,
        None,
        "the same gateway identity is not re-pushed"
    );
}
