//! Turn 1 lane B: `additional-roots.test.ts`. The directories handed to the SDK
//! are the user's `claudeCode.options.additionalDirectories` first, then the
//! official ACP `additionalDirectories` (or, only when that field is omitted,
//! the legacy `_meta.additionalRoots`), passed through without normalization.

use std::path::PathBuf;

use claude_agent_acp_rs::agent::{spawn_inputs, ServeOptions};
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use claude_agent_acp_rs::process::{build_argv, SpawnOptions};
use claude_agent_acp_rs::sdk_options::claude_options;
use serde_json::json;

fn dirs(params: serde_json::Value) -> Vec<PathBuf> {
    claude_options(&params).additional_directories
}

#[test]
fn passes_through_relative_roots_as_provided() {
    let got = dirs(json!({"cwd": "/project", "_meta": {"additionalRoots": ["."]}}));
    assert_eq!(got, [PathBuf::from(".")]);
}

#[test]
fn merges_additional_roots_with_user_directories_without_normalization() {
    let got = dirs(json!({"_meta": {
        "additionalRoots": ["", "/tmp/claude-root-x"],
        "claudeCode": {"options": {"additionalDirectories": ["/workspace/shared"]}},
    }}));
    assert_eq!(
        got,
        [
            PathBuf::from("/workspace/shared"),
            PathBuf::from(""),
            PathBuf::from("/tmp/claude-root-x"),
        ]
    );
}

#[test]
fn prefers_the_official_field_over_meta_additional_roots() {
    let got = dirs(json!({
        "additionalDirectories": ["/from/official"],
        "_meta": {"additionalRoots": ["/from/meta"]},
    }));
    assert_eq!(got, [PathBuf::from("/from/official")]);
}

#[test]
fn merges_official_directories_with_the_sdk_option() {
    let got = dirs(json!({
        "additionalDirectories": ["/from/official"],
        "_meta": {"claudeCode": {"options": {"additionalDirectories": ["/from/sdk"]}}},
    }));
    assert_eq!(
        got,
        [PathBuf::from("/from/sdk"), PathBuf::from("/from/official")]
    );
}

#[test]
fn falls_back_to_meta_additional_roots_when_the_official_field_is_omitted() {
    let got = dirs(json!({"_meta": {"additionalRoots": ["/from/meta"]}}));
    assert_eq!(got, [PathBuf::from("/from/meta")]);
}

/// FA1-11 (`acp-agent.ts:11142-11143`): a present official field is spread as
/// Node does — a string becomes its characters — and `_meta.additionalRoots`
/// is never consulted once the field is present. The SDK argv is exactly
/// `--add-dir=/` + `--add-dir=x`.
#[test]
fn fa1_11_a_string_official_field_spreads_into_characters_not_meta_roots() {
    let params = json!({
        "additionalDirectories": "/x",
        "_meta": {
            "additionalRoots": ["/from/meta"],
            "claudeCode": {"options": {"additionalDirectories": ["/opt"]}},
        },
    });
    let directories = dirs(params);
    assert_eq!(
        directories,
        [
            PathBuf::from("/opt"),
            PathBuf::from("/"),
            PathBuf::from("x")
        ]
    );
    let spawn = SpawnOptions {
        additional_directories: directories,
        ..SpawnOptions::default()
    };
    let argv = build_argv(&spawn);
    let add_dirs: Vec<&str> = argv
        .iter()
        .filter(|arg| arg.starts_with("--add-dir="))
        .map(String::as_str)
        .collect();
    assert_eq!(
        add_dirs,
        ["--add-dir=/opt", "--add-dir=/", "--add-dir=x"],
        "{argv:?}"
    );
}

/// FA1-11: a present non-iterable official field fails session creation (Node's
/// `[...value]` throws) instead of silently falling back to
/// `_meta.additionalRoots`.
#[test]
fn fa1_11_a_present_non_iterable_official_field_fails_session_creation() {
    for bad in [json!(5), json!({"a": 1}), json!(true)] {
        let options = claude_options(&json!({
            "additionalDirectories": bad,
            "_meta": {"additionalRoots": ["/from/meta"]},
        }));
        assert!(
            options.additional_directories.is_empty(),
            "a non-iterable field is not replaced by additionalRoots: {bad}"
        );

        let tmp = std::env::temp_dir().join(format!(
            "fa1-11-roots-{}-{}",
            std::process::id(),
            bad.to_string().len()
        ));
        std::fs::create_dir_all(&tmp).expect("temp dir");
        let serve = ServeOptions::from_snapshot(EnvSnapshot::from_pairs([
            ("HOME".to_string(), tmp.to_string_lossy().into_owned()),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                tmp.to_string_lossy().into_owned(),
            ),
        ]));
        let params = json!({
            "cwd": tmp.to_string_lossy(),
            "additionalDirectories": bad,
            "_meta": {"additionalRoots": ["/from/meta"]},
        });
        let error = spawn_inputs(&params, Some(&tmp), &serve, None)
            .expect_err("a non-iterable additionalDirectories fails the session");
        assert_eq!(i32::from(error.code), -32603, "{bad}");
        let details = error
            .data
            .as_ref()
            .and_then(|data| data.get("details"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(details.contains("not iterable"), "{bad}: {details}");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
