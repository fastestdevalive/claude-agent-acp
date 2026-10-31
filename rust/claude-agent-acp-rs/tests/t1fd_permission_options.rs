//! Tests for Turn 1 Fix Round Lane D: permission option correctness (P-25, P-22, P-23, P-20).

use std::path::PathBuf;

use claude_agent_acp_rs::permission_options::{
    build_options, ChangeSet, OptionsContext, OPTION_ALLOW_WITH_UPDATES,
};
use serde_json::json;

fn make_ctx(
    tool_name: &str,
    input: serde_json::Value,
    cwd: PathBuf,
    change_set: Option<ChangeSet>,
) -> OptionsContext {
    OptionsContext {
        tool_name: tool_name.to_string(),
        display_name: None,
        input,
        cwd,
        change_set,
        no_persistent_rule: false,
        default_to_no: false,
        available_modes: vec!["default".into(), "acceptEdits".into()],
        auto_unavailable: false,
        pre_plan_mode: None,
        context_used_percent: None,
        home_dir: Some(PathBuf::from("/home/user")),
    }
}

fn option_names(ctx: &OptionsContext) -> Vec<String> {
    build_options(ctx)
        .into_iter()
        .filter_map(|opt| opt.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect()
}

fn durable_label(ctx: &OptionsContext) -> Option<String> {
    build_options(ctx)
        .into_iter()
        .find(|opt| {
            opt.get("optionId").and_then(|id| id.as_str()) == Some(OPTION_ALLOW_WITH_UPDATES)
        })
        .and_then(|opt| opt.get("name").and_then(|n| n.as_str()).map(String::from))
}

#[test]
fn inv_p25_webfetch_whatwg_url_parsing() {
    let cwd = PathBuf::from("/repo");

    // Case 1: Backslash userinfo trick - evil.com\@trusted.com/
    // Node parses hostname as "evil.com". Old Rust parser extracted "trusted.com".
    let ctx = make_ctx(
        "WebFetch",
        json!({ "url": "https://evil.com\\@trusted.com/" }),
        cwd.clone(),
        None,
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, and don't ask again for evil.com"),
        "Backslash userinfo must resolve hostname evil.com"
    );

    // Case 2: Invalid port 99999 - Node throws Invalid URL, offers no durable option.
    let ctx = make_ctx(
        "WebFetch",
        json!({ "url": "https://example.com:99999/" }),
        cwd.clone(),
        None,
    );
    assert_eq!(
        option_names(&ctx),
        vec!["Yes", "No"],
        "Port 99999 must not produce a durable option"
    );

    // Case 3: Internationalized domain name - Node punycodes to xn--bcher-kva.example
    let ctx = make_ctx(
        "WebFetch",
        json!({ "url": "https://bücher.example/" }),
        cwd.clone(),
        None,
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, and don't ask again for xn--bcher-kva.example"),
        "IDN domain must be punycoded"
    );

    // Case 4: Uppercase host - Node lowercases to example.com
    let ctx = make_ctx(
        "WebFetch",
        json!({ "url": "https://EXAMPLE.COM/" }),
        cwd.clone(),
        None,
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, and don't ask again for example.com")
    );

    // Case 5: IPv6 literal - Node keeps brackets [::1]
    let ctx = make_ctx(
        "WebFetch",
        json!({ "url": "https://[::1]/" }),
        cwd.clone(),
        None,
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, and don't ask again for [::1]")
    );

    // Case 6: IPv6 with port - Node keeps brackets [2001:db8::1]
    let ctx = make_ctx(
        "WebFetch",
        json!({ "url": "https://[2001:db8::1]:8080/path" }),
        cwd.clone(),
        None,
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, and don't ask again for [2001:db8::1]")
    );

    // Case 7: Trailing dot in domain - Node preserves trailing dot
    let ctx = make_ctx(
        "WebFetch",
        json!({ "url": "https://example.com./" }),
        cwd.clone(),
        None,
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, and don't ask again for example.com.")
    );

    // Case 8: file: scheme - Node hostname is empty, no durable option
    let ctx = make_ctx(
        "WebFetch",
        json!({ "url": "file:///path/to/file" }),
        cwd.clone(),
        None,
    );
    assert_eq!(option_names(&ctx), vec!["Yes", "No"]);

    // Case 9: data: scheme - Node hostname is empty, no durable option
    let ctx = make_ctx(
        "WebFetch",
        json!({ "url": "data:text/plain,hello" }),
        cwd.clone(),
        None,
    );
    assert_eq!(option_names(&ctx), vec!["Yes", "No"]);

    // Case 10: No scheme - invalid URL, no durable option
    let ctx = make_ctx("WebFetch", json!({ "url": "no-scheme" }), cwd.clone(), None);
    assert_eq!(option_names(&ctx), vec!["Yes", "No"]);

    // Case 11: Tab in URL - WHATWG strips ASCII whitespace
    let ctx = make_ctx(
        "WebFetch",
        json!({ "url": "https://example.com\t/path" }),
        cwd.clone(),
        None,
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, and don't ask again for example.com")
    );

    // Case 12: Newline in URL - WHATWG strips ASCII whitespace
    let ctx = make_ctx(
        "WebFetch",
        json!({ "url": "https://example.com\n/path" }),
        cwd,
        None,
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, and don't ask again for example.com")
    );
}

#[test]
fn inv_p22_p23_path_lexical_normalization_dotdot() {
    let cwd = PathBuf::from("/repo/sub");
    let cs = ChangeSet {
        updates: vec![
            json!({ "type": "setMode", "destination": "session", "mode": "acceptEdits" }),
            json!({ "type": "addRules", "destination": "session", "behavior": "allow", "rules": [{ "toolName": "Edit" }] }),
        ],
    };

    // Case A: /repo/sub/../../etc/hosts escapes cwd via '..'
    // Must NOT get "Yes, allow all edits during this session" (which is the in-project label).
    // Must get "Yes, allow all edits in etc/ during this session".
    let ctx = make_ctx(
        "Edit",
        json!({ "file_path": "/repo/sub/../../etc/hosts" }),
        cwd.clone(),
        Some(cs.clone()),
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, allow all edits in etc/ during this session"),
        "Path escaping project via .. must get outside directory label, not in-project label"
    );

    // Case B: .claude path hidden behind ..
    // /repo/sub/../.claude/config.json with cwd=/repo
    let ctx = make_ctx(
        "Edit",
        json!({ "file_path": "/repo/sub/../.claude/config.json" }),
        PathBuf::from("/repo"),
        Some(cs),
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, and allow Claude to edit its own settings for this session"),
        ".claude path hidden behind .. must be recognized as Claude settings path"
    );

    // Case C: relative ../shared/** rule in change set
    let cs_shared = ChangeSet {
        updates: vec![json!({
            "type": "addRules",
            "destination": "session",
            "behavior": "allow",
            "rules": [{ "toolName": "Edit", "ruleContent": "../shared/**" }],
        })],
    };
    let ctx = make_ctx(
        "Edit",
        json!({ "file_path": "/repo/shared/file.rs" }),
        cwd,
        Some(cs_shared),
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, allow all edits in shared/ during this session"),
        "Relative rule ../shared/** must match /repo/shared/file.rs when cwd is /repo/sub"
    );
}

#[test]
fn inv_p20_shell_durable_labels_backslash_and_duplicates() {
    let cwd = PathBuf::from("/repo");

    // Case 1: Backslash path on Unix "foo\\bar" -> "foo\\bar/"
    let cs_bs = ChangeSet {
        updates: vec![json!({
            "type": "addDirectories",
            "destination": "session",
            "directories": ["foo\\bar"],
        })],
    };
    let ctx = make_ctx(
        "Bash",
        json!({ "command": "echo hi" }),
        cwd.clone(),
        Some(cs_bs),
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, and always allow access to foo\\bar/ from this project"),
        "Backslash path on Unix must be preserved in display path"
    );

    // Case 2: Subdirectory with backslash "sub/foo\\bar"
    let cs_bs_sub = ChangeSet {
        updates: vec![json!({
            "type": "addDirectories",
            "destination": "session",
            "directories": ["sub/foo\\bar"],
        })],
    };
    let ctx = make_ctx(
        "Bash",
        json!({ "command": "echo hi" }),
        cwd.clone(),
        Some(cs_bs_sub),
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, and always allow access to foo\\bar/ from this project"),
        "Basename of sub/foo\\bar must be foo\\bar"
    );

    // Case 3: Duplicate directories ["foo", "foo"] with command ["git"]
    let cs_dup_dirs = ChangeSet {
        updates: vec![
            json!({
                "type": "addDirectories",
                "destination": "session",
                "directories": ["foo", "foo"],
            }),
            json!({
                "type": "addRules",
                "destination": "session",
                "behavior": "allow",
                "rules": [{ "toolName": "Bash", "ruleContent": "git" }],
            }),
        ],
    };
    let ctx = make_ctx(
        "Bash",
        json!({ "command": "git status" }),
        cwd.clone(),
        Some(cs_dup_dirs),
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, and allow foo/ access and git commands"),
        "Duplicate paths with command must use 'allow <path> access and <cmd> commands'"
    );

    // Case 4: Duplicate read rules ["foo/**", "foo/**"]
    let cs_dup_reads = ChangeSet {
        updates: vec![json!({
            "type": "addRules",
            "destination": "session",
            "behavior": "allow",
            "rules": [
                { "toolName": "Read", "ruleContent": "foo/**" },
                { "toolName": "Read", "ruleContent": "foo/**" },
            ],
        })],
    };
    let ctx = make_ctx(
        "Bash",
        json!({ "command": "cat foo/a" }),
        cwd.clone(),
        Some(cs_dup_reads),
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, allow reading from foo/ from this project")
    );

    // Case 5: Disambiguating basenames ["dir1/foo", "dir2/foo"]
    let cs_disambig = ChangeSet {
        updates: vec![json!({
            "type": "addDirectories",
            "destination": "session",
            "directories": ["dir1/foo", "dir2/foo"],
        })],
    };
    let ctx = make_ctx("Bash", json!({ "command": "ls" }), cwd, Some(cs_disambig));
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, and always allow access to dir1/foo/ and dir2/foo/ from this project")
    );
}

