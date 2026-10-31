//! Turn 1 fix round (lane C): diff engine edge cases found by the review of the
//! first port: FIFO hang, resolved patch paths, `create` without `content`,
//! bounded search memory, and the PostToolUse final change per client.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use claude_agent_acp_rs::diff::{
    isolate_diff_memo_for_test, patch_update_from_diff_tool_response, peak_search_bytes,
    preview_patch_content, read_patch_source, reset_diff_stats, PatchForm, PatchSource,
};
use claude_agent_acp_rs::hooks::{final_change, post_tool_use_update};
use serde_json::{json, Value};

static COUNTER: AtomicU64 = AtomicU64::new(1);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("t1fc-{prefix}-{}-{count}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn patch_text(content: &[Value]) -> String {
    content[0]["_meta"]["jetbrains"]["air"]["diffPatch"]["text"]
        .as_str()
        .expect("expected diffPatch.text")
        .to_string()
}

fn git_name(path: &std::path::Path) -> String {
    path.to_string_lossy().trim_start_matches('/').to_string()
}

/// Run `work` on its own thread and fail instead of hanging when it blocks.
fn bounded<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx.recv_timeout(Duration::from_secs(30))
        .expect("the diff engine blocked (a named pipe must not be opened)")
}

#[test]
fn a_named_pipe_gives_no_preview_and_does_not_block() {
    let tmp = TempDir::new("fifo");
    let fifo = tmp.path.join("pipe");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(made.success());
    let fifo_str = fifo.to_string_lossy().to_string();

    let (source, edit, write, hook) = bounded(move || {
        let source = read_patch_source(&fifo);
        let edit = preview_patch_content(
            "Edit",
            &json!({"file_path": &fifo_str, "old_string": "a", "new_string": "b"}),
            None,
            PatchForm::Air,
        );
        let write = preview_patch_content(
            "Write",
            &json!({"file_path": &fifo_str, "content": "x\n"}),
            None,
            PatchForm::Air,
        );
        let hook = patch_update_from_diff_tool_response(
            &json!({"filePath": &fifo_str, "originalFile": "a\n"}),
            PatchForm::Air,
        );
        (source, edit, write, hook)
    });

    assert_eq!(source, PatchSource::Invalid);
    assert!(edit.is_none(), "an Edit on a pipe has no preview");
    // A Write on a path that cannot be read shows the overwrite notice.
    let write = write.expect("write notice");
    assert_eq!(write[0]["type"], "content");
    assert!(write[0]["content"]["text"]
        .as_str()
        .unwrap()
        .starts_with("Overwrites the existing file `"));
    assert!(hook.is_none());
}

#[test]
fn patch_content_names_the_resolved_path_not_the_raw_one() {
    let tmp = TempDir::new("resolved");
    fs::write(tmp.path.join("a.txt"), "one\ntwo\n").unwrap();
    let resolved = tmp.path.join("a.txt");
    let name = git_name(&resolved);

    let edit = preview_patch_content(
        "Edit",
        &json!({"file_path": "sub/../a.txt", "old_string": "two", "new_string": "2"}),
        Some(&tmp.path),
        PatchForm::Air,
    )
    .expect("edit preview");
    assert_eq!(edit[0]["path"], resolved.to_string_lossy().as_ref());
    assert!(patch_text(&edit).starts_with(&format!("diff --git a/{name} b/{name}\n")));

    let write = preview_patch_content(
        "Write",
        &json!({"file_path": "./a.txt", "content": "one\n2\n"}),
        Some(&tmp.path),
        PatchForm::Air,
    )
    .expect("write preview");
    assert_eq!(write[0]["path"], resolved.to_string_lossy().as_ref());
    assert!(patch_text(&write).contains(&format!("+++ b/{name}\n")));

    // The v2 standard diff names the resolved file too; the v1 standard diff and
    // the overwrite notice keep the path of the input (upstream `write.file_path`).
    let missing = preview_patch_content(
        "Write",
        &json!({"file_path": "new/../b.txt", "content": "x\r\n"}),
        Some(&tmp.path),
        PatchForm::Air,
    )
    .expect("standard diff");
    assert_eq!(missing[0]["path"], "new/../b.txt");
}

