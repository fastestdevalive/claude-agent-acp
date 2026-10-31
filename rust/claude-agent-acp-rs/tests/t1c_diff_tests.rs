use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use claude_agent_acp_rs::diff::{
    clear_diff_memo, diff_call_count, git_patch_text, isolate_diff_memo_for_test,
    patch_update_from_diff_tool_response, preview_patch_content, reset_diff_stats,
    set_diff_budget_for_test, v2_update_from_diff_tool_response, FileChange, GitPatchPaths,
    PatchForm, PatchHunk, MAX_PATCH_FILE_BYTES,
};
use claude_agent_acp_rs::tools::tool_info_from_tool_use_facts;
use serde_json::json;

static COUNTER: AtomicU64 = AtomicU64::new(1);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{now}-{count}"));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn file(&self, name: &str, content: Option<&[u8]>) -> PathBuf {
        let file_path = self.path.join(name);
        if let Some(bytes) = content {
            fs::write(&file_path, bytes).unwrap();
        }
        file_path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn patch_text(content: &[serde_json::Value]) -> String {
    content[0]["_meta"]["jetbrains"]["air"]["diffPatch"]["text"]
        .as_str()
        .expect("expected diffPatch.text")
        .to_string()
}

fn git_name(path: &std::path::Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    s.trim_start_matches('/').to_string()
}

/// A fresh memo and counters for the test's own thread: the memo and the
/// counter are per thread here, so parallel tests cannot disturb the counts.
fn fresh_diff_state() {
    isolate_diff_memo_for_test();
    clear_diff_memo();
    reset_diff_stats();
}

/// 2000 lines, and the same file with line 1000 replaced (upstream `before`/`after`).
fn memo_texts() -> (String, String) {
    let before: String = (0..2000).map(|i| format!("line {i}\n")).collect();
    let after = before.replace("line 1000\n", "line one thousand\n");
    (before, after)
}

fn edit_line_1000(file_str: &str) -> serde_json::Value {
    json!({
        "file_path": file_str,
        "old_string": "line 1000\n",
        "new_string": "line one thousand\n",
    })
}

// diff-memo.test.ts > "diffs an Edit once and gives the hook the same patch"
#[test]
fn test_diff_memo_reuse_edit_approval_and_hook() {
    fresh_diff_state();
    let tmp = TempDir::new("diff-memo");
    let (before, after) = memo_texts();
    let file = tmp.file("file.txt", Some(before.as_bytes()));
    let file_str = file.to_string_lossy().to_string();

    let preview = preview_patch_content("Edit", &edit_line_1000(&file_str), None, PatchForm::Air)
        .expect("preview");
    assert_eq!(diff_call_count(), 1);

    fs::write(&file, after.as_bytes()).unwrap();
    let hook = patch_update_from_diff_tool_response(
        &json!({
            "filePath": &file_str,
            "originalFile": before,
            "oldString": "line 1000\n",
        }),
        PatchForm::Air,
    )
    .expect("hook update");

    // The hook reads the diff of the approval instead of diffing again.
    assert_eq!(diff_call_count(), 1);
    assert_eq!(patch_text(&hook.content), patch_text(&preview));
    assert_eq!(hook.locations.len(), 1);
    assert_eq!(hook.locations[0].line, Some(998));
}

// diff-memo.test.ts > "diffs again when the written file differs from the preview"
#[test]
fn test_diff_memo_rediff_on_change() {
    fresh_diff_state();
    let tmp = TempDir::new("diff-memo-change");
    let (before, _) = memo_texts();
    let file = tmp.file("file.txt", Some(before.as_bytes()));
    let file_str = file.to_string_lossy().to_string();

    preview_patch_content("Edit", &edit_line_1000(&file_str), None, PatchForm::Air)
        .expect("preview");
    let written = before.replace("line 1000\n", "line 1000 changed later\n");
    fs::write(&file, written.as_bytes()).unwrap();

    let hook = patch_update_from_diff_tool_response(
        &json!({"filePath": &file_str, "originalFile": before}),
        PatchForm::Air,
    )
    .expect("hook");

    assert_eq!(diff_call_count(), 2);
    assert!(patch_text(&hook.content).contains("+line 1000 changed later\n"));
}

