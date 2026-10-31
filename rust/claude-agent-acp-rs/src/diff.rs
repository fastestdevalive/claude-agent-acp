//! Line-diff engine for Edit/Write previews and PostToolUse patches.
//! Pinned to upstream v0.94.0 `src/diff.ts`.

use std::cell::Cell;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v1::ToolCallLocation;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use crate::hooks::tool_update_from_diff_tool_response;

/// The largest file, in bytes, that the adapter turns into a git patch (1 MiB).
pub const MAX_PATCH_FILE_BYTES: usize = 1024 * 1024;

/// The wall-clock budget of one line diff for a patch (100 ms).
const PATCH_DIFF_TIMEOUT_MS: u64 = 100;

/// The number of context lines around a change in a hunk.
const PATCH_CONTEXT_LINES: usize = 3;

/// Git reads this many leading bytes to decide that a file is binary.
const BINARY_SNIFF_BYTES: usize = 8000;

pub const NO_NEWLINE_MARKER: &str = "\\ No newline at end of file";

/// The most path nodes that one line diff keeps. The 100 ms budget bounds it
/// in practice; the cap makes the bound hold on any machine, and a search that
/// reaches it is out of budget. Only a dense change reaches it: a one-sided
/// change of any size is linear (diagonal pruning).
const MAX_TRACE_NODES: usize = 2_500_000;

/// A step may finish its diagonals after the cap is passed; the allocation is
/// bounded by the cap plus this slack, and a push beyond it gives up.
const TRACE_SLACK_NODES: usize = 40_000;

/// The node list is allocated for exactly [`MAX_TRACE_NODES`] plus the slack
/// at most: 12 B each is 30.5 MB (29.1 MiB), not the 48 MiB a doubling
/// `Vec` reaches. It grows by at most this many nodes per reallocation (3 MiB),
/// so the old and new buffers together stay within the bound plus 3 MiB.
const TRACE_GROWTH_NODES: usize = 262_144;

/// The number of recent line diffs that [`memoized_diff_hunks`] keeps.
const DIFF_MEMO_SIZE: usize = 4;

/// One unified-diff hunk in the `diff` package convention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchHunk {
    #[serde(rename = "oldStart")]
    pub old_start: u32,
    #[serde(rename = "oldLines")]
    pub old_lines: u32,
    #[serde(rename = "newStart")]
    pub new_start: u32,
    #[serde(rename = "newLines")]
    pub new_lines: u32,
    pub lines: Vec<String>,
}

/// The kind of file change that a git patch describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChange {
    Create,
    Update,
}

/// A file change and the hunks of its git patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    pub change: FileChange,
    pub hunks: Vec<PatchHunk>,
}

/// The diff that carries an exact patch: AIR's `diffPatch` extension or an ACP v2 diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchForm {
    Air,
    V2,
}

/// How a git patch names its file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitPatchPaths {
    Git,
    Absolute,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PatchUpdateResult {
    pub content: Vec<Value>,
    pub locations: Vec<ToolCallLocation>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PatchSource {
    Missing,
    Invalid,
    Text(String),
}

struct MemoEntry {
    old_text: Option<String>,
    new_text: String,
    hunks: Vec<PatchHunk>,
}

enum MemoOp {
    GetAndRemove {
        old_text: Option<String>,
        new_text: String,
        reply: std::sync::mpsc::Sender<Option<Vec<PatchHunk>>>,
    },
    Record {
        old_text: Option<String>,
        new_text: String,
        hunks: Vec<PatchHunk>,
    },
    Clear,
}

fn memo_sender() -> &'static std::sync::mpsc::Sender<MemoOp> {
    static TX: OnceLock<std::sync::mpsc::Sender<MemoOp>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<MemoOp>();
        let _ = std::thread::Builder::new()
            .name("diff-memo".to_string())
            .spawn(move || {
                let mut memo = DiffMemo::default();
                while let Ok(op) = rx.recv() {
                    match op {
                        MemoOp::GetAndRemove {
                            old_text,
                            new_text,
                            reply,
                        } => {
                            let _ = reply.send(memo.take(old_text.as_deref(), &new_text));
                        }
                        MemoOp::Record {
                            old_text,
                            new_text,
                            hunks,
                        } => memo.record(old_text, new_text, hunks),
                        MemoOp::Clear => memo.entries.clear(),
                    }
                }
            });
        tx
    })
}

/// The recent line diffs, the most recent last (upstream `diffMemo`).
#[derive(Default)]
struct DiffMemo {
    entries: Vec<MemoEntry>,
}

impl DiffMemo {
    /// The hunks of a remembered pair, which then leaves the memo: the hook is
    /// normally the last reader of a pair.
    fn take(&mut self, old_text: Option<&str>, new_text: &str) -> Option<Vec<PatchHunk>> {
        let index = self
            .entries
            .iter()
            .position(|e| e.old_text.as_deref() == old_text && e.new_text == new_text)?;
        Some(self.entries.remove(index).hunks)
    }

    fn record(&mut self, old_text: Option<String>, new_text: String, hunks: Vec<PatchHunk>) {
        self.entries.push(MemoEntry {
            old_text,
            new_text,
            hunks,
        });
        if self.entries.len() > DIFF_MEMO_SIZE {
            self.entries.remove(0);
        }
    }
}

thread_local! {
    /// A memo private to this thread, for tests: the process-wide memo is shared
    /// by tests that run in parallel, which would evict each other's entries.
    static LOCAL_MEMO: std::cell::RefCell<Option<DiffMemo>> = const { std::cell::RefCell::new(None) };
}

