//! G4 — F25 session-failure rows of `src/tests/acp-agent.test.ts` (stop reason
//! propagation). The product code is the Turn 3 lane A controller
//! (`session_failure.rs`, `session/failures.rs`, the result arm in
//! `session.rs`); these tests prove the remaining upstream cases end to end
//! through `handle_session_frame`.

use std::sync::Arc;

use super::tests_t2a::Rig;
use super::*;
use crate::session_failure::ClaudeFailureKind;

fn air_rig(capabilities: &[&str]) -> Rig {
    let mut rig = Rig::new();
    let caps =
        json!({"_meta": {"jetbrains": {"air": {"version": 1, "capabilities": capabilities}}}});
    rig.ext.air = crate::air::AirClient::from_client_capabilities(&caps);
    rig
}

fn session_scoped() -> failures::FailureOptions {
    failures::FailureOptions {
        session_scoped: true,
        ..failures::FailureOptions::default()
    }
}

fn assistant_error(error: &str, text: &str) -> Value {
    json!({
        "type": "assistant",
        "parent_tool_use_id": null,
        "error": error,
        "uuid": "msg-uuid",
        "session_id": "s",
        "message": {
            "id": format!("msg-{error}"),
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type": "text", "text": text, "citations": null}],
            "usage": {"input_tokens": 0, "output_tokens": 0,
                "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
        }
    })
}

fn usage_limit_assistant() -> Value {
    json!({
        "type": "assistant",
        "parent_tool_use_id": null,
        "error": "rate_limit",
        "uuid": "msg-limit",
        "session_id": "s",
        "message": {
            "id": "msg-<synthetic>",
            "type": "message",
            "role": "assistant",
            "model": "<synthetic>",
            "content": [{"type": "text",
                "text": "You've hit your individual spend limit · run /usage-credits to ask your admin for a higher limit",
                "citations": null}],
            "usage": {"input_tokens": 0, "output_tokens": 0,
                "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
        }
    })
}

fn api_retry(
    attempt: u64,
    max_retries: u64,
    error_status: Value,
    error: &str,
    no_response: Option<Value>,
) -> Value {
    let mut frame = json!({
        "type": "system",
        "subtype": "api_retry",
        "attempt": attempt,
        "max_retries": max_retries,
        "retry_delay_ms": 100,
        "error_status": error_status,
        "error": error,
        "uuid": "retry-uuid",
        "session_id": "s",
    });
    if let Some(no_response) = no_response {
        frame["no_response"] = no_response;
    }
    frame
}

fn result(is_error: bool, text: &str) -> Value {
    json!({
        "type": "result",
        "subtype": "success",
        "stop_reason": "end_turn",
        "is_error": is_error,
        "result": text,
        "errors": [],
        "duration_ms": 0,
        "duration_api_ms": 0,
        "num_turns": 1,
        "total_cost_usd": 0,
        "usage": {"input_tokens": 10, "output_tokens": 5,
            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
        "modelUsage": {},
        "permission_denials": [],
        "uuid": "result-uuid",
        "session_id": "s",
    })
}

/// The `_meta.jetbrains.air.sessionFailure` a failed AIR turn carries on its
/// successful prompt answer.
fn reply_failure(rig: &mut Rig, uuid: &str) -> Value {
    let reply = rig
        .replies
        .get_mut(uuid)
        .expect("prompt was echoed")
        .try_recv()
        .expect("the prompt was answered")
        .expect("an AIR failure rides a successful reply");
    let meta = reply.quota.expect("quota meta");
    meta["jetbrains"]["air"]["sessionFailure"].clone()
}

/// The error a failed plain-client turn answers with.
fn reply_error(rig: &mut Rig, uuid: &str) -> SessionError {
    let failure = rig
        .replies
        .get_mut(uuid)
        .expect("prompt was echoed")
        .try_recv()
        .expect("the prompt was answered")
        .expect_err("a plain client gets an error reply");
    failure.error
}

fn has_session_failure(updates: &[Value]) -> bool {
    serde_json::to_string(updates)
        .unwrap()
        .contains("sessionFailure")
}

/// `acp-agent.test.ts` > "stop reason propagation" > "publishes the Claude
/// failure title for compatible AIR version %i" (`it.each([1, 2])`): the title
/// is the result text, the category is `service`, and the AIR record carries
/// none of the removed fields; nothing is published as an update.
#[test]
fn g4_f25_publishes_the_claude_failure_title_for_compatible_air_versions() {
    for version in [1, 2] {
        let mut rig = Rig::new();
        let caps = json!({"_meta": {"jetbrains": {"air": {
            "version": version, "capabilities": ["sessionFailure"]
        }}}});
        rig.ext.air = crate::air::AirClient::from_client_capabilities(&caps);
        rig.echo("p1");
        rig.updates();
        rig.feed(result(true, "Claude could not complete the request."));
        let failure = reply_failure(&mut rig, "p1");
        assert_eq!(failure["revision"], 1, "v{version}");
        assert_eq!(failure["category"], "service", "v{version}");
        assert_eq!(failure["severity"], "error", "v{version}");
        assert_eq!(
            failure["title"], "Claude could not complete the request.",
            "v{version}"
        );
        assert_eq!(failure["actions"], json!(["retry"]), "v{version}");
        for removed in [
            "phase",
            "source",
            "safeMessage",
            "retryable",
            "retryAfterMs",
            "turnId",
        ] {
            assert!(failure.get(removed).is_none(), "v{version}: {removed}");
        }
        let updates = rig.updates();
        assert!(!has_session_failure(&updates), "v{version}: {updates:?}");
    }
}

/// `acp-agent.test.ts` > "updates an api retry warning into the terminal
/// failure": the warning's id is reused and the terminal failure takes the next
/// revision with the result title.
#[test]
fn g4_f25_updates_an_api_retry_warning_into_the_terminal_failure() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.updates();
    rig.feed(api_retry(1, 5, json!(429), "rate_limit", None));
    rig.feed(assistant_error("rate_limit", "Rate limit reached."));
    rig.feed(result(true, "Rate limit reached."));

    let warning = rig
        .updates()
        .into_iter()
        .find_map(|update| {
            update
                .get("_meta")
                .and_then(|meta| meta.pointer("/jetbrains/air/sessionFailure"))
                .cloned()
        })
        .expect("the retry warning");
    assert_eq!(warning["category"], "limit");
    assert_eq!(warning["severity"], "warning");
    assert_eq!(warning["revision"], 1);
    assert_eq!(warning["title"], "Retrying Claude, attempt 1 of 5.");
    assert_eq!(warning["actions"], json!([]));

    let terminal = reply_failure(&mut rig, "p1");
    assert_eq!(terminal["id"], warning["id"]);
    assert_eq!(terminal["category"], "limit");
    assert_eq!(terminal["severity"], "error");
    assert_eq!(terminal["revision"], 2);
    assert_eq!(terminal["title"], "Rate limit reached.");
    assert_eq!(terminal["actions"], json!(["retry"]));
}

/// `acp-agent.test.ts` > "says how long a no-response retry waited and will
/// wait": the `no_response` window is rendered in seconds.
#[test]
fn g4_f25_says_how_long_a_no_response_retry_waited_and_will_wait() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.updates();
    rig.feed(api_retry(
        1,
        1,
        Value::Null,
        "unknown",
        Some(json!({"waited_ms": 180_000, "retry_wait_ms": 600_000})),
    ));
    rig.feed(result(false, "ok"));
    let warning = rig
        .updates()
        .into_iter()
        .find_map(|update| {
            update
                .get("_meta")
                .and_then(|meta| meta.pointer("/jetbrains/air/sessionFailure"))
                .cloned()
        })
        .expect("the retry warning");
    assert_eq!(warning["category"], "connection");
    assert_eq!(warning["severity"], "warning");
    assert_eq!(
        warning["title"],
        "Reconnecting to Claude, attempt 1 of 1. No response after 180s; waiting up to 600s."
    );
}

/// `acp-agent.test.ts` > "clears a connection retry warning internally after the
/// turn succeeds": the warning is delivered, then retired from the active set
/// without a clear record.
#[test]
fn g4_f25_clears_a_connection_retry_warning_after_the_turn_succeeds() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.updates();
    rig.feed(api_retry(2, 5, Value::Null, "unknown", None));
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg-a","model":"claude-sonnet-4-20250514","role":"assistant",
            "content":[{"type":"text","text":"Recovered."}],
            "usage":{"input_tokens":1,"output_tokens":1}}}));
    rig.feed(result(false, "Recovered."));

    let warning = rig
        .updates()
        .into_iter()
        .find_map(|update| {
            update
                .get("_meta")
                .and_then(|meta| meta.pointer("/jetbrains/air/sessionFailure"))
                .cloned()
        })
        .expect("the retry warning");
    assert_eq!(warning["category"], "connection");
    assert_eq!(warning["severity"], "warning");
    assert_eq!(warning["title"], "Reconnecting to Claude, attempt 2 of 5.");
    assert_eq!(warning["actions"], json!([]));
    let id = warning["id"].as_str().expect("an id");
    assert!(
        rig.ext
            .t2a
            .failures
            .active()
            .iter()
            .all(|failure| failure.failure.id != id),
        "the warning is no longer active"
    );
}

