//! T8D — `create-session-options.test.ts` behaviours that had no Rust proof:
//! execution-limit rebuild detection, the SDK settings tier variants, the
//! provider cache key, the legacy/AIR system prompt precedence, and the AIR
//! `loadReplayEnd` marker after a `session/load` replay.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_client_protocol::{Agent, Channel, Client, ConnectionTo, Error, UntypedMessage};
use claude_agent_acp_rs::agent::{
    compute_session_fingerprint, serve, session_spawn_options, ServeOptions,
};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::permission_mode::allow_bypass;
use claude_agent_acp_rs::process::{spawn_raw, SpawnOptions, Timings};
use claude_agent_acp_rs::sdk_options::{resolve_settings_arg, ClaudeOptions};
use claude_agent_acp_rs::session_config::build_available_modes;
use claude_agent_acp_rs::session_index::project_dirs::{encode_project_path, Platform};
use claude_agent_acp_rs::session_model::provider_cache_key_for;
use claude_agent_acp_rs::system_prompt::{initialize_fields, SystemPromptFields};
use common::fake_claude_binary;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;

fn temp_dir(tag: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!("acp-t8d-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("create temp dir");
    tmp
}

fn session_params(limit: &str, value: Option<Value>) -> Value {
    let mut options = serde_json::Map::new();
    if let Some(value) = value {
        options.insert(limit.to_string(), value);
    }
    json!({
        "cwd": "/project",
        "mcpServers": [],
        "_meta": {"claudeCode": {"options": Value::Object(options)}},
    })
}

/// resumeSession/loadSession applies added, tightened and removed execution
/// limits to the SDK query, and reuses it when they are unchanged.
#[test]
fn t8d_execution_limits_rebuild_and_reuse_the_query() {
    for limit in ["maxTurns", "maxBudgetUsd"] {
        let fp = |value: Option<Value>| compute_session_fingerprint(&session_params(limit, value));
        assert_eq!(
            fp(Some(json!(10))),
            fp(Some(json!(10))),
            "{limit}: unchanged limits keep the query"
        );
        for (before, after) in [
            (None, Some(json!(10))),
            (Some(json!(10)), Some(json!(1))),
            (Some(json!(1)), None),
        ] {
            assert_ne!(
                fp(before.clone()),
                fp(after.clone()),
                "{limit}: {before:?} -> {after:?} must rebuild"
            );
        }
    }
}

/// `allowDangerouslySkipPermissions` defaults to the bypass availability and the
/// mode catalog mirrors it.
#[test]
fn t8d_bypass_default_and_mode_catalog() {
    let bypass = allow_bypass(false, false);
    assert!(bypass, "not root: bypass is available");
    let spawn = session_spawn_options(
        &ServeOptions::default(),
        Some(std::env::temp_dir()),
        "sess".to_string(),
        ClaudeOptions::default(),
        "default".to_string(),
        bypass,
        true,
    );
    assert_eq!(spawn.allow_dangerously_skip_permissions, bypass);
    assert!(
        build_available_modes(bypass, false)
            .iter()
            .any(|mode| mode.id == "bypassPermissions"),
        "the mode catalog mirrors the bypass availability"
    );
    assert!(
        !build_available_modes(false, false)
            .iter()
            .any(|mode| mode.id == "bypassPermissions"),
        "no bypass mode when bypass is unavailable"
    );
}

/// The child inherits HOME and PATH when no env is provided, a user env entry
/// merges on top of the inherited one, and a user entry overrides an inherited
/// one.
#[tokio::test]
async fn t8d_inherits_home_and_path_and_merges_extra_env() {
    async fn read(extra: Vec<(String, String)>) -> Value {
        let script =
            r#"printf '{"home":"%s","path":"%s","custom":"%s"}\n' "$HOME" "$PATH" "$T8D_CUSTOM""#;
        let opts = SpawnOptions {
            extra_env: extra,
            ..Default::default()
        };
        let mut process = spawn_raw(
            PathBuf::from("sh"),
            vec!["-c".to_string(), script.to_string()],
            &opts,
        )
        .await
        .expect("spawn sh");
        let line = timeout(Duration::from_secs(10), process.lines.recv())
            .await
            .expect("a line")
            .expect("the child wrote a line");
        let _ = process.shutdown(&Timings::default()).await;
        line
    }

    let inherited = read(Vec::new()).await;
    assert_eq!(
        inherited["home"].as_str(),
        std::env::var("HOME").ok().as_deref(),
        "HOME is inherited"
    );
    assert_eq!(
        inherited["path"].as_str(),
        std::env::var("PATH").ok().as_deref(),
        "PATH is inherited"
    );
    assert_eq!(inherited["custom"].as_str(), Some(""));

    let merged = read(vec![("T8D_CUSTOM".to_string(), "custom-value".to_string())]).await;
    assert_eq!(merged["custom"].as_str(), Some("custom-value"));
    assert_eq!(
        merged["home"].as_str(),
        std::env::var("HOME").ok().as_deref()
    );

    let overridden = read(vec![("HOME".to_string(), "/custom/home".to_string())]).await;
    assert_eq!(overridden["home"].as_str(), Some("/custom/home"));
}

/// CLAUDE_MODEL_CONFIG is the base settings tier; session settings merge over
/// it, and the session's own field wins.
#[test]
fn t8d_model_config_settings_variants() {
    let cwd = std::env::temp_dir();
    let arg = |env: &EnvSnapshot, settings: Option<&Value>| {
        resolve_settings_arg(&cwd, env, settings, None).expect("settings")
    };

    let overrides = EnvSnapshot::from_pairs([(
        "CLAUDE_MODEL_CONFIG",
        r#"{"modelOverrides":{"claude-opus-4-6":"us.anthropic.claude-opus-4-6-v1"}}"#,
    )]);
    assert_eq!(
        arg(&overrides, None).as_deref(),
        Some(r#"{"modelOverrides":{"claude-opus-4-6":"us.anthropic.claude-opus-4-6-v1"}}"#)
    );

    let available = EnvSnapshot::from_pairs([(
        "CLAUDE_MODEL_CONFIG",
        r#"{"availableModels":["opus","sonnet"]}"#,
    )]);
    assert_eq!(
        arg(&available, None).as_deref(),
        Some(r#"{"availableModels":["opus","sonnet"]}"#)
    );

    let both = EnvSnapshot::from_pairs([(
        "CLAUDE_MODEL_CONFIG",
        r#"{"modelOverrides":{"claude-opus-4-6":"us.anthropic.claude-opus-4-6-v1"},"availableModels":["opus"]}"#,
    )]);
    assert_eq!(
        arg(&both, None).as_deref(),
        Some(
            r#"{"modelOverrides":{"claude-opus-4-6":"us.anthropic.claude-opus-4-6-v1"},"availableModels":["opus"]}"#
        )
    );

    // Session hooks ride on top of the model settings.
    let hooks = json!({"PostToolUse": [{"hooks": [{"type": "command", "command": "echo ready"}]}]});
    assert_eq!(
        arg(&both, Some(&json!({"hooks": hooks}))).as_deref(),
        Some(
            r#"{"modelOverrides":{"claude-opus-4-6":"us.anthropic.claude-opus-4-6-v1"},"availableModels":["opus"],"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"echo ready"}]}]}}"#
        )
    );

    // A session settings file merges the same way.
    let dir = temp_dir("settings-file");
    let file = dir.join("settings.json");
    std::fs::write(&file, r#"{"hooks":{"Stop":[]}}"#).expect("settings file");
    assert_eq!(
        arg(&available, Some(&json!(file.to_string_lossy()))).as_deref(),
        Some(r#"{"availableModels":["opus","sonnet"],"hooks":{"Stop":[]}}"#)
    );
    let _ = std::fs::remove_dir_all(&dir);

    // The session's own value wins over CLAUDE_MODEL_CONFIG's.
    assert_eq!(
        arg(
            &both,
            Some(&json!({
                "model": "claude-sonnet-4-6",
                "modelOverrides": {"claude-opus-4-6": "meta-value"},
            })),
        )
        .as_deref(),
        Some(
            r#"{"modelOverrides":{"claude-opus-4-6":"meta-value"},"availableModels":["opus"],"model":"claude-sonnet-4-6"}"#
        )
    );

    // No env var and no session settings: no `--settings` flag.
    assert_eq!(arg(&EnvSnapshot::empty(), None), None);
}

/// The provider cache key distinguishes the routing env the CLI sees: a proxy
/// base URL and a header variant land in different buckets, while the same
/// routing shares one.
#[test]
fn t8d_provider_cache_key_scopes_by_headers() {
    let base = EnvSnapshot::from_pairs([("ANTHROPIC_BASE_URL", "https://gw.example")]);
    let plain = provider_cache_key_for(&base);
    let beta = provider_cache_key_for(&base.with_var(
        "ANTHROPIC_CUSTOM_HEADERS",
        "anthropic-beta: context-1m-2025-08-07",
    ));
    assert_ne!(
        plain, beta,
        "same endpoint, different headers: another bucket"
    );
    assert_eq!(
        plain,
        provider_cache_key_for(&base),
        "same routing shares a bucket"
    );
    assert_ne!(
        plain,
        provider_cache_key_for(&EnvSnapshot::empty()),
        "the default route is another bucket"
    );
}

/// The legacy `_meta.systemPrompt` string wins over AIR custom instructions; a
/// client `systemPrompt` wins over both.
#[test]
fn t8d_system_prompt_precedence() {
    let legacy_and_air = json!({
        "_meta": {
            "systemPrompt": "legacy prompt",
            "jetbrains": {"air": {"customInstructions": "AIR instructions"}},
        }
    });
    assert_eq!(
        initialize_fields(&legacy_and_air).expect("fields"),
        SystemPromptFields {
            system_prompt: Some(json!(["legacy prompt"])),
            ..SystemPromptFields::default()
        }
    );

    let client_and_both = json!({
        "systemPrompt": {"mode": "append", "content": [{"type": "text", "text": "Client prompt."}]},
        "_meta": {
            "systemPrompt": "legacy prompt",
            "jetbrains": {"air": {"customInstructions": "AIR instructions"}},
        }
    });
    assert_eq!(
        initialize_fields(&client_and_both).expect("fields"),
        SystemPromptFields {
            append_system_prompt: Some("Client prompt.\n\nAIR instructions".to_string()),
            ..SystemPromptFields::default()
        }
    );

    let air_only = json!({
        "_meta": {"jetbrains": {"air": {"customInstructions": "AIR instructions"}}}
    });
    assert_eq!(
        initialize_fields(&air_only).expect("fields"),
        SystemPromptFields {
            append_system_prompt: Some("AIR instructions".to_string()),
            ..SystemPromptFields::default()
        }
    );
}

// ---- session/load replay end (AIR `loadReplayEnd`) --------------------------

fn seed_load_transcript(config_dir: &Path, cwd: &Path, session_id: &str, messages: &[Value]) {
    let dir = config_dir.join("projects").join(encode_project_path(
        &cwd.to_string_lossy(),
        Platform::host(),
    ));
    std::fs::create_dir_all(&dir).expect("project dir");
    let text: String = messages.iter().map(|m| format!("{m}\n")).collect();
    std::fs::write(dir.join(format!("{session_id}.jsonl")), text).expect("transcript");
}

fn user_message(session_id: &str, uuid: &str, text: &str) -> Value {
    json!({
        "type": "user",
        "uuid": uuid,
        "parentUuid": null,
        "sessionId": session_id,
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
    })
}

fn init_transcript(tmp: &Path, delay_ms: u64) -> PathBuf {
    let path = tmp.join("load.transcript.jsonl");
    let init = json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [
            {"sleep_ms": delay_ms},
            {"type": "control_response", "response": {
                "subtype": "success", "request_id": "$REQ",
                "response": {"models": [], "commands": [], "account": {}}
            }}
        ]
    });
    std::fs::write(&path, format!("{init}\n")).expect("transcript");
    path
}

fn is_end(params: &Value) -> bool {
    params
        .pointer("/update/_meta/jetbrains/air/loadReplayEnd")
        .and_then(Value::as_bool)
        == Some(true)
}

fn kinds(updates: &[(Instant, Value)]) -> Vec<&str> {
    updates
        .iter()
        .filter(|(_, params)| params.get("method").is_none())
        .filter_map(|(_, params)| {
            if is_end(params) {
                Some("end")
            } else {
                params
                    .pointer("/update/sessionUpdate")
                    .and_then(Value::as_str)
            }
        })
        .filter(|kind| *kind != "available_commands_update")
        .collect()
}

/// Run a `session/load` against a seeded transcript and collect the
/// notifications (timestamped) plus the response.
async fn load_exchange(
    tag: &str,
    caps: Value,
    delay_ms: u64,
    messages: &[Value],
) -> (Vec<(Instant, Value)>, Value, Instant) {
    let tmp = temp_dir(tag);
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let config = tmp.join("config");
    std::fs::create_dir_all(&config).expect("config");
    let id = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    seed_load_transcript(&config, &cwd, id, messages);
    let transcript = init_transcript(&tmp, delay_ms);
    let config_str = config.to_string_lossy().into_owned();
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            transcript.to_string_lossy().into_owned(),
        )],
        default_cwd: Some(cwd.clone()),
        env: EnvSnapshot::from_pairs([("CLAUDE_CONFIG_DIR", config_str.as_str())]),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let (tx, mut rx) = mpsc::unbounded_channel::<(Instant, Value)>();
    let collector = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(item) = rx.recv().await {
            seen.push(item);
        }
        seen
    });
    let cwd_for_client = cwd.clone();
    let id_for_client = id.to_string();
    let result = timeout(
        Duration::from_secs(20),
        Client
            .builder()
            .on_receive_notification(
                async move |notification: UntypedMessage, _cx| {
                    let _ = tx.send((Instant::now(), notification.params().clone()));
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                client_channel,
                move |connection: ConnectionTo<Agent>| async move {
                    connection
                        .send_request(UntypedMessage::new(
                            "initialize",
                            json!({"protocolVersion": 1, "clientCapabilities": caps}),
                        )?)
                        .block_task()
                        .await?;
                    let response = connection
                        .send_request(UntypedMessage::new(
                            "session/load",
                            json!({
                                "sessionId": id_for_client,
                                "cwd": cwd_for_client.to_string_lossy(),
                                "mcpServers": [],
                            }),
                        )?)
                        .block_task()
                        .await?;
                    Ok::<Value, Error>(response)
                },
            ),
    )
    .await
    .expect("the exchange finished in time")
    .expect("the exchange succeeded");
    let responded = Instant::now();
    let _ = timeout(Duration::from_secs(5), serve_task).await;
    let updates = collector.await.expect("collector");
    let _ = std::fs::remove_dir_all(&tmp);
    (updates, result, responded)
}

