//! The four `diff-memo.test.ts` cases through the PROCESS-WIDE memo, which is
//! the one production uses (the session actor can run on any runtime thread).
//! `t1c_diff_tests.rs` runs the same cases against a per-thread memo so that
//! parallel tests cannot evict each other's entries. This binary holds a single
//! test, so nothing else touches the shared memo.

use std::fs;
use std::time::Duration;

use claude_agent_acp_rs::diff::{
    clear_diff_memo, diff_call_count, patch_update_from_diff_tool_response, preview_patch_content,
    reset_diff_stats, set_diff_budget_for_test, PatchForm,
};
use serde_json::{json, Value};

fn patch_text(content: &[Value]) -> String {
    content[0]["_meta"]["jetbrains"]["air"]["diffPatch"]["text"]
        .as_str()
        .expect("diffPatch.text")
        .to_string()
}

#[test]
fn process_wide_memo_matches_the_upstream_call_counts() {
    let dir = std::env::temp_dir().join(format!("t1fc-memo-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("file.txt");
    let file_str = file.to_string_lossy().to_string();
    let before: String = (0..2000).map(|i| format!("line {i}\n")).collect();
    let after = before.replace("line 1000\n", "line one thousand\n");
    let edit = json!({"file_path": &file_str, "old_string": "line 1000\n",
                      "new_string": "line one thousand\n"});

    // 1. The hook reuses the approval's diff.
    clear_diff_memo();
    reset_diff_stats();
    fs::write(&file, &before).unwrap();
    let preview = preview_patch_content("Edit", &edit, None, PatchForm::Air).expect("preview");
    assert_eq!(diff_call_count(), 1);
    fs::write(&file, &after).unwrap();
    let hook = patch_update_from_diff_tool_response(
        &json!({"filePath": &file_str, "originalFile": &before, "oldString": "line 1000\n"}),
        PatchForm::Air,
    )
    .expect("hook");
    assert_eq!(diff_call_count(), 1);
    assert_eq!(patch_text(&hook.content), patch_text(&preview));
    assert_eq!(hook.locations[0].line, Some(998));

    // 2. A written file that differs from the preview is diffed again.
    clear_diff_memo();
    reset_diff_stats();
    fs::write(&file, &before).unwrap();
    preview_patch_content("Edit", &edit, None, PatchForm::Air).expect("preview");
    fs::write(
        &file,
        before.replace("line 1000\n", "line 1000 changed later\n"),
    )
    .unwrap();
    let hook = patch_update_from_diff_tool_response(
        &json!({"filePath": &file_str, "originalFile": &before}),
        PatchForm::Air,
    )
    .expect("hook");
    assert_eq!(diff_call_count(), 2);
    assert!(patch_text(&hook.content).contains("+line 1000 changed later\n"));

    // 3. A diff that ran out of its budget is not remembered.
    clear_diff_memo();
    reset_diff_stats();
    fs::write(&file, &before).unwrap();
    let write = json!({"file_path": &file_str, "content": &after});
    set_diff_budget_for_test(Some(Duration::ZERO));
    let first = preview_patch_content("Write", &write, None, PatchForm::Air).expect("first");
    set_diff_budget_for_test(None);
    let second = preview_patch_content("Write", &write, None, PatchForm::Air).expect("second");
    assert_eq!(diff_call_count(), 2);
    assert_eq!(
        first,
        vec![json!({"type": "diff", "path": &file_str, "oldText": &before, "newText": &after})]
    );
    assert!(patch_text(&second).contains("+line one thousand\n"));

    // 4. Only a few recent diffs are kept, each until it is read again.
    clear_diff_memo();
    reset_diff_stats();
    let write_tail = |i: usize| {
        preview_patch_content(
            "Write",
            &json!({"file_path": &file_str, "content": format!("{before}tail {i}\n")}),
            None,
            PatchForm::Air,
        )
        .expect("preview")
    };
    for i in 0..6 {
        write_tail(i);
    }
    assert_eq!(diff_call_count(), 6);
    write_tail(5);
    assert_eq!(diff_call_count(), 6);
    write_tail(0);
    assert_eq!(diff_call_count(), 7);
    write_tail(5);
    assert_eq!(diff_call_count(), 8);

    let _ = fs::remove_dir_all(&dir);
}