/// `acp-agent.test.ts` > "does not publish typed failures for %s" (`it.each`
/// over nine malformed capability declarations): the prompt rejects with the
/// provider text and no `sessionFailure` is published.
#[test]
fn g4_f25_does_not_publish_typed_failures_for_a_malformed_air_declaration() {
    let cases = [
        ("missing envelope", json!({})),
        (
            "missing capability",
            json!({"_meta": {"jetbrains": {"air": {"version": 1, "capabilities": []}}}}),
        ),
        (
            "string version",
            json!({"_meta": {"jetbrains": {"air": {"version": "1", "capabilities": ["sessionFailure"]}}}}),
        ),
        (
            "fractional version",
            json!({"_meta": {"jetbrains": {"air": {"version": 1.5, "capabilities": ["sessionFailure"]}}}}),
        ),
        (
            "zero version",
            json!({"_meta": {"jetbrains": {"air": {"version": 0, "capabilities": ["sessionFailure"]}}}}),
        ),
        (
            "negative version",
            json!({"_meta": {"jetbrains": {"air": {"version": -1, "capabilities": ["sessionFailure"]}}}}),
        ),
        (
            "non-finite version",
            json!({"_meta": {"jetbrains": {"air": {"version": null, "capabilities": ["sessionFailure"]}}}}),
        ),
        (
            "non-string capability",
            json!({"_meta": {"jetbrains": {"air": {"version": 1, "capabilities": [1]}}}}),
        ),
        (
            "legacy nested version",
            json!({"_meta": {"jetbrains": {"air": {"sessionFailure": {"version": 1}}}}}),
        ),
    ];
    for (label, caps) in cases {
        let mut rig = Rig::new();
        rig.ext.air = crate::air::AirClient::from_client_capabilities(&caps);
        rig.echo("p1");
        rig.updates();
        rig.feed(result(true, "provider detail"));
        let error = reply_error(&mut rig, "p1");
        let SessionError::PromptFailed { message, .. } = error else {
            panic!("{label}: expected a prompt failure, got {error:?}");
        };
        assert_eq!(message, "provider detail", "{label}");
        assert!(!has_session_failure(&rig.updates()), "{label}");
    }
}

