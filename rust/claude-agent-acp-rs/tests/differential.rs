//! Ordered-frame differential tests (Decision D13).
//!
//! Phase 1 covers the differ itself with synthetic frame lists (item 1.4) —
//! no agent is needed. Phase 2 wires these against a real fake-`claude`
//! transcript.

mod common;

use common::{
    diff_frames, normalize_agent_info_version, normalize_for_test, normalize_root_paths,
    parse_fixture_frames, pinned_upstream_version, repo_root, run_recorder, run_recorder_with_argv,
    rust_agent_binary, Frame, JsonPath,
};
use serde_json::json;

/// Per-call unique temp-dir suffix. `inv_24_full_corpus` and the per-corpus
/// `inv_24_*` tests share the same `{name}` (e.g. `text-only`, `resume-load`)
/// and the same process id, so `{pid}`-only temp dirs collide when those tests
/// run concurrently in one binary — one test's `remove_dir_all` deletes another's
/// `out.jsonl` mid-read (flaky NotFound). A monotonic per-binary counter keeps
/// each `run_and_diff` call's scratch dir disjoint.
static NEXT_DIFF_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 1.T1 — the differ is order-sensitive: two frames swapped fails; the same
/// set in order passes.
#[test]
fn inv_24_order_sensitive() {
    let request = Frame::send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
    let response = Frame::recv(json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}));

    // Same set, same order -> passes.
    assert!(diff_frames(
        &[request.clone(), response.clone()],
        &[request.clone(), response.clone()],
        &[]
    )
    .is_ok());

    // Same set, swapped order -> fails.
    let swapped = diff_frames(
        &[request.clone(), response.clone()],
        &[response.clone(), request.clone()],
        &[],
    );
    assert!(swapped.is_err(), "swapped frames must not compare equal");
    assert_eq!(swapped.unwrap_err().index, 0);
}

/// 1.T2 — only uuids/timestamps differ -> passes; differing text -> fails.
#[test]
fn inv_24_normalize_ids_and_timestamps() {
    let a = Frame::recv(json!({
        "id": 1,
        "result": {
            "sessionId": "11111111-1111-1111-1111-111111111111",
            "createdAt": "2026-09-20T10:00:00Z"
        }
    }));
    let b = Frame::recv(json!({
        "id": 1,
        "result": {
            "sessionId": "22222222-2222-2222-2222-222222222222",
            "createdAt": "2026-09-20T11:00:00Z"
        }
    }));
    assert!(
        diff_frames(&[a], &[b], &[]).is_ok(),
        "uuid/timestamp-only differences must be normalised away"
    );

    let c = Frame::recv(json!({"id":1,"result":{"text":"hello"}}));
    let d = Frame::recv(json!({"id":1,"result":{"text":"world"}}));
    let diff = diff_frames(&[c], &[d], &[]);
    assert!(
        diff.is_err(),
        "a differing text payload must not compare equal"
    );
}

/// 1.T3 — an ignored JSON path absorbs a difference; the same difference on a
/// non-ignored path fails; an ignored whole frame missing on one side passes.
#[test]
fn inv_24_ignore_paths() {
    let ignored = [JsonPath::new("result.configOptions")];

    let a = Frame::recv(json!({"id":1,"result":{"text":"hi","configOptions":["a"]}}));
    let b = Frame::recv(json!({"id":1,"result":{"text":"hi","configOptions":["b"]}}));

    assert!(
        diff_frames(std::slice::from_ref(&a), std::slice::from_ref(&b), &ignored).is_ok(),
        "a differing field on an ignored path must pass"
    );
    assert!(
        diff_frames(std::slice::from_ref(&a), std::slice::from_ref(&b), &[]).is_err(),
        "the same difference on a non-ignored path must fail"
    );

    // A whole synthetic notification missing on one side is ignored via a
    // value-filtered path.
    let synthetic_update = Frame::recv(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": "33333333-3333-3333-3333-333333333333",
            "update": { "sessionUpdate": "synthetic_test_update", "count": 10 }
        }
    }));
    let settled = Frame::recv(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": "33333333-3333-3333-3333-333333333333",
            "update": { "sessionUpdate": "sessionState_changed", "state": "idle" }
        }
    }));
    let ignore_synthetic =
        [
            std::str::FromStr::from_str("params.update[sessionUpdate=synthetic_test_update]")
                .unwrap(),
        ];
    assert!(
        diff_frames(
            &[synthetic_update.clone(), settled.clone()],
            std::slice::from_ref(&settled),
            &ignore_synthetic
        )
        .is_ok(),
        "an ignored whole frame missing on one side must pass"
    );
    assert!(
        diff_frames(&[synthetic_update, settled.clone()], &[settled], &[]).is_err(),
        "without the ignore path a missing whole frame must fail"
    );
}