#[test]
fn inv_p22_relative_paths_from_session_cwd() {
    let cs = ChangeSet {
        updates: vec![json!({
            "type": "addRules",
            "destination": "session",
            "behavior": "allow",
            "rules": [{ "toolName": "Read", "ruleContent": "src/**" }],
        })],
    };
    let ctx = make_ctx(
        "Read",
        json!({ "file_path": "/workspace/src/a.ts" }),
        PathBuf::from("/workspace"),
        Some(cs),
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, during this session")
    );
}

#[test]
fn inv_p22_backslash_glob_suffix() {
    let cs = ChangeSet {
        updates: vec![json!({
            "type": "addRules",
            "destination": "session",
            "behavior": "allow",
            "rules": [{ "toolName": "Read", "ruleContent": "/outside\\**" }],
        })],
    };
    let ctx = make_ctx(
        "Read",
        json!({ "file_path": "/outside/a.ts" }),
        PathBuf::from("/workspace"),
        Some(cs),
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, allow reading from outside/ during this session")
    );
}

#[test]
fn inv_p22_accept_edits_broad_only_for_ordinary_write_paths() {
    let cs = ChangeSet {
        updates: vec![json!({
            "type": "setMode",
            "mode": "acceptEdits",
            "destination": "session",
        })],
    };
    let ctx_edit = make_ctx(
        "Edit",
        json!({ "file_path": "/workspace/a.ts" }),
        PathBuf::from("/workspace"),
        Some(cs.clone()),
    );
    assert_eq!(
        durable_label(&ctx_edit).as_deref(),
        Some("Yes, allow all edits during this session")
    );

    let ctx_claude = make_ctx(
        "Edit",
        json!({ "file_path": "/workspace/.claude/settings.json" }),
        PathBuf::from("/workspace"),
        Some(cs.clone()),
    );
    assert_eq!(option_names(&ctx_claude), vec!["Yes", "No"]);

    let ctx_read = make_ctx(
        "Read",
        json!({ "file_path": "/workspace/a.ts" }),
        PathBuf::from("/workspace"),
        Some(cs),
    );
    assert_eq!(option_names(&ctx_read), vec!["Yes", "No"]);
}