// diff-memo.test.ts > "diffs again after a diff that ran out of its budget"
#[test]
fn test_diff_memo_rediff_after_budget() {
    fresh_diff_state();
    let tmp = TempDir::new("diff-memo-budget");
    let (before, after) = memo_texts();
    let file = tmp.file("file.txt", Some(before.as_bytes()));
    let file_str = file.to_string_lossy().to_string();
    let input = json!({"file_path": &file_str, "content": after});

    // The first diff runs out of its budget (upstream: the mocked
    // `structuredPatch` returns undefined once).
    set_diff_budget_for_test(Some(Duration::ZERO));
    let first = preview_patch_content("Write", &input, None, PatchForm::Air).expect("first");
    set_diff_budget_for_test(None);
    let second = preview_patch_content("Write", &input, None, PatchForm::Air).expect("second");

    assert_eq!(diff_call_count(), 2);
    // Without a patch, the approval falls back to the standard diff.
    assert_eq!(
        first,
        vec![json!({"type": "diff", "path": &file_str, "oldText": before, "newText": after})]
    );
    // The second diff completes and gets the exact patch.
    assert!(patch_text(&second).contains("+line one thousand\n"));
}

// diff-memo.test.ts > "keeps only a few recent diffs, each until it is read again"
#[test]
fn test_diff_memo_cache_eviction() {
    fresh_diff_state();
    let tmp = TempDir::new("diff-memo-evict");
    let (before, _) = memo_texts();
    let file = tmp.file("file.txt", Some(before.as_bytes()));
    let file_str = file.to_string_lossy().to_string();
    let write = |i: usize| {
        preview_patch_content(
            "Write",
            &json!({"file_path": &file_str, "content": format!("{before}tail {i}\n")}),
            None,
            PatchForm::Air,
        )
    };

    for i in 0..6 {
        write(i).expect("preview");
    }
    assert_eq!(diff_call_count(), 6);
    // The most recent diffs are kept; the oldest were pushed out.
    write(5).expect("5 again");
    assert_eq!(diff_call_count(), 6);
    write(0).expect("0 again");
    assert_eq!(diff_call_count(), 7);
    // A diff that was read again is gone.
    write(5).expect("5 a third time");
    assert_eq!(diff_call_count(), 8);
}

#[test]
fn test_compact_patch_12000_lines() {
    let tmp = TempDir::new("patch-12k");
    let lines: Vec<String> = (1..=12_000).map(|i| format!("line {i}")).collect();
    let file_content = format!("{}\n", lines.join("\n"));
    let file = tmp.file("file.ts", Some(file_content.as_bytes()));
    let file_str = file.to_string_lossy().to_string();

    let preview = preview_patch_content(
        "Edit",
        &json!({
            "file_path": &file_str,
            "old_string": "line 6000\n",
            "new_string": "changed line\n",
        }),
        None,
        PatchForm::Air,
    )
    .expect("preview");

    assert_eq!(preview[0]["type"], "diff");
    assert_eq!(preview[0]["oldText"], serde_json::Value::Null);
    assert_eq!(preview[0]["newText"], "");

    let name = git_name(&file);
    let expected = [
        format!("diff --git a/{name} b/{name}"),
        format!("--- a/{name}"),
        format!("+++ b/{name}"),
        "@@ -5997,7 +5997,7 @@".to_string(),
        " line 5997".to_string(),
        " line 5998".to_string(),
        " line 5999".to_string(),
        "-line 6000".to_string(),
        "+changed line".to_string(),
        " line 6001".to_string(),
        " line 6002".to_string(),
        " line 6003".to_string(),
        "".to_string(),
    ]
    .join("\n");

    assert_eq!(patch_text(&preview), expected);
}

#[test]
fn test_patch_replace_all() {
    let tmp = TempDir::new("patch-replace-all");
    let file = tmp.file("file.ts", Some(b"old\nmiddle\nold\n"));
    let file_str = file.to_string_lossy().to_string();

    let preview = preview_patch_content(
        "Edit",
        &json!({
            "file_path": &file_str,
            "old_string": "old",
            "new_string": "new",
            "replace_all": true,
        }),
        None,
        PatchForm::Air,
    )
    .expect("preview");

    let text = patch_text(&preview);
    assert_eq!(text.matches("-old").count(), 2);
    assert_eq!(text.matches("+new").count(), 2);
}

