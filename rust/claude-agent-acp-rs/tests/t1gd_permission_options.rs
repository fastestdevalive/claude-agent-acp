//! Tests for Turn 1 Fix Round 2 Lane D: permission option correctness (G9, T1-T5 tilde & dotdot, S6 trailing slash).

use std::path::{Path, PathBuf};

use claude_agent_acp_rs::permission_options::{
    build_options, resolve_lexical, ChangeSet, OptionsContext, OPTION_ALLOW_WITH_UPDATES,
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

/// Item 3 / Defect 7: Shell suggestions label must preserve distinction between paths
/// that differ only by a trailing slash (e.g. `/a/b/` vs `/a/b`), matching Node `path.normalize`.
#[test]
fn inv_p20_shell_trailing_slash_preserved() {
    let cwd = PathBuf::from("/repo");

    // S6: addDirectories ["/a/b/", "/a/b"]
    let cs = ChangeSet {
        updates: vec![json!({
            "type": "addDirectories",
            "destination": "session",
            "directories": ["/a/b/", "/a/b"],
        })],
    };
    let ctx = make_ctx("Bash", json!({ "command": "ls" }), cwd.clone(), Some(cs));
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, and always allow access to /a/b/ and /a/b/ from this project"),
        "Paths differing only by trailing slash must not be deduplicated away"
    );

    // Exact duplicate trailing slashes should still deduplicate
    let cs_dup = ChangeSet {
        updates: vec![json!({
            "type": "addDirectories",
            "destination": "session",
            "directories": ["/a/b/", "/a/b/"],
        })],
    };
    let ctx_dup = make_ctx("Bash", json!({ "command": "ls" }), cwd, Some(cs_dup));
    assert_eq!(
        durable_label(&ctx_dup).as_deref(),
        Some("Yes, and always allow access to b/ from this project"),
        "Exact duplicate paths must be deduplicated"
    );
}

/// Item 2 / Defect 3: Tilde (~) is not expanded by Node's path.resolve (treated as literal filename ~/x).
/// Rules like `~/proj/**` do not match absolute paths in `/home/user/proj/`.
#[test]
fn inv_p22_tilde_not_expanded_edit_rules() {
    let cwd = PathBuf::from("/repo");

    // T1: Edit rule "~/proj/**" with file "/home/user/proj/a.txt"
    let cs_rule = ChangeSet {
        updates: vec![json!({
            "type": "addRules",
            "destination": "session",
            "behavior": "allow",
            "rules": [{ "toolName": "Edit", "ruleContent": "~/proj/**" }],
        })],
    };
    let ctx = make_ctx(
        "Edit",
        json!({ "file_path": "/home/user/proj/a.txt" }),
        cwd.clone(),
        Some(cs_rule),
    );
    assert_eq!(
        option_names(&ctx),
        vec!["Yes", "No"],
        "Edit rule with ~/ must not expand ~ and must not match /home/user/ path"
    );

    // T4: ~ alone rule with file "/home/user/a.txt"
    let cs_alone = ChangeSet {
        updates: vec![json!({
            "type": "addRules",
            "destination": "session",
            "behavior": "allow",
            "rules": [{ "toolName": "Edit", "ruleContent": "~" }],
        })],
    };
    let ctx_alone = make_ctx(
        "Edit",
        json!({ "file_path": "/home/user/a.txt" }),
        cwd,
        Some(cs_alone),
    );
    assert_eq!(
        option_names(&ctx_alone),
        vec!["Yes", "No"],
        "Edit rule with ~ alone must not expand to home_dir"
    );
}