/// `acp-agent.test.ts` > "maps SDK error %s to broad category %s" (`it.each`
/// over eight SDK errors): the terminal failure's category follows the mapping
/// and the provider text never reaches an update.
#[test]
fn g4_f25_maps_sdk_errors_to_broad_categories() {
    for (sdk_error, expected) in [
        ("billing_error", "limit"),
        ("account_on_hold", "limit"),
        ("verification_required", "service"),
        ("cloud_credential_error", "service"),
        ("rate_limit", "limit"),
        ("overloaded", "service"),
        ("invalid_request", "request"),
        ("max_output_tokens", "limit"),
    ] {
        let mut rig = air_rig(&["sessionFailure"]);
        rig.echo("p1");
        rig.updates();
        rig.feed(assistant_error(sdk_error, "Claude request failed."));
        rig.feed(result(true, "Claude request failed."));
        let failure = reply_failure(&mut rig, "p1");
        assert_eq!(failure["category"], expected, "{sdk_error}");
        assert_eq!(failure["severity"], "error", "{sdk_error}");
        assert_eq!(failure["title"], "Claude request failed.", "{sdk_error}");
        let updates = serde_json::to_string(&rig.updates()).unwrap();
        assert!(!updates.contains("sessionFailure"), "{sdk_error}");
        assert!(!updates.contains("Claude request failed."), "{sdk_error}");
    }
}

/// `acp-agent.test.ts` > "classifies a live synthetic spend limit as exhausted
/// quota": the synthetic text is the title, the prior usage survives into the
/// last `usage_update`, and no assistant chunk renders the limit prose.
#[test]
fn g4_f25_classifies_a_live_synthetic_spend_limit_as_exhausted_quota() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.updates();
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg-prior","model":"claude-sonnet-4-20250514","role":"assistant",
            "content":[],
            "usage":{"input_tokens":170000,"output_tokens":910,
                "cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}));
    rig.feed(usage_limit_assistant());
    rig.feed(result(true, "private provider detail"));

    let failure = reply_failure(&mut rig, "p1");
    assert_eq!(failure["category"], "limit");
    assert_eq!(failure["severity"], "error");
    assert_eq!(
        failure["title"],
        "You've hit your individual spend limit · run /usage-credits to ask your admin for a higher limit"
    );
    assert_eq!(failure["actions"], json!([]));

    let updates = rig.updates();
    assert!(
        updates
            .iter()
            .all(|update| update["sessionUpdate"] != "agent_message_chunk"),
        "{updates:?}"
    );
    let last_usage = updates
        .iter()
        .rfind(|update| update["sessionUpdate"] == "usage_update")
        .expect("a usage_update");
    assert_eq!(last_usage["used"], 170_910);
    assert_eq!(last_usage["size"], 200_000);
}