/// Run `session/new` and then a `session/load` on that live session (M-131):
/// the replay of an already-live session ends with the marker too.
async fn live_load_exchange(
    tag: &str,
    caps: Value,
    messages: &[Value],
) -> (Vec<(Instant, Value)>, Value) {
    let tmp = temp_dir(tag);
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let config = tmp.join("config");
    std::fs::create_dir_all(&config).expect("config");
    let transcript = init_transcript(&tmp, 0);
    let config_str = config.to_string_lossy().into_owned();
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![(
            "FAKE_CLAUDE_SCRIPT".to_string(),
            transcript.to_string_lossy().into_owned(),
        )],
        default_cwd: Some(cwd.clone()),
        env: EnvSnapshot::from_pairs([("CLAUDE_CONFIG_DIR", config_str.as_str())]),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let (tx, mut rx) = mpsc::unbounded_channel::<(Instant, Value)>();
    let collector = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(item) = rx.recv().await {
            seen.push(item);
        }
        seen
    });
    let cwd_for_client = cwd.clone();
    let config_for_client = config.clone();
    let messages = messages.to_vec();
    let result = timeout(
        Duration::from_secs(20),
        Client
            .builder()
            .on_receive_notification(
                async move |notification: UntypedMessage, _cx| {
                    let _ = tx.send((Instant::now(), notification.params().clone()));
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                client_channel,
                move |connection: ConnectionTo<Agent>| async move {
                    connection
                        .send_request(UntypedMessage::new(
                            "initialize",
                            json!({"protocolVersion": 1, "clientCapabilities": caps}),
                        )?)
                        .block_task()
                        .await?;
                    let created = connection
                        .send_request(UntypedMessage::new(
                            "session/new",
                            json!({
                                "cwd": cwd_for_client.to_string_lossy(),
                                "mcpServers": [],
                            }),
                        )?)
                        .block_task()
                        .await?;
                    let id = created
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .expect("a session id")
                        .to_string();
                    seed_load_transcript(&config_for_client, &cwd_for_client, &id, &messages);
                    let response = connection
                        .send_request(UntypedMessage::new(
                            "session/load",
                            json!({
                                "sessionId": id,
                                "cwd": cwd_for_client.to_string_lossy(),
                                "mcpServers": [],
                            }),
                        )?)
                        .block_task()
                        .await?;
                    Ok::<Value, Error>(response)
                },
            ),
    )
    .await
    .expect("the exchange finished in time")
    .expect("the exchange succeeded");
    let _ = timeout(Duration::from_secs(5), serve_task).await;
    let updates = collector.await.expect("collector");
    let _ = std::fs::remove_dir_all(&tmp);
    (updates, result)
}