/// 9b.T1 — the normalizer materialises a `tool_call`'s omitted `status`/
/// `content` defaults, so an explicit `"pending"`/`[]` on one side equals an
/// omitted one on the other. Only on `tool_call` frames.
#[test]
fn inv_24_tool_call_default_status_content() {
    // Node: explicit `status: "pending"` and `content: []`.
    let node = Frame::recv(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": "33333333-3333-3333-3333-333333333333",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "toolu_01ABC",
                "title": "echo hi",
                "kind": "execute",
                "status": "pending",
                "content": []
            }
        }
    }));
    // Rust: the crate omits `status` (pending default) and empty `content`.
    let rust = Frame::recv(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": "33333333-3333-3333-3333-333333333333",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "toolu_01ABC",
                "title": "echo hi",
                "kind": "execute"
            }
        }
    }));
    assert!(
        diff_frames(&[node], &[rust], &[]).is_ok(),
        "an omitted tool_call default must equal an explicit default"
    );
}

/// 9b.T2 — a differing `status` on a `tool_call_update` is NOT equal: updates
/// are never default-normalised.
#[test]
fn inv_24_tool_call_update_status_must_match() {
    let a = Frame::recv(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": "33333333-3333-3333-3333-333333333333",
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "toolu_01ABC",
                "status": "completed"
            }
        }
    }));
    let b = Frame::recv(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": "33333333-3333-3333-3333-333333333333",
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "toolu_01ABC",
                "status": "failed"
            }
        }
    }));
    assert!(
        diff_frames(&[a], &[b], &[]).is_err(),
        "a differing status on a tool_call_update must not compare equal"
    );
}

/// 9b.T3 — a differing `content` on a `tool_call_update` is NOT equal.
#[test]
fn inv_24_tool_call_update_content_must_match() {
    let a = Frame::recv(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": "33333333-3333-3333-3333-333333333333",
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "toolu_01ABC",
                "content": [{"type": "content", "content": {"type": "text", "text": "hi"}}]
            }
        }
    }));
    let b = Frame::recv(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": "33333333-3333-3333-3333-333333333333",
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "toolu_01ABC",
                "content": [{"type": "content", "content": {"type": "text", "text": "bye"}}]
            }
        }
    }));
    assert!(
        diff_frames(&[a], &[b], &[]).is_err(),
        "a differing content on a tool_call_update must not compare equal"
    );
}

/// The JSON-path ignore list shared by differentials: empty after parity
/// phases 1 and 2 (modes, configOptions, models, and usage updates are all ported).
fn differential_ignore() -> Vec<JsonPath> {
    base_ignore()
}

/// The ignores common to every differential: the fields the Rust side
/// intentionally diverges on regardless of phase.
///
/// Tool frames are NOT ignored — `kind`/`title`/`status`/`content`/
/// `locations`/`rawInput`/`_meta` are compared against the fixture. The only
/// crate-inherent serialization difference — the pinned `agent-client-protocol`
/// crate omits `status` (the `pending` default) and empty `content` on a
/// `tool_call`, which the Node adapter always emits — is reconciled by the
/// frame normalizer materialising those defaults on both sides (see
/// `normalize_tool_call_defaults` in `common/mod.rs`), not by an ignore path.
fn base_ignore() -> Vec<JsonPath> {
    vec![]
}

/// The ignore set for the phase-9 tool-shape differentials (`inv_24_tools`).
fn tools_differential_ignore() -> Vec<JsonPath> {
    base_ignore()
}

/// 9.T3 — single-tool, multi-tool, streamed-partial-input, subagent, and
/// error-result scripts diff clean against the Node fixtures WITHOUT ignoring
/// the tool frames, verifying per-tool `kind`/`title`/`locations`/`_meta`.
#[test]
fn inv_24_tools() {
    for name in [
        "single-tool",
        "multi-tool",
        "streamed-partial-input",
        "subagent-drain",
        "error-result",
    ] {
        run_and_diff_with(name, &tools_differential_ignore());
    }
}