/// Give this thread its own empty memo. For tests.
pub fn isolate_diff_memo_for_test() {
    LOCAL_MEMO.with(|m| *m.borrow_mut() = Some(DiffMemo::default()));
}

/// Forget the remembered line diffs. For tests.
pub fn clear_diff_memo() {
    if LOCAL_MEMO.with(|m| {
        m.borrow_mut()
            .as_mut()
            .map(|memo| memo.entries.clear())
            .is_some()
    }) {
        return;
    }
    let _ = memo_sender().send(MemoOp::Clear);
}

thread_local! {
    /// Line diffs run on this thread (upstream's `structuredPatch` spy). The
    /// engine is synchronous, so a test reads the count of its own thread.
    static DIFF_CALLS: Cell<usize> = const { Cell::new(0) };
    /// A test override of the line-diff budget on this thread.
    static BUDGET_OVERRIDE: Cell<Option<Duration>> = const { Cell::new(None) };
    /// The most path-node capacity (allocated nodes, not used ones) a Myers
    /// search held on this thread.
    static TRACE_CELLS: Cell<usize> = const { Cell::new(0) };
}

/// The number of line diffs run on this thread since [`reset_diff_stats`]. For tests.
pub fn diff_call_count() -> usize {
    DIFF_CALLS.with(Cell::get)
}

/// Reset the per-thread diff counters and the budget override. For tests.
pub fn reset_diff_stats() {
    DIFF_CALLS.with(|c| c.set(0));
    BUDGET_OVERRIDE.with(|c| c.set(None));
    TRACE_CELLS.with(|c| c.set(0));
}

/// Override the line-diff budget on this thread (`None` restores 100 ms). A
/// zero budget makes every diff run out of its time, like a dense change on a
/// large file. For tests.
pub fn set_diff_budget_for_test(budget: Option<Duration>) {
    BUDGET_OVERRIDE.with(|c| c.set(budget));
}

/// The bytes of search state (allocated, not used) the largest Myers search on
/// this thread held since [`reset_diff_stats`]. For tests.
pub fn peak_search_bytes() -> usize {
    TRACE_CELLS.with(Cell::get) * std::mem::size_of::<PathNode>()
}

/// [`diff_hunks`] of a recent pair of texts again, without a second diff.
///
/// The approval of an Edit or Write diffs the file against the predicted text,
/// and the PostToolUse hook diffs the same texts once Claude wrote them. Only a
/// diff that completed is remembered: one that ran out of its budget says
/// nothing about the texts, so a later call diffs them again.
pub fn memoized_diff_hunks(old_text: Option<&str>, new_text: &str) -> Option<Vec<PatchHunk>> {
    let local = LOCAL_MEMO.with(|m| m.borrow().is_some());
    let cached = if local {
        LOCAL_MEMO.with(|m| {
            m.borrow_mut()
                .as_mut()
                .and_then(|memo| memo.take(old_text, new_text))
        })
    } else {
        let (reply, rx) = std::sync::mpsc::channel();
        let sent = memo_sender().send(MemoOp::GetAndRemove {
            old_text: old_text.map(String::from),
            new_text: new_text.to_string(),
            reply,
        });
        if sent.is_ok() {
            rx.recv().ok().flatten()
        } else {
            None
        }
    };
    if cached.is_some() {
        return cached;
    }

    let hunks = diff_hunks(old_text, new_text)?;
    if local {
        LOCAL_MEMO.with(|m| {
            if let Some(memo) = m.borrow_mut().as_mut() {
                memo.record(
                    old_text.map(String::from),
                    new_text.to_string(),
                    hunks.clone(),
                );
            }
        });
    } else {
        let _ = memo_sender().send(MemoOp::Record {
            old_text: old_text.map(String::from),
            new_text: new_text.to_string(),
            hunks: hunks.clone(),
        });
    }
    Some(hunks)
}

/// Whether a tool input text can be the new side of an exact patch.
pub fn is_patchable_text(text: &str) -> bool {
    text.len() <= MAX_PATCH_FILE_BYTES && !text.contains('\0') && !text.contains('\r')
}

/// The lines of a text, each with its line break. The last line can have none.
pub fn text_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            lines.push(text[start..=i].to_string());
            start = i + 1;
        }
    }
    if start < bytes.len() {
        lines.push(text[start..].to_string());
    }
    lines
}