/// M-131 — an already-live `session/load` replay ends with the AIR
/// `loadReplayEnd` marker, before its response, exactly like a fresh load.
#[tokio::test]
async fn t8d_live_load_replay_end_is_sent_after_the_replay() {
    let messages = [user_message(
        "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee",
        "u1",
        "hello",
    )];
    let (updates, response) = live_load_exchange(
        "live-replay-end",
        json!({"_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["loadReplayEnd"]}}}}),
        &messages,
    )
    .await;
    assert_eq!(
        kinds(&updates),
        vec!["user_message_chunk", "end"],
        "{updates:?}"
    );
    let id = response["sessionId"].as_str().expect("a session id");
    let end = updates
        .iter()
        .find(|(_, params)| is_end(params))
        .expect("an end event");
    assert_eq!(
        end.1,
        json!({
            "sessionId": id,
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": {"jetbrains": {"air": {"version": 1, "loadReplayEnd": true}}}
            }
        }),
        "the end event's exact body"
    );
    // The end event follows the replayed message (the commands update may come
    // after it).
    let end_at = updates
        .iter()
        .position(|(_, params)| is_end(params))
        .expect("an end event");
    let chunk_at = updates
        .iter()
        .position(|(_, params)| {
            params
                .pointer("/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("user_message_chunk")
        })
        .expect("the replayed message");
    assert!(chunk_at < end_at, "the replay precedes the end event");
}

/// One end event after the replay, before the response, with no title/updatedAt;
/// and it is sent before the CLI start finishes.
#[tokio::test]
async fn t8d_load_replay_end_is_sent_after_the_replay_before_the_response() {
    let id = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    let mut second = user_message(id, "u2", "second");
    second["parentUuid"] = json!("u1");
    let messages = [user_message(id, "u1", "first"), second];
    let (updates, _response, responded) = load_exchange(
        "replay-end",
        json!({"_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["loadReplayEnd"]}}}}),
        400,
        &messages,
    )
    .await;
    assert_eq!(
        kinds(&updates),
        vec!["user_message_chunk", "user_message_chunk", "end"]
    );
    let end = updates
        .iter()
        .find(|(_, params)| is_end(params))
        .expect("an end event");
    assert_eq!(
        end.1,
        json!({
            "sessionId": id,
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": {"jetbrains": {"air": {"version": 1, "loadReplayEnd": true}}}
            }
        })
    );
    assert!(
        end.0 < responded,
        "the end event is sent before the CLI start finishes and the response"
    );
}

/// An empty history still gets the end event.
#[tokio::test]
async fn t8d_load_replay_end_is_sent_for_an_empty_history() {
    let (updates, _response, _responded) = load_exchange(
        "replay-end-empty",
        json!({"_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["loadReplayEnd"]}}}}),
        0,
        &[],
    )
    .await;
    assert_eq!(kinds(&updates), vec!["end"]);
}

/// A client without the capability gets no end event.
#[tokio::test]
async fn t8d_load_replay_end_is_not_sent_without_the_capability() {
    let id = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    let messages = [user_message(id, "u1", "first")];
    for (tag, caps) in [
        ("replay-end-plain", json!({})),
        (
            "replay-end-air-empty",
            json!({"_meta": {"jetbrains": {"air": {"version": 1, "capabilities": []}}}}),
        ),
    ] {
        let (updates, _response, _responded) = load_exchange(tag, caps, 0, &messages).await;
        assert_eq!(kinds(&updates), vec!["user_message_chunk"], "{tag}");
        assert!(
            !updates.iter().any(|(_, params)| is_end(params)),
            "no end event without loadReplayEnd: {tag}"
        );
    }
}

/// `create-session-options.test.ts` > "systemPrompt via _meta > forwards
/// excludeDynamicSections" and "... forwards append and excludeDynamicSections
/// together": the preset object's extra keys reach the initialize request, with
/// `type`/`preset` locked to the `claude_code` preset (O-168).
#[test]
fn t8d_exclude_dynamic_sections_is_forwarded() {
    use claude_agent_acp_rs::control::InitializeOptions;

    let fields = initialize_fields(&json!({
        "_meta": {"systemPrompt": {"excludeDynamicSections": true}}
    }))
    .expect("fields");
    assert_eq!(fields.system_prompt, None);
    assert_eq!(fields.append_system_prompt, None);
    assert_eq!(fields.exclude_dynamic_sections, Some(json!(true)));
    let request = InitializeOptions {
        exclude_dynamic_sections: fields.exclude_dynamic_sections,
        ..Default::default()
    }
    .build_request();
    assert_eq!(request["excludeDynamicSections"], json!(true));
    assert!(
        request.get("appendSystemPrompt").is_none(),
        "no append: {request}"
    );

    let fields = initialize_fields(&json!({
        "_meta": {"systemPrompt": {
            "append": "extra instructions",
            "excludeDynamicSections": true
        }}
    }))
    .expect("fields");
    assert_eq!(fields.system_prompt, None);
    assert_eq!(
        fields.append_system_prompt,
        Some("extra instructions".to_string())
    );
    assert_eq!(fields.exclude_dynamic_sections, Some(json!(true)));
    let request = InitializeOptions {
        append_system_prompt: fields.append_system_prompt,
        exclude_dynamic_sections: fields.exclude_dynamic_sections,
        ..Default::default()
    }
    .build_request();
    assert_eq!(request["appendSystemPrompt"], "extra instructions");
    assert_eq!(request["excludeDynamicSections"], json!(true));

    // An absent key stays absent; caller-provided type/preset are locked and
    // never forwarded.
    let fields = initialize_fields(&json!({
        "_meta": {"systemPrompt": {"type": "something-else", "preset": "other", "append": "x"}}
    }))
    .expect("fields");
    assert_eq!(fields.exclude_dynamic_sections, None);
    let request = InitializeOptions {
        exclude_dynamic_sections: fields.exclude_dynamic_sections,
        ..Default::default()
    }
    .build_request();
    assert!(request.get("excludeDynamicSections").is_none());
}

// ---- load replay failure semantics (M-131 / M-132) --------------------------

/// Run `body` against a served agent whose fake CLI replays `steps`; collects
/// every notification (timestamped) and hands the body the connection, the
/// session cwd and the `CLAUDE_CONFIG_DIR` it seeded.
async fn with_session_agent<F, Fut>(
    tag: &str,
    caps: Value,
    steps: Vec<Value>,
    body: F,
) -> Vec<(Instant, Value)>
where
    F: FnOnce(ConnectionTo<Agent>, PathBuf, PathBuf) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), Error>> + Send,
{
    let tmp = temp_dir(tag);
    let cwd = tmp.join("cwd");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let config = tmp.join("config");
    std::fs::create_dir_all(&config).expect("config");
    let script = tmp.join("steps.transcript.jsonl");
    let text: String = steps.iter().map(|step| step.to_string() + "\n").collect();
    std::fs::write(&script, text).expect("script");
    let opts = ServeOptions {
        claude_path: Some(fake_claude_binary()),
        extra_env: vec![
            (
                "FAKE_CLAUDE_SCRIPT".to_string(),
                script.to_string_lossy().into_owned(),
            ),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                config.to_string_lossy().into_owned(),
            ),
        ],
        default_cwd: Some(cwd.clone()),
        env: EnvSnapshot::from_pairs([(
            "CLAUDE_CONFIG_DIR".to_string(),
            config.to_string_lossy().into_owned(),
        )]),
        ..Default::default()
    };
    let (agent_channel, client_channel) = Channel::duplex();
    let serve_task = tokio::spawn(async move { serve(agent_channel, opts).await });
    let (tx, mut rx) = mpsc::unbounded_channel::<(Instant, Value)>();
    let collector = tokio::spawn(async move {
        let mut seen = Vec::new();
        while let Some(item) = rx.recv().await {
            seen.push(item);
        }
        seen
    });
    let body_cwd = cwd.clone();
    let body_config = config.clone();
    timeout(
        Duration::from_secs(60),
        Client
            .builder()
            .on_receive_notification(
                async move |notification: UntypedMessage, _cx| {
                    let _ = tx.send((Instant::now(), notification.params().clone()));
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(
                client_channel,
                move |connection: ConnectionTo<Agent>| async move {
                    connection
                        .send_request(UntypedMessage::new(
                            "initialize",
                            json!({"protocolVersion": 1, "clientCapabilities": caps}),
                        )?)
                        .block_task()
                        .await?;
                    body(connection, body_cwd, body_config).await
                },
            ),
    )
    .await
    .expect("the exchange finished in time")
    .expect("the exchange succeeded");
    let _ = timeout(Duration::from_secs(10), serve_task).await;
    let updates = collector.await.expect("collector");
    let _ = std::fs::remove_dir_all(&tmp);
    updates
}

fn failed_init_step(session_id: &str) -> Value {
    json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "error", "request_id": "$REQ",
            "error": format!("No conversation found with session ID: {session_id}")
        }}]
    })
}