#[test]
fn test_write_update_and_creation_patches() {
    let tmp = TempDir::new("patch-write");
    let existing = tmp.file("existing.ts", Some(b"before\n"));
    let missing = tmp.path.join("missing.ts");

    let update = preview_patch_content(
        "Write",
        &json!({"file_path": existing.to_string_lossy(), "content": "after\n"}),
        None,
        PatchForm::Air,
    )
    .expect("update preview");

    let creation = preview_patch_content(
        "Write",
        &json!({"file_path": missing.to_string_lossy(), "content": "created\n"}),
        None,
        PatchForm::Air,
    )
    .expect("creation preview");

    assert!(patch_text(&update).contains("-before\n+after\n"));

    let m_name = git_name(&missing);
    let expected_creation = [
        format!("diff --git a/{m_name} b/{m_name}"),
        "new file mode 100644".to_string(),
        "--- /dev/null".to_string(),
        format!("+++ b/{m_name}"),
        "@@ -0,0 +1 @@".to_string(),
        "+created".to_string(),
        "".to_string(),
    ]
    .join("\n");

    assert_eq!(patch_text(&creation), expected_creation);
}

#[test]
fn test_write_alias_keys() {
    let tmp = TempDir::new("write-aliases");
    let existing = tmp.file("existing.ts", Some(b"before\n"));
    let p_str = existing.to_string_lossy().to_string();

    for input in [
        json!({"path": &p_str, "content": "after\n"}),
        json!({"file_path": &p_str, "file_text": "after\n"}),
        json!({"path": &p_str, "file_content": "after\n"}),
    ] {
        let preview =
            preview_patch_content("Write", &input, None, PatchForm::Air).expect("preview");
        assert!(patch_text(&preview).contains("-before\n+after\n"));
    }
}

#[test]
fn test_no_preview_when_not_exact() {
    let tmp = TempDir::new("not-exact");
    let duplicate = tmp.file("dup.txt", Some(b"same\nsame\n"));
    let missing = tmp.path.join("missing.txt");
    let crlf = tmp.file("crlf.txt", Some(b"one\r\ntwo\r\n"));
    let binary = tmp.file("bin.bin", Some(&[0x61, 0x00, 0x62, 0x0a]));
    let large = tmp.file(
        "large.txt",
        Some(format!("{}\n", "x".repeat(MAX_PATCH_FILE_BYTES)).as_bytes()),
    );
    let unchanged = tmp.file("unchanged.txt", Some(b"before\n"));

    assert!(preview_patch_content(
        "Edit",
        &json!({"file_path": duplicate.to_string_lossy(), "old_string": "same", "new_string": "n"}),
        None,
        PatchForm::Air
    )
    .is_none());

    assert!(preview_patch_content(
        "Edit",
        &json!({"file_path": missing.to_string_lossy(), "old_string": "a", "new_string": "b"}),
        None,
        PatchForm::Air
    )
    .is_none());

    assert!(preview_patch_content(
        "Edit",
        &json!({"file_path": crlf.to_string_lossy(), "old_string": "one", "new_string": "1"}),
        None,
        PatchForm::Air
    )
    .is_none());

    assert!(preview_patch_content(
        "Edit",
        &json!({"file_path": binary.to_string_lossy(), "old_string": "a", "new_string": "c"}),
        None,
        PatchForm::Air
    )
    .is_none());

    assert!(preview_patch_content(
        "Edit",
        &json!({"file_path": large.to_string_lossy(), "old_string": "x", "new_string": "y"}),
        None,
        PatchForm::Air
    )
    .is_none());

    assert!(preview_patch_content(
        "Edit",
        &json!({"file_path": unchanged.to_string_lossy(), "old_string": "b", "new_string": "b"}),
        None,
        PatchForm::Air
    )
    .is_none());

    assert!(preview_patch_content(
        "Edit",
        &json!({"file_path": unchanged.to_string_lossy(), "old_string": "", "new_string": "b"}),
        None,
        PatchForm::Air
    )
    .is_none());

    assert!(preview_patch_content(
        "Write",
        &json!({"file_path": unchanged.to_string_lossy(), "content": "before\n"}),
        None,
        PatchForm::Air
    )
    .is_none());

    // Write on crlf file shows the overwrite notice
    let write_crlf = preview_patch_content(
        "Write",
        &json!({"file_path": crlf.to_string_lossy(), "content": "one\n"}),
        None,
        PatchForm::Air,
    )
    .expect("write crlf");
    assert_eq!(write_crlf[0]["type"], "content");

    // Write on missing with crlf content gets the standard diff
    let write_crlf_content = preview_patch_content(
        "Write",
        &json!({"file_path": missing.to_string_lossy(), "content": "a\r\nb\r\n"}),
        None,
        PatchForm::Air,
    )
    .expect("write missing crlf");
    assert_eq!(write_crlf_content[0]["type"], "diff");
    assert_eq!(write_crlf_content[0]["oldText"], serde_json::Value::Null);
    assert_eq!(write_crlf_content[0]["newText"], "a\r\nb\r\n");
}