#[test]
fn a_create_response_without_content_has_no_patch() {
    let tmp = TempDir::new("create");
    let file = tmp.path.join("created.txt");
    fs::write(&file, "hello\n").unwrap();
    let file_str = file.to_string_lossy().to_string();

    // Upstream: `response.type === "create" && newText !== response.content`
    // declines when `content` is absent.
    assert!(patch_update_from_diff_tool_response(
        &json!({"type": "create", "filePath": &file_str}),
        PatchForm::Air
    )
    .is_none());
    assert!(patch_update_from_diff_tool_response(
        &json!({"type": "create", "filePath": &file_str, "content": "other\n"}),
        PatchForm::Air
    )
    .is_none());
    let ok = patch_update_from_diff_tool_response(
        &json!({"type": "create", "filePath": &file_str, "content": "hello\n"}),
        PatchForm::Air,
    )
    .expect("a matching create");
    assert!(patch_text(&ok.content).contains("new file mode 100644\n"));
    assert_eq!(ok.locations.len(), 1);
    assert_eq!(ok.locations[0].line, None);
}

#[test]
fn full_rewrite_of_a_large_file_is_bounded_and_falls_back_like_a_timeout() {
    isolate_diff_memo_for_test();
    reset_diff_stats();
    let tmp = TempDir::new("rewrite");
    // About 840 KB per side (under the 1 MiB limit), no line in common.
    let before: String = (0..60_000).map(|i| format!("old {i:09}\n")).collect();
    let after: String = (0..60_000).map(|i| format!("new {i:09}\n")).collect();
    assert!(before.len() < 1024 * 1024);
    let file = tmp.path.join("big.txt");
    fs::write(&file, &before).unwrap();
    let file_str = file.to_string_lossy().to_string();

    let started = Instant::now();
    let write = preview_patch_content(
        "Write",
        &json!({"file_path": &file_str, "content": &after}),
        None,
        PatchForm::Air,
    )
    .expect("a Write always shows its change");
    // Out of its budget, the approval shows the standard diff (upstream test
    // "falls back to the standard Write diff when the patch budget expires").
    assert_eq!(
        write,
        vec![json!({"type": "diff", "path": &file_str, "oldText": &before, "newText": &after})]
    );
    // An Edit has no standard diff to fall back to: no preview.
    let edit = preview_patch_content(
        "Edit",
        &json!({"file_path": &file_str, "old_string": &before, "new_string": &after}),
        None,
        PatchForm::Air,
    );
    assert!(edit.is_none());
    // The hook repeats the diff of the written file and is just as bounded.
    fs::write(&file, &after).unwrap();
    let hook = patch_update_from_diff_tool_response(
        &json!({"filePath": &file_str, "originalFile": &before}),
        PatchForm::Air,
    );
    assert!(hook.is_none());
    assert!(started.elapsed() < Duration::from_secs(10));

    // The search keeps one 12-byte node per move, capped at 32 MiB. The earlier
    // design copied a 2 * (n + m) vector of 8-byte cells every step (about 1.9 MB
    // per step here), which reached hundreds of MB inside the same budget.
    let peak = peak_search_bytes();
    eprintln!("peak search state: {peak} bytes");
    assert!(peak > 0, "the search ran");
    assert!(peak <= 32 * 1024 * 1024, "peak search state {peak} bytes");
}

#[test]
fn a_dense_but_small_change_still_completes_with_exact_hunks() {
    isolate_diff_memo_for_test();
    let tmp = TempDir::new("dense");
    let before: String = (0..3000).map(|i| format!("line {i}\n")).collect();
    // Every 10th line changes: many edits, but well inside the budget.
    let after: String = (0..3000)
        .map(|i| {
            if i % 10 == 0 {
                format!("changed {i}\n")
            } else {
                format!("line {i}\n")
            }
        })
        .collect();
    let file = tmp.path.join("dense.txt");
    fs::write(&file, &before).unwrap();
    let preview = preview_patch_content(
        "Write",
        &json!({"file_path": file.to_string_lossy(), "content": &after}),
        None,
        PatchForm::Air,
    )
    .expect("preview");
    let text = patch_text(&preview);
    assert_eq!(text.matches("\n-line ").count(), 300);
    assert_eq!(text.matches("\n+changed ").count(), 300);
    // Nine unchanged lines between changes keep the hunks apart.
    assert_eq!(text.matches("\n@@ ").count(), 300);
    assert!(
        text.contains("\n@@ -1,4 +1,4 @@\n-line 0\n+changed 0\n line 1\n"),
        "{text}"
    );
}

