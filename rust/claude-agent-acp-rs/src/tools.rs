//! Per-tool ACP shaping (phase 9): a tool name + input → `kind` / `title` /
//! `locations` / `content` for `ToolCall` / `ToolCallUpdate`, and a tool-result
//! → `content` shaping for completing `ToolCallUpdate`s.
//!
//! This is the Rust port of `toolInfoFromToolUse` and `toolUpdateFromToolResult`
//! (`src/tools.ts`). Wire truth is the Node adapter at `v0.70.0`: the shapes here
//! must produce byte-identical ACP tool frames to the captured fixtures.
//!
//! Two upstream parameters are intentionally not threaded yet (recorded in the
//! plan under Key Decisions): `supportsTerminalOutput` (always `false` — this
//! crate does not yet advertise `clientCapabilities._meta.terminal_output`, so
//! Bash renders as code-block output, never terminal `_meta`) and `cwd` (no
//! display-path relativisation; `toDisplayPath` returns the path unchanged).
//! The captured fixtures all use `terminal:false` and no Read/Write/Edit path
//! tooling, so this matches them exactly.

use agent_client_protocol::schema::v1::{
    Content, ContentBlock, Diff, TextContent, ToolCallContent, ToolCallLocation, ToolKind,
};
use serde_json::Value;

/// The ACP tool-shape fields derived from a tool name + input (port of
/// `toolInfoFromToolUse`, `src/tools.ts:131-502`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolShape {
    pub kind: ToolKind,
    pub title: String,
    pub locations: Vec<ToolCallLocation>,
    pub content: Vec<ToolCallContent>,
}

impl ToolShape {
    fn new(kind: ToolKind, title: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            locations: Vec::new(),
            content: Vec::new(),
        }
    }

    fn locations(mut self, locations: Vec<ToolCallLocation>) -> Self {
        self.locations = locations;
        self
    }

    fn content(mut self, content: Vec<ToolCallContent>) -> Self {
        self.content = content;
        self
    }
}

/// One text content block (`{type:"content", content:{type:"text",text}}`).
fn text_content(text: impl Into<String>) -> ToolCallContent {
    ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(text))))
}

/// One diff content block (`{type:"diff", path, oldText, newText}`).
fn diff_content(path: &str, old_text: Option<&str>, new_text: &str) -> ToolCallContent {
    let mut diff = Diff::new(path, new_text);
    if let Some(old) = old_text {
        diff = diff.old_text(old);
    }
    ToolCallContent::Diff(diff)
}

/// A file location for the "follow-along" feature.
fn location(path: &str, line: Option<u32>) -> ToolCallLocation {
    let mut loc = ToolCallLocation::new(path);
    if let Some(line) = line {
        loc = loc.line(line);
    }
    loc
}

/// Build the ACP `_meta.claudeCode` object for a tool use (port of
/// `claudeCodeMetaFromToolUse`, `src/acp-agent.ts:7983-8010`).
///
/// `skillPath` is not resolved (that probes the filesystem — out of scope for
/// this pure mapper); `skill` is still emitted when present.
pub fn claude_code_meta(name: &str, input: &Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("toolName".to_string(), Value::String(name.to_string()));

    if name == "Bash" {
        if let Some(desc) = input.get("description").and_then(Value::as_str) {
            map.insert("title".to_string(), Value::String(desc.to_string()));
        }
    }
    if name == "Agent" || name == "Task" {
        map.insert("subagent".to_string(), Value::Bool(true));
    }
    if name == "Skill" {
        if let Some(skill) = input.get("skill").and_then(Value::as_str) {
            map.insert("skill".to_string(), Value::String(skill.to_string()));
        }
    }
    Value::Object(map)
}

/// Build the full ACP `_meta` map for a tool use: `{"claudeCode": {...}}`.
pub fn tool_meta(name: &str, input: &Value) -> serde_json::Map<String, Value> {
    let mut meta = serde_json::Map::new();
    meta.insert("claudeCode".to_string(), claude_code_meta(name, input));
    meta
}

/// The absolute display path, unchanged (upstream `toDisplayPath` without a
/// `cwd` — this crate does not thread a cwd yet, see the module docs).
fn display_path(path: &str) -> String {
    path.to_string()
}