/// Item 2 / Defect 3: Tilde in candidate file paths resolves relative to cwd (`<cwd>/~/x`).
#[test]
fn inv_p22_tilde_not_expanded_file_paths() {
    let cwd = PathBuf::from("/repo");
    let ed = ChangeSet {
        updates: vec![
            json!({ "type": "setMode", "destination": "session", "mode": "acceptEdits" }),
            json!({ "type": "addRules", "destination": "session", "behavior": "allow", "rules": [{ "toolName": "Edit" }] }),
        ],
    };

    // T2: file_path "~/x.txt" with acceptEdits resolves to "/repo/~/x.txt", inside cwd
    let ctx_file = make_ctx(
        "Edit",
        json!({ "file_path": "~/x.txt" }),
        cwd.clone(),
        Some(ed.clone()),
    );
    assert_eq!(
        durable_label(&ctx_file).as_deref(),
        Some("Yes, allow all edits during this session"),
        "~/x.txt resolves to <cwd>/~/x.txt, inside cwd, getting in-project label"
    );

    // T3: file_path "~/.claude/settings.json" resolves to "/repo/~/.claude/settings.json",
    // not in real ~/.claude, so does not get special Claude settings label
    let ctx_claude = make_ctx(
        "Edit",
        json!({ "file_path": "~/.claude/settings.json" }),
        cwd,
        Some(ed),
    );
    assert_eq!(
        durable_label(&ctx_claude).as_deref(),
        Some("Yes, allow all edits during this session"),
        "~/.claude/settings.json resolves under <cwd>/, not real ~/.claude"
    );
}

/// Item 2 / Defect 3: Empty candidate resolves base normalizing `..` lexically.
#[test]
fn inv_p22_cwd_with_dotdot_normalization() {
    let ed = ChangeSet {
        updates: vec![
            json!({ "type": "setMode", "destination": "session", "mode": "acceptEdits" }),
            json!({ "type": "addRules", "destination": "session", "behavior": "allow", "rules": [{ "toolName": "Edit" }] }),
        ],
    };

    // T5: cwd with ..: cwd="/repo/x/..", file="/repo/x/a.txt"
    // Node path.resolve('/repo/x/..', '') -> '/repo'
    // /repo/x/a.txt is inside /repo, so Node gives "Yes, allow all edits during this session"
    let ctx = make_ctx(
        "Edit",
        json!({ "file_path": "/repo/x/a.txt" }),
        PathBuf::from("/repo/x/.."),
        Some(ed),
    );
    assert_eq!(
        durable_label(&ctx).as_deref(),
        Some("Yes, allow all edits during this session"),
        "Cwd containing .. must be normalized so file inside /repo is recognized as inside cwd"
    );
}

/// Item 1: Lexical resolution edge cases, root clamping, relative traversal, and empty candidate normalization.
#[test]
fn inv_resolve_lexical_posix_and_edge_cases() {
    // Empty candidate normalizes base
    assert_eq!(
        resolve_lexical(Path::new("/repo/x/.."), ""),
        PathBuf::from("/repo")
    );
    assert_eq!(
        resolve_lexical(Path::new("/repo/sub/../sibling"), ""),
        PathBuf::from("/repo/sibling")
    );
    assert_eq!(resolve_lexical(Path::new("/"), ""), PathBuf::from("/"));

    // Rooted absolute candidate replaces base
    assert_eq!(
        resolve_lexical(Path::new("/repo/sub"), "/etc/hosts"),
        PathBuf::from("/etc/hosts")
    );
    assert_eq!(
        resolve_lexical(Path::new("/repo/sub"), "/"),
        PathBuf::from("/")
    );

    // Relative candidate with .. cancellation
    assert_eq!(
        resolve_lexical(Path::new("/repo/sub"), "../../etc/hosts"),
        PathBuf::from("/etc/hosts")
    );
    assert_eq!(
        resolve_lexical(Path::new("/repo/sub"), "../sibling/file.txt"),
        PathBuf::from("/repo/sibling/file.txt")
    );

    // .. at filesystem root does not escape root (clamped at /)
    assert_eq!(resolve_lexical(Path::new("/"), ".."), PathBuf::from("/"));
    assert_eq!(
        resolve_lexical(Path::new("/"), "../../.."),
        PathBuf::from("/")
    );
    assert_eq!(
        resolve_lexical(Path::new("/a"), "../.."),
        PathBuf::from("/")
    );

    // Relative bases (`foo/bar`) are deliberately not asserted: Node resolves
    // them against process.cwd(), and the adapter's cwd is never relative.
    // Absolute bases keep `..` at the root and drop dot segments:
    assert_eq!(
        resolve_lexical(Path::new("/foo/bar"), "../baz"),
        PathBuf::from("/foo/baz")
    );
    assert_eq!(
        resolve_lexical(Path::new("/foo"), "../../baz"),
        PathBuf::from("/baz")
    );
    assert_eq!(resolve_lexical(Path::new("/foo"), ".."), PathBuf::from("/"));
}