/// `acp-agent.test.ts` > "keeps the prior turn's context usage when a fresh turn
/// is refused for quota": no `usage_update` is published and the measurement is
/// unchanged.
#[test]
fn g4_f25_keeps_the_prior_context_usage_when_a_fresh_turn_is_refused() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.updates();
    rig.ext.usage.set_used(170_910);
    rig.feed(usage_limit_assistant());
    rig.feed(result(true, "private provider detail"));
    let failure = reply_failure(&mut rig, "p1");
    assert_eq!(failure["category"], "limit");
    let updates = rig.updates();
    assert!(
        updates
            .iter()
            .all(|update| update["sessionUpdate"] != "usage_update"),
        "{updates:?}"
    );
    assert_eq!(rig.ext.usage.used, 170_910);
}

/// `acp-agent.test.ts` > "preserves live usage-limit prose for clients without
/// typed failures": the prose is rendered and the prompt rejects with the
/// provider text.
#[test]
fn g4_f25_preserves_live_usage_limit_prose_for_clients_without_typed_failures() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.updates();
    rig.feed(usage_limit_assistant());
    rig.feed(result(true, "provider detail"));
    let error = reply_error(&mut rig, "p1");
    assert!(
        format!("{error:?}").contains("provider detail"),
        "{error:?}"
    );
    let updates = serde_json::to_string(&rig.updates()).unwrap();
    assert!(updates.contains("individual spend limit"), "{updates}");
}

fn synthetic_login() -> Value {
    json!({
        "type": "assistant",
        "parent_tool_use_id": null,
        "error": "authentication_failed",
        "uuid": "msg-login",
        "session_id": "s",
        "message": {
            "id": "msg-<synthetic>",
            "type": "message",
            "role": "assistant",
            "model": "<synthetic>",
            "content": [{"type": "text",
                "text": "Not logged in · Please run /login", "citations": null}],
            "usage": {"input_tokens": 0, "output_tokens": 0,
                "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
        }
    })
}

/// `acp-agent.test.ts` > "does not republish auth_required when the result
/// repeats the login text": the CLI's synthetic login message already failed the
/// turn, so the repeated result publishes nothing and is not logged as a fault.
#[test]
fn g4_f25_does_not_republish_auth_required_for_the_repeated_login_text() {
    let dir = std::env::temp_dir().join(format!("g4-f25-login-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("agent.log");
    let mut rig = air_rig(&["sessionFailure"]);
    rig.ext.logger = Some(Arc::new(crate::logger::Logger::to_file(&log)));
    rig.echo("p1");
    rig.updates();
    rig.feed(synthetic_login());
    rig.feed(result(true, "Not logged in · Please run /login"));
    assert!(matches!(
        reply_error(&mut rig, "p1"),
        SessionError::PromptFailed { .. }
    ));
    assert!(!has_session_failure(&rig.updates()));
    assert!(rig.ext.t2a.failures.active().is_empty());
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !logged.contains("cannot fail active turn"),
        "the repeated login text is not a fault: {logged}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `acp-agent.test.ts` > "rejects a later sign-out after a real model answer": a
/// real answer proves the session is signed in again; a later sign-out is an
/// auth rejection again, never an AIR typed failure.
#[test]
fn g4_f25_rejects_a_later_sign_out_after_a_real_model_answer() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.updates();
    rig.feed(synthetic_login());
    assert!(matches!(
        reply_error(&mut rig, "p1"),
        SessionError::PromptFailed { .. }
    ));

    rig.echo("p2");
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg-real","model":"claude-sonnet-4-20250514","role":"assistant",
            "content":[{"type":"text","text":"signed in again"}],
            "usage":{"input_tokens":1,"output_tokens":1}}}));
    rig.feed(result(false, "signed in again"));
    let reply = rig
        .replies
        .get_mut("p2")
        .expect("p2 echoed")
        .try_recv()
        .expect("p2 answered")
        .expect("a real answer succeeds");
    assert_eq!(reply.stop_reason, "end_turn");
    assert!(rig.ext.t2a.failures.active().is_empty());

    rig.echo("p3");
    rig.feed(synthetic_login());
    assert!(matches!(
        reply_error(&mut rig, "p3"),
        SessionError::PromptFailed { .. }
    ));
    assert!(!has_session_failure(&rig.updates()));
}