/// Per-tool ACP shape from a tool name + input (port of `toolInfoFromToolUse`,
/// `src/tools.ts:131-502`). `supportsTerminalOutput` is always `false` here.
pub fn tool_shape(name: &str, input: &Value) -> ToolShape {
    match name {
        "Agent" | "Task" => {
            let description = input.get("description").and_then(Value::as_str);
            let mut shape = ToolShape::new(ToolKind::Think, description.unwrap_or("Task"));
            if let Some(prompt) = input.get("prompt").and_then(Value::as_str) {
                shape = shape.content(vec![text_content(prompt)]);
            }
            shape
        }
        "Bash" => {
            let command = input.get("command").and_then(Value::as_str);
            let mut shape = ToolShape::new(ToolKind::Execute, command.unwrap_or("Terminal"));
            if let Some(description) = input.get("description").and_then(Value::as_str) {
                shape = shape.content(vec![text_content(description)]);
            }
            shape
        }
        "Read" => {
            let file_path = input.get("file_path").and_then(Value::as_str);
            let offset = input.get("offset").and_then(Value::as_u64);
            let limit = input.get("limit").and_then(Value::as_u64);
            let title = if let Some(limit) = limit {
                if limit > 0 {
                    let start = offset.unwrap_or(1);
                    format!(
                        "Read {} ({} - {})",
                        display_path(file_path.unwrap_or("File")),
                        start,
                        start + limit - 1
                    )
                } else {
                    format!(
                        "Read {} (from line {})",
                        display_path(file_path.unwrap_or("File")),
                        offset.unwrap_or(1)
                    )
                }
            } else {
                format!("Read {}", display_path(file_path.unwrap_or("File")))
            };
            let locations = file_path
                .map(|p| vec![location(p, Some(offset.unwrap_or(1) as u32))])
                .unwrap_or_default();
            ToolShape::new(ToolKind::Read, title).locations(locations)
        }
        "Write" => {
            let file_path = input.get("file_path").and_then(Value::as_str);
            let new_content = input.get("content").and_then(Value::as_str);
            let content = if let Some(path) = file_path {
                vec![diff_content(path, None, new_content.unwrap_or(""))]
            } else if let Some(content) = new_content {
                vec![text_content(content)]
            } else {
                Vec::new()
            };
            let title = match file_path {
                Some(p) => format!("Write {}", display_path(p)),
                None => "Preparing file…".to_string(),
            };
            let locations = file_path
                .map(|p| vec![location(p, None)])
                .unwrap_or_default();
            ToolShape::new(ToolKind::Edit, title)
                .content(content)
                .locations(locations)
        }
        "Edit" => {
            let file_path = input.get("file_path").and_then(Value::as_str);
            let old_string = input.get("old_string").and_then(Value::as_str);
            let new_string = input.get("new_string").and_then(Value::as_str);
            let content = match (file_path, old_string, new_string) {
                (Some(path), old, Some(new)) => {
                    let has_old = old.map(|s| !s.is_empty()).unwrap_or(false);
                    let has_new = !new.is_empty();
                    if has_old || has_new {
                        vec![diff_content(path, old, new)]
                    } else {
                        Vec::new()
                    }
                }
                (Some(path), old, None) => {
                    let has_old = old.map(|s| !s.is_empty()).unwrap_or(false);
                    if has_old {
                        vec![diff_content(path, old, "")]
                    } else {
                        Vec::new()
                    }
                }
                _ => Vec::new(),
            };
            let title = match file_path {
                Some(p) => format!("Edit {}", display_path(p)),
                None => "Edit".to_string(),
            };
            let locations = file_path
                .map(|p| vec![location(p, None)])
                .unwrap_or_default();
            ToolShape::new(ToolKind::Edit, title)
                .content(content)
                .locations(locations)
        }
        "Glob" => {
            let path = input.get("path").and_then(Value::as_str);
            let pattern = input.get("pattern").and_then(Value::as_str);
            let mut label = "Find".to_string();
            if let Some(path) = path {
                label.push_str(&format!(" `{path}`"));
            }
            if let Some(pattern) = pattern {
                label.push_str(&format!(" `{pattern}`"));
            }
            let locations = path.map(|p| vec![location(p, None)]).unwrap_or_default();
            ToolShape::new(ToolKind::Search, label).locations(locations)
        }
        "Grep" => {
            let mut label = "grep".to_string();
            if input.get("-i").and_then(Value::as_bool).unwrap_or(false) {
                label.push_str(" -i");
            }
            if input.get("-n").and_then(Value::as_bool).unwrap_or(false) {
                label.push_str(" -n");
            }
            if let Some(a) = input.get("-A") {
                label.push_str(&format!(" -A {a}"));
            }
            if let Some(b) = input.get("-B") {
                label.push_str(&format!(" -B {b}"));
            }
            if let Some(c) = input.get("-C") {
                label.push_str(&format!(" -C {c}"));
            }
            if let Some(mode) = input.get("output_mode").and_then(Value::as_str) {
                match mode {
                    "files_with_matches" => label.push_str(" -l"),
                    "count" => label.push_str(" -c"),
                    _ => {}
                }
            }
            if let Some(head) = input.get("head_limit") {
                label.push_str(&format!(" | head -{head}"));
            }
            if let Some(glob) = input.get("glob").and_then(Value::as_str) {
                label.push_str(&format!(" --include=\"{glob}\""));
            }
            if let Some(ty) = input.get("type").and_then(Value::as_str) {
                label.push_str(&format!(" --type={ty}"));
            }
            if input
                .get("multiline")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                label.push_str(" -P");
            }
            if let Some(pattern) = input.get("pattern").and_then(Value::as_str) {
                label.push_str(&format!(" \"{pattern}\""));
            }
            if let Some(path) = input.get("path").and_then(Value::as_str) {
                label.push_str(&format!(" {path}"));
            }
            ToolShape::new(ToolKind::Search, label)
        }
        "WebFetch" => {
            let url = input.get("url").and_then(Value::as_str);
            let mut shape = ToolShape::new(
                ToolKind::Fetch,
                url.map(|u| format!("Fetch {u}")).unwrap_or("Fetch".into()),
            );
            if let Some(prompt) = input.get("prompt").and_then(Value::as_str) {
                shape = shape.content(vec![text_content(prompt)]);
            }
            shape
        }
        "WebSearch" => {
            let query = input.get("query").and_then(Value::as_str);
            let mut label = query
                .map(|q| format!("\"{q}\""))
                .unwrap_or_else(|| "Web search".to_string());
            if let Some(allowed) = input.get("allowed_domains").and_then(Value::as_array) {
                if !allowed.is_empty() {
                    let joined = allowed
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ");
                    label.push_str(&format!(" (allowed: {joined})"));
                }
            }
            if let Some(blocked) = input.get("blocked_domains").and_then(Value::as_array) {
                if !blocked.is_empty() {
                    let joined = blocked
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ");
                    label.push_str(&format!(" (blocked: {joined})"));
                }
            }
            ToolShape::new(ToolKind::Fetch, label)
        }
        "TodoWrite" => {
            let todos = input.get("todos").and_then(Value::as_array);
            let title = match todos {
                Some(list) if !list.is_empty() => {
                    let joined = list
                        .iter()
                        .filter_map(|t| t.get("content").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("Update TODOs: {joined}")
                }
                _ => "Update TODOs".to_string(),
            };
            ToolShape::new(ToolKind::Think, title)
        }
        "ReportFindings" => {
            let findings = input.get("findings").and_then(Value::as_array);
            let count = findings.map_or(0, Vec::len);
            let title = if count == 0 {
                "Report findings: none found".to_string()
            } else {
                let plural = if count == 1 { "" } else { "s" };
                format!("Report {count} finding{plural}")
            };
            let content = findings
                .map(|list| {
                    list.iter()
                        .filter_map(|f| {
                            let file = f.get("file").and_then(Value::as_str)?;
                            let line = f.get("line").and_then(Value::as_u64);
                            let summary = f.get("summary").and_then(Value::as_str).unwrap_or("");
                            let label = match line {
                                Some(l) => format!("**{file}:{l}** — {summary}"),
                                None => format!("**{file}** — {summary}"),
                            };
                            Some(text_content(label))
                        })
                        .collect()
                })
                .unwrap_or_default();
            ToolShape::new(ToolKind::Think, title).content(content)
        }
        "TaskCreate" => {
            let subject = input.get("subject").and_then(Value::as_str);
            let title = subject
                .map(|s| format!("Create task: {s}"))
                .unwrap_or_else(|| "Create task".to_string());
            ToolShape::new(ToolKind::Think, title)
        }
        "TaskUpdate" => {
            let subject = input.get("subject").and_then(Value::as_str);
            let title = subject
                .map(|s| format!("Update task: {s}"))
                .unwrap_or_else(|| "Update task".to_string());
            ToolShape::new(ToolKind::Think, title)
        }
        "TaskList" => ToolShape::new(ToolKind::Think, "List tasks"),
        "TaskGet" => ToolShape::new(ToolKind::Think, "Get task"),
        "ExitPlanMode" => {
            let mut shape = ToolShape::new(ToolKind::SwitchMode, "Ready to code?");
            if let Some(plan) = input.get("plan").and_then(Value::as_str) {
                shape = shape.content(vec![text_content(plan)]);
            }
            shape
        }
        "Skill" => {
            let skill = input.get("skill").and_then(Value::as_str);
            let title = skill
                .map(|s| format!("Load skill: {s}"))
                .unwrap_or_else(|| "Load skill".to_string());
            ToolShape::new(ToolKind::Other, title)
        }
        "AskUserQuestion" => {
            let questions = input.get("questions").and_then(Value::as_array);
            let count = questions.map_or(0, Vec::len);
            let title = if count == 1 {
                questions
                    .and_then(|q| q.first())
                    .and_then(|q| q.get("question").and_then(Value::as_str))
                    .map(str::to_string)
                    .unwrap_or_else(|| "Asking for your input".to_string())
            } else {
                "Asking for your input".to_string()
            };
            let content = questions
                .map(|list| {
                    list.iter()
                        .filter_map(|q| q.get("question").and_then(Value::as_str))
                        .map(text_content)
                        .collect()
                })
                .unwrap_or_default();
            ToolShape::new(ToolKind::Other, title).content(content)
        }
        "Other" => {
            let text = format!("```json\n{}\n```", pretty_json(input));
            ToolShape::new(ToolKind::Other, name.to_string()).content(vec![text_content(text)])
        }
        // Unknown tool — generic fallback (never panics, 9.T2).
        _ => ToolShape::new(ToolKind::Other, name.to_string()),
    }
}

