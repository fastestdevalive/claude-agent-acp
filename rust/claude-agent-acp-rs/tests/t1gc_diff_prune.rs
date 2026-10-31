//! Turn 1 fix round 2 (lane C): the line diff follows jsdiff's diagonal
//! pruning, absolute tool paths reach the OS unchanged, and the file read of a
//! patch cannot block on a named pipe.
//!
//! `t1gc_diff_prune_corpus.json` holds random and one-sided text pairs with the
//! hunks that the compiled upstream `structuredPatch` (diff 9.0.0, through the
//! adapter's prefix/suffix trim) returned for them.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use claude_agent_acp_rs::diff::{diff_hunks, preview_patch_content, PatchForm};
use serde_json::{json, Value};

static COUNTER: AtomicU64 = AtomicU64::new(1);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("t1gc-{prefix}-{}-{count}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        // Resolve symlinks of the temp root (macOS /var -> /private/var).
        let path = fs::canonicalize(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Run `work` on its own thread and fail instead of hanging when it blocks.
fn bounded<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx.recv_timeout(Duration::from_secs(60))
        .expect("the work did not finish in time")
}

#[test]
fn hunks_match_the_node_corpus() {
    let corpus: Vec<Value> = serde_json::from_str(include_str!("t1gc_diff_prune_corpus.json"))
        .expect("corpus is valid json");
    assert!(corpus.len() >= 1000, "corpus too small: {}", corpus.len());
    let failures = bounded(move || {
        let mut failures = Vec::new();
        for (index, case) in corpus.iter().enumerate() {
            let old = case["old"].as_str().unwrap();
            let new = case["new"].as_str().unwrap();
            let got = diff_hunks(Some(old), new).expect("in budget");
            let got = serde_json::to_value(got).unwrap();
            if got != case["hunks"] {
                failures.push(index);
            }
        }
        failures
    });
    assert!(failures.is_empty(), "hunks differ from Node: {failures:?}");
}

fn head_tail(n: usize) -> (String, String) {
    let small = "head\ntail\n".to_string();
    let mut big = String::from("head\n");
    for i in 0..n {
        big.push_str(&format!("new line {i}\n"));
    }
    big.push_str("tail\n");
    (small, big)
}

/// The one hunk that Node returns for `head\ntail\n` against `head\n` + n
/// lines + `tail\n` (`sign` is `+` for an insert, `-` for a delete).
fn expected_block_hunk(n: usize, sign: char) -> Value {
    let mut lines = vec![" head".to_string()];
    for i in 0..n {
        lines.push(format!("{sign}new line {i}"));
    }
    lines.push(" tail".to_string());
    let (old_lines, new_lines) = if sign == '+' { (2, n + 2) } else { (n + 2, 2) };
    json!([{
        "oldStart": 1, "oldLines": old_lines, "newStart": 1, "newLines": new_lines,
        "lines": lines,
    }])
}

fn check_block_change(n: usize) {
    let (small, big) = head_tail(n);
    let started = Instant::now();
    let (insert, delete) = bounded(move || {
        (
            diff_hunks(Some(&small), &big),
            diff_hunks(Some(&big), &small),
        )
    });
    // Node finishes these in milliseconds; a debug build gets a generous bound.
    assert!(started.elapsed() < Duration::from_secs(20));
    let insert = serde_json::to_value(insert.expect("insert in budget")).unwrap();
    let delete = serde_json::to_value(delete.expect("delete in budget")).unwrap();
    assert_eq!(insert, expected_block_hunk(n, '+'));
    assert_eq!(delete, expected_block_hunk(n, '-'));
}

#[test]
fn a_block_of_4200_lines_gets_the_exact_patch() {
    // The textbook search stops at its cap here (d = 4095); Node finishes in 3 ms.
    check_block_change(4200);
}

#[test]
fn a_block_of_20000_lines_gets_the_exact_patch() {
    check_block_change(20_000);
}

#[test]
fn a_dense_change_is_out_of_budget_not_a_hang() {
    // Every line differs on both sides: quadratic for any Myers search. Node
    // gives up after 100 ms and returns undefined; so does the port.
    let old: String = (0..20_000).map(|i| format!("old {i}\n")).collect();
    let new: String = (0..20_000).map(|i| format!("new {i}\n")).collect();
    let started = Instant::now();
    let got = bounded(move || diff_hunks(Some(&old), &new));
    assert!(got.is_none(), "a dense 20000-line change is out of budget");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn an_edit_inserting_a_large_block_has_a_preview() {
    let tmp = TempDir::new("edit-block");
    let file = tmp.path.join("big.txt");
    fs::write(&file, "head\ntail\n").unwrap();
    let (_, big) = head_tail(5000);
    let inserted = big.strip_prefix("head\n").unwrap().to_string();
    let input = json!({
        "file_path": file.to_string_lossy(),
        "old_string": "tail\n",
        "new_string": inserted,
    });
    let preview = bounded(move || preview_patch_content("Edit", &input, None, PatchForm::Air))
        .expect("an exact patch preview");
    let text = preview[0]["_meta"]["jetbrains"]["air"]["diffPatch"]["text"]
        .as_str()
        .expect("diffPatch text");
    assert!(
        text.contains("@@ -1,2 +1,5002 @@"),
        "hunk header in {}",
        &text[..200]
    );
}

#[cfg(unix)]
#[test]
fn an_absolute_path_with_dotdot_is_left_to_the_os() {
    let tmp = TempDir::new("symlink");
    let repo = tmp.path.join("repo");
    let elsewhere = tmp.path.join("elsewhere");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(elsewhere.join("dir")).unwrap();
    std::os::unix::fs::symlink(elsewhere.join("dir"), repo.join("link")).unwrap();
    // The folded path `repo/a.txt` and the OS path `elsewhere/a.txt` differ.
    fs::write(repo.join("a.txt"), "wrong\nold\n").unwrap();
    fs::write(elsewhere.join("a.txt"), "right\nold\n").unwrap();

    let raw = format!("{}/link/../a.txt", repo.display());
    let input = json!({"file_path": raw, "old_string": "old", "new_string": "new"});
    let preview = preview_patch_content("Edit", &input, Some(&repo), PatchForm::Air)
        .expect("the OS-resolved file has the old string");
    let text = preview[0]["_meta"]["jetbrains"]["air"]["diffPatch"]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains(" right\n"), "reads elsewhere/a.txt: {text}");
    assert!(!text.contains("wrong"), "must not read repo/a.txt: {text}");
    let name = raw.trim_start_matches('/');
    assert!(
        text.starts_with(&format!("diff --git a/{name} b/{name}\n")),
        "the header names the raw path: {text}"
    );

    // A relative path is still folded against the cwd, as path.resolve does.
    let input = json!({"file_path": "link/../a.txt", "old_string": "old", "new_string": "new"});
    let preview = preview_patch_content("Edit", &input, Some(&repo), PatchForm::Air)
        .expect("relative path resolves lexically");
    let text = preview[0]["_meta"]["jetbrains"]["air"]["diffPatch"]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains(" wrong\n"), "relative path folds: {text}");
}