/// The hunk that adds every line of `text` to an empty file.
pub fn whole_file_hunk(text: &str) -> Option<PatchHunk> {
    if text.is_empty() {
        return None;
    }
    let terminated = text.ends_with('\n');
    let content = if terminated {
        &text[..text.len() - 1]
    } else {
        text
    };
    let raw_lines: Vec<&str> = content.split('\n').collect();
    let mut lines: Vec<String> = raw_lines.iter().map(|l| format!("+{l}")).collect();
    if !terminated {
        lines.push(NO_NEWLINE_MARKER.to_string());
    }
    Some(PatchHunk {
        old_start: 1,
        old_lines: 0,
        new_start: 1,
        new_lines: raw_lines.len() as u32,
        lines,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum DiffOp {
    Equal(String),
    Delete(String),
    Insert(String),
}

fn myers_diff(
    old_tokens: &[String],
    new_tokens: &[String],
    timeout: Duration,
) -> Option<Vec<DiffOp>> {
    let start_time = Instant::now();
    let old_len = old_tokens.len() as isize;
    let new_len = new_tokens.len() as isize;
    let max_edit_length = old_len + new_len;
    // `best[diagonal + offset]` is jsdiff's `bestPath[diagonal]`, split in two
    // arrays that start as zero pages: the old position plus 2 (0 is
    // `undefined`, since the position starts at -1) and the path's last node.
    let offset = max_edit_length + 1;
    let slots = 2 * max_edit_length as usize + 3;
    let mut best_pos = vec![0u32; slots];
    let mut best_node = vec![NO_NODE; slots];
    // The path is a persistent list of nodes, like jsdiff's `lastComponent`.
    let mut nodes: Vec<PathNode> = Vec::new();

    // Seed edit length 0: the content starts with the same lines.
    let (seed_old_pos, seed_new_pos, seed_common) = extract_common(old_tokens, new_tokens, -1, 0);
    let seeded = push_node(
        &mut nodes,
        PathNode {
            prev: NO_NODE,
            kind: PathKind::Start,
            common: seed_common,
        },
    );
    debug_assert!(seeded);
    if seed_old_pos + 1 >= old_len && seed_new_pos + 1 >= new_len {
        return Some(build_ops(&nodes, 0, old_tokens, new_tokens));
    }
    best_pos[offset as usize] = (seed_old_pos + 2) as u32;
    best_node[offset as usize] = 0;

    // Once a diagonal reaches the right edge of the edit graph, no move to a
    // higher diagonal can finish sooner; once one reaches the bottom edge, no
    // move to a lower one can. jsdiff records that as these two bounds and
    // never visits a diagonal outside them, so a one-sided change costs
    // O(n + d) instead of the O(d^2) of the textbook search.
    let mut min_diagonal = isize::MIN;
    let mut max_diagonal = isize::MAX;

    let mut edit_length = 1isize;
    while edit_length <= max_edit_length {
        if (edit_length - 1) % 16 == 0 && start_time.elapsed() >= timeout {
            TRACE_CELLS.with(|c| c.set(c.get().max(nodes.capacity())));
            return None;
        }
        let lowest = min_diagonal.max(-edit_length);
        let highest = max_diagonal.min(edit_length);
        let mut diagonal = lowest;
        while diagonal <= highest {
            let at = (diagonal + offset) as usize;
            // The remove path comes from diagonal - 1 (no one else uses it
            // afterwards), the add path from diagonal + 1.
            let remove_pos = best_pos[at - 1];
            let remove_node = best_node[at - 1];
            best_pos[at - 1] = 0;
            let add_pos = best_pos[at + 1];
            let add_node = best_node[at + 1];

            let can_add = add_pos != 0 && {
                let add_new_pos = (add_pos as isize - 2) - diagonal;
                0 <= add_new_pos && add_new_pos < new_len
            };
            let can_remove = remove_pos != 0 && (remove_pos as isize - 2) + 1 < old_len;
            if !can_add && !can_remove {
                // A terminal path is dropped.
                best_pos[at] = 0;
                diagonal += 2;
                continue;
            }

            // Branch from the prior path that is furthest in the old text and
            // still inside the graph.
            let (base_old_pos, base_node, kind) =
                if !can_remove || (can_add && remove_pos < add_pos) {
                    (add_pos as isize - 2, add_node, PathKind::Insert)
                } else {
                    (remove_pos as isize - 2 + 1, remove_node, PathKind::Delete)
                };
            let (old_pos, new_pos, common) =
                extract_common(old_tokens, new_tokens, base_old_pos, diagonal);
            let node = nodes.len() as u32;
            let pushed = push_node(
                &mut nodes,
                PathNode {
                    prev: base_node,
                    kind,
                    common,
                },
            );
            if !pushed {
                TRACE_CELLS.with(|c| c.set(c.get().max(nodes.capacity())));
                return None;
            }
            if old_pos + 1 >= old_len && new_pos + 1 >= new_len {
                TRACE_CELLS.with(|c| c.set(c.get().max(nodes.capacity())));
                return Some(build_ops(&nodes, node, old_tokens, new_tokens));
            }
            best_pos[at] = (old_pos + 2) as u32;
            best_node[at] = node;
            if old_pos + 1 >= old_len {
                max_diagonal = max_diagonal.min(diagonal - 1);
            }
            if new_pos + 1 >= new_len {
                min_diagonal = min_diagonal.max(diagonal + 1);
            }
            diagonal += 2;
        }
        if nodes.len() > MAX_TRACE_NODES {
            TRACE_CELLS.with(|c| c.set(c.get().max(nodes.capacity())));
            return None;
        }
        edit_length += 1;
    }
    None
}

/// Append a node without ever allocating past
/// `MAX_TRACE_NODES + TRACE_SLACK_NODES` nodes; `false` when that limit is
/// reached. Growth is by `reserve_exact`, doubling up to
/// [`TRACE_GROWTH_NODES`] and then linear.
fn push_node(nodes: &mut Vec<PathNode>, node: PathNode) -> bool {
    let limit = MAX_TRACE_NODES + TRACE_SLACK_NODES;
    if nodes.len() >= limit {
        return false;
    }
    if nodes.len() == nodes.capacity() {
        let step = nodes.len().clamp(16, TRACE_GROWTH_NODES);
        nodes.reserve_exact(step.min(limit - nodes.len()));
    }
    nodes.push(node);
    true
}

/// The last move of a path in [`myers_diff`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    Start,
    Insert,
    Delete,
}

/// One move of a path (an insert or a delete) and the equal lines that follow
/// it, linked to the move before it.
struct PathNode {
    prev: u32,
    kind: PathKind,
    common: u32,
}

const NO_NODE: u32 = u32::MAX;

/// jsdiff `extractCommon`: from `old_pos` (the last consumed old line) on
/// `diagonal`, the lines both texts share. Returns the new old position, the
/// new position and the number of shared lines.
fn extract_common(
    old_tokens: &[String],
    new_tokens: &[String],
    old_pos: isize,
    diagonal: isize,
) -> (isize, isize, u32) {
    let mut old_pos = old_pos;
    let mut new_pos = old_pos - diagonal;
    let mut common = 0u32;
    while ((new_pos + 1) as usize) < new_tokens.len()
        && ((old_pos + 1) as usize) < old_tokens.len()
        && old_tokens[(old_pos + 1) as usize] == new_tokens[(new_pos + 1) as usize]
    {
        new_pos += 1;
        old_pos += 1;
        common += 1;
    }
    (old_pos, new_pos, common)
}

/// jsdiff `buildValues`: the operations of the path that ends at `last`.
fn build_ops(
    nodes: &[PathNode],
    last: u32,
    old_tokens: &[String],
    new_tokens: &[String],
) -> Vec<DiffOp> {
    let mut chain = Vec::new();
    let mut at = last;
    while at != NO_NODE {
        chain.push(at);
        at = nodes[at as usize].prev;
    }
    let mut ops = Vec::new();
    let mut old_at = 0usize;
    let mut new_at = 0usize;
    for &index in chain.iter().rev() {
        let node = &nodes[index as usize];
        match node.kind {
            PathKind::Start => {}
            PathKind::Insert => {
                ops.push(DiffOp::Insert(new_tokens[new_at].clone()));
                new_at += 1;
            }
            PathKind::Delete => {
                ops.push(DiffOp::Delete(old_tokens[old_at].clone()));
                old_at += 1;
            }
        }
        for _ in 0..node.common {
            ops.push(DiffOp::Equal(old_tokens[old_at].clone()));
            old_at += 1;
            new_at += 1;
        }
    }
    ops
}

struct DiffItem {
    lines: Vec<String>,
    added: bool,
    removed: bool,
}

fn structured_patch_hunks(ops: Vec<DiffOp>) -> Vec<PatchHunk> {
    let mut items: Vec<DiffItem> = Vec::new();
    for op in ops {
        match op {
            DiffOp::Equal(line) => {
                if let Some(last) = items.last_mut() {
                    if !last.added && !last.removed {
                        last.lines.push(line);
                        continue;
                    }
                }
                items.push(DiffItem {
                    lines: vec![line],
                    added: false,
                    removed: false,
                });
            }
            DiffOp::Delete(line) => {
                if let Some(last) = items.last_mut() {
                    if !last.added && last.removed {
                        last.lines.push(line);
                        continue;
                    }
                }
                items.push(DiffItem {
                    lines: vec![line],
                    added: false,
                    removed: true,
                });
            }
            DiffOp::Insert(line) => {
                if let Some(last) = items.last_mut() {
                    if last.added && !last.removed {
                        last.lines.push(line);
                        continue;
                    }
                }
                items.push(DiffItem {
                    lines: vec![line],
                    added: true,
                    removed: false,
                });
            }
        }
    }

    items.push(DiffItem {
        lines: Vec::new(),
        added: false,
        removed: false,
    });

    let context = PATCH_CONTEXT_LINES;
    let mut hunks = Vec::new();
    let mut old_range_start = 0u32;
    let mut new_range_start = 0u32;
    let mut cur_range = Vec::new();
    let mut old_line = 1u32;
    let mut new_line = 1u32;

    for i in 0..items.len() {
        let current = &items[i];
        let lines = &current.lines;
        if current.added || current.removed {
            if old_range_start == 0 {
                old_range_start = old_line;
                new_range_start = new_line;
                if i > 0 {
                    let prev = &items[i - 1];
                    let take = prev.lines.len().min(context);
                    let ctx_lines = &prev.lines[prev.lines.len() - take..];
                    for l in ctx_lines {
                        cur_range.push(format!(" {l}"));
                    }
                    old_range_start -= take as u32;
                    new_range_start -= take as u32;
                }
            }
            let prefix = if current.added { '+' } else { '-' };
            for l in lines {
                cur_range.push(format!("{prefix}{l}"));
            }
            if current.added {
                new_line += lines.len() as u32;
            } else {
                old_line += lines.len() as u32;
            }
        } else if old_range_start != 0 {
            if lines.len() <= context * 2 && i < items.len() - 2 {
                for l in lines {
                    cur_range.push(format!(" {l}"));
                }
            } else {
                let context_size = lines.len().min(context);
                for l in &lines[..context_size] {
                    cur_range.push(format!(" {l}"));
                }
                let hunk = PatchHunk {
                    old_start: old_range_start,
                    old_lines: old_line - old_range_start + context_size as u32,
                    new_start: new_range_start,
                    new_lines: new_line - new_range_start + context_size as u32,
                    lines: std::mem::take(&mut cur_range),
                };
                hunks.push(hunk);
                old_range_start = 0;
                new_range_start = 0;
            }
            old_line += lines.len() as u32;
            new_line += lines.len() as u32;
        } else {
            old_line += lines.len() as u32;
            new_line += lines.len() as u32;
        }
    }

    for hunk in &mut hunks {
        let mut i = 0;
        while i < hunk.lines.len() {
            if let Some(stripped) = hunk.lines[i].strip_suffix('\n') {
                hunk.lines[i] = stripped.to_string();
                i += 1;
            } else {
                hunk.lines.insert(i + 1, NO_NEWLINE_MARKER.to_string());
                i += 2;
            }
        }
    }

    hunks
}

pub fn diff_hunks(old_text: Option<&str>, new_text: &str) -> Option<Vec<PatchHunk>> {
    let old_text = match old_text {
        None => {
            return whole_file_hunk(new_text)
                .map(|h| vec![h])
                .or_else(|| Some(Vec::new()));
        }
        Some(t) => t,
    };
    DIFF_CALLS.with(|c| c.set(c.get() + 1));
    let old_lines = text_lines(old_text);
    let new_lines = text_lines(new_text);
    let common = old_lines.len().min(new_lines.len());
    let mut prefix = 0;
    while prefix < common && old_lines[prefix] == new_lines[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < common - prefix
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let skipped = prefix.saturating_sub(PATCH_CONTEXT_LINES);
    let skipped_end = suffix.saturating_sub(PATCH_CONTEXT_LINES);
    let old_slice = &old_lines[skipped..old_lines.len() - skipped_end];
    let new_slice = &new_lines[skipped..new_lines.len() - skipped_end];

    let budget = BUDGET_OVERRIDE
        .with(Cell::get)
        .unwrap_or(Duration::from_millis(PATCH_DIFF_TIMEOUT_MS));
    let ops = myers_diff(old_slice, new_slice, budget)?;
    let hunks = structured_patch_hunks(ops);
    Some(
        hunks
            .into_iter()
            .map(|mut h| {
                h.old_start += skipped as u32;
                h.new_start += skipped as u32;
                h
            })
            .collect(),
    )
}

pub fn file_patch(old_text: Option<&str>, new_text: &str) -> Option<FilePatch> {
    let hunks = memoized_diff_hunks(old_text, new_text)?;
    if hunks.is_empty() {
        return None;
    }
    Some(FilePatch {
        change: if old_text.is_none() {
            FileChange::Create
        } else {
            FileChange::Update
        },
        hunks,
    })
}

const GIT_PATH_ESCAPES: &[(u8, char)] = &[
    (0x07, 'a'),
    (0x08, 'b'),
    (0x09, 't'),
    (0x0a, 'n'),
    (0x0b, 'v'),
    (0x0c, 'f'),
    (0x0d, 'r'),
    (0x22, '"'),
    (0x5c, '\\'),
];

fn quote_git_path(prefix: &str, name: &str) -> String {
    let full = format!("{prefix}{name}");
    let mut quoted = String::new();
    let mut needs_quotes = false;
    for &b in full.as_bytes() {
        if let Some((_, c)) = GIT_PATH_ESCAPES.iter().find(|(val, _)| *val == b) {
            quoted.push('\\');
            quoted.push(*c);
            needs_quotes = true;
        } else if !(0x20..0x7f).contains(&b) {
            quoted.push_str(&format!("\\{:03o}", b));
            needs_quotes = true;
        } else {
            quoted.push(b as char);
        }
    }
    if needs_quotes {
        format!("\"{quoted}\"")
    } else {
        full
    }
}

fn hunk_range(start: u32, count: u32) -> String {
    if count == 0 {
        format!("{},0", start.saturating_sub(1))
    } else if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

fn hunk_header(hunk: &PatchHunk) -> String {
    format!(
        "@@ -{} +{} @@",
        hunk_range(hunk.old_start, hunk.old_lines),
        hunk_range(hunk.new_start, hunk.new_lines)
    )
}

pub fn git_patch_text(
    file_path: &str,
    change: FileChange,
    hunks: &[PatchHunk],
    paths: GitPatchPaths,
) -> String {
    let slashed = file_path.replace('\\', "/");
    let name = if paths == GitPatchPaths::Git {
        slashed.trim_start_matches('/').to_string()
    } else {
        slashed
    };
    let old_name = quote_git_path(
        if paths == GitPatchPaths::Git {
            "a/"
        } else {
            ""
        },
        &name,
    );
    let new_name = quote_git_path(
        if paths == GitPatchPaths::Git {
            "b/"
        } else {
            ""
        },
        &name,
    );
    let tab = if name.contains(' ') { "\t" } else { "" };
    let mut lines = Vec::new();
    lines.push(format!("diff --git {old_name} {new_name}"));
    if change == FileChange::Create {
        lines.push("new file mode 100644".to_string());
        lines.push("--- /dev/null".to_string());
        lines.push(format!("+++ {new_name}{tab}"));
    } else {
        lines.push(format!("--- {old_name}{tab}"));
        lines.push(format!("+++ {new_name}{tab}"));
    }
    for hunk in hunks {
        lines.push(hunk_header(hunk));
        for line in &hunk.lines {
            lines.push(line.clone());
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

pub fn air_patch_content(file_path: &str, patch: &FilePatch) -> Value {
    let patch_text = git_patch_text(file_path, patch.change, &patch.hunks, GitPatchPaths::Git);
    json!({
        "type": "diff",
        "path": file_path,
        "oldText": Value::Null,
        "newText": "",
        "_meta": {
            "jetbrains": {
                "air": {
                    "version": 1,
                    "diffPatch": {
                        "version": 1,
                        "format": "git_patch",
                        "text": patch_text,
                    }
                }
            }
        }
    })
}

pub fn v2_diff_content(file_path: &str, change: FileChange, hunks: Option<&[PatchHunk]>) -> Value {
    let operation = match change {
        FileChange::Create => "add",
        FileChange::Update => "modify",
    };
    let mut diff = json!({
        "type": "diff",
        "changes": [
            {
                "operation": operation,
                "path": file_path,
                "fileType": "text"
            }
        ]
    });
    if let Some(hunks) = hunks {
        if !hunks.is_empty() {
            let patch_text = git_patch_text(file_path, change, hunks, GitPatchPaths::Absolute);
            diff["patch"] = json!({
                "format": "git_patch",
                "text": patch_text
            });
        }
    }
    diff
}

fn patch_content(file_path: &str, patch: &FilePatch, form: PatchForm) -> Value {
    if form == PatchForm::V2 {
        v2_diff_content(file_path, patch.change, Some(&patch.hunks))
    } else {
        air_patch_content(file_path, patch)
    }
}

/// Reads a file as patch input (upstream `readPatchSource`).
///
/// Like upstream, the file is opened with `O_NONBLOCK` (so a named pipe does
/// not wait for a writer) and its kind is read from the opened handle
/// ([`crate::process::open_regular_file_nonblocking`]): anything that is not a
/// regular file is [`PatchSource::Invalid`], and a path swapped to a FIFO
/// after any earlier look cannot block this thread.
pub fn read_patch_source(file_path: &Path) -> PatchSource {
    let mut file = match crate::process::open_regular_file_nonblocking(file_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PatchSource::Missing,
        Err(_) => return PatchSource::Invalid,
    };
    // The checks and the read use one open file, so a swap of the path
    // between them has no effect.
    let meta = match file.metadata() {
        Ok(m) => m,
        Err(_) => return PatchSource::Invalid,
    };
    if !meta.file_type().is_file() || meta.len() > MAX_PATCH_FILE_BYTES as u64 {
        return PatchSource::Invalid;
    }

    let mut buffer = Vec::new();
    use std::io::Read;
    let read_result = (&mut file).take(meta.len() + 1).read_to_end(&mut buffer);
    if read_result.is_err()
        || buffer.len() as u64 > meta.len()
        || buffer.len() > MAX_PATCH_FILE_BYTES
    {
        return PatchSource::Invalid;
    }

    let sniff_len = buffer.len().min(BINARY_SNIFF_BYTES);
    if buffer[..sniff_len].contains(&0) {
        return PatchSource::Invalid;
    }

    let text = match std::str::from_utf8(&buffer) {
        Ok(s) => s,
        Err(_) => return PatchSource::Invalid,
    };

    if text.contains('\r') {
        return PatchSource::Invalid;
    }

    PatchSource::Text(text.to_string())
}

fn replaced_text(file_text: &str, old_string: &str, new_string: &str, replace_all: bool) -> String {
    let target = if new_string.is_empty()
        && !old_string.ends_with('\n')
        && file_text.contains(&format!("{old_string}\n"))
    {
        format!("{old_string}\n")
    } else {
        old_string.to_string()
    };
    if replace_all {
        file_text.replace(&target, new_string)
    } else if let Some(pos) = file_text.find(&target) {
        let mut res = String::with_capacity(file_text.len() + new_string.len());
        res.push_str(&file_text[..pos]);
        res.push_str(new_string);
        res.push_str(&file_text[pos + target.len()..]);
        res
    } else {
        file_text.to_string()
    }
}

fn replaced_text_bytes_bound(
    file_text: &str,
    old_string: &str,
    new_string: &str,
    occurrences: usize,
) -> usize {
    let growth = new_string.len().saturating_sub(old_string.len());
    file_text.len() + occurrences * growth
}

/// Whether `text` is empty after JS `String.prototype.trim` (U+FEFF counts as
/// space there, U+0085 does not).
fn is_js_blank(text: &str) -> bool {
    text.chars().all(crate::tools::is_js_space)
}

/// Upstream `resolveToolPath`: an absolute path is returned unchanged, so the
/// OS resolves its `..` segments through symlinks exactly as the tool will; a
/// relative one is `path.resolve(cwd ?? process.cwd(), filePath)`, whose `.`
/// and `..` segments are folded without touching the file system.
fn resolve_tool_path(file_path: &str, cwd: Option<&Path>) -> PathBuf {
    let path = Path::new(file_path);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let base = match cwd {
        Some(cwd) if cwd.is_absolute() => cwd.to_path_buf(),
        Some(cwd) => std::env::current_dir().unwrap_or_default().join(cwd),
        None => std::env::current_dir().unwrap_or_default(),
    };
    let joined = base.join(path);
    let mut resolved = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if resolved.parent().is_some() {
                    resolved.pop();
                }
            }
            other => resolved.push(other.as_os_str()),
        }
    }
    resolved
}

/// The approval preview of an Edit or Write (upstream `previewPatchContent`).
pub fn preview_patch_content(
    tool_name: &str,
    input: &Value,
    cwd: Option<&Path>,
    form: PatchForm,
) -> Option<Vec<Value>> {
    if tool_name == "Edit" {
        let file_path_str = input.get("file_path").and_then(Value::as_str)?;
        let old_string = input.get("old_string").and_then(Value::as_str)?;
        let new_string = input.get("new_string").and_then(Value::as_str)?;
        let replace_all = input
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let resolved = resolve_tool_path(file_path_str, cwd);
        let resolved_str = resolved.to_string_lossy().into_owned();

        let old_source = read_patch_source(&resolved);
        let old_text = match old_source {
            PatchSource::Missing => None,
            PatchSource::Invalid => return None,
            PatchSource::Text(t) => Some(t),
        };

        if old_string == new_string || !is_patchable_text(new_string) {
            return None;
        }

        if old_string.is_empty() {
            if let Some(ref text) = old_text {
                if !is_js_blank(text) || text.contains('\u{FEFF}') {
                    return None;
                }
            }
            let patch = file_patch(old_text.as_deref(), new_string)?;
            return Some(vec![patch_content(&resolved_str, &patch, form)]);
        }

        let old_text = old_text?;
        let occurrences = old_text.matches(old_string).count();
        if occurrences == 0 || (!replace_all && occurrences != 1) {
            return None;
        }

        if replaced_text_bytes_bound(&old_text, old_string, new_string, occurrences)
            > MAX_PATCH_FILE_BYTES
        {
            return None;
        }

        let new_text = replaced_text(&old_text, old_string, new_string, replace_all);
        let patch = file_patch(Some(&old_text), &new_text)?;
        return Some(vec![patch_content(&resolved_str, &patch, form)]);
    }

    if tool_name == "Write" {
        let obj = input.as_object()?;
        let file_path_str = obj
            .get("file_path")
            .and_then(Value::as_str)
            .or_else(|| obj.get("path").and_then(Value::as_str))?;
        let content = ["content", "file_text", "file_content"]
            .into_iter()
            .find(|key| obj.get(*key).is_some_and(|value| !value.is_null()))
            .and_then(|key| obj.get(key).and_then(Value::as_str))?;

        let resolved = resolve_tool_path(file_path_str, cwd);
        let resolved_str = resolved.to_string_lossy().into_owned();

        let old_source = read_patch_source(&resolved);
        let old_text = match old_source {
            PatchSource::Missing => None,
            PatchSource::Invalid => {
                return Some(vec![json!({
                    "type": "content",
                    "content": {
                        "type": "text",
                        "text": format!("Overwrites the existing file `{file_path_str}`. The adapter cannot show its current content.")
                    }
                })]);
            }
            PatchSource::Text(t) => Some(t),
        };

        if old_text.as_deref() == Some(content) {
            return None;
        }

        let standard = if form == PatchForm::V2 {
            vec![v2_diff_content(
                &resolved_str,
                if old_text.is_none() {
                    FileChange::Create
                } else {
                    FileChange::Update
                },
                None,
            )]
        } else {
            vec![json!({
                "type": "diff",
                "path": file_path_str,
                "oldText": old_text.as_deref(),
                "newText": content
            })]
        };

        if !is_patchable_text(content) {
            return Some(standard);
        }

        if let Some(patch) = file_patch(old_text.as_deref(), content) {
            return Some(vec![patch_content(&resolved_str, &patch, form)]);
        }

        return Some(standard);
    }

    None
}

/// The exact patch of a finished Edit or Write, built from the file on disk
/// (upstream `patchUpdateFromDiffToolResponse`).
pub fn patch_update_from_diff_tool_response(
    tool_response: &Value,
    form: PatchForm,
) -> Option<PatchUpdateResult> {
    let file_path = tool_response.get("filePath").and_then(Value::as_str)?;
    let ty = tool_response.get("type").and_then(Value::as_str);
    let old_string = tool_response.get("oldString").and_then(Value::as_str);
    let original_file = tool_response.get("originalFile");

    if ty.is_none() && old_string == Some("") && original_file.and_then(Value::as_str) == Some("") {
        return None;
    }

    if ty == Some("update") {
        if let (Some(content), Some(orig)) = (
            tool_response.get("content").and_then(Value::as_str),
            original_file.and_then(Value::as_str),
        ) {
            if content == orig {
                return None;
            }
        }
    }

    let old_text: Option<String> = if ty == Some("create") {
        None
    } else {
        let orig = original_file.and_then(Value::as_str)?;
        Some(orig.to_string())
    };

    if let Some(ref text) = old_text {
        if text.len() > MAX_PATCH_FILE_BYTES {
            return None;
        }
    }

    let new_source = read_patch_source(Path::new(file_path));
    let new_text = match new_source {
        PatchSource::Text(t) => t,
        _ => return None,
    };

    if new_text.starts_with('\u{FEFF}') {
        return None;
    }

    // `newText !== response.content` also holds when the response has no
    // `content`: a created file is only trusted when it matches what was written.
    if ty == Some("create")
        && tool_response.get("content").and_then(Value::as_str) != Some(new_text.as_str())
    {
        return None;
    }

    let patch = file_patch(old_text.as_deref(), &new_text)?;
    let content_val = patch_content(file_path, &patch, form);

    let locations = if old_text.is_none() {
        vec![ToolCallLocation::new(file_path)]
    } else {
        patch
            .hunks
            .iter()
            .map(|h| ToolCallLocation::new(file_path).line(h.new_start))
            .collect()
    };

    Some(PatchUpdateResult {
        content: vec![content_val],
        locations,
    })
}

pub fn v2_update_from_diff_tool_response(tool_response: &Value) -> Option<PatchUpdateResult> {
    if let Some(exact) = patch_update_from_diff_tool_response(tool_response, PatchForm::V2) {
        return Some(exact);
    }
    let file_path = tool_response.get("filePath").and_then(Value::as_str)?;
    let ty = tool_response.get("type").and_then(Value::as_str);

    if ty == Some("create") {
        let content_str = tool_response.get("content").and_then(Value::as_str);
        let hunk = if let Some(text) = content_str {
            if is_patchable_text(text) {
                whole_file_hunk(text)
            } else {
                None
            }
        } else {
            None
        };
        let hunks = hunk.map(|h| vec![h]);
        let diff_val = v2_diff_content(file_path, FileChange::Create, hunks.as_deref());
        return Some(PatchUpdateResult {
            content: vec![diff_val],
            locations: vec![ToolCallLocation::new(file_path)],
        });
    }

    if ty.is_none()
        && tool_response.get("oldString").and_then(Value::as_str) == Some("")
        && tool_response.get("originalFile").and_then(Value::as_str) == Some("")
    {
        return None;
    }

    if ty == Some("update")
        && tool_response.get("content").and_then(Value::as_str)
            == tool_response.get("originalFile").and_then(Value::as_str)
    {
        return None;
    }

    let parsed_hunks = tool_response
        .get("structuredPatch")
        .and_then(Value::as_array)
        .and_then(|arr| {
            if arr.is_empty() {
                None
            } else {
                serde_json::from_value::<Vec<PatchHunk>>(Value::Array(arr.clone())).ok()
            }
        });

    let diff_val = v2_diff_content(file_path, FileChange::Update, parsed_hunks.as_deref());
    let locations = if let Some(ref hunks) = parsed_hunks {
        hunks
            .iter()
            .map(|h| ToolCallLocation::new(file_path).line(h.new_start))
            .collect()
    } else {
        vec![ToolCallLocation::new(file_path)]
    };

    Some(PatchUpdateResult {
        content: vec![diff_val],
        locations,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-fix search: it copies its whole working vector on every step.
    /// The banded search must produce exactly its operations.
    fn reference_myers(old_tokens: &[String], new_tokens: &[String]) -> Vec<DiffOp> {
        let n = old_tokens.len();
        let m = new_tokens.len();
        let max = n + m;
        let offset = max as isize;
        let mut v = vec![0isize; 2 * max + 2];
        let mut trace: Vec<Vec<isize>> = Vec::new();
        let mut final_d = 0;
        'outer: for d in 0..=max {
            trace.push(v.clone());
            let d_isize = d as isize;
            for k in (-d_isize..=d_isize).step_by(2) {
                let k_idx = (k + offset) as usize;
                let mut x = if k == -d_isize || (k != d_isize && v[k_idx - 1] < v[k_idx + 1]) {
                    v[k_idx + 1]
                } else {
                    v[k_idx - 1] + 1
                };
                let mut y = x - k;
                while x < n as isize
                    && y < m as isize
                    && old_tokens[x as usize] == new_tokens[y as usize]
                {
                    x += 1;
                    y += 1;
                }
                v[k_idx] = x;
                if x >= n as isize && y >= m as isize {
                    final_d = d;
                    break 'outer;
                }
            }
        }
        let mut x = n as isize;
        let mut y = m as isize;
        let mut ops = Vec::new();
        for d in (1..=final_d).rev() {
            let k = x - y;
            let d_isize = d as isize;
            let v_prev = &trace[d];
            let k_idx = (k + offset) as usize;
            let prev_k = if k == -d_isize || (k != d_isize && v_prev[k_idx - 1] < v_prev[k_idx + 1])
            {
                k + 1
            } else {
                k - 1
            };
            let prev_x = v_prev[(prev_k + offset) as usize];
            let prev_y = prev_x - prev_k;
            while x > prev_x && y > prev_y {
                x -= 1;
                y -= 1;
                ops.push(DiffOp::Equal(old_tokens[x as usize].clone()));
            }
            if x == prev_x {
                y -= 1;
                ops.push(DiffOp::Insert(new_tokens[y as usize].clone()));
            } else {
                x -= 1;
                ops.push(DiffOp::Delete(old_tokens[x as usize].clone()));
            }
        }
        while x > 0 && y > 0 {
            x -= 1;
            y -= 1;
            ops.push(DiffOp::Equal(old_tokens[x as usize].clone()));
        }
        ops.reverse();
        ops
    }

    fn lines(seed: &mut u64, len: usize, alphabet: u64) -> Vec<String> {
        (0..len)
            .map(|_| {
                *seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                format!("{}\n", (*seed >> 33) % alphabet)
            })
            .collect()
    }

    #[test]
    fn banded_search_matches_the_full_trace_reference() {
        let mut seed = 7u64;
        for round in 0..400usize {
            let alphabet = 2 + (round % 5) as u64;
            let old = lines(&mut seed, round % 37, alphabet);
            let new = lines(&mut seed, (round * 7) % 41, alphabet);
            let got = myers_diff(&old, &new, Duration::from_secs(10)).expect("in budget");
            assert_eq!(got, reference_myers(&old, &new), "round {round}");
        }
    }

    #[test]
    fn search_state_is_capped() {
        // Every line differs, so the search runs until the cap or the budget.
        reset_diff_stats();
        let old: Vec<String> = (0..20_000).map(|i| format!("old {i}\n")).collect();
        let new: Vec<String> = (0..20_000).map(|i| format!("new {i}\n")).collect();
        assert!(myers_diff(&old, &new, Duration::from_secs(120)).is_none());
        // The ALLOCATION (capacity, not used length) stays within the cap plus
        // the slack of one step: 30.5 MB. A doubling Vec reaches 48 MiB here.
        let bound = (MAX_TRACE_NODES + TRACE_SLACK_NODES) * std::mem::size_of::<PathNode>();
        assert!(
            peak_search_bytes() <= bound,
            "{} > {bound}",
            peak_search_bytes()
        );
        assert!(peak_search_bytes() > MAX_TRACE_NODES * std::mem::size_of::<PathNode>());
    }

    #[test]
    fn push_node_never_allocates_past_the_limit() {
        let limit = MAX_TRACE_NODES + TRACE_SLACK_NODES;
        let node = || PathNode {
            prev: NO_NODE,
            kind: PathKind::Insert,
            common: 0,
        };
        let mut nodes: Vec<PathNode> = Vec::new();
        let mut max_capacity = 0;
        for _ in 0..limit {
            assert!(push_node(&mut nodes, node()));
            max_capacity = max_capacity.max(nodes.capacity());
        }
        assert_eq!(nodes.len(), limit);
        assert_eq!(max_capacity, limit, "capacity, not length, is bounded");
        assert!(!push_node(&mut nodes, node()));
        assert_eq!(nodes.capacity(), limit);
    }
}