#[test]
fn inv_p25_webfetch_and_sandbox_host_labels() {
    let cs_wf = ChangeSet {
        updates: vec![json!({
            "type": "addRules",
            "behavior": "allow",
            "destination": "localSettings",
            "rules": [{ "toolName": "WebFetch", "ruleContent": "domain:example.com" }],
        })],
    };
    let ctx_wf = make_ctx(
        "WebFetch",
        json!({ "url": "https://example.com/a" }),
        PathBuf::from("/workspace"),
        Some(cs_wf),
    );
    assert_eq!(
        durable_label(&ctx_wf).as_deref(),
        Some("Yes, and don't ask again for example.com")
    );

    let cs_net = ChangeSet {
        updates: vec![json!({
            "type": "addRules",
            "behavior": "allow",
            "destination": "localSettings",
            "rules": [{ "toolName": "SandboxNetworkAccess", "ruleContent": "example.com" }],
        })],
    };
    let ctx_net = make_ctx(
        "SandboxNetworkAccess",
        json!({ "host": "example.com" }),
        PathBuf::from("/workspace"),
        Some(cs_net),
    );
    assert_eq!(
        durable_label(&ctx_net).as_deref(),
        Some("Yes, and don't ask again for example.com")
    );
}

#[test]
fn inv_p22_claude_settings_special_session_label() {
    let cs = ChangeSet {
        updates: vec![json!({
            "type": "addRules",
            "behavior": "allow",
            "destination": "session",
            "rules": [{ "toolName": "Edit", "ruleContent": "/workspace/.claude/**" }],
        })],
    };
    let ctx = make_ctx(
        "Edit",
        json!({ "file_path": "/workspace/.claude/settings.json" }),
        PathBuf::from("/workspace"),
        Some(cs),
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, and allow Claude to edit its own settings for this session")
    );
}