/// The full corpus: every `.acp.json` script under `porting/corpus/`. Phase 13
/// (13.T1) requires ALL of them to diff clean against the committed Node
/// fixtures, not just the subset each earlier phase exercised.
fn all_corpus_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(repo_root().join("porting/corpus"))
        .expect("read corpus dir")
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".acp.json").map(|s| s.to_string())
        })
        .collect();
    names.sort();
    assert!(
        names.len() >= 10,
        "corpus must have at least 10 scripts, found {}",
        names.len()
    );
    names
}

/// 13.T1 — the FULL corpus diffs clean: every script in `porting/corpus/` is
/// run through the Rust binary against the fake-claude transcript and its
/// ordered frames are diffed against the committed Node fixture (which is the
/// Node path's output). This is the whole-corpus, both-paths frame-equality
/// check (INV-24, REQ-2).
#[test]
fn inv_24_full_corpus() {
    for name in all_corpus_names() {
        run_and_diff(&name);
    }
}

/// Run the Rust binary against `name`'s corpus script via the recorder +
/// fake-claude, and diff the captured frames against the committed Node
/// fixture. Missing binaries fail loudly (8.6). `#[ignore]`-able per-corpus.
fn run_and_diff(name: &str) {
    run_and_diff_with(name, &differential_ignore());
}