/// `acp-agent.test.ts` > "publishes no retry warning for an authentication_failed
/// api_retry": the 401 retry is the CLI's credential re-check, not a warning.
#[test]
fn g4_f25_publishes_no_retry_warning_for_an_auth_api_retry() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.updates();
    rig.feed(api_retry(1, 10, json!(401), "authentication_failed", None));
    assert!(rig.updates().is_empty(), "the 401 retry publishes nothing");
}

/// `acp-agent.test.ts` > "keeps the historical failure record unchanged after
/// recovery": the failed turn's record stays as it was; the recovered turn
/// publishes no clear record and the failure is no longer active.
#[test]
fn g4_f25_keeps_the_historical_failure_record_unchanged_after_recovery() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.updates();
    rig.feed(result(true, "first turn failed"));
    let active = reply_failure(&mut rig, "p1");
    assert_eq!(active["revision"], 1);
    assert_eq!(active["severity"], "error");
    let id = active["id"].as_str().expect("an id").to_string();

    rig.echo("p2");
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg-real","model":"claude-sonnet-4-20250514","role":"assistant",
            "content":[{"type":"text","text":"recovered"}],
            "usage":{"input_tokens":1,"output_tokens":1}}}));
    rig.feed(result(false, "recovered"));
    assert!(rig.updates().iter().all(|update| update["_meta"]
        .pointer("/jetbrains/air/sessionFailure")
        .is_none()));
    assert!(rig
        .ext
        .t2a
        .failures
        .active()
        .iter()
        .all(|failure| failure.failure.id != id));
}

/// `acp-agent.test.ts` > "does not publish a clear record when the next turn is
/// refused": a refusal still retires the earlier turn's record silently.
#[test]
fn g4_f25_does_not_publish_a_clear_record_when_the_next_turn_is_refused() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.updates();
    rig.feed(result(true, "first turn failed"));
    let active = reply_failure(&mut rig, "p1");
    let id = active["id"].as_str().expect("an id").to_string();

    rig.echo("p2");
    rig.feed(
        json!({"type":"result","subtype":"success","stop_reason":"refusal",
        "is_error":true,"errors":[],"usage":{"input_tokens":1,"output_tokens":1}}),
    );
    let reply = rig
        .replies
        .get_mut("p2")
        .expect("p2 echoed")
        .try_recv()
        .expect("p2 answered")
        .expect("a refusal settles");
    assert_eq!(reply.stop_reason, "refusal");
    assert!(rig.updates().iter().all(|update| update["_meta"]
        .pointer("/jetbrains/air/sessionFailure")
        .is_none()));
    assert!(rig
        .ext
        .t2a
        .failures
        .active()
        .iter()
        .all(|failure| failure.failure.id != id));
}

/// `acp-agent.test.ts` > "does not publish clear records on repeated successful
/// turns": two recovered turns publish no clear record either.
#[test]
fn g4_f25_does_not_publish_clear_records_on_repeated_successful_turns() {
    let mut rig = air_rig(&["sessionFailure"]);
    rig.echo("p1");
    rig.updates();
    rig.feed(result(true, "first turn failed"));
    let active = reply_failure(&mut rig, "p1");
    let id = active["id"].as_str().expect("an id").to_string();

    for uuid in ["p2", "p3"] {
        rig.echo(uuid);
        rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
            "message":{"id":"msg-real","model":"claude-sonnet-4-20250514","role":"assistant",
                "content":[{"type":"text","text":"ok"}],
                "usage":{"input_tokens":1,"output_tokens":1}}}));
        rig.feed(result(false, "ok"));
    }
    assert!(rig.updates().iter().all(|update| update["_meta"]
        .pointer("/jetbrains/air/sessionFailure")
        .is_none()));
    assert!(rig
        .ext
        .t2a
        .failures
        .active()
        .iter()
        .all(|failure| failure.failure.id != id));
}