#[cfg(unix)]
mod fifo {
    use super::*;
    use claude_agent_acp_rs::diff::{read_patch_source, PatchSource};
    use claude_agent_acp_rs::process::open_regular_file_nonblocking;

    fn mkfifo(path: &std::path::Path) {
        let made = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("run mkfifo");
        assert!(made.success());
    }

    #[test]
    fn the_open_helper_refuses_a_pipe_without_blocking() {
        let tmp = TempDir::new("open-fifo");
        let pipe = tmp.path.join("pipe");
        mkfifo(&pipe);
        let link = tmp.path.join("link");
        std::os::unix::fs::symlink(&pipe, &link).unwrap();
        let dir = tmp.path.clone();
        let regular = tmp.path.join("file.txt");
        fs::write(&regular, "x").unwrap();
        let missing = tmp.path.join("nope");

        let results = bounded(move || {
            (
                open_regular_file_nonblocking(&pipe)
                    .map(|_| ())
                    .unwrap_err()
                    .kind(),
                open_regular_file_nonblocking(&link)
                    .map(|_| ())
                    .unwrap_err()
                    .kind(),
                open_regular_file_nonblocking(&dir)
                    .map(|_| ())
                    .unwrap_err()
                    .kind(),
                open_regular_file_nonblocking(&regular).is_ok(),
                open_regular_file_nonblocking(&missing)
                    .map(|_| ())
                    .unwrap_err()
                    .kind(),
            )
        });
        use std::io::ErrorKind::{InvalidInput, NotFound};
        assert_eq!(
            results,
            (InvalidInput, InvalidInput, InvalidInput, true, NotFound)
        );
    }

    #[test]
    fn a_path_swapped_to_a_pipe_while_the_reader_runs_never_blocks() {
        // The path flips between a regular file and a pipe while many reads
        // run: whichever kind a read sees, none may wait for a writer.
        let tmp = TempDir::new("swap-fifo");
        let path = tmp.path.join("target");
        let spare = tmp.path.join("spare");
        mkfifo(&spare);
        fs::write(&path, "text\n").unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let swapper = {
            let (path, spare, stop) = (path.clone(), spare.clone(), stop.clone());
            let file_aside = tmp.path.join("file-aside");
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    // path (file) -> aside, spare (pipe) -> path, and back.
                    let _ = fs::rename(&path, &file_aside);
                    let _ = fs::rename(&spare, &path);
                    let _ = fs::rename(&path, &spare);
                    let _ = fs::rename(&file_aside, &path);
                }
            })
        };
        let read_path = path.clone();
        let kinds = bounded(move || {
            let mut seen = [0usize; 3];
            for _ in 0..3000 {
                match read_patch_source(&read_path) {
                    PatchSource::Missing => seen[0] += 1,
                    PatchSource::Invalid => seen[1] += 1,
                    PatchSource::Text(_) => seen[2] += 1,
                }
            }
            seen
        });
        stop.store(true, Ordering::Relaxed);
        swapper.join().unwrap();
        assert_eq!(kinds.iter().sum::<usize>(), 3000);
    }
}