/// `JSON.stringify(input, null, 2)` fallback for the `Other` shape; a
/// non-object input falls back to `"{}"` (upstream catches the throw).
fn pretty_json(input: &Value) -> String {
    match input {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => "{}".to_string(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string_pretty(input).unwrap_or_else(|_| "{}".to_string())
        }
    }
}

/// Wrap text as a code block when the result is an error (upstream
/// `toAcpContentUpdate` string branch).
fn text_or_error(text: String, is_error: bool) -> String {
    if is_error {
        format!("```\n{text}\n```")
    } else {
        text
    }
}

/// Markdown-escape `text` by wrapping it in a sufficiently-long fence
/// (port of `markdownEscape`, `src/tools.ts:1277-1285`).
fn markdown_escape(text: &str) -> String {
    let mut fence = "```".to_string();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            let run = trimmed.chars().take_while(|&c| c == '`').count();
            while run >= fence.len() {
                fence.push('`');
            }
        }
    }
    let tail = if text.ends_with('\n') { "" } else { "\n" };
    format!("{fence}\n{text}{tail}{fence}")
}

/// Tool-result → `ToolCallUpdate` `content` (port of `toolUpdateFromToolResult`,
/// `src/tools.ts:586-928`). `supportsTerminalOutput` is always `false`.
///
/// Returns `None` when the tool result carries no content to surface (upstream
/// returns `{}`), or `Some(vec)` of content blocks.
pub fn tool_result_content(
    name: &str,
    result_content: &Value,
    is_error: bool,
) -> Option<Vec<ToolCallContent>> {
    match name {
        "Read" => {
            // The raw tool_result text is the model-facing view; line-number it
            // from the structured `tool_use_result` when present, else fall back
            // to the raw text (markdown-escaped).
            let raw = result_text(result_content);
            match raw {
                Some(text) if !text.is_empty() => Some(vec![text_content(markdown_escape(&text))]),
                _ => None,
            }
        }
        "Bash" => {
            // No terminal support: format as a console code block.
            let text = result_text(result_content).unwrap_or_default();
            let trimmed = text.trim_end();
            if trimmed.is_empty() {
                None
            } else {
                Some(vec![text_content(format!("```console\n{}\n```", trimmed))])
            }
        }
        "Agent" | "Task" => generic_content(result_content, is_error),
        "Skill" | "Edit" | "Write" => None,
        "ExitPlanMode" => None,
        _ => generic_content(result_content, is_error),
    }
}