/// As [`run_and_diff`], but with an explicit ignore set (the phase-9 tool
/// differentials use `tools_differential_ignore`).
fn run_and_diff_with(name: &str, ignore: &[JsonPath]) {
    let root = repo_root();
    let script = root.join(format!("porting/corpus/{name}.acp.json"));
    let transcript = root.join(format!("porting/corpus/{name}.transcript.jsonl"));
    let fixture = root.join(format!("porting/fixtures/{name}.frames.jsonl"));

    assert!(script.exists(), "missing script {script:?}");
    assert!(transcript.exists(), "missing transcript {transcript:?}");
    assert!(fixture.exists(), "missing fixture {fixture:?}");

    let tmp = std::env::temp_dir().join(format!(
        "acp-diff-{name}-{}-{}",
        std::process::id(),
        NEXT_DIFF_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let out = tmp.join("out.jsonl");

    let agent_cmd = rust_agent_binary();
    assert!(
        agent_cmd.exists(),
        "rust agent binary missing: {}",
        agent_cmd.display()
    );
    let frames = run_recorder(&agent_cmd.to_string_lossy(), &script, &transcript, &out);
    // The recorder ran from the repo root, so the session/new cwd is the root
    // path; normalise it to `$ROOT` like capture.sh does before diffing.
    let frames = normalize_root_paths(frames, &root);
    let expected = parse_fixture_frames(&fixture);
    let (frames, expected) =
        normalize_agent_info_version(frames, expected, &pinned_upstream_version());
    if let Err(diff) = diff_frames(&frames, &expected, ignore) {
        panic!("{name} differential failed:\n{diff}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// 8.T1 — text-only and resume/load corpus scripts diff clean.
#[test]
fn inv_24_text_only() {
    run_and_diff("text-only");
    run_and_diff("resume-load");
}

/// Phase 15 (Item 1) — a `session/new` against fake-claude emits a PROACTIVE
/// `available_commands_update` carrying the REAL command list (from the
/// initialize response's `commands`) right after session/new, WITHOUT any
/// `commands_changed` frame. Diffs the Rust output against the Node fixture
/// (Node's `sendAvailableCommandsUpdate` reads the same initialize `commands`
/// via `supportedCommands()`), and additionally asserts the update is non-empty
/// on the wire.
#[test]
fn inv_24_commands_proactive() {
    let root = repo_root();
    let script = root.join("porting/corpus/commands-proactive.acp.json");
    let transcript = root.join("porting/corpus/commands-proactive.transcript.jsonl");
    let fixture = root.join("porting/fixtures/commands-proactive.frames.jsonl");
    let tmp = std::env::temp_dir().join(format!(
        "acp-diff-commands-proactive-{}-{}",
        std::process::id(),
        NEXT_DIFF_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    let out = tmp.join("out.jsonl");

    let agent_cmd = rust_agent_binary();
    let frames = run_recorder(&agent_cmd.to_string_lossy(), &script, &transcript, &out);
    let frames = normalize_root_paths(frames, &root);
    let expected = parse_fixture_frames(&fixture);
    let (frames, expected) =
        normalize_agent_info_version(frames, expected, &pinned_upstream_version());
    if let Err(diff) = diff_frames(&frames, &expected, &differential_ignore()) {
        panic!("commands-proactive differential failed:\n{diff}");
    }

    // The wire must carry a non-empty available_commands_update emitted
    // proactively after session/new, without a commands_changed frame.
    let mut saw_non_empty = false;
    for frame in &frames {
        if frame.json["method"] == "session/update" {
            let u = &frame.json["params"]["update"];
            if u["sessionUpdate"] == "available_commands_update" {
                let cmds = u["availableCommands"].as_array().expect("array");
                assert!(
                    !cmds.is_empty(),
                    "available_commands_update must carry a non-empty availableCommands"
                );
                saw_non_empty = true;
            }
        }
    }
    assert!(
        saw_non_empty,
        "a proactive non-empty available_commands_update must be emitted after session/new"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

/// rust-parity-tests Phase 1 — the full tool loop including the return `user`
/// frame carrying `tool_result` content blocks (emitted by fake-claude as part
/// of the prompt step's emit array, with a distinct literal uuid and no
/// `isReplay`). Diffs the Rust output against the Node fixture; this is the
/// regression guard for the phase 15 `map_consolidated(MsgRole::User, ...)`
/// fix in `session.rs` `handle_session_frame`.
#[test]
fn inv_24_tool_result_echo() {
    run_and_diff("tool-result-echo");
}

/// rust-parity-tests Phase 2 — two sequential prompts where the first turn uses
/// a tool (distinct from `lagging-idle` which is text→text). The second prompt
/// re-enters the activation path after the tool-using turn settles; also covers
/// the lagging `session_state_changed` idle emit inside the second prompt step.
#[test]
fn inv_24_tool_then_text() {
    run_and_diff("tool-then-text");
}

/// rust-upstream-sync Phase 3 (item 3.6) — an AIR client (declaring
/// `clientCapabilities._meta.jetbrains.air` asyncTasks) gets the async-task
/// `session/update` spawn/progress/state life-cycle for a workflow task held
/// until its terminal notification.
#[test]
fn inv_24_async_task_air() {
    run_and_diff("async-task-air");
}

/// rust-upstream-sync Phase 3 (item 3.6) — a NON-AIR client gets none of the
/// async-task updates despite emitting the same CLI task frames.
#[test]
fn inv_24_async_task_non_air() {
    run_and_diff("async-task-non-air");
}

/// rust-upstream-sync Phase 3 (item 3.7, E-12) — a dispatched tool call with no
/// `tool_result` fails the `end_turn` prompt `-32603` `incomplete_tool_call`
/// after a `failed` `tool_call_update`.
#[test]
fn inv_24_unfinished_tool() {
    run_and_diff("unfinished-tool");
}

/// rust-upstream-sync Phase 3 (item 3.8, R87-1) — an autonomous-origin result
/// naming the queued prompt's uuid answers it (folded prompt) instead of being
/// skipped as another cycle's.
#[test]
fn inv_24_folded_prompt() {
    run_and_diff("folded-prompt");
}

/// rust-upstream-sync Phase 3 (item 3.9) — a Bash `can_use_tool` with an
/// `addRules` suggestion offers the durable `allow-with-updates` option, and
/// choosing it returns the suggestions verbatim as `updatedPermissions`
/// (the transcript step `expect`s that `control_response`).
#[test]
fn inv_24_permission_suggestions() {
    run_and_diff("permission-suggestions");
}

/// P7 (3.6) — Edit with an acceptEdits session change set offers the durable
/// "Yes, allow all edits during this session" option and, on `allow_with_updates`,
/// returns the CLI's `setMode` suggestion verbatim as `updatedPermissions`
/// classified `user_permanent`.
#[test]
fn inv_24_permission_edit_always() {
    run_and_diff("permission-edit-always");
}

/// P7 (3.6) — WebFetch offers a generated domain rule and returns it on
/// `allow_with_updates`.
#[test]
fn inv_24_permission_webfetch() {
    run_and_diff("permission-webfetch");
}

/// P7 (3.6) — a generic tool offers a generated `localSettings` rule and returns
/// it on `allow_with_updates`.
#[test]
fn inv_24_permission_generic_tool() {
    run_and_diff("permission-generic-tool");
}

/// 8.T7 — lagging trailing idle after the next echo is absorbed, not a false
/// #825 fail.
#[test]
fn inv_24_lagging_idle() {
    run_and_diff("lagging-idle");
}

/// rust-air-gaps 2.4 — `logout` runs `claude auth logout`, re-reads the store
/// (the fake's marker makes it answer logged-out) and pushes the `none` status
/// before the `{}` response.
#[test]
fn inv_24_logout_auth() {
    run_and_diff("logout-auth");
}

/// rust-air-gaps 2.3 — `session/resume`, `session/list` (+ cursor error,
/// delete, idempotent delete) and `session/fork` (a transcript copy that then
/// lists as `<title> (fork)`) match Node's SDK-backed behaviour.
#[test]
fn inv_24_session_methods() {
    run_and_diff("session-resume");
    run_and_diff("session-list");
    run_and_diff("session-fork");
}

/// rust-air-gaps 2.3 — `session/close` is idempotent and a prompt on a closed
/// session is `-32603 {details:"Session not found"}`.
#[test]
fn inv_24_session_close() {
    run_and_diff("session-close");
}

/// rust-air-gaps 2.2 — the `_auth/status_update` push after `initialize`
/// (CLI `auth status --json` probe) lands between the initialize response and
/// `session/new`, exactly once.
#[test]
fn inv_24_auth_status() {
    run_and_diff("auth-status");
}

/// rust-air-gaps 2.1 — `_meta.quota.model_usage` carries per-model increment
/// rows (insertion order, zero-total dropped, a rewound reading is its own
/// increment).
#[test]
fn inv_24_model_usage() {
    run_and_diff("model-usage");
}

/// rust-steering-lanes Phase 3 — a `_session/steering` injected mid-turn settles
/// the steered turn at the idle (steer idle lane, R6), never at the first
/// result, with the summed usage.
#[test]
fn inv_24_steer_mid_turn() {
    run_and_diff("steer-mid-turn");
}

/// rust-steering-lanes Phase 3 — an error-shaped result while steering with a
/// pending steered echo stays on the lane (R4), then settles `end_turn` at the
/// idle.
#[test]
fn inv_24_steer_error_result() {
    run_and_diff("steer-error-result");
}

/// 8.T8 — `/context` (echo-less local-only) promotes and settles at its own
/// result.
#[test]
fn inv_24_context_echo_less() {
    run_and_diff("context-echo-less");
}

/// 8.T9 — subagent hold → followup result settles the held turn inside the
/// turn.
#[test]
fn inv_24_subagent_followup() {
    run_and_diff("subagent-followup");
}

/// 8.T10 — cancel with a queued prompt, then an echo-less next prompt — the
/// queued prompt's late result is orphaned, never activates/settles the next.
#[test]
fn inv_24_cancel_queued_echo_less() {
    run_and_diff("cancel-queued-echo-less");
}

/// 11.T4 — cancel mid-turn (a `session/cancel` sent while a prompt is actively
/// streaming) diffs clean against the Node fixture. The agent must interrupt
/// the CLI and settle the prompt `cancelled` (INV-17 / INV-24).
#[test]
fn inv_24_cancel() {
    run_and_diff("cancel-mid-turn");
}

/// 8.T11 — EOF mid-turn settles the active turn and rejects queued prompts
/// with SESSION_ENDED_MESSAGE, then a later prompt rejects up front.
#[test]
fn inv_24_eof_mid_turn() {
    run_and_diff("eof-mid-turn");
}

/// 10.T4 — the permission-allow and permission-deny scripts diff clean against
/// the (deterministic, phase-10) Node fixtures. The `can_use_tool` round-trip
/// surfaces the `tool_call`, sends `session/request_permission`, and the allow /
/// deny outcome settles the turn on the next `result` (INV-20 / INV-21).
#[test]
fn inv_24_permissions() {
    run_and_diff("permission-allow");
    run_and_diff("permission-deny");
}

/// Phase-10 — the `session/request_permission` request id (a volatile
/// correlation id: `0` from the Node, a uuid from the pinned crate) and its
/// echoed response id are normalised to the same fixed token, so a uuid-bearing
/// Rust frame equals the Node's fixed `0` frame.
#[test]
fn request_permission_id_is_normalised_to_fixed_token() {
    let node_request = Frame::recv(json!({
        "jsonrpc": "2.0", "id": 0, "method": "session/request_permission",
        "params": {"sessionId": "s", "options": [], "toolCall": {"toolCallId": "t", "title": "x"}}
    }));
    let rust_request = Frame::recv(json!({
        "jsonrpc": "2.0", "id": "11111111-1111-1111-1111-111111111111",
        "method": "session/request_permission",
        "params": {"sessionId": "s", "options": [], "toolCall": {"toolCallId": "t", "title": "x"}}
    }));
    assert!(
        diff_frames(&[node_request], &[rust_request], &[]).is_ok(),
        "the request_permission request id must be normalised to a fixed token"
    );

    // The client's echoed response id is normalised the same way.
    let node_response = Frame::send(json!({
        "jsonrpc": "2.0", "id": 0, "result": {"outcome": {"outcome": "selected", "optionId": "allow"}}
    }));
    let rust_response = Frame::send(json!({
        "jsonrpc": "2.0", "id": "22222222-2222-2222-2222-222222222222",
        "result": {"outcome": {"outcome": "selected", "optionId": "allow"}}
    }));
    assert!(
        diff_frames(&[node_response], &[rust_response], &[]).is_ok(),
        "the request_permission response id must be normalised to the fixed token"
    );
}

/// rust-air-gaps 1.1 (Decision 5) — uuids EMBEDDED in a longer string get
/// first-appearance placeholders (mirrors capture.sh `replaceUuids`), the same
/// uuid maps to the same placeholder, and a whole-string uuid is unchanged.
#[test]
fn inv_49_normalize_embedded_uuid() {
    let a = "11111111-1111-1111-1111-111111111111";
    let b = "22222222-2222-2222-2222-222222222222";
    let out = normalize_for_test(json!({
        "id": format!("{a}:error"),
        "again": format!("x/{a}/y"),
        "other": format!("{b}:error"),
        "whole": a,
        "plain": "no uuid here, naïve ünïcode",
    }));
    assert_eq!(
        out,
        json!({
            "id": "$0:error",
            "again": "x/$0/y",
            "other": "$1:error",
            "whole": "$0",
            "plain": "no uuid here, naïve ünïcode",
        })
    );
}

/// rust-air-gaps Phase 1 (1.3, G3/G5) — the client `systemPrompt` option
/// (append / override / load / combined with AIR `customInstructions`) is
/// accepted with the Node's frames, and an unsupported mode or a non-text block
/// answers the Node's `-32602` (message and `data`) with no session created.
#[test]
fn inv_24_system_prompt() {
    for name in [
        "system-prompt-append",
        "system-prompt-override",
        "system-prompt-load",
        "system-prompt-bad-mode",
        "system-prompt-bad-block",
        "custom-instructions",
        "system-prompt-combined",
    ] {
        run_and_diff(name);
    }
}

/// rust-air-gaps Phase 1 (1.4, G11) — for an AIR client the tool reports carry
/// `_meta.jetbrains.air.{commandTitle,subagent,skill,mcp}`, the AIR-only
/// `is_mcp_tool_call` flag, and unchanged `_meta` keys are filtered from later
/// updates of the same call.
#[test]
fn inv_24_air_tool_meta() {
    run_and_diff("air-tool-meta");
}

/// rust-air-gaps Phase 1 (1.5, G1) — `_session/async_task/stop` on an announced
/// AIR task: the `stopped` state update, the "Task stopped by user" notice, the
/// `{stopped:true}` response (never before its updates), then the turn's own
/// `end_turn`.
#[test]
fn inv_24_async_task_stop() {
    run_and_diff("async-task-stop");
}

/// rust-parity-fixes Phase 1 (item 1.4) — P1/P2 spawn corpus: the
/// `_meta.claudeCode.options` extras (betas / extraArgs / env / model), the
/// settings-driven mode, and the bypass clamp (`disableBypassPermissionsMode`,
/// `allowDangerouslySkipPermissions:false`).
#[test]
fn inv_24_spawn_vst_meta() {
    run_and_diff("spawn-vst-meta");
}

#[test]
fn inv_24_spawn_settings_mode() {
    run_and_diff("spawn-settings-mode");
}

#[test]
fn inv_24_spawn_bypass_disabled() {
    run_and_diff("spawn-bypass-disabled");
}

#[test]
fn inv_24_spawn_bypass_opt_out() {
    run_and_diff("spawn-bypass-opt-out");
}

#[test]
fn inv_24_spawn_effort() {
    run_and_diff("spawn-effort");
}

#[test]
fn inv_24_spawn_additional_directories() {
    run_and_diff("spawn-additional-directories");
}

/// rust-parity-fixes Phase 1 (item 1.5, P3): the `[image,text]` prompt echo is
/// dropped and an assistant image maps to `agent_message_chunk` (no
/// `user_message_chunk` in the fixture).
#[test]
fn inv_24_image_prompt_echo() {
    run_and_diff("image-prompt-echo");
}

/// rust-parity-fixes Phase 1 (item 1.6, P5): empty / signature-only thinking
/// never produces an `agent_thought_chunk`.
#[test]
fn inv_24_thinking_empty() {
    run_and_diff("thinking-empty");
}

/// rust-parity-fixes Phase 1 (item 1.7, B1/X9): a consolidated assistant
/// message with no streamed deltas forwards its whole text as
/// `agent_message_chunk` (`unstreamedRemainder`).
#[test]
fn inv_24_consolidated_text() {
    run_and_diff("consolidated-text");
}

/// rust-parity-fixes Phase 1 (item 1.7, B1/X9): a stream cut short mid-block
/// forwards only the un-streamed tail of the consolidated text.
#[test]
fn inv_24_consolidated_text_partial_stream() {
    run_and_diff("consolidated-text-partial-stream");
}

/// rust-parity-fixes Phase 1 (Decision 1, item 1.1/1.4) — for every corpus
/// script that owns a committed `<name>.argv.json` fixture, run the Rust adapter
/// under the recorder with `FAKE_CLAUDE_ARGV_OUT` set and compare the argv it
/// would pass to `claude` (normalised exactly like `capture.sh normalize_object
/// ... argv`) against the Node-captured fixture.
#[test]
fn inv_57_argv_matches_node() {
    let root = repo_root();
    let mut scripts: Vec<String> = std::fs::read_dir(root.join("porting/corpus"))
        .expect("read corpus dir")
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".acp.json").map(|s| s.to_string())
        })
        .collect();
    scripts.sort();

    let mut checked = 0usize;
    for name in scripts {
        let fixture_path = root.join(format!("porting/fixtures/{name}.argv.json"));
        if !fixture_path.exists() {
            // A script that never spawns the CLI (its request fails before
            // spawn) owns no argv fixture — nothing to compare.
            continue;
        }
        let script = root.join(format!("porting/corpus/{name}.acp.json"));
        let transcript = root.join(format!("porting/corpus/{name}.transcript.jsonl"));
        let tmp = std::env::temp_dir().join(format!(
            "acp-argv-{name}-{}-{}",
            std::process::id(),
            NEXT_DIFF_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let out = tmp.join("out.jsonl");

        let agent_cmd = rust_agent_binary();
        let (_frames, rust_argv) =
            run_recorder_with_argv(&agent_cmd.to_string_lossy(), &script, &transcript, &out);
        let expected: serde_json::Value = common::read_json(&fixture_path);
        assert_eq!(
            rust_argv, expected,
            "{name} argv diverges from the Node fixture"
        );
        let _ = std::fs::remove_dir_all(&tmp);
        checked += 1;
    }
    assert!(
        checked >= 1,
        "inv_57_argv_matches_node must compare at least one argv fixture"
    );
}

/// INV-57 regression test — host-environment variables (CLAUDE_CODE_ARTIFACT,
/// CLAUDE_CODE_ENTRYPOINT, ANTHROPIC_* decoys, SSH_CONNECTION) set in the
/// parent test process must not leak into the recorded argv/env block: the
/// block recorded with them equals the one recorded without them.
///
/// Failure messages report key NAMES only — values are never printed.
#[test]
fn inv_57_argv_is_host_env_independent() {
    let root = repo_root();
    let script = root.join("porting/corpus/text-only.acp.json");
    let transcript = root.join("porting/corpus/text-only.transcript.jsonl");
    let fixture_path = root.join("porting/fixtures/text-only.argv.json");
    assert!(script.exists(), "text-only script exists");
    assert!(transcript.exists(), "text-only transcript exists");
    assert!(fixture_path.exists(), "text-only argv fixture exists");

    let agent_cmd = rust_agent_binary();

    // Helper to run recorder and return the recorded argv/env Value.
    let run = || {
        let tmp = std::env::temp_dir().join(format!(
            "acp-argv-env-indep-{}-{}",
            std::process::id(),
            NEXT_DIFF_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let out = tmp.join("out.jsonl");
        let (_frames, rust_argv) =
            run_recorder_with_argv(&agent_cmd.to_string_lossy(), &script, &transcript, &out);
        let _ = std::fs::remove_dir_all(&tmp);
        rust_argv
    };

    // Assert equality comparing and reporting key NAMES only (never printing values).
    let assert_argv_env_equal = |a: &serde_json::Value, b: &serde_json::Value| {
        assert_eq!(a["argv"], b["argv"], "argv arrays differ");
        assert_eq!(
            a["node_options_present"], b["node_options_present"],
            "node_options_present differs"
        );
        let a_env = a["env"].as_object().expect("env object");
        let b_env = b["env"].as_object().expect("env object");
        let a_keys: std::collections::BTreeSet<_> = a_env.keys().collect();
        let b_keys: std::collections::BTreeSet<_> = b_env.keys().collect();
        assert_eq!(
            a_keys, b_keys,
            "env key sets differ: left={a_keys:?}, right={b_keys:?}"
        );
        for k in a_keys {
            if a_env[k] != b_env[k] {
                panic!("env value mismatch for key: {k}");
            }
        }
    };

    // 1. Run without extra host variables (baseline).
    let baseline = run();

    // 2. Pollute the parent process environment.
    struct EnvGuard {
        vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, orig) in &self.vars {
                match orig {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    let polluted_keys = [
        "CLAUDE_CODE_ARTIFACT",
        "CLAUDE_CODE_ENTRYPOINT",
        "ANTHROPIC_DECOY_SECRET_KEY",
        "SSH_CONNECTION",
    ];
    let guard = EnvGuard {
        vars: polluted_keys
            .iter()
            .map(|&k| (k, std::env::var_os(k)))
            .collect(),
    };

    std::env::set_var("CLAUDE_CODE_ARTIFACT", "decoy-artifact-1");
    std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "host-entrypoint-decoy");
    std::env::set_var("ANTHROPIC_DECOY_SECRET_KEY", "decoy-secret-key-12345");
    std::env::set_var("SSH_CONNECTION", "127.0.0.1 12345 127.0.0.1 22");

    let polluted = run();
    drop(guard);

    // 3. Assert baseline and polluted runs are equal.
    assert_argv_env_equal(&baseline, &polluted);

    // 4. Assert both also equal the committed Node fixture.
    let expected: serde_json::Value = common::read_json(&fixture_path);
    assert_argv_env_equal(&polluted, &expected);
}

#[test]
fn inv_24_usage_scenarios() {
    run_and_diff("usage-update");
    run_and_diff("usage-compact");
    run_and_diff("usage-rate-limit");
    run_and_diff("usage-no-usage");
    run_and_diff("usage-1m");
    run_and_diff("usage-1m-seed");
}

/// rust-parity-fixes Phase 2 (item 2.8, P6): the session/new modes +
/// configOptions response and the `session/set_mode|set_config_option` wire
/// behaviour, plus the `session/set_model` -32601 parity case.
#[test]
fn inv_24_config_scenarios() {
    run_and_diff("config-session-new");
    run_and_diff("config-set-mode");
    run_and_diff("config-set-model");
    run_and_diff("config-set-effort");
    run_and_diff("config-invalid");
    run_and_diff("config-set-model-rpc");
}

/// rust-parity-fixes Phase 3 (item 3.2, P9): hook_callback handling, toolResponse meta
/// update for every tool (hook-bash), structuredPatch and final locations for Edit
/// (hook-edit), and optimistic creation diff replacement for Write (hook-write-existing).
#[test]
fn inv_24_hook_scenarios() {
    run_and_diff("hook-bash");
    run_and_diff("hook-edit");
    run_and_diff("hook-write-existing");
}

/// rust-parity-fixes Phase 3 (item 3.4, X12/Decision 12): the Task* plan. Both
/// the TaskCreated/TaskCompleted hooks (`task-plan-hooks`) and the task
/// tool_result path (`task-plan-no-hook`) accumulate task state and publish a
/// `plan` update exactly like Node.
#[test]
fn inv_24_task_plan_scenarios() {
    run_and_diff("task-plan-hooks");
    run_and_diff("task-plan-no-hook");
}

/// rust-parity-fixes Phase 3 (item 3.5, P10/Decision 14): `session/load`
/// replays the stored history before the response — user/assistant chunks,
/// thinking, tool_use/tool_result rendering, and an image user message — with
/// frame order deterministic (the fake's initialize reply is paced with
/// `sleep_ms`).
#[test]
fn inv_24_session_load_replay() {
    run_and_diff("session-load-replay");
    // X8: a `session/load` targeting an already-live session replays too.
    run_and_diff("session-load-replay-live");
}
