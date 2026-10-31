//! Turn 1 lane B: `additional-roots.test.ts`. The directories handed to the SDK
//! are the user's `claudeCode.options.additionalDirectories` first, then the
//! official ACP `additionalDirectories` (or, only when that field is omitted,
//! the legacy `_meta.additionalRoots`), passed through without normalization.

use std::path::PathBuf;

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