/// Render the raw `tool_result.content` (string or text-block array) as ACP
/// content blocks (upstream `toAcpContentUpdate` generic path).
fn generic_content(result_content: &Value, is_error: bool) -> Option<Vec<ToolCallContent>> {
    match result_content {
        Value::String(text) if !text.is_empty() => {
            Some(vec![text_content(text_or_error(text.clone(), is_error))])
        }
        Value::Array(blocks) if !blocks.is_empty() => {
            let mut out = Vec::new();
            for block in blocks {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    out.push(text_content(text_or_error(text.to_string(), is_error)));
                } else if block.get("type").and_then(Value::as_str) == Some("image") {
                    out.push(image_content(block));
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// An ACP image content block from an `{type:"image", source:{...}}` block.
fn image_content(block: &Value) -> ToolCallContent {
    let source = block.get("source");
    let source_type = source.and_then(|s| s.get("type")).and_then(Value::as_str);
    let text = match source_type {
        Some("base64") => {
            let data = source
                .and_then(|s| s.get("data"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let mime = source
                .and_then(|s| s.get("media_type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            return ToolCallContent::Content(Content::new(ContentBlock::Image(
                agent_client_protocol::schema::v1::ImageContent::new(data, mime),
            )));
        }
        Some("url") => "[image: url]".to_string(),
        _ => "[image: file reference]".to_string(),
    };
    text_content(text)
}

/// Extract a plain text view of `result_content` (string, or joined text
/// blocks).
fn result_text(result_content: &Value) -> Option<String> {
    match result_content {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ToolCallContent;
    use serde_json::json;

    fn kind_of(content: &ToolCallContent) -> &'static str {
        match content {
            ToolCallContent::Content(_) => "content",
            ToolCallContent::Diff(_) => "diff",
            ToolCallContent::Terminal(_) => "terminal",
            _ => "other",
        }
    }

    /// 9.T1 (INV-30) — table-driven: one case per tool, asserting `kind`,
    /// `title`, and `locations` (and content where it matters).
    #[test]
    fn inv_24_tool_shapes_table() {
        struct Case {
            name: &'static str,
            input: Value,
            kind: ToolKind,
            title: &'static str,
            locations: usize,
        }
        let cases = vec![
            Case {
                name: "Agent",
                input: json!({"description": "Do a subtask", "prompt": "go"}),
                kind: ToolKind::Think,
                title: "Do a subtask",
                locations: 0,
            },
            Case {
                name: "Task",
                input: json!({"description": "Do a subtask", "prompt": "go"}),
                kind: ToolKind::Think,
                title: "Do a subtask",
                locations: 0,
            },
            Case {
                name: "Bash",
                input: json!({"command": "echo hi"}),
                kind: ToolKind::Execute,
                title: "echo hi",
                locations: 0,
            },
            Case {
                name: "Read",
                input: json!({"file_path": "/tmp/a.txt", "offset": 3, "limit": 2}),
                kind: ToolKind::Read,
                title: "Read /tmp/a.txt (3 - 4)",
                locations: 1,
            },
            Case {
                name: "Write",
                input: json!({"file_path": "/tmp/a.txt", "content": "hi"}),
                kind: ToolKind::Edit,
                title: "Write /tmp/a.txt",
                locations: 1,
            },
            Case {
                name: "Edit",
                input: json!({"file_path": "/tmp/a.txt", "old_string": "a", "new_string": "b"}),
                kind: ToolKind::Edit,
                title: "Edit /tmp/a.txt",
                locations: 1,
            },
            Case {
                name: "Glob",
                input: json!({"path": "/tmp", "pattern": "**/*.rs"}),
                kind: ToolKind::Search,
                title: "Find `/tmp` `**/*.rs`",
                locations: 1,
            },
            Case {
                name: "Grep",
                input: json!({"pattern": "foo", "path": "/tmp", "-i": true}),
                kind: ToolKind::Search,
                title: "grep -i \"foo\" /tmp",
                locations: 0,
            },
            Case {
                name: "WebFetch",
                input: json!({"url": "https://x.com", "prompt": "summarize"}),
                kind: ToolKind::Fetch,
                title: "Fetch https://x.com",
                locations: 0,
            },
            Case {
                name: "WebSearch",
                input: json!({"query": "rust"}),
                kind: ToolKind::Fetch,
                title: "\"rust\"",
                locations: 0,
            },
            Case {
                name: "TodoWrite",
                input: json!({"todos": [{"content": "a"}, {"content": "b"}]}),
                kind: ToolKind::Think,
                title: "Update TODOs: a, b",
                locations: 0,
            },
            Case {
                name: "ReportFindings",
                input: json!({"findings": [{"file": "a.rs", "line": 2, "summary": "s"}]}),
                kind: ToolKind::Think,
                title: "Report 1 finding",
                locations: 0,
            },
            Case {
                name: "TaskCreate",
                input: json!({"subject": "thing"}),
                kind: ToolKind::Think,
                title: "Create task: thing",
                locations: 0,
            },
            Case {
                name: "TaskUpdate",
                input: json!({"subject": "thing"}),
                kind: ToolKind::Think,
                title: "Update task: thing",
                locations: 0,
            },
            Case {
                name: "TaskList",
                input: json!({}),
                kind: ToolKind::Think,
                title: "List tasks",
                locations: 0,
            },
            Case {
                name: "TaskGet",
                input: json!({}),
                kind: ToolKind::Think,
                title: "Get task",
                locations: 0,
            },
            Case {
                name: "ExitPlanMode",
                input: json!({"plan": "do it"}),
                kind: ToolKind::SwitchMode,
                title: "Ready to code?",
                locations: 0,
            },
            Case {
                name: "Skill",
                input: json!({"skill": "coding"}),
                kind: ToolKind::Other,
                title: "Load skill: coding",
                locations: 0,
            },
            Case {
                name: "AskUserQuestion",
                input: json!({"questions": [{"question": "ok?"}]}),
                kind: ToolKind::Other,
                title: "ok?",
                locations: 0,
            },
        ];

        for c in &cases {
            let shape = tool_shape(c.name, &c.input);
            assert_eq!(shape.kind, c.kind, "{}: kind", c.name);
            assert_eq!(shape.title, c.title, "{}: title", c.name);
            assert_eq!(shape.locations.len(), c.locations, "{}: locations", c.name);
        }

        // Content spot-checks on the tools that carry it.
        let agent = tool_shape("Agent", &json!({"description": "d", "prompt": "go"}));
        assert_eq!(agent.content.len(), 1);
        assert_eq!(kind_of(&agent.content[0]), "content");

        let write = tool_shape("Write", &json!({"file_path": "/tmp/a", "content": "hi"}));
        assert_eq!(kind_of(&write.content[0]), "diff");

        let edit = tool_shape(
            "Edit",
            &json!({"file_path": "/tmp/a", "old_string": "x", "new_string": "y"}),
        );
        assert_eq!(kind_of(&edit.content[0]), "diff");
    }

    /// 9.T2 — an unknown tool falls back to the generic `Other` shape and never
    /// panics.
    #[test]
    fn inv_24_unknown_tool_falls_back() {
        let shape = tool_shape("DefinitelyNotATool", &json!({"a": 1}));
        assert_eq!(shape.kind, ToolKind::Other);
        assert_eq!(shape.title, "DefinitelyNotATool");
        assert!(shape.content.is_empty());

        // The `Other` tool itself renders its input as a json code block.
        let other = tool_shape("Other", &json!({"a": 1}));
        assert_eq!(other.kind, ToolKind::Other);
        assert_eq!(other.title, "Other");
        assert_eq!(other.content.len(), 1);
        assert_eq!(kind_of(&other.content[0]), "content");

        // A hostile/broken name must never panic.
        let _ = tool_shape("", &json!(null));
        let _ = tool_shape("Other", &json!("not-an-object"));
    }

    /// 9.1 — the `_meta.claudeCode` object carries toolName, Bash description
    /// title, subagent flag, and Skill name.
    #[test]
    fn claude_code_meta_shapes() {
        let bash = claude_code_meta("Bash", &json!({"description": "run ls"}));
        assert_eq!(bash["toolName"], "Bash");
        assert_eq!(bash["title"], "run ls");
        assert!(bash.get("subagent").is_none());

        let task = claude_code_meta("Task", &json!({}));
        assert_eq!(task["subagent"], true);

        let skill = claude_code_meta("Skill", &json!({"skill": "coding"}));
        assert_eq!(skill["skill"], "coding");
    }

    /// 9.2 — tool-result content: Bash renders a console code block; a string
    /// falls through to generic content; an error wraps in a fence.
    #[test]
    fn tool_result_content_shapes() {
        let bash = tool_result_content("Bash", &json!("echo hi"), false).unwrap();
        assert_eq!(kind_of(&bash[0]), "content");

        let read = tool_result_content("Read", &json!("line one\nline two"), false).unwrap();
        assert_eq!(kind_of(&read[0]), "content");

        // Empty Bash output surfaces nothing.
        assert!(tool_result_content("Bash", &json!("  "), false).is_none());

        // Edit/Write/Skill results surface no content.
        assert!(tool_result_content("Edit", &json!("x"), false).is_none());
        assert!(tool_result_content("Write", &json!("x"), false).is_none());
        assert!(tool_result_content("Skill", &json!("x"), false).is_none());
    }
}