#[test]
fn test_creation_patch_for_edit_empty_old_string() {
    let tmp = TempDir::new("edit-empty-old");
    let missing = tmp.path.join("missing.ts");
    let empty = tmp.file("empty.ts", Some(b""));

    let creation = patch_text(
        &preview_patch_content(
            "Edit",
            &json!({"file_path": missing.to_string_lossy(), "old_string": "", "new_string": "a\n"}),
            None,
            PatchForm::Air,
        )
        .expect("creation"),
    );

    let fill = patch_text(
        &preview_patch_content(
            "Edit",
            &json!({"file_path": empty.to_string_lossy(), "old_string": "", "new_string": "a\n"}),
            None,
            PatchForm::Air,
        )
        .expect("fill"),
    );

    assert!(creation.contains("new file mode 100644\n--- /dev/null\n"));
    assert!(creation.contains("@@ -0,0 +1 @@\n+a\n"));

    let empty_name = git_name(&empty);
    assert!(fill.contains(&format!(
        "--- a/{empty_name}\n+++ b/{empty_name}\n@@ -0,0 +1 @@\n"
    )));
}

#[test]
fn test_missing_final_newline_marker() {
    let tmp = TempDir::new("no-newline");
    let file = tmp.file("file.ts", Some(b"a\nb"));

    let patch = patch_text(
        &preview_patch_content(
            "Edit",
            &json!({"file_path": file.to_string_lossy(), "old_string": "b", "new_string": "c"}),
            None,
            PatchForm::Air,
        )
        .expect("patch"),
    );

    assert!(patch.contains("-b\n\\ No newline at end of file\n+c\n\\ No newline at end of file\n"));
}

#[test]
fn test_empty_new_string_deletes_line_break() {
    let tmp = TempDir::new("delete-line");
    let file = tmp.file("file.ts", Some(b"keep\ndrop\nkeep\n"));
    let repeated = tmp.file("repeated.ts", Some(b"x\nx"));

    let patch = patch_text(
        &preview_patch_content(
            "Edit",
            &json!({"file_path": file.to_string_lossy(), "old_string": "drop", "new_string": ""}),
            None,
            PatchForm::Air,
        )
        .expect("delete"),
    );
    assert!(patch.contains("@@ -1,3 +1,2 @@\n keep\n-drop\n keep\n"));

    let all = patch_text(
        &preview_patch_content(
            "Edit",
            &json!({"file_path": repeated.to_string_lossy(), "old_string": "x", "new_string": "", "replace_all": true}),
            None,
            PatchForm::Air,
        )
        .expect("replace all"),
    );
    assert!(all.contains("@@ -1,2 +1 @@\n-x\n x\n"));
}