#[test]
fn final_change_per_client_for_a_written_file() {
    let tmp = TempDir::new("final");
    let file = tmp.path.join("a.txt");
    fs::write(&file, "one\n2\nthree\n").unwrap();
    let file_str = file.to_string_lossy().to_string();
    let response = json!({
        "type": "update",
        "filePath": &file_str,
        "content": "one\n2\nthree\n",
        "originalFile": "one\ntwo\nthree\n",
        "structuredPatch": [{
            "oldStart": 1, "oldLines": 3, "newStart": 1, "newLines": 3,
            "lines": [" one", "-two", "+2", " three"]
        }],
    });

    // A diffPatch client gets the exact git patch of the written file.
    let (content, locations) = final_change(&response, true);
    assert_eq!(content[0]["type"], "diff");
    assert_eq!(content[0]["oldText"], Value::Null);
    assert_eq!(content[0]["newText"], "");
    assert!(patch_text(&content).contains("-two\n+2\n three\n"));
    assert_eq!(locations[0]["line"], 1);

    // Any other client keeps the structuredPatch diff.
    let (content, locations) = final_change(&response, false);
    assert_eq!(content[0]["type"], "diff");
    assert_eq!(content[0]["oldText"], "one\ntwo\nthree");
    assert_eq!(content[0]["newText"], "one\n2\nthree");
    assert!(content[0].get("_meta").is_none());
    assert_eq!(locations[0]["line"], 1);

    // The update carries it.
    // A plain client's update repeats the whole tool_response...
    let update = post_tool_use_update("t1", "Write", None, &response, false, false).unwrap();
    assert_eq!(update["_meta"]["claudeCode"]["toolResponse"], response);
    // ...an AIR client's carries the patch and no tool_response.
    let update = post_tool_use_update("t1", "Write", None, &response, true, true).unwrap();
    assert!(patch_text(update["content"].as_array().unwrap()).contains("-two\n+2\n"));
    assert_eq!(update["_meta"]["claudeCode"]["toolName"], "Write");
    assert!(update["_meta"]["claudeCode"].get("toolResponse").is_none());
}

#[test]
fn an_air_hook_update_carries_markers_or_a_change_or_nothing() {
    // No marker and no change: nothing to report (`hookResult` returns undefined).
    let bash = json!({"stdout": "hi", "stderr": ""});
    assert!(post_tool_use_update("t1", "Bash", None, &bash, false, true).is_none());
    // The markers alone are reported.
    let bg = json!({"stdout": "", "status": "completed", "isAsync": true});
    let update = post_tool_use_update("t1", "Bash", None, &bg, false, true).unwrap();
    assert_eq!(
        update["_meta"]["claudeCode"]["toolResponse"],
        json!({"status": "completed", "isAsync": true})
    );
    // Any other client gets the tool_response whole, whatever it holds.
    let update = post_tool_use_update("t1", "Bash", None, &bash, false, false).unwrap();
    assert_eq!(update["_meta"]["claudeCode"]["toolResponse"], bash);
}

#[test]
fn final_change_falls_back_when_no_exact_patch_exists() {
    let tmp = TempDir::new("fallback");
    let missing = tmp.path.join("gone.txt");
    let missing_str = missing.to_string_lossy().to_string();

    // A created file that cannot be read back: the standard diff of the created
    // file (the diffPatch client's Write tool call shows no diff), then nothing
    // more for a plain client (the optimistic creation diff stands).
    let created = json!({"type": "create", "filePath": &missing_str, "content": "new\n",
                         "structuredPatch": []});
    let (content, locations) = final_change(&created, true);
    assert_eq!(
        content,
        vec![json!({"type": "diff", "path": &missing_str, "oldText": null, "newText": "new\n"})]
    );
    assert_eq!(locations, vec![json!({"path": &missing_str})]);
    let (content, locations) = final_change(&created, false);
    assert!(content.is_empty() && locations.is_empty());

    // An update whose structuredPatch is empty: the whole-file diff / the
    // "too large" note, for both client kinds.
    let update = json!({"type": "update", "filePath": &missing_str, "content": "n\n",
                        "originalFile": "o\n", "structuredPatch": []});
    for diff_patch in [true, false] {
        let (content, locations) = final_change(&update, diff_patch);
        assert_eq!(
            content,
            vec![json!({"type": "diff", "path": &missing_str, "oldText": "o\n", "newText": "n\n"})]
        );
        assert_eq!(locations, vec![json!({"path": &missing_str})]);
    }
    let too_large = json!({"type": "update", "filePath": &missing_str, "content": "n\n",
                           "originalFile": null, "structuredPatch": []});
    let (content, _) = final_change(&too_large, true);
    assert_eq!(content[0]["type"], "content");
    assert_eq!(
        content[0]["content"]["text"],
        format!("Updated `{missing_str}` (previous content too large to diff)")
    );
}
