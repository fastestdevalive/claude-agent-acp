//! Version-string composition tests (Phase 1, Decisions 2 & 3).
//!
//! The crate version is built from `porting/UPSTREAM_VERSION` by `build.rs` into
//! the `ACP_VERSION_STRING` / `UPSTREAM_NPM_VERSION` compile-time env vars, and
//! surfaced (a) by the `--version` flag on the stdio binary and (b) in
//! `initialize.agentInfo.version`.

mod common;

use common::{diff_frames, normalize_agent_info_version, pinned_upstream_version, Frame};
use serde_json::json;

/// 1.15 — the stdio binary's `--version` prints
/// `<CARGO_PKG_VERSION>+upstream.<pin>\n`, where `<pin>` is the committed
/// `porting/UPSTREAM_VERSION`.
#[test]
fn inv_35_version_string_composed_from_pin() {
    let pin = pinned_upstream_version();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_claude-agent-acp-rs"))
        .arg("--version")
        .output()
        .expect("run claude-agent-acp-rs --version");
    assert!(out.status.success(), "--version must exit 0");
    let stdout = String::from_utf8(out.stdout).expect("version string is utf8");
    let expected = format!("0.1.0+upstream.{}\n", pin);
    assert_eq!(
        stdout, expected,
        "--version must print the composed version"
    );
}

/// 1.15 — the compiled `VERSION` const is `<CARGO_PKG_VERSION>+upstream.<pin>`,
/// i.e. it ends with `+upstream.` + the mirrored `UPSTREAM_VERSION`.
#[test]
fn inv_35_agent_info_version_is_composed() {
    let composed = format!("0.1.0+upstream.{}", claude_agent_acp_rs::UPSTREAM_VERSION);
    let suffix = format!("+upstream.{}", claude_agent_acp_rs::UPSTREAM_VERSION);
    assert_eq!(claude_agent_acp_rs::VERSION, composed);
    assert!(
        claude_agent_acp_rs::VERSION.ends_with(&suffix),
        "VERSION must end with {suffix}"
    );
}

/// 1.6 — `normalize_agent_info_version` rejects a fixture whose `agentInfo`
/// version does not equal the given pin (a stale pin cannot silently pass):
/// the assertion trips rather than comparing unequal versions.
#[test]
#[should_panic(expected = "expected pin")]
fn inv_35_agent_info_normalizer_rejects_stale_pin() {
    let rust_frame = Frame::recv(json!({
        "id": 1,
        "result": {"agentInfo": {"name":"x","title":"y","version":"0.1.0+upstream.0.70.0"}}
    }));
    let fixture_frame = Frame::recv(json!({
        "id": 1,
        "result": {"agentInfo": {"name":"x","title":"y","version":"0.70.0"}}
    }));
    // The fixture records 0.70.0 but a stale pin 0.69.9 is requested: reject.
    let _ = normalize_agent_info_version(vec![rust_frame], vec![fixture_frame], "0.69.9");
}

/// Review follow-up (3.17-C) — the normalizer rewrites the FIXTURE side only
/// (Decision 3); it must NEVER rewrite the Rust side's value. Feeding a WRONG
/// Rust-side version therefore leaves the differential RED (the Rust value is
/// compared exactly), proving the normalizer does not paper over a Rust bug.
#[test]
fn inv_35_agent_info_normalizer_does_not_rewrite_rust_value() {
    let pin = pinned_upstream_version();
    // Rust side carries a DELIBERATELY WRONG composed version (a stale pin).
    let rust_frame = Frame::recv(json!({
        "id": 1,
        "result": {"agentInfo": {"name":"x","title":"y","version":"0.1.0+upstream.0.70.0"}}
    }));
    let fixture_frame = Frame::recv(json!({
        "id": 1,
        "result": {"agentInfo": {"name":"x","title":"y","version": pin}}
    }));
    let (rust, fixture) = normalize_agent_info_version(vec![rust_frame], vec![fixture_frame], &pin);
    // The fixture is rewritten to the composed value; the Rust side is NOT.
    // Because Rust's value is wrong, the diff must FAIL.
    assert!(
        diff_frames(&rust, &fixture, &[]).is_err(),
        "the normalizer must not rewrite the Rust side; a wrong Rust agentInfo.version must fail the diff"
    );
}