#[test]
fn test_git_patch_headers() {
    let hunk = PatchHunk {
        old_start: 1,
        old_lines: 1,
        new_start: 1,
        new_lines: 1,
        lines: vec!["-a".to_string(), "+b".to_string()],
    };

    let patch = git_patch_text(
        "/work/src/App.ts",
        FileChange::Update,
        std::slice::from_ref(&hunk),
        GitPatchPaths::Git,
    );
    assert!(patch.starts_with("diff --git a/work/src/App.ts b/work/src/App.ts\n--- a/work/src/App.ts\n+++ b/work/src/App.ts\n"));

    let win_patch = git_patch_text(
        "C:\\work\\App.ts",
        FileChange::Update,
        std::slice::from_ref(&hunk),
        GitPatchPaths::Git,
    );
    assert!(win_patch.contains("diff --git a/C:/work/App.ts b/C:/work/App.ts\n"));

    let quoted_quote = git_patch_text(
        "/work/a\"b.ts",
        FileChange::Update,
        std::slice::from_ref(&hunk),
        GitPatchPaths::Git,
    );
    assert!(quoted_quote.contains(
        "diff --git \"a/work/a\\\"b.ts\" \"b/work/a\\\"b.ts\"\n--- \"a/work/a\\\"b.ts\"\n"
    ));

    let quoted_non_ascii = git_patch_text(
        "/work/é.ts",
        FileChange::Update,
        std::slice::from_ref(&hunk),
        GitPatchPaths::Git,
    );
    assert!(
        quoted_non_ascii.contains("diff --git \"a/work/\\303\\251.ts\" \"b/work/\\303\\251.ts\"\n")
    );

    let space_tab = git_patch_text(
        "/work/my file.ts",
        FileChange::Update,
        std::slice::from_ref(&hunk),
        GitPatchPaths::Git,
    );
    assert!(space_tab.contains("diff --git a/work/my file.ts b/work/my file.ts\n--- a/work/my file.ts\t\n+++ b/work/my file.ts\t\n"));

    let created = git_patch_text(
        "/f",
        FileChange::Create,
        &[PatchHunk {
            old_start: 1,
            old_lines: 0,
            new_start: 1,
            new_lines: 1,
            lines: vec!["+b".to_string()],
        }],
        GitPatchPaths::Git,
    );
    assert_eq!(
        created,
        "diff --git a/f b/f\nnew file mode 100644\n--- /dev/null\n+++ b/f\n@@ -0,0 +1 @@\n+b\n"
    );
}

#[test]
fn test_v2_diff_content_and_updates() {
    let tmp = TempDir::new("v2-diff");
    let file = tmp.file("file.ts", Some(b"line 1\nnew\n"));
    let file_str = file.to_string_lossy().to_string();

    let update = v2_update_from_diff_tool_response(&json!({
        "filePath": &file_str,
        "oldString": "old",
        "originalFile": "line 1\nold\n",
        "structuredPatch": [],
    }))
    .expect("v2 update");

    assert_eq!(update.locations.len(), 1);
    assert_eq!(update.locations[0].line, Some(1));

    let val = &update.content[0];
    assert_eq!(val["type"], "diff");
    assert_eq!(val["changes"][0]["operation"], "modify");
    assert_eq!(val["changes"][0]["path"], file_str);
    assert!(val["patch"]["text"]
        .as_str()
        .unwrap()
        .contains("diff --git"));
}

#[test]
fn test_t40_write_input_diff_gating() {
    let input = json!({
        "file_path": "/work/a.ts",
        "content": "a\n"
    });

    // Client without diffPatch: receives input diff with oldText: null
    let shape_no_patch = tool_info_from_tool_use_facts("Write", &input, false, None, false);
    assert_eq!(shape_no_patch.content.len(), 1);
    let val = serde_json::to_value(&shape_no_patch.content[0]).unwrap();
    assert_eq!(val["type"], "diff");
    assert_eq!(val["path"], "/work/a.ts");
    assert_eq!(val["oldText"], serde_json::Value::Null);
    assert_eq!(val["newText"], "a\n");

    // Client with diffPatch (not replay): live tool call shows no diff
    let shape_with_patch = tool_info_from_tool_use_facts("Write", &input, false, None, true);
    assert!(shape_with_patch.content.is_empty());

    // Replay with diffPatch: keeps the input diff
    let shape_replay = tool_info_from_tool_use_facts("Write", &input, true, None, true);
    assert_eq!(shape_replay.content.len(), 1);
    let val_replay = serde_json::to_value(&shape_replay.content[0]).unwrap();
    assert_eq!(val_replay["type"], "diff");
    assert_eq!(val_replay["oldText"], serde_json::Value::Null);
    assert_eq!(val_replay["newText"], "a\n");
}