/// `acp-agent.test.ts` > "logs loudly when a failure has no active turn to
/// reject": a duplicate result that no turn can take is logged with the result
/// text instead of being dropped silently (`acp-agent.ts:5527-5529`).
#[test]
fn g4_f25_logs_a_failure_with_no_active_turn() {
    let dir = std::env::temp_dir().join(format!("g4-f25-noturn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("agent.log");
    let mut rig = Rig::new();
    rig.ext.logger = Some(Arc::new(crate::logger::Logger::to_file(&log)));
    rig.echo("p1");
    rig.updates();
    rig.feed(result(true, "first failure"));
    assert!(matches!(
        reply_error(&mut rig, "p1"),
        SessionError::PromptFailed { .. }
    ));
    rig.feed(result(true, "late duplicate failure"));
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logged.contains("no unsettled active turn exists"),
        "{logged}"
    );
    assert!(logged.contains("late duplicate failure"), "{logged}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `acp-agent.test.ts` > "ignores a late worker shutdown from a replaced query
/// consumer": the Rust session actor owns exactly one stream and replaces it on
/// a fresh context, so the replaced stream's frames are never read; two
/// consumers also never share a failure book (the fresh one starts empty).
/// TEST-MAP records the late-frame half as `different:`.
#[test]
fn g4_f25_a_replaced_consumer_keeps_its_own_failure_book() {
    // Two consumers (session actors) never share a failure book: a fresh
    // consumer starts from an empty active set.
    let mut first = failures::SessionFailures::default();
    let mut second = failures::SessionFailures::default();
    let a = first.publish("s", ClaudeFailureKind::WorkerShutdown, &session_scoped());
    let b = second.publish("s", ClaudeFailureKind::WorkerShutdown, &session_scoped());
    let id = |value: &Value| {
        value["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"]["id"]
            .as_str()
            .expect("an id")
            .to_string()
    };
    assert_ne!(id(&a), id(&b));
    assert!(id(&a).starts_with("s:session-error:"));
    assert!(id(&b).starts_with("s:session-error:"));
    assert_eq!(second.active().len(), 1);
}

/// `acp-agent.test.ts` > "uses a fresh session-scoped failure identity for each
/// query-stream consumer": each consumer's incident ids are distinct even for
/// the same session and kind.
#[test]
fn g4_f25_uses_a_fresh_session_scoped_failure_identity_per_consumer() {
    let mut ids = Vec::new();
    for _ in 0..2 {
        let mut failures = failures::SessionFailures::default();
        let update = failures.publish(
            "test-session",
            ClaudeFailureKind::WorkerShutdown,
            &session_scoped(),
        );
        ids.push(
            update["update"]["_meta"]["jetbrains"]["air"]["sessionFailure"]["id"]
                .as_str()
                .expect("an id")
                .to_string(),
        );
    }
    assert_ne!(ids[0], ids[1], "{ids:?}");
}

// ---------------------------------------------------------------- G4 / F11

/// `acp-agent-settings.test.ts` > "auto mode availability per model" >
/// "advertises `auto` when the resolved model lacks supportsAutoMode" and
/// "includes `auto` when the resolved model has supportsAutoMode: true": the
/// catalog is model-independent and never offers `dontAsk`.
#[test]
fn g4_f11_auto_is_advertised_regardless_of_model_support() {
    let modes = crate::session_config::build_available_modes(false, false);
    let ids: Vec<&str> = modes.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["default", "acceptEdits", "plan", "auto"]);
    assert!(!ids.contains(&"dontAsk"));
    assert_eq!(
        serde_json::to_value(&modes[0]).unwrap(),
        json!({"id": "default", "name": "Manual",
               "description": "Always ask before making changes"})
    );
    assert_eq!(
        serde_json::to_value(&modes[1]).unwrap(),
        json!({"id": "acceptEdits", "name": "Accept edits",
               "description": "Automatically accept all file edits"})
    );
    assert_eq!(
        serde_json::to_value(&modes[2]).unwrap(),
        json!({"id": "plan", "name": "Plan",
               "description": "Create a plan before making changes"})
    );
    assert_eq!(
        serde_json::to_value(&modes[3]).unwrap(),
        json!({"id": "auto", "name": "Auto",
               "description": "Claude handles permission decisions"})
    );
}

/// `acp-agent-settings.test.ts` > "does not clamp permissions.defaultMode='auto'
/// on a model that supports auto": the supporting model keeps Auto, while a
/// listed model without support falls back to Accept edits.
#[test]
fn g4_f11_auto_default_mode_is_not_clamped_on_a_supporting_model() {
    let supported = crate::session_config::ModelInfo {
        value: "claude-opus-4-5".into(),
        supports_auto_mode: Some(true),
        ..Default::default()
    };
    assert!(!auto_mode_downgrade_needed(Some(&supported)));
    assert_eq!(
        crate::agent::auto_mode_clamp_log("auto", false, "claude-opus-4-5", true),
        None
    );
    let lacking = crate::session_config::ModelInfo {
        value: "claude-haiku-4-5".into(),
        supports_auto_mode: None,
        ..Default::default()
    };
    assert!(auto_mode_downgrade_needed(Some(&lacking)));
}

/// `acp-agent-settings.test.ts` > "on resume > continues in the last permission
/// mode on session/load": the transcript's last user-turn mode wins over the
/// settings default.
#[test]
fn g4_f11_session_load_continues_in_the_last_permission_mode() {
    let dir = std::env::temp_dir().join(format!("g4-f11-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let slug = dir.join("projects").join("-p");
    std::fs::create_dir_all(&slug).expect("create transcript dir");
    let id = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let body = [
        r#"{"type":"user","permissionMode":"plan","message":{"role":"user","content":"one"}}"#,
        r#"{"type":"user","permissionMode":"acceptEdits","message":{"role":"user","content":"two"}}"#,
    ]
    .join("\n");
    std::fs::write(slug.join(format!("{id}.jsonl")), format!("{body}\n"))
        .expect("write transcript");
    let env = crate::env_snapshot::EnvSnapshot::from_pairs([(
        "CLAUDE_CONFIG_DIR".to_string(),
        dir.to_string_lossy().into_owned(),
    )]);
    let tail = crate::resumed_session::read_resumed_tail_with_env(id, false, &env);
    assert_eq!(tail.permission_mode.as_deref(), Some("acceptEdits"));
    assert_eq!(
        crate::permission_mode::resolve_initial_permission_mode(
            None,
            tail.permission_mode.as_deref(),
            Some("plan"),
            true
        ),
        "acceptEdits"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- G4 / F21

fn exit_plan_ctx(available: &[&str], input: Value) -> crate::permission_options::OptionsContext {
    crate::permission_options::OptionsContext {
        tool_name: "ExitPlanMode".to_string(),
        display_name: None,
        input,
        cwd: std::path::PathBuf::from("/workspace"),
        change_set: None,
        no_persistent_rule: false,
        default_to_no: false,
        available_modes: available.iter().map(|id| (*id).to_string()).collect(),
        auto_unavailable: false,
        pre_plan_mode: None,
        context_used_percent: None,
        home_dir: None,
    }
}

fn exit_plan_config(
    ext: &mut crate::session_ext::SessionExt,
    available: &[&str],
    supports_auto: bool,
) {
    ext.config.modes.current_mode_id = "plan".to_string();
    ext.config.modes.available_modes = available
        .iter()
        .map(|id| crate::session_config::ModeInfo {
            id: (*id).to_string(),
            name: (*id).to_string(),
            description: String::new(),
            _meta: None,
        })
        .collect();
    ext.config.config_options = vec![crate::session_config::ConfigOption {
        id: crate::session_config::MODE_CONFIG_ID.to_string(),
        name: "Mode".to_string(),
        description: String::new(),
        category: "mode".to_string(),
        option_type: "select".to_string(),
        current_value: "plan".to_string(),
        options: Vec::new(),
        _meta: None,
    }];
    ext.config.current_model_id = "default".to_string();
    ext.config.model_infos = vec![crate::session_config::ModelInfo {
        value: "default".to_string(),
        supports_auto_mode: Some(supports_auto),
        ..Default::default()
    }];
}

/// `acp-agent.test.ts` > "permission request cancellation" > "publishes the
/// effective mode after %s approval" (`it.each` over bypass/auto/accept-edits/
/// default): the answer allows with the setMode effect, the session adopts the
/// mode, and both `current_mode_update` and `config_option_update` go out.
#[test]
fn g4_f21_publishes_the_effective_mode_after_plan_approval() {
    for (option_id, expected_mode, supports_auto, available) in [
        (
            "exit-plan-bypass",
            "bypassPermissions",
            true,
            vec!["default", "plan", "bypassPermissions"],
        ),
        (
            "exit-plan-auto",
            "auto",
            true,
            vec!["default", "plan", "auto"],
        ),
        (
            "exit-plan-auto",
            "acceptEdits",
            false,
            vec!["default", "plan", "auto"],
        ),
        (
            "exit-plan-default",
            "default",
            true,
            vec!["default", "plan"],
        ),
    ] {
        let mut ext = crate::session_ext::SessionExt::new(
            None,
            std::path::PathBuf::new(),
            crate::env_snapshot::EnvSnapshot::empty(),
        );
        exit_plan_config(&mut ext, &available, supports_auto);
        let (tx, mut rx) = mpsc::unbounded_channel::<SessionOutbound>();
        let input = json!({"plan": "Implement it"});
        let ctx = exit_plan_ctx(&available, input.clone());
        let value = json!({"outcome": {"outcome": "selected", "optionId": option_id}});
        let response = resolve_permission_answer(
            PermissionAnswer {
                value: &value,
                ctx: &ctx,
                request_id: "request-plan",
                tool_use_id: "tool-plan",
                tool_input: &input,
            },
            &mut ext,
            &tx,
            "session-1",
        );
        assert_eq!(
            response["response"]["response"]["behavior"],
            json!("allow"),
            "{option_id}"
        );
        assert_eq!(
            response["response"]["response"]["updatedPermissions"],
            json!([{"type": "setMode", "mode": expected_mode, "destination": "session"}]),
            "{option_id}"
        );
        assert_eq!(
            ext.config.modes.current_mode_id, expected_mode,
            "{option_id}"
        );
        let mut updates = Vec::new();
        while let Ok(item) = rx.try_recv() {
            if let SessionOutbound::RawUpdate(value) = item {
                updates.push(value["update"].clone());
            }
        }
        assert!(
            updates
                .iter()
                .any(|update| update["sessionUpdate"] == "current_mode_update"
                    && update["currentModeId"] == expected_mode),
            "{option_id}: {updates:?}"
        );
        assert!(
            updates.iter().any(|update| {
                update["sessionUpdate"] == "config_option_update"
                    && update["configOptions"][0]["currentValue"] == expected_mode
            }),
            "{option_id}: {updates:?}"
        );
    }
}

/// `acp-agent.test.ts` > "keeps the plan approval when publishing the new mode
/// fails": a closed client channel logs the failure and the approval still
/// returns the provider's setMode effect.
#[test]
fn g4_f21_keeps_the_plan_approval_when_publishing_the_mode_fails() {
    let dir = std::env::temp_dir().join(format!("g4-f21-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("agent.log");
    let mut ext = crate::session_ext::SessionExt::new(
        None,
        std::path::PathBuf::new(),
        crate::env_snapshot::EnvSnapshot::empty(),
    );
    ext.logger = Some(std::sync::Arc::new(crate::logger::Logger::to_file(&log)));
    exit_plan_config(&mut ext, &["default", "plan"], true);
    let (tx, rx) = mpsc::unbounded_channel::<SessionOutbound>();
    drop(rx);
    let input = json!({"plan": "Implement it"});
    let ctx = exit_plan_ctx(&["default", "plan"], input.clone());
    let value = json!({"outcome": {"outcome": "selected", "optionId": "exit-plan-default"}});
    let response = resolve_permission_answer(
        PermissionAnswer {
            value: &value,
            ctx: &ctx,
            request_id: "request-plan",
            tool_use_id: "tool-plan",
            tool_input: &input,
        },
        &mut ext,
        &tx,
        "session-1",
    );
    assert_eq!(
        response["response"]["response"],
        json!({
            "behavior": "allow",
            "updatedInput": input,
            "updatedPermissions": [{"type": "setMode", "mode": "default", "destination": "session"}],
            "toolUseID": "tool-plan",
            "decisionClassification": "user_temporary",
        }),
        "the approval is kept"
    );
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logged.contains("Failed to publish mode after plan approval:"),
        "{logged}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- G4 / F14 + F34

/// `acp-agent.test.ts` > "stop reason propagation" > "forwards background output
/// that arrives after the turn resolves (issue #679)": the persistent consumer
/// still maps a top-level assistant message that arrives after the turn's idle.
#[test]
fn g4_f14_forwards_background_output_after_the_turn_resolves() {
    let mut rig = Rig::new();
    rig.echo("p1");
    rig.feed(json!({"type":"result","subtype":"success","is_error":false,
        "stop_reason":"end_turn","result":"ok",
        "usage":{"input_tokens":1,"output_tokens":1}}));
    rig.feed(json!({"type":"system","subtype":"session_state_changed","state":"idle"}));
    rig.updates();
    rig.feed(json!({"type":"assistant","parent_tool_use_id":null,
        "message":{"id":"msg_bg","model":"claude-sonnet-4-5","role":"assistant",
            "content":[{"type":"text","text":"between-turn background note"}],
            "usage":{"input_tokens":1,"output_tokens":1}}}));
    let updates = rig.updates();
    assert!(
        updates
            .iter()
            .any(|update| update["sessionUpdate"] == "agent_message_chunk"
                && update["content"]["text"] == "between-turn background note"),
        "the between-turn note is forwarded: {updates:?}"
    );
}

/// `acp-agent-settings.test.ts` > "on resume > keeps the live mode when it
/// recreates the session": both recreate paths read the actor's live mode and
/// pass it into the spawn (`recreate.rs` `live_mode` -> `session_spawn_options`),
/// so the recreated child runs `--permission-mode=<live>`.
#[test]
fn g4_f34_a_recreation_spawn_carries_the_live_mode() {
    let serve = crate::agent::ServeOptions {
        claude_path: Some(std::path::PathBuf::from("/fake")),
        ..Default::default()
    };
    let spawn = crate::agent::session_spawn_options(
        &serve,
        None,
        "recreated".to_string(),
        crate::sdk_options::ClaudeOptions::default(),
        "default".to_string(),
        false,
        true,
    );
    let argv = crate::process::build_argv(&spawn);
    assert!(
        argv.iter().any(|arg| arg == "--permission-mode=default"),
        "the recreated child keeps the live mode: {argv:?}"
    );
}