fn delayed_failed_init_step(session_id: &str, delay_ms: u64) -> Value {
    json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [
            {"sleep_ms": delay_ms},
            {"type": "control_response", "response": {
                "subtype": "error", "request_id": "$REQ",
                "error": format!("No conversation found with session ID: {session_id}")
            }}
        ]
    })
}

fn init_step_json() -> Value {
    json!({
        "expect": {"type": "control_request", "request": {"subtype": "initialize"}},
        "emit": [{"type": "control_response", "response": {
            "subtype": "success", "request_id": "$REQ",
            "response": {"models": [], "commands": [], "account": {}}
        }}]
    })
}

fn load_end_caps() -> Value {
    json!({"_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["loadReplayEnd"]}}}})
}

fn history(id: &str, count: usize) -> Vec<Value> {
    (0..count)
        .map(|i| {
            let mut message = user_message(id, &format!("u{i}"), &format!("message {i}"));
            if i > 0 {
                message["parentUuid"] = json!(format!("u{}", i - 1));
            }
            message
        })
        .collect()
}

async fn load_request(
    connection: &ConnectionTo<Agent>,
    id: &str,
    cwd: &Path,
) -> Result<Value, Error> {
    connection
        .send_request(UntypedMessage::new(
            "session/load",
            json!({
                "sessionId": id,
                "cwd": cwd.to_string_lossy(),
                "mcpServers": [],
            }),
        )?)
        .block_task()
        .await
}

/// `create-session-options.test.ts` > "loadSession history replay > stops the
/// replay when the CLI start fails": the failed start stops the replay, so the
/// client gets no replayed update at all.
#[tokio::test]
async fn t8d_failed_cli_start_stops_the_load_replay() {
    let id = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    let updates = with_session_agent(
        "load-start-fails",
        json!({}),
        vec![failed_init_step(id)],
        move |connection, cwd, config| async move {
            seed_load_transcript(&config, &cwd, id, &history(id, 5_000));
            let error = load_request(&connection, id, &cwd)
                .await
                .expect_err("a failed start fails the load");
            assert_eq!(i32::from(error.code), -32002, "{error}");
            Ok(())
        },
    )
    .await;
    let session_updates: Vec<&Value> = updates
        .iter()
        .map(|(_, params)| params)
        .filter(|params| params.get("update").is_some())
        .collect();
    assert!(
        session_updates.is_empty(),
        "a failed start sends no replayed update: {session_updates:?}"
    );
}

/// `create-session-options.test.ts` > "sends no update after a load that fails
/// while the replay runs": the replay sends its frames, and once the load
/// rejects no further update arrives.
#[tokio::test]
async fn t8d_no_update_after_a_load_that_fails_while_the_replay_runs() {
    let id = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    let updates = with_session_agent(
        "load-fails-mid-replay",
        json!({}),
        vec![delayed_failed_init_step(id, 400)],
        move |connection, cwd, config| async move {
            seed_load_transcript(&config, &cwd, id, &history(id, 3));
            let error = load_request(&connection, id, &cwd)
                .await
                .expect_err("a failed start fails the load");
            assert_eq!(i32::from(error.code), -32002, "{error}");
            Ok(())
        },
    )
    .await;
    let replayed: Vec<&Value> = updates
        .iter()
        .map(|(_, params)| params)
        .filter(|params| params.get("update").is_some())
        .collect();
    assert!(
        !replayed.is_empty(),
        "the replay runs while the CLI starts: {updates:?}"
    );
    // The collector drains until the serve task ends, after the load's error:
    // no update can arrive after it (the replay future is joined before the
    // response).
    assert_eq!(
        replayed.len(),
        3,
        "every message replayed before the failure: {updates:?}"
    );
}

/// `create-session-options.test.ts` > "load replay end > sends no end event when
/// the CLI start fails part way through the replay".
#[tokio::test]
async fn t8d_no_replay_end_when_the_cli_start_fails() {
    let id = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    let updates = with_session_agent(
        "load-end-start-fails",
        load_end_caps(),
        vec![failed_init_step(id)],
        move |connection, cwd, config| async move {
            seed_load_transcript(&config, &cwd, id, &history(id, 5_000));
            let error = load_request(&connection, id, &cwd)
                .await
                .expect_err("a failed start fails the load");
            assert_eq!(i32::from(error.code), -32002, "{error}");
            Ok(())
        },
    )
    .await;
    assert!(
        !updates.iter().any(|(_, params)| is_end(params)),
        "no end event when the start failed: {updates:?}"
    );
    let session_updates: Vec<&Value> = updates
        .iter()
        .map(|(_, params)| params)
        .filter(|params| params.get("update").is_some())
        .collect();
    assert!(
        session_updates.is_empty(),
        "no replayed update either: {session_updates:?}"
    );
}

/// `create-session-options.test.ts` > "load replay end > sends the end event
/// after the replay of a session that is already live": the live-load branch
/// sends the marker after its replay and before the response.
#[tokio::test]
async fn t8d_live_load_sends_the_replay_end_after_the_replay() {
    let updates = with_session_agent(
        "load-end-live",
        load_end_caps(),
        vec![init_step_json()],
        move |connection, cwd, config| async move {
            let created = connection
                .send_request(UntypedMessage::new(
                    "session/new",
                    json!({"cwd": cwd.to_string_lossy(), "mcpServers": []}),
                )?)
                .block_task()
                .await?;
            let id = created["sessionId"]
                .as_str()
                .expect("a session id")
                .to_string();
            seed_load_transcript(&config, &cwd, &id, &history(&id, 2));
            load_request(&connection, &id, &cwd).await?;
            Ok(())
        },
    )
    .await;
    assert_eq!(
        kinds(&updates),
        vec!["user_message_chunk", "user_message_chunk", "end"],
        "{updates:?}"
    );
    let end = updates
        .iter()
        .find(|(_, params)| is_end(params))
        .expect("an end event");
    assert_eq!(
        end.1,
        json!({
            "sessionId": end.1["sessionId"].clone(),
            "update": {
                "sessionUpdate": "session_info_update",
                "_meta": {"jetbrains": {"air": {"version": 1, "loadReplayEnd": true}}}
            }
        })
    );
}

/// `create-session-options.test.ts` > "load replay end > sends no end event when
/// the replay fails": an unreadable history fails the load and sends no marker.
#[tokio::test]
async fn t8d_no_replay_end_when_the_history_is_unreadable() {
    let updates = with_session_agent(
        "load-end-replay-fails",
        load_end_caps(),
        vec![init_step_json()],
        move |connection, cwd, config| async move {
            let created = connection
                .send_request(UntypedMessage::new(
                    "session/new",
                    json!({"cwd": cwd.to_string_lossy(), "mcpServers": []}),
                )?)
                .block_task()
                .await?;
            let id = created["sessionId"]
                .as_str()
                .expect("a session id")
                .to_string();
            // A transcript that exists but cannot be decoded: the load must
            // reject instead of replaying it as empty.
            let dir = config.join("projects").join(encode_project_path(
                &cwd.to_string_lossy(),
                Platform::host(),
            ));
            std::fs::create_dir_all(&dir).expect("project dir");
            std::fs::write(dir.join(format!("{id}.jsonl")), b"\xff\xfe\xfd\n")
                .expect("unreadable transcript");
            let error = load_request(&connection, &id, &cwd)
                .await
                .expect_err("an unreadable history fails the load");
            assert_eq!(i32::from(error.code), -32603, "{error}");
            Ok(())
        },
    )
    .await;
    assert!(
        !updates.iter().any(|(_, params)| is_end(params)),
        "no end event when the replay failed: {updates:?}"
    );
}
