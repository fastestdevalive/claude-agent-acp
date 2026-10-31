//! Per-tool ACP shaping (phase 9): a tool name + input → `kind` / `title` /
//! `locations` / `content` for `ToolCall` / `ToolCallUpdate`, and a tool-result
//! → `content` shaping for completing `ToolCallUpdate`s.
//!
//! This is the Rust port of `toolInfoFromToolUse` and `toolUpdateFromToolResult`
//! (`src/tools.ts`). Wire truth is the Node adapter at the pinned version (`porting/UPSTREAM_VERSION`, currently 0.94.0): the shapes here
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
    Content, ContentBlock, Diff, PlanEntry, PlanEntryPriority, PlanEntryStatus, TextContent,
    ToolCallContent, ToolCallLocation, ToolKind,
};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub(crate) mod background;
pub(crate) mod reporters;

pub use display_path as to_display_path;
pub use tool_shape as tool_info_from_tool_use;

/// The ACP tool-shape fields derived from a tool name + input (port of
/// `toolInfoFromToolUse`, `src/tools.ts:131-502`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolShape {
    pub kind: ToolKind,
    pub title: String,
    pub locations: Vec<ToolCallLocation>,
    pub content: Vec<ToolCallContent>,
    /// The content is a display copy of the input (`ToolUseFacts.display`,
    /// `renderer.ts:750-757`), not a change. AIR renders `rawInput` itself, so
    /// a `rawInputRendering` client gets no display copy.
    pub display_only: bool,
}

/// Upstream `ClaudePlanEntry` (`tools.ts:65-69`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudePlanEntry {
    pub content: String,
    pub status: String,
    #[serde(default)]
    pub active_form: String,
}

/// Upstream `planEntries` helper (`tools.ts:71-78`).
#[must_use]
pub fn plan_entries(todos: Option<&[ClaudePlanEntry]>) -> Vec<PlanEntry> {
    todos
        .unwrap_or_default()
        .iter()
        .map(|todo| {
            let content = if todo.status == "in_progress" && !todo.active_form.is_empty() {
                todo.active_form.clone()
            } else {
                todo.content.clone()
            };
            let status = match todo.status.as_str() {
                "in_progress" => PlanEntryStatus::InProgress,
                "completed" => PlanEntryStatus::Completed,
                _ => PlanEntryStatus::Pending,
            };
            PlanEntry::new(content, PlanEntryPriority::Medium, status)
        })
        .collect()
}

impl ToolShape {
    fn new(kind: ToolKind, title: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            locations: Vec::new(),
            content: Vec::new(),
            display_only: false,
        }
    }

    fn locations(mut self, locations: Vec<ToolCallLocation>) -> Self {
        self.locations = locations;
        self
    }

    /// The change of the tool (a diff): kept for every client.
    fn content(mut self, content: Vec<ToolCallContent>) -> Self {
        self.content = content;
        self
    }

    /// A display copy of the input (`facts.display`): dropped for a client
    /// that renders `rawInput` itself.
    fn display(mut self, content: Vec<ToolCallContent>) -> Self {
        self.content = content;
        self.display_only = true;
        self
    }
}

/// Whether the tool is one of the Task* tools whose tool_use is suppressed.
pub fn is_task_tool(name: &str) -> bool {
    matches!(name, "TaskCreate" | "TaskUpdate" | "TaskList" | "TaskGet")
}

/// Whether the streamed tool_use path surfaces this tool as a standalone
/// `tool_call` (`shouldEmitToolCall`, `acp-agent.ts:12820-12824`). TodoWrite is
/// rendered as a `plan` and Task* tools are suppressed (their plan snapshot is
/// emitted at tool_result time), so neither produces a streamed
/// tool_call/tool_call_update.
pub fn should_emit_tool_call(name: &str) -> bool {
    name != "TodoWrite" && !is_task_tool(name)
}

/// One text content block (`{type:"content", content:{type:"text",text}}`).
pub(crate) fn text_content(text: impl Into<String>) -> ToolCallContent {
    ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(text))))
}

/// One diff content block (`{type:"diff", path, oldText, newText}`).
pub(crate) fn diff_content(path: &str, old_text: Option<&str>, new_text: &str) -> ToolCallContent {
    let mut diff = Diff::new(path, new_text);
    if let Some(old) = old_text {
        diff = diff.old_text(old);
    }
    ToolCallContent::Diff(diff)
}

/// A file location for the "follow-along" feature.
pub(crate) fn location(path: &str, line: Option<u32>) -> ToolCallLocation {
    let mut loc = ToolCallLocation::new(path);
    if let Some(line) = line {
        loc = loc.line(line);
    }
    loc
}

/// Build the ACP `_meta.claudeCode` object for a tool use (port of
/// `claudeCodeMetaFromToolUse`, `src/acp-agent.ts:7983-8010`).
///
/// 2.10 (B2/X-9): for EVERY client the non-AIR `_meta` carries ONLY `{toolName}`
/// — the `subagent` flag, the Bash description `title` and the `skill` moved to
/// AIR-only `_meta.jetbrains.air.{subagent,commandTitle,skill}` (not re-added in
/// this plan, OQ-14 / PARITY pending). Unconditional: Phase 2 does not depend on
/// `AirClient` threading (3.2).
pub fn claude_code_meta(name: &str, _input: &Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("toolName".to_string(), Value::String(name.to_string()));
    Value::Object(map)
}

/// Build the full ACP `_meta` map for a tool use: `{"claudeCode": {...}}`.
pub fn tool_meta(name: &str, input: &Value) -> serde_json::Map<String, Value> {
    let mut meta = serde_json::Map::new();
    meta.insert("claudeCode".to_string(), claude_code_meta(name, input));
    meta
}

/// The AIR extension version stamped into `_meta.jetbrains.air.version`
/// (`AIR_EXTENSION_VERSION`, `dist/air-extension.js`).
const AIR_EXTENSION_VERSION: u64 = 1;

/// A shell tool's `description` (`reporters/bash.js:8` `shellDescription`),
/// falling back to `command_description`. `None` for an absent / non-string
/// value; an empty string is returned as-is (callers test truthiness).
fn shell_description(input: &Value) -> Option<&str> {
    let obj = input.as_object()?;
    if let Some(description) = obj.get("description").and_then(Value::as_str) {
        return Some(description);
    }
    obj.get("command_description").and_then(Value::as_str)
}

/// JavaScript truthiness of a JSON value (`if (value)`): `undefined` (a missing
/// key), `null`, `false`, `0` and `""` are falsy, every other value is truthy.
/// The reporters test their optional input fields this way.
fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// Template-literal interpolation of a JSON value (`${value}`): a missing key
/// is `"undefined"`, `null` is `"null"`, arrays join with `,` (a `null` element
/// becomes the empty string) and a plain object is `"[object Object]"`.
fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(Value::Number(number)) => match (number.as_i64(), number.as_u64()) {
            (Some(int), _) => int.to_string(),
            (_, Some(uint)) => uint.to_string(),
            _ => number.as_f64().map(|n| n.to_string()).unwrap_or_default(),
        },
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".to_string(),
    }
}

/// The output mode of a Grep input (`grepOutputMode`,
/// `reporters/search.ts:50-61`): an explicit string wins, else the `-l` / `-c`
/// flags (a boolean `true` or the string `"true"`) name the mode.
fn grep_output_mode(input: &Value) -> Option<&str> {
    let obj = input.as_object()?;
    if let Some(mode) = obj.get("output_mode").and_then(Value::as_str) {
        return Some(mode);
    }
    let flag_is_true = |flag: &str| match obj.get(flag) {
        Some(Value::Bool(true)) => true,
        Some(Value::String(text)) => text == "true",
        _ => false,
    };
    if flag_is_true("-l") {
        return Some("files_with_matches");
    }
    if flag_is_true("-c") {
        return Some("count");
    }
    None
}

/// The path of a Grep input (`grepPath`, `reporters/search.ts:63-67`): `path`
/// when it is a string, else the `file_path` alias. An empty string is a path
/// (the caller then tests its truthiness).
fn grep_path(input: &Value) -> Option<&str> {
    let obj = input.as_object()?;
    if let Some(path) = obj.get("path").and_then(Value::as_str) {
        return Some(path);
    }
    obj.get("file_path").and_then(Value::as_str)
}

/// Split `mcp__<server>__<tool>` the way Claude Code does
/// (`renderer.js` `mcpToolName`): the server is the segment up to the next
/// `__`, the tool is the whole rest. `None` for any other shape.
pub fn mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let separator = rest.find("__")?;
    if separator == 0 {
        return None;
    }
    let tool = &rest[separator + 2..];
    if tool.is_empty() {
        return None;
    }
    Some((&rest[..separator], tool))
}

/// The `_meta` of a tool-use report for an AIR client (port of
/// `AcpToolCallRenderer.toolUseMeta`, `dist/tool-calls/renderer.js:566-590`):
/// `{claudeCode:{toolName}}` plus, under `jetbrains.air` (always stamped with
/// `version`), `commandTitle` (Bash / PowerShell description), `subagent`
/// (`Agent` / `Task`), `skill {name, path?}` and `mcp {server, tool}`.
/// `cwd` locates a skill's `SKILL.md` (`None` or empty: no path).
pub fn air_tool_meta(
    name: &str,
    input: &Value,
    cwd: Option<&Path>,
) -> serde_json::Map<String, Value> {
    let mut meta = tool_meta(name, input);
    let mut air = serde_json::Map::new();
    if matches!(name, "Bash" | "PowerShell") {
        if let Some(description) = shell_description(input).filter(|d| !d.is_empty()) {
            air.insert("commandTitle".into(), Value::String(description.into()));
        }
    }
    if matches!(name, "Agent" | "Task") {
        air.insert("subagent".into(), Value::Bool(true));
    }
    if name == "Skill" {
        if let Some(skill) = input
            .get("skill")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            let mut entry = serde_json::Map::new();
            entry.insert("name".into(), Value::String(skill.into()));
            if let Some(path) = resolve_skill_path(skill, cwd) {
                entry.insert(
                    "path".into(),
                    Value::String(path.to_string_lossy().into_owned()),
                );
            }
            air.insert("skill".into(), Value::Object(entry));
        }
    }
    if let Some((server, tool)) = mcp_tool_name(name) {
        air.insert(
            "mcp".into(),
            serde_json::json!({ "server": server, "tool": tool }),
        );
    }
    if !air.is_empty() {
        let mut stamped = serde_json::Map::new();
        stamped.insert("version".into(), Value::from(AIR_EXTENSION_VERSION));
        stamped.extend(air);
        meta.insert(
            "jetbrains".into(),
            serde_json::json!({ "air": Value::Object(stamped) }),
        );
    }
    meta
}

/// Whether AIR shows this tool use as the list of viewed files
/// (`renderer.js` `namesViewedFile`): a read or a search with a path in its
/// locations or in the `path` of its input (Grep also takes `file_path`).
pub fn names_viewed_file(name: &str, input: &Value) -> bool {
    let shape = tool_shape(name, input, None);
    if !matches!(shape.kind, ToolKind::Read | ToolKind::Search) {
        return false;
    }
    if shape
        .locations
        .iter()
        .any(|l| !l.path.as_os_str().is_empty())
    {
        return true;
    }
    let path = if name == "Grep" {
        input
            .get("path")
            .and_then(Value::as_str)
            .or_else(|| input.get("file_path").and_then(Value::as_str))
    } else {
        input.get("path").and_then(Value::as_str)
    };
    path.is_some_and(|p| !p.is_empty())
}

/// Roots a skill's directory may sit under, relative to a scope directory.
const SKILL_CONTAINER_DIRS: [&str; 2] = [".claude/skills", ".agents/skills"];
/// Bytes of a `SKILL.md` that are read for its frontmatter.
const MAX_FRONTMATTER_BYTES: usize = 8192;

/// Whether `segment` would take a join outside the directory it is joined to.
fn escapes_container(segment: &str) -> bool {
    Path::new(segment).is_absolute() || segment.split(['/', '\\']).any(|part| part == "..")
}

/// The head of `file`, up to [`MAX_FRONTMATTER_BYTES`], cut back to its last
/// complete line when the buffer filled.
fn frontmatter_head(file: &Path) -> Option<String> {
    use std::io::Read;
    let mut buffer = Vec::new();
    std::fs::File::open(file)
        .ok()?
        .take(MAX_FRONTMATTER_BYTES as u64)
        .read_to_end(&mut buffer)
        .ok()?;
    let head = String::from_utf8_lossy(&buffer).into_owned();
    if buffer.len() < MAX_FRONTMATTER_BYTES {
        return Some(head);
    }
    Some(match head.rfind('\n') {
        Some(cut) => head[..cut].to_string(),
        None => String::new(),
    })
}

/// The value `raw` carries, quotes resolved and a trailing comment dropped
/// (`frontmatterScalar`).
fn frontmatter_scalar(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let quote = chars.first().copied();
    if quote != Some('"') && quote != Some('\'') {
        // A `#` starts a comment at the start of the value or after a blank.
        let mut cut = chars.len();
        for (i, c) in chars.iter().enumerate() {
            if *c == '#' && (i == 0 || matches!(chars[i - 1], ' ' | '\t')) {
                cut = if i == 0 { 0 } else { i - 1 };
                break;
            }
        }
        return chars[..cut].iter().collect::<String>().trim().to_string();
    }
    let quote = quote.unwrap_or('"');
    let mut value = String::new();
    let mut index = 1;
    while index < chars.len() {
        let c = chars[index];
        if c == quote {
            if quote == '\'' && chars.get(index + 1) == Some(&'\'') {
                value.push('\'');
                index += 2;
                continue;
            }
            return value;
        }
        if quote == '"' && c == '\\' && index + 1 < chars.len() {
            index += 1;
            value.push(chars[index]);
            index += 1;
            continue;
        }
        value.push(c);
        index += 1;
    }
    value
}

/// The `name:` the frontmatter of `skill_md` declares (`declaredSkillName`).
fn declared_skill_name(skill_md: &Path) -> Option<String> {
    let head = frontmatter_head(skill_md)?;
    let mut lines = head.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" || trimmed == "..." {
            break;
        }
        // `^name[ \t]*:(?:[ \t]+(.*))?$` — a key at column 0; the value needs a
        // blank after the colon (`name:value` is a plain scalar, not a mapping).
        let Some(rest) = line.strip_prefix("name") else {
            continue;
        };
        let Some(after) = rest.trim_start_matches([' ', '\t']).strip_prefix(':') else {
            continue;
        };
        let value = if after.is_empty() {
            ""
        } else if after.starts_with([' ', '\t']) {
            after.trim()
        } else {
            continue;
        };
        let scalar = frontmatter_scalar(value);
        return (!scalar.is_empty()).then_some(scalar);
    }
    None
}

/// The `SKILL.md` paths of `container`, keyed by declared name (sorted listing,
/// first declaration wins).
fn skills_by_declared_name(container: &Path) -> std::collections::HashMap<String, PathBuf> {
    let mut by_name = std::collections::HashMap::new();
    let mut entries: Vec<String> = std::fs::read_dir(container)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    for entry in entries {
        let file = container.join(&entry).join("SKILL.md");
        if std::fs::metadata(&file).is_err() {
            continue;
        }
        if let Some(name) = declared_skill_name(&file) {
            by_name.entry(name).or_insert(file);
        }
    }
    by_name
}

/// Absolute path of a skill's `SKILL.md`, or `None` (port of
/// `resolveSkillPath`, `reporters/interaction.js:358`). The home directory is
/// read from `HOME`.
pub fn resolve_skill_path(skill_name: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    resolve_skill_path_with_env(
        skill_name,
        cwd,
        &crate::env_snapshot::EnvSnapshot::from_process(),
    )
}

/// Resolve skill path with an explicit environment snapshot.
pub fn resolve_skill_path_with_env(
    skill_name: &str,
    cwd: Option<&Path>,
    env: &crate::env_snapshot::EnvSnapshot,
) -> Option<PathBuf> {
    let home = env.get_path("HOME");
    resolve_skill_path_in(skill_name, cwd, home.as_deref())
}

/// [`resolve_skill_path`] with an explicit home directory (tests).
pub fn resolve_skill_path_in(
    skill_name: &str,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let cwd = cwd.filter(|c| !c.as_os_str().is_empty())?;
    let (scope, name) = match skill_name.split_once(':') {
        Some((scope, name)) => (Some(scope), name),
        None => (None, skill_name),
    };
    // The name comes from the agent: a separator or `..` must never reach the fs.
    if name.is_empty()
        || name.contains(['/', '\\'])
        || name == ".."
        || scope.is_some_and(escapes_container)
    {
        return None;
    }
    let bases: Vec<&Path> = std::iter::once(cwd).chain(home).collect();
    let mut containers: Vec<PathBuf> = Vec::new();
    let add_containers = |containers: &mut Vec<PathBuf>, base: &Path| {
        for dir in SKILL_CONTAINER_DIRS {
            containers.push(base.join(dir));
        }
    };
    // A `<prefix>:<name>` skill is either directory-scoped or a plugin's.
    let add_scoped = |containers: &mut Vec<PathBuf>, base: &Path| {
        let Some(scope) = scope else { return };
        for dir in SKILL_CONTAINER_DIRS {
            containers.push(base.join(scope).join(dir));
        }
        containers.push(base.join(".claude/plugins").join(scope).join("skills"));
        let marketplaces = base.join(".claude/plugins/marketplaces");
        let Ok(listing) = std::fs::read_dir(&marketplaces) else {
            return;
        };
        let mut installed: Vec<String> = listing
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        installed.sort();
        for marketplace in installed {
            containers.push(
                marketplaces
                    .join(marketplace)
                    .join("plugins")
                    .join(scope)
                    .join("skills"),
            );
        }
    };
    for base in &bases {
        add_scoped(&mut containers, base);
    }
    for base in &bases {
        add_containers(&mut containers, base);
    }
    for container in &containers {
        let candidate = container.join(name).join("SKILL.md");
        if !std::fs::metadata(&candidate).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let declared = declared_skill_name(&candidate);
        if declared.is_none() || declared.as_deref() == Some(name) {
            return Some(candidate);
        }
    }
    // A prompt command of `.claude/commands` is no skill: only the directory
    // probe above may answer for it.
    if scope.is_none()
        && bases.iter().any(|base| {
            base.join(".claude/commands")
                .join(format!("{name}.md"))
                .exists()
        })
    {
        return None;
    }
    containers
        .iter()
        .find_map(|container| skills_by_declared_name(container).remove(name))
}

/// The display path of `path` relative to `cwd` (port of `toDisplayPath`,
/// `tool-calls/content.ts:66-74`): a file under `cwd` (or `cwd` itself) becomes
/// cwd-relative, every other path is returned unchanged. Both sides are made
/// absolute against the process directory first; no symlink canonicalisation.
/// An absent cwd returns the path unchanged (Node `if (!cwd) return filePath`).
pub fn display_path(path: &str, cwd: Option<&Path>) -> String {
    let Some(cwd) = cwd else {
        return path.to_string();
    };
    let (Ok(resolved_cwd), Ok(resolved_file)) = (
        std::path::absolute(cwd),
        std::path::absolute(Path::new(path)),
    ) else {
        return path.to_string();
    };
    if resolved_file == resolved_cwd {
        return String::new();
    }
    resolved_file
        .strip_prefix(&resolved_cwd)
        .map(|relative| relative.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// A Write input read the way the CLI validates it (`normalizeWriteInput`,
/// `reporters/file-edit.ts:62-79`): `path` is an alias for `file_path`,
/// `file_text` / `file_content` for `content`. The canonical key wins when both
/// spellings are present; a `null` alias does not count.
struct NormalizedWriteInput<'a> {
    file_path: Option<&'a str>,
    content: Option<&'a str>,
}

fn normalize_write_input(input: &Value) -> NormalizedWriteInput<'_> {
    let Some(obj) = input.as_object() else {
        return NormalizedWriteInput {
            file_path: None,
            content: None,
        };
    };
    let file_path = obj
        .get("file_path")
        .and_then(Value::as_str)
        .or_else(|| obj.get("path").and_then(Value::as_str));
    // The canonical key wins when both spellings are present; a `null` alias
    // does not count (`raw[key] !== undefined && raw[key] !== null`). The key
    // itself only matters for an AIR client's `rawInput` (not ported here).
    let content = ["content", "file_text", "file_content"]
        .into_iter()
        .find(|key| obj.get(*key).is_some_and(|value| !value.is_null()))
        .and_then(|key| obj.get(key).and_then(Value::as_str));
    NormalizedWriteInput { file_path, content }
}

/// JS `\s` (the characters `trim` and `/\s+/` match). Rust's
/// `char::is_whitespace` omits U+FEFF and adds U+0085.
pub(crate) fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// Node's `humanText` (`permissions/presentation.ts:35-52`): control
/// characters removed, trimmed (all whitespace runs folded when `single_line`),
/// and `None` when empty or longer than `max_len` UTF-16 units.
pub(crate) fn human_text(value: &str, max_len: usize, single_line: bool) -> Option<String> {
    let without_controls: String = value
        .chars()
        .filter(|c| {
            let code = *c as u32;
            !(code <= 0x08
                || code == 0x0b
                || code == 0x0c
                || (0x0e..=0x1f).contains(&code)
                || code == 0x7f)
        })
        .collect();
    let normalized = if single_line {
        without_controls
            .split(is_js_space)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        without_controls.trim_matches(is_js_space).to_string()
    };
    (!normalized.is_empty() && normalized.encode_utf16().count() <= max_len).then_some(normalized)
}

/// The subject a permission request names instead of the tool's own heading
/// (`subjectTitle`, `permissions/presentation.ts:77-81`): the host of a sandbox
/// network request, the display name of a computer-use call. Compacted to one
/// line of at most 160 characters; `None` for any other tool.
pub fn permission_subject(
    tool_name: &str,
    input: &Value,
    display_name: Option<&str>,
) -> Option<String> {
    let compact = |value: Option<&str>| value.and_then(|text| human_text(text, 160, true));
    if tool_name == "SandboxNetworkAccess" {
        if let Some(host) = compact(input.get("host").and_then(Value::as_str)) {
            return Some(host);
        }
    }
    tool_name
        .starts_with("mcp__computer-use__")
        .then(|| compact(display_name))
        .flatten()
}

/// The title and explanation of a permission prompt (upstream `permissions/presentation.ts:82-94`).
///
/// Computes the heading (`title`) and reason (`description`), shared between AIR metadata
/// and ACP v2 permission presentation.
pub fn permission_title_and_description(
    tool_name: &str,
    input: &Value,
    display_name: Option<&str>,
    tool_call_title: &str,
    decision_reason: Option<&str>,
) -> (String, Option<String>) {
    let call_title = permission_subject(tool_name, input, display_name)
        .unwrap_or_else(|| tool_call_title.to_string());
    let permission_title = if tool_name == "ExitPlanMode" {
        "Ready to code?".to_string()
    } else {
        call_title
    };
    // Shell titles are executable input: compacting whitespace changes quoted
    // arguments and comment boundaries, so they go out as they are.
    let title = if tool_name == "Bash" || tool_name == "PowerShell" {
        permission_title
    } else {
        human_text(&permission_title, 4_000, true).unwrap_or_else(|| "Use tool?".to_string())
    };
    let description = decision_reason
        .and_then(|reason| human_text(reason, 4_000, false))
        .map(|reason| format!("Reason: {reason}"));
    (title, description)
}

/// The AIR-only permission presentation `_meta` of a `session/request_permission`
/// (`buildClaudePermissionPresentation`, `permissions/presentation.ts:70-136`):
/// the heading and the explanation of the prompt. `tool_call_title` is the
/// title of the request's tool call.
pub fn air_permission_meta(
    tool_name: &str,
    input: &Value,
    display_name: Option<&str>,
    tool_call_title: &str,
    decision_reason: Option<&str>,
    default_to_no: bool,
) -> Value {
    let (title, description) = permission_title_and_description(
        tool_name,
        input,
        display_name,
        tool_call_title,
        decision_reason,
    );
    let mut payload = serde_json::Map::new();
    payload.insert("version".into(), Value::from(1));
    payload.insert("title".into(), Value::String(title));
    if let Some(description) = description {
        payload.insert("description".into(), Value::String(description));
    }
    if default_to_no {
        payload.insert("defaultToNo".into(), Value::Bool(true));
    }
    crate::air::with_air_meta(None, "permission", Value::Object(payload))
}

/// The `rawInput` an AIR client gets for a tool use (`AcpToolCallRenderer.rawInput`,
/// `renderer.ts:729-743`): without the file text that the tool call's diff
/// already holds (`fileTextKeys` of the Write and Edit reporters,
/// `reporters/file-edit.ts:55,119`). Every other tool, and every other client,
/// gets the input as it is.
pub fn air_raw_input(name: &str, input: &Value) -> Value {
    air_raw_input_with(name, input, false)
}

/// [`air_raw_input`] with the AIR `planFile` capability: an ExitPlanMode whose
/// `planFilePath` names an existing file gets the path in place of the plan
/// text (`AcpToolCallRenderer.rawInput`, `renderer.ts:729-743`).
pub fn air_raw_input_with(name: &str, input: &Value, plan_file: bool) -> Value {
    if name == "ExitPlanMode" && plan_file {
        if let Some(path) = existing_plan_file(input.get("planFilePath")) {
            return plan_file_input(input, &path);
        }
    }
    let Some(obj) = input.as_object() else {
        return input.clone();
    };
    let mut stripped = obj.clone();
    match name {
        "Write" => {
            let file_path = normalize_write_input(input).file_path;
            if file_path.is_some_and(|path| !path.is_empty()) {
                // The first present non-null alias holds the text.
                if let Some(key) = ["content", "file_text", "file_content"]
                    .into_iter()
                    .find(|key| obj.get(*key).is_some_and(|value| !value.is_null()))
                {
                    stripped.remove(key);
                }
            }
        }
        "Edit" => {
            let has_path = obj
                .get("file_path")
                .and_then(Value::as_str)
                .is_some_and(|path| !path.is_empty());
            let has_text = ["old_string", "new_string"].into_iter().any(|key| {
                obj.get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.is_empty())
            });
            if has_path && has_text {
                stripped.remove("old_string");
                stripped.remove("new_string");
            }
        }
        _ => {}
    }
    Value::Object(stripped)
}

/// The absolute path of a regular file (`existingPlanFile`,
/// `reporters/interaction.ts:84-92`): anything else, including a missing file,
/// is no plan file.
pub fn existing_plan_file(value: Option<&Value>) -> Option<String> {
    let path = value.and_then(Value::as_str)?;
    let path = Path::new(path);
    if !path.is_absolute() || !path.is_file() {
        return None;
    }
    Some(path.to_string_lossy().into_owned())
}

/// The input with the path of the plan file in place of the plan text
/// (`planFileInput`, `renderer.ts:810-818`).
pub fn plan_file_input(raw_input: &Value, plan_file_path: &str) -> Value {
    let mut input = raw_input.as_object().cloned().unwrap_or_default();
    input.remove("plan");
    input.insert(
        "planFilePath".into(),
        Value::String(plan_file_path.to_string()),
    );
    Value::Object(input)
}

/// The display copy of a NotebookEdit's new cell source
/// (`notebookSource`, `reporters/file-edit.ts:215-220`): the source wrapped in
/// a markdown fence. A deletion has no source.
pub fn notebook_source(input: &Value) -> Option<Vec<ToolCallContent>> {
    if input.get("edit_mode").and_then(Value::as_str) == Some("delete") {
        return None;
    }
    let source = input.get("new_source").and_then(Value::as_str)?;
    Some(vec![text_content(markdown_escape(source))])
}

/// Whether a NotebookEdit carries a new cell source, which its result text
/// repeats (`reporters/file-edit.ts:203-212`).
pub fn notebook_source_present(input: &Value) -> bool {
    notebook_source(input).is_some()
}

/// Per-tool ACP shape from a tool name + input (port of `toolInfoFromToolUse`,
/// `src/tools.ts:131-502`). `cwd` relativises the displayed file paths
/// (`toDisplayPath`, Decision 6); `supportsTerminalOutput` is always `false`.
pub fn tool_shape(name: &str, input: &Value, cwd: Option<&Path>) -> ToolShape {
    tool_shape_with(name, input, cwd, false)
}

/// The rendering choices of one client: the AIR flavour of the reporter facts
/// (`AskUserQuestion`'s fixed title), the replay flag, and the `diffPatch` /
/// `rawInputRendering` / `planFile` capabilities (`ClientCapabilities`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShapeOptions {
    pub air: bool,
    pub replay: bool,
    pub diff_patch: bool,
    pub raw_input_rendering: bool,
    pub plan_file: bool,
}

/// [`tool_shape`] with the client flavour: `air` selects the AIR-only reporter
/// facts (`AskUserQuestion`'s fixed title). The permission path keeps the
/// three-argument [`tool_shape`]; the streamed path passes the map's AIR flag.
pub fn tool_shape_with(name: &str, input: &Value, cwd: Option<&Path>, air: bool) -> ToolShape {
    tool_shape_full(
        name,
        input,
        cwd,
        ShapeOptions {
            air,
            ..ShapeOptions::default()
        },
    )
}

/// Full tool shape port of `toolInfoFromToolUse` (`src/tools.ts:131-502`).
pub fn tool_info_from_tool_use_facts(
    name: &str,
    input: &Value,
    replay: bool,
    cwd: Option<&Path>,
    diff_patch: bool,
) -> ToolShape {
    tool_shape_full(
        name,
        input,
        cwd,
        ShapeOptions {
            replay,
            diff_patch,
            ..ShapeOptions::default()
        },
    )
}

/// [`tool_shape_with`] with the rendering choices of the client (T-40, T-85,
/// T-86).
pub fn tool_shape_full(
    name: &str,
    input: &Value,
    cwd: Option<&Path>,
    options: ShapeOptions,
) -> ToolShape {
    let shape = shape_for(
        name,
        input,
        cwd,
        options.air,
        options.replay,
        options.diff_patch,
    );
    if !options.air || !shape.display_only {
        return shape;
    }
    // `toolUseContent` (`renderer.ts:750-757`): a client that renders
    // `rawInput` itself gets no display copy of the input. A `planFile` client
    // whose ExitPlanMode names an existing plan file gets none either, since
    // its `rawInput` carries the path instead of the plan text.
    let plan_file_hides = name == "ExitPlanMode"
        && options.plan_file
        && existing_plan_file(input.get("planFilePath")).is_some();
    if options.raw_input_rendering || plan_file_hides {
        let mut shape = shape;
        shape.content.clear();
        shape
    } else {
        shape
    }
}

fn shape_for(
    name: &str,
    input: &Value,
    cwd: Option<&Path>,
    air: bool,
    replay: bool,
    diff_patch: bool,
) -> ToolShape {
    match name {
        "Agent" | "Task" => {
            let description = input
                .get("description")
                .and_then(Value::as_str)
                .filter(|description| !description.is_empty());
            // `effort` is set only when the user asked for one (CLI 2.1.292+).
            // A `subagent_type: "fork"` child ignores it and runs at the
            // parent's effort (`reporters/agent.ts:10-13`).
            let effort = input
                .get("effort")
                .filter(|effort| js_truthy(Some(effort)))
                .filter(|_| input.get("subagent_type").and_then(Value::as_str) != Some("fork"));
            let suffix = effort
                .map(|effort| format!(" ({} effort)", js_string(Some(effort))))
                .unwrap_or_default();
            let mut shape = ToolShape::new(
                ToolKind::Think,
                format!("{}{suffix}", description.unwrap_or("Task")),
            );
            if let Some(prompt) = input.get("prompt").and_then(Value::as_str) {
                shape = shape.display(vec![text_content(prompt)]);
            }
            shape
        }
        // PowerShell shares the Bash reporter (`reporters/index.ts:23`).
        "Bash" | "PowerShell" => {
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .filter(|command| !command.is_empty());
            let mut shape = ToolShape::new(ToolKind::Execute, command.unwrap_or("Terminal"));
            // `shellDescription` falls back to `command_description`; an empty
            // description adds no display copy (`reporters/bash.ts:6-17,28`).
            if let Some(description) = shell_description(input).filter(|d| !d.is_empty()) {
                shape = shape.display(vec![text_content(description)]);
            }
            shape
        }
        "Read" => {
            let file_path = input
                .get("file_path")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty());
            let offset = input.get("offset").and_then(Value::as_u64);
            let limit = input.get("limit").and_then(Value::as_u64);
            let display = display_path(file_path.unwrap_or("File"), cwd);
            // Node: a truthy limit > 0 shows the range, else a truthy offset
            // shows the start line (`reporters/read.ts:11-18`).
            let suffix = match (limit, offset) {
                (Some(limit), _) if limit > 0 => {
                    let start = offset.unwrap_or(1);
                    format!(" ({start} - {})", start + limit - 1)
                }
                (_, Some(offset)) if offset > 0 => format!(" (from line {offset})"),
                _ => String::new(),
            };
            let locations = file_path
                .map(|p| vec![location(p, Some(offset.unwrap_or(1) as u32))])
                .unwrap_or_default();
            ToolShape::new(ToolKind::Read, format!("Read {display}{suffix}")).locations(locations)
        }
        "Write" => {
            let write = normalize_write_input(input);
            let file_path = write.file_path.filter(|path| !path.is_empty());
            let input_diff = !diff_patch || replay;
            let (content, display_only) = if let Some(path) = file_path {
                // A client that negotiated diff patches gets the exact patch
                // from the approval preview / PostToolUse hook; the input does
                // not tell whether the file exists, so its live tool call shows
                // no diff (`reporters/file-edit.ts:33-55`). This crate serves
                // no v2 client and `diffPatch` only reaches AIR through the
                // session wiring; a non-AIR client always gets the input diff.
                if input_diff {
                    (
                        vec![diff_content(path, None, write.content.unwrap_or(""))],
                        false,
                    )
                } else {
                    (Vec::new(), false)
                }
            } else if let Some(content) = write.content.filter(|content| !content.is_empty()) {
                (vec![text_content(content)], true)
            } else {
                (Vec::new(), false)
            };
            let title = match file_path {
                Some(path) => format!("Write {}", display_path(path, cwd)),
                None => "Preparing file…".to_string(),
            };
            let locations = file_path
                .map(|path| vec![location(path, None)])
                .unwrap_or_default();
            let mut shape = ToolShape::new(ToolKind::Edit, title)
                .content(content)
                .locations(locations);
            shape.display_only = display_only;
            shape
        }
        "Edit" => {
            let file_path = input
                .get("file_path")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty());
            let old_string = input.get("old_string").and_then(Value::as_str);
            let new_string = input.get("new_string").and_then(Value::as_str);
            let has_old = old_string.is_some_and(|s| !s.is_empty());
            let has_new = new_string.is_some_and(|s| !s.is_empty());
            let content = match file_path {
                Some(path) if has_old || has_new => {
                    // `oldText: edit.old_string || null`: an empty old string
                    // is no old text, never `""` (`reporters/file-edit.ts:110-120`).
                    vec![diff_content(
                        path,
                        old_string.filter(|s| !s.is_empty()),
                        new_string.unwrap_or(""),
                    )]
                }
                _ => Vec::new(),
            };
            let title = match file_path {
                Some(path) => format!("Edit {}", display_path(path, cwd)),
                None => "Edit".to_string(),
            };
            let locations = file_path
                .map(|path| vec![location(path, None)])
                .unwrap_or_default();
            ToolShape::new(ToolKind::Edit, title)
                .content(content)
                .locations(locations)
        }
        "Glob" => {
            let path = input
                .get("path")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty());
            let pattern = input
                .get("pattern")
                .and_then(Value::as_str)
                .filter(|pattern| !pattern.is_empty());
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
            if js_truthy(input.get("-i")) {
                label.push_str(" -i");
            }
            if js_truthy(input.get("-n")) {
                label.push_str(" -n");
            }
            // `!== undefined`: an explicit `null` prints as "null".
            for flag in ["-A", "-B", "-C"] {
                if let Some(value) = input.get(flag) {
                    label.push_str(&format!(" {flag} {}", js_string(Some(value))));
                }
            }
            match grep_output_mode(input) {
                Some("files_with_matches") => label.push_str(" -l"),
                Some("count") => label.push_str(" -c"),
                _ => {}
            }
            if let Some(head) = input.get("head_limit") {
                label.push_str(&format!(" | head -{}", js_string(Some(head))));
            }
            if let Some(glob) = input.get("glob").filter(|glob| js_truthy(Some(glob))) {
                label.push_str(&format!(" --include=\"{}\"", js_string(Some(glob))));
            }
            if let Some(ty) = input.get("type").filter(|ty| js_truthy(Some(ty))) {
                label.push_str(&format!(" --type={}", js_string(Some(ty))));
            }
            if js_truthy(input.get("multiline")) {
                label.push_str(" -P");
            }
            if let Some(pattern) = input
                .get("pattern")
                .filter(|pattern| js_truthy(Some(pattern)))
            {
                label.push_str(&format!(" \"{}\"", js_string(Some(pattern))));
            }
            if let Some(path) = grep_path(input).filter(|path| !path.is_empty()) {
                label.push_str(&format!(" {path}"));
            }
            ToolShape::new(ToolKind::Search, label)
        }
        "WebFetch" => {
            let url = input
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty());
            // `offset` reads on through a page too long for one call
            // (CLI 2.1.290+).
            let from = input
                .get("offset")
                .filter(|offset| js_truthy(Some(offset)))
                .map(|offset| format!(" (from char {})", js_string(Some(offset))))
                .unwrap_or_default();
            let mut shape = ToolShape::new(
                ToolKind::Fetch,
                url.map(|url| format!("Fetch {url}{from}"))
                    .unwrap_or_else(|| "Fetch".to_string()),
            );
            if let Some(prompt) = input
                .get("prompt")
                .and_then(Value::as_str)
                .filter(|prompt| !prompt.is_empty())
            {
                shape = shape.display(vec![text_content(prompt)]);
            }
            shape
        }
        "WebSearch" => {
            let query = input
                .get("query")
                .and_then(Value::as_str)
                .filter(|query| !query.is_empty());
            let title = query
                .map(|query| format!("Search \"{query}\""))
                .unwrap_or_else(|| "Web search".to_string());
            ToolShape::new(ToolKind::Fetch, title)
        }
        "TodoWrite" => {
            // `Array.isArray(todos) ? \`Update TODOs: ${...}\` : "Update TODOs"`
            // (`reporters/interaction.ts:139-146`): an empty array still names
            // the colon and an empty list.
            let todos = input.get("todos").and_then(Value::as_array);
            let title = match todos {
                Some(list) => {
                    let joined = list
                        .iter()
                        .map(|todo| js_string(todo.get("content")))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("Update TODOs: {joined}")
                }
                None => "Update TODOs".to_string(),
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
            // `**${finding.file}${finding.line ? ":"+line : ""}** — ${summary}`:
            // a line of 0 is falsy and no entry is dropped
            // (`reporters/interaction.ts:165-185`).
            let content = findings
                .map(|list| {
                    list.iter()
                        .map(|finding| {
                            let file = js_string(finding.get("file"));
                            let line = finding
                                .get("line")
                                .filter(|line| js_truthy(Some(line)))
                                .map(|line| format!(":{}", js_string(Some(line))))
                                .unwrap_or_default();
                            let summary = js_string(finding.get("summary"));
                            text_content(format!("**{file}{line}** — {summary}"))
                        })
                        .collect()
                })
                .unwrap_or_default();
            ToolShape::new(ToolKind::Think, title).display(content)
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
            // `ExitPlanModeReporter.toolUse` (`reporters/interaction.ts:37-48`):
            // the plan is input that the user approves, so its text is the
            // display copy. A `planFile` client gets the path in `rawInput`
            // instead and no display copy when the file exists.
            let mut shape = ToolShape::new(ToolKind::SwitchMode, "Approve Plan");
            if let Some(plan) = input.get("plan").and_then(Value::as_str) {
                if !plan.is_empty() {
                    shape = shape.display(vec![text_content(plan)]);
                }
            }
            shape
        }
        "Skill" => {
            let skill = input
                .get("skill")
                .and_then(Value::as_str)
                .filter(|skill| !skill.is_empty());
            let title = skill
                .map(|skill| format!("Load skill: {skill}"))
                .unwrap_or_else(|| "Load skill".to_string());
            ToolShape::new(ToolKind::Other, title)
        }
        "AskUserQuestion" => {
            // AIR gets a fixed title, because the question is input; every
            // other client gets the question of a single question
            // (`reporters/interaction.ts:94-116`).
            let questions = input.get("questions").and_then(Value::as_array);
            let single = if air {
                None
            } else {
                questions
                    .filter(|list| list.len() == 1)
                    .and_then(|list| list.first())
                    .and_then(|question| question.get("question").and_then(Value::as_str))
                    .filter(|question| !question.is_empty())
            };
            let title = single.unwrap_or("Asking for your input").to_string();
            let content = questions
                .map(|list| {
                    list.iter()
                        .filter_map(|question| question.get("question").and_then(Value::as_str))
                        .map(text_content)
                        .collect()
                })
                .unwrap_or_default();
            ToolShape::new(ToolKind::Other, title).display(content)
        }
        "NotebookEdit" => {
            // `NotebookEditReporter.toolUse` (`reporters/file-edit.ts:180-201`):
            // only AIR gets this rendering; every other client gets the generic
            // `{title:"NotebookEdit", kind:"other"}` shape.
            if !air {
                return ToolShape::new(ToolKind::Other, "NotebookEdit");
            }
            let notebook_path = input
                .get("notebook_path")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty());
            let cell = input
                .get("cell_id")
                .and_then(Value::as_str)
                .filter(|cell| !cell.is_empty())
                .map(|cell| format!(" {cell}"))
                .unwrap_or_default();
            let verb = match input.get("edit_mode").and_then(Value::as_str) {
                Some("insert") => "Insert",
                Some("delete") => "Delete",
                _ => "Edit",
            };
            let title = notebook_path.map_or_else(
                || "Edit notebook".to_string(),
                |path| format!("{verb} cell{cell} in {}", display_path(path, cwd)),
            );
            let locations = notebook_path
                .map(|path| vec![location(path, None)])
                .unwrap_or_default();
            let mut shape = ToolShape::new(ToolKind::Edit, title).locations(locations);
            if let Some(source) = notebook_source(input) {
                shape = shape.display(source);
            }
            shape
        }
        "Other" => {
            // `\`\`\`json\n${json}\`\`\`` — no newline before the closing fence
            // (`reporters/interaction.ts:191-206`).
            let text = format!("```json\n{}```", pretty_json(input));
            ToolShape::new(ToolKind::Other, name.to_string()).display(vec![text_content(text)])
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

/// Markdown-escape `text` by wrapping it in a sufficiently-long fence (port of
/// `markdownEscape`, `content.ts:238-246`). The fence run counts only backticks
/// at the start of a line (`/^```+/gm`), so an indented run does not widen it.
fn markdown_escape(text: &str) -> String {
    let mut fence = "```".to_string();
    for line in text.lines() {
        if line.starts_with("```") {
            let run = line.chars().take_while(|&c| c == '`').count();
            while run >= fence.len() {
                fence.push('`');
            }
        }
    }
    let tail = if text.ends_with('\n') { "" } else { "\n" };
    format!("{fence}\n{text}{tail}{fence}")
}

/// The agent-control tools (`reporters/index.ts:36-40`): SendMessage, TaskStop,
/// ListAgents, Monitor.
pub fn is_agent_control_tool(name: &str) -> bool {
    matches!(name, "SendMessage" | "TaskStop" | "ListAgents" | "Monitor")
}

/// AIR's error result of an agent-control tool: the raw content as plain text,
/// without the error fence (`AgentControlReporter.errorResult`,
/// `reporters/interaction.ts:209-219`). Every other client falls back to the
/// fenced error text.
pub fn agent_control_error_content(content: &Value) -> Option<Vec<ToolCallContent>> {
    to_acp_content_update(content, false)
}

/// Tool-result → `ToolCallUpdate` `content` (port of `toolUpdateFromToolResult`,
/// `src/tools.ts:586-928` + the reporters in `tool-calls/reporters/*`).
/// `supportsTerminalOutput` is always `false`; `structured` is the message-level
/// `tool_use_result` of the user frame (Decision 7), `input` the tool_use input
/// (Read falls back to its `offset` when the structured result names no
/// `startLine`).
///
/// Returns `None` when the tool result carries no content to surface (upstream
/// returns `{}`), or `Some(vec)` of content blocks.
pub fn tool_result_content(
    name: &str,
    input: &Value,
    structured: Option<&Value>,
    result_content: &Value,
    is_error: bool,
) -> Option<Vec<ToolCallContent>> {
    match name {
        "Read" => read_result_content(structured, input, result_content),
        // PowerShell shares the Bash reporter, including its result
        // (`reporters/index.ts:23`).
        "Bash" | "PowerShell" => bash_result_content(structured, result_content, is_error),
        "Agent" | "Task" => {
            if has_error_text(result_content, is_error) {
                generic_content(result_content, true)
            } else {
                reporters::agent_result_content(structured, result_content, is_error)
            }
        }
        "WebSearch" => {
            if has_error_text(result_content, is_error) {
                generic_content(result_content, true)
            } else {
                reporters::web_search_content(structured, result_content, is_error)
            }
        }
        // Edit / Write / Skill results are only a confirmation: the diff or
        // display copy already carries the change (`reporters/file-edit.ts`,
        // `reporters/interaction.ts:119-128`). An error result has no reporter
        // `errorResult`, so the error text is the result to show.
        "Skill" | "Edit" | "Write" => {
            if is_error {
                generic_content(result_content, true)
            } else {
                None
            }
        }
        "ExitPlanMode" => None,
        _ => generic_content(result_content, is_error),
    }
}

/// Rebuild the Read result from the structured `FileReadOutput` when it carries
/// text (`reporters/read.ts:22-48`): line-numbered from `file.startLine ?? input.offset ?? 1`,
/// the truncation banner re-established, then markdown-escaped. Image, notebook
/// and PDF variants fall back to the raw content blocks; an empty file keeps the
/// raw "file is empty" note.
pub fn read_result_content(
    structured: Option<&Value>,
    input: &Value,
    raw: &Value,
) -> Option<Vec<ToolCallContent>> {
    if let Some(structured) = structured {
        if structured.get("type").and_then(Value::as_str) == Some("text") {
            let file = structured.get("file");
            let content = file
                .and_then(|file| file.get("content"))
                .and_then(Value::as_str);
            if let Some(content) = content.filter(|content| !content.is_empty()) {
                let start_line = file
                    .and_then(|file| file.get("startLine"))
                    .and_then(Value::as_u64)
                    .or_else(|| input.get("offset").and_then(Value::as_u64))
                    .unwrap_or(1);
                // A trailing newline ends the last line; it is not an extra line.
                let body = content.strip_suffix('\n').unwrap_or(content);
                let mut numbered = body
                    .split('\n')
                    .enumerate()
                    .map(|(index, line)| format!("{}\t{line}", start_line + index as u64))
                    .collect::<Vec<_>>()
                    .join("\n");
                if file
                    .and_then(|file| file.get("truncatedByTokenCap"))
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    let num_lines = file
                        .and_then(|file| file.get("numLines"))
                        .and_then(Value::as_u64);
                    let total_lines = file
                        .and_then(|file| file.get("totalLines"))
                        .and_then(Value::as_u64);
                    let detail = match (num_lines, total_lines) {
                        (Some(num), Some(total)) => format!(": showing {num} of {total} lines"),
                        _ => String::new(),
                    };
                    numbered.push_str(&format!("\n[File truncated{detail}]"));
                }
                return Some(vec![text_content(markdown_escape(&numbered))]);
            }
        }
    }
    // Raw fallback: every text block is markdown-escaped on its own; a
    // non-text block is converted like `toAcpContentBlock`.
    match raw {
        Value::Array(blocks) if !blocks.is_empty() => Some(
            blocks
                .iter()
                .map(|block| {
                    if block.get("type").and_then(Value::as_str) == Some("text") {
                        text_content(markdown_escape(
                            block.get("text").and_then(Value::as_str).unwrap_or(""),
                        ))
                    } else {
                        acp_content_block(block, false)
                    }
                })
                .collect(),
        ),
        Value::String(text) if !text.is_empty() => Some(vec![text_content(markdown_escape(text))]),
        _ => None,
    }
}

/// Rebuild a Bash result from the structured `BashOutput` (`reporters/bash.ts:53-80`):
/// `stdout`+`stderr` joined, the interruption and persisted-output notes
/// re-established; skipped for image output and backgrounded commands (the raw
/// text carries the background notice). The non-terminal renderer wraps a
/// non-empty output in a `console` fence (`renderer.ts:440-460`).
///
/// An error result never reads the structured output: a non-terminal client has
/// no Bash `errorResult`, so the fenced raw error text is the result to show
/// (`renderer.ts:359-368`, `content.ts:89-107`).
pub fn bash_result_content(
    structured: Option<&Value>,
    raw: &Value,
    is_error: bool,
) -> Option<Vec<ToolCallContent>> {
    if has_error_text(raw, is_error) {
        return generic_content(raw, true);
    }
    match reporters::bash_facts(structured, raw, is_error) {
        reporters::BashFacts::Content(content) => content,
        reporters::BashFacts::Command(command) => command_console_block(&command),
    }
}

/// The non-terminal rendering of a finished command: a non-empty output in a
/// `console` fence (`renderer.ts:440-460`).
pub(crate) fn command_console_block(
    command: &crate::client_caps::CommandOutcome,
) -> Option<Vec<ToolCallContent>> {
    let trimmed = command.output.trim_end();
    if trimmed.trim().is_empty() {
        return None;
    }
    Some(vec![text_content(format!("```console\n{trimmed}\n```"))])
}

/// Whether an error result carries text to show (`hasErrorText`,
/// `renderer.ts:366-372`): a non-empty string or array of blocks. Such a result
/// goes through the reporter's `errorResult`, else the fenced generic text.
pub(crate) fn has_error_text(raw: &Value, is_error: bool) -> bool {
    is_error
        && match raw {
            Value::String(text) => !text.is_empty(),
            Value::Array(blocks) => !blocks.is_empty(),
            _ => false,
        }
}

/// Join the non-empty strings of `parts` with a newline (`[a,b].filter(Boolean).join("\n")`).
fn join_non_empty<const N: usize>(parts: [&str; N]) -> String {
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A non-text SDK content block as ACP content (`toAcpContentBlock`,
/// `content.ts:143-236`): an image becomes an ACP image, a document a compact
/// placeholder, and every other block type its text form. An unknown block
/// falls back to its JSON text, like the upstream `default` arm.
fn acp_content_block(block: &Value, is_error: bool) -> ToolCallContent {
    let wrap = |text: String| text_content(text_or_error(text, is_error));
    match block.get("type").and_then(Value::as_str) {
        Some("text") => wrap(
            block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        Some("image") => image_content(block, is_error),
        Some("document") => wrap(document_placeholder(block)),
        Some("tool_reference") => wrap(format!("Tool: {}", js_string(block.get("tool_name")))),
        Some("tool_search_tool_search_result") => {
            let names = block
                .get("tool_references")
                .and_then(Value::as_array)
                .map(|references| {
                    references
                        .iter()
                        .map(|reference| js_string(reference.get("tool_name")))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let names = if names.is_empty() {
                "none".to_string()
            } else {
                names
            };
            wrap(format!("Tools found: {names}"))
        }
        Some("tool_search_tool_result_error") => {
            let message = block
                .get("error_message")
                .filter(|message| js_truthy(Some(message)))
                .map(|message| format!(" - {}", js_string(Some(message))))
                .unwrap_or_default();
            wrap(format!(
                "Error: {}{message}",
                js_string(block.get("error_code"))
            ))
        }
        Some("web_search_result") => wrap(format!(
            "{} ({})",
            js_string(block.get("title")),
            js_string(block.get("url"))
        )),
        Some("web_search_tool_result_error") => {
            wrap(format!("Error: {}", js_string(block.get("error_code"))))
        }
        Some("web_fetch_result") => wrap(format!("Fetched: {}", js_string(block.get("url")))),
        Some("web_fetch_tool_result_error") => {
            wrap(format!("Error: {}", js_string(block.get("error_code"))))
        }
        Some("code_execution_result") | Some("bash_code_execution_result") => {
            let stdout = block.get("stdout").and_then(Value::as_str).unwrap_or("");
            let stderr = block.get("stderr").and_then(Value::as_str).unwrap_or("");
            let output = if !stdout.is_empty() { stdout } else { stderr };
            wrap(format!("Output: {output}"))
        }
        Some("code_execution_tool_result_error")
        | Some("bash_code_execution_tool_result_error") => {
            wrap(format!("Error: {}", js_string(block.get("error_code"))))
        }
        Some("text_editor_code_execution_view_result") => wrap(js_string(block.get("content"))),
        Some("text_editor_code_execution_create_result") => wrap(
            if js_truthy(block.get("is_file_update")) {
                "File updated"
            } else {
                "File created"
            }
            .to_string(),
        ),
        Some("text_editor_code_execution_str_replace_result") => {
            let lines = block
                .get("lines")
                .and_then(Value::as_array)
                .map(|lines| {
                    lines
                        .iter()
                        .map(|line| js_string(Some(line)))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            wrap(lines)
        }
        Some("text_editor_code_execution_tool_result_error") => {
            let message = block
                .get("error_message")
                .filter(|message| js_truthy(Some(message)))
                .map(|message| format!(" - {}", js_string(Some(message))))
                .unwrap_or_default();
            wrap(format!(
                "Error: {}{message}",
                js_string(block.get("error_code"))
            ))
        }
        _ => wrap(serde_json::to_string(block).unwrap_or_default()),
    }
}

/// A document block's compact placeholder (`content.ts:170-198`): a title
/// suffix when it names one, the URL for a URL source, and the media type and
/// byte size otherwise. The base64 payload is never shown.
fn document_placeholder(block: &Value) -> String {
    let title = block
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .map(|title| format!(" \"{title}\""))
        .unwrap_or_default();
    let source = block.get("source");
    match source
        .and_then(|source| source.get("type"))
        .and_then(Value::as_str)
    {
        Some("url") => format!(
            "[document{title}: {}]",
            js_string(source.and_then(|source| source.get("url")))
        ),
        Some("base64") => {
            let data = source
                .and_then(|source| source.get("data"))
                .and_then(Value::as_str)
                .unwrap_or("");
            // base64 inflates the byte count by 4/3.
            format!(
                "[document{title}: {}, {}]",
                js_string(source.and_then(|source| source.get("media_type"))),
                format_byte_size(data.len() * 3 / 4)
            )
        }
        Some("text") => {
            let data = source
                .and_then(|source| source.get("data"))
                .and_then(Value::as_str)
                .unwrap_or("");
            format!(
                "[document{title}: {}, {}]",
                js_string(source.and_then(|source| source.get("media_type"))),
                format_byte_size(data.len())
            )
        }
        _ => format!("[document{title}]"),
    }
}

/// Human-readable byte size for a document placeholder ("312 B", "2.4 KB",
/// "1.3 MB"), `content.ts:99-104`.
fn format_byte_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Port of `toAcpContentUpdate` (`content.ts:89-108`): an array maps every
/// block, an object with a `type` is one block, a non-empty string is text
/// (fenced on error), everything else carries no content.
fn to_acp_content_update(content: &Value, is_error: bool) -> Option<Vec<ToolCallContent>> {
    match content {
        Value::Array(blocks) if !blocks.is_empty() => Some(
            blocks
                .iter()
                .map(|block| acp_content_block(block, is_error))
                .collect(),
        ),
        Value::Object(_) if content.get("type").is_some() => {
            Some(vec![acp_content_block(content, is_error)])
        }
        Value::String(text) if !text.is_empty() => {
            Some(vec![text_content(text_or_error(text.clone(), is_error))])
        }
        _ => None,
    }
}

/// Strip exactly one complete outer Markdown fence the way Node's
/// `exitPlanModeRawOutput` regex does (`exit-plan.ts:163-169`):
/// `/^\s*```[^\r\n]*\r?\n([\s\S]*?)\r?\n```\s*$/`. Leading whitespace, an
/// opening fence with an optional language tag, the body, and a closing fence
/// followed only by whitespace. Any other text is not a fence.
fn strip_outer_fence(text: &str) -> Option<&str> {
    let trimmed = text.trim_start_matches(char::is_whitespace);
    let after_open = trimmed.strip_prefix("```")?;
    let newline = after_open.find('\n')?;
    let raw_language = &after_open[..newline];
    // The regex allows a `\r` only as the first half of `\r?\n`.
    let language = raw_language.strip_suffix('\r').unwrap_or(raw_language);
    if language.contains(['\r', '\n']) {
        return None;
    }
    let body = &after_open[newline + 1..];
    let mut search_from = 0;
    while let Some(offset) = body[search_from..].find("\n```") {
        let index = search_from + offset;
        // `\r?\n` before the closing fence is separator, not body.
        let inner_end = if body[..index].ends_with('\r') {
            index - 1
        } else {
            index
        };
        if body[index + 4..].chars().all(char::is_whitespace) {
            return Some(&body[..inner_end]);
        }
        search_from = index + 1;
    }
    None
}

/// Node `exitPlanModeRawOutput` (`exit-plan.ts:163-169`): for an ExitPlanMode
/// result whose content is a string, strip exactly one complete outer fence;
/// every other value (including an absent content) is returned unchanged.
pub fn exit_plan_mode_raw_output(content: Option<&Value>) -> Option<Value> {
    match content {
        None => None,
        Some(Value::String(text)) => Some(Value::String(
            strip_outer_fence(text).map_or_else(|| text.clone(), str::to_string),
        )),
        Some(value) => Some(value.clone()),
    }
}

/// The ExitPlanMode result facts (`reporters/interaction.ts:51-72`) reduced to
/// the fields the renderer emits (`renderer.ts:438-516`).
#[derive(Debug, Clone, PartialEq)]
pub struct ExitPlanModeResult {
    /// `Some("Exited Plan Mode")` for an accepted plan; an error result has no
    /// title, so the call keeps "Approve Plan".
    pub title: Option<&'static str>,
    /// The display copy: a non-AIR client gets the fenced error text for a
    /// rejected plan. An accepted plan has no content (the raw output repeats
    /// the input).
    pub content: Option<Vec<ToolCallContent>>,
    /// The result the client shows: the unfenced error text, or the approval
    /// text. AIR gets the error as raw output only; the non-AIR renderer also
    /// sends it, while an accepted plan is raw output for a non-AIR client and
    /// stays out for AIR (`reporters/interaction.ts:55` sets it to undefined).
    pub raw_output: Option<Value>,
}

/// Build the ExitPlanMode result fields (`ExitPlanModeReporter.toolResult` /
/// `errorResult`, `reporters/interaction.ts:51-72`).
///
/// `renderer.result` treats an error result with text as the reporter's
/// `errorResult` and everything else as `toolResult`. The AIR `planFile` path
/// (a `planFilePath`/`planFileReleased` field and its clear-signal update) is
/// not threaded here yet (T-60/T-04).
pub fn exit_plan_mode_result(
    content: Option<&Value>,
    is_error: bool,
    air: bool,
) -> ExitPlanModeResult {
    // `resultFacts` only takes the error path for an error with non-empty
    // string/array content (`renderer.ts:359-368`).
    let has_error_text = is_error
        && match content {
            Some(Value::String(text)) => !text.is_empty(),
            Some(Value::Array(blocks)) => !blocks.is_empty(),
            _ => false,
        };
    if has_error_text {
        ExitPlanModeResult {
            title: None,
            // `errorResult` is AIR-only; every other client falls back to
            // `toAcpContentUpdate(content, true)` (a plain fenced error).
            content: if air {
                None
            } else {
                content.and_then(|c| generic_content(c, true))
            },
            raw_output: exit_plan_mode_raw_output(content),
        }
    } else {
        ExitPlanModeResult {
            title: Some("Exited Plan Mode"),
            content: None,
            raw_output: if air {
                None
            } else {
                exit_plan_mode_raw_output(content)
            },
        }
    }
}

/// Render the raw `tool_result.content` as ACP content blocks (upstream
/// `toAcpContentUpdate`, the `resultText` generic path): every block type goes
/// through [`acp_content_block`], a bare object with a `type` is one block, and
/// a non-empty string is text (fenced on error).
fn generic_content(result_content: &Value, is_error: bool) -> Option<Vec<ToolCallContent>> {
    to_acp_content_update(result_content, is_error)
}

/// An ACP image content block from an `{type:"image", source:{...}}` block
/// (`content.ts:146-168`). A base64 source becomes an ACP image; URL and file
/// sources cannot (ACP needs data), so they become a bracketed text placeholder
/// (fenced on error).
fn image_content(block: &Value, is_error: bool) -> ToolCallContent {
    let source = block.get("source");
    let source_type = source.and_then(|s| s.get("type")).and_then(Value::as_str);
    match source_type {
        Some("base64") => {
            let data = source
                .and_then(|s| s.get("data"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let mime = source
                .and_then(|s| s.get("media_type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            ToolCallContent::Content(Content::new(ContentBlock::Image(
                agent_client_protocol::schema::v1::ImageContent::new(data, mime),
            )))
        }
        Some("url") => text_content(text_or_error(
            format!("[image: {}]", js_string(source.and_then(|s| s.get("url")))),
            is_error,
        )),
        _ => text_content(text_or_error(
            "[image: file reference]".to_string(),
            is_error,
        )),
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
                title: "Search \"rust\"",
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
                title: "Approve Plan",
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
            let shape = tool_shape(c.name, &c.input, None);
            assert_eq!(shape.kind, c.kind, "{}: kind", c.name);
            assert_eq!(shape.title, c.title, "{}: title", c.name);
            assert_eq!(shape.locations.len(), c.locations, "{}: locations", c.name);
        }

        // Content spot-checks on the tools that carry it.
        let agent = tool_shape("Agent", &json!({"description": "d", "prompt": "go"}), None);
        assert_eq!(agent.content.len(), 1);
        assert_eq!(kind_of(&agent.content[0]), "content");

        let write = tool_shape(
            "Write",
            &json!({"file_path": "/tmp/a", "content": "hi"}),
            None,
        );
        assert_eq!(kind_of(&write.content[0]), "diff");

        let edit = tool_shape(
            "Edit",
            &json!({"file_path": "/tmp/a", "old_string": "x", "new_string": "y"}),
            None,
        );
        assert_eq!(kind_of(&edit.content[0]), "diff");
    }

    /// 9.T2 — an unknown tool falls back to the generic `Other` shape and never
    /// panics.
    #[test]
    fn inv_24_unknown_tool_falls_back() {
        let shape = tool_shape("DefinitelyNotATool", &json!({"a": 1}), None);
        assert_eq!(shape.kind, ToolKind::Other);
        assert_eq!(shape.title, "DefinitelyNotATool");
        assert!(shape.content.is_empty());

        // The `Other` tool itself renders its input as a json code block.
        let other = tool_shape("Other", &json!({"a": 1}), None);
        assert_eq!(other.kind, ToolKind::Other);
        assert_eq!(other.title, "Other");
        assert_eq!(other.content.len(), 1);
        assert_eq!(kind_of(&other.content[0]), "content");

        // A hostile/broken name must never panic.
        let _ = tool_shape("", &json!(null), None);
        let _ = tool_shape("Other", &json!("not-an-object"), None);
    }

    /// 9.1 — the `_meta.claudeCode` object carries toolName (2.10: only
    /// toolName — `subagent`/Bash `title`/`skill` moved to AIR-only meta).
    #[test]
    fn claude_code_meta_shapes() {
        let bash = claude_code_meta("Bash", &json!({"description": "run ls"}));
        assert_eq!(bash["toolName"], "Bash");
        assert!(bash.get("title").is_none(), "Bash title dropped (2.10)");

        let task = claude_code_meta("Task", &json!({}));
        assert!(
            task.get("subagent").is_none(),
            "subagent flag dropped (2.10)"
        );

        let skill = claude_code_meta("Skill", &json!({"skill": "coding"}));
        assert!(skill.get("skill").is_none(), "skill dropped (2.10)");
    }

    /// 2.10 (B2/X-9) — `_meta.claudeCode` is `{toolName}` only, unconditionally,
    /// for every client (Task subagent, Bash title, Skill name all dropped).
    #[test]
    fn inv_46_tool_meta_is_tool_name_only() {
        let cases = [
            ("Bash", json!({"description": "run ls"})),
            ("Task", json!({"description": "Do a subtask"})),
            ("Skill", json!({"skill": "coding"})),
            ("Read", json!({"file_path": "/tmp/a.txt"})),
        ];
        for (name, input) in cases {
            let meta = tool_meta(name, &input);
            assert_eq!(
                Value::Object(meta),
                serde_json::json!({"claudeCode": {"toolName": name}}),
                "2.10: _meta must be {{claudeCode:{{toolName}}}} for {name}"
            );
        }
    }

    /// 9.2 — tool-result content: Bash renders a console code block; a string
    /// falls through to generic content; an error wraps in a fence.
    #[test]
    fn tool_result_content_shapes() {
        let bash = tool_result_content("Bash", &json!({}), None, &json!("echo hi"), false).unwrap();
        assert_eq!(kind_of(&bash[0]), "content");

        let read = tool_result_content(
            "Read",
            &json!({}),
            None,
            &json!("line one\nline two"),
            false,
        )
        .unwrap();
        assert_eq!(kind_of(&read[0]), "content");

        // Empty Bash output surfaces nothing.
        assert!(tool_result_content("Bash", &json!({}), None, &json!("  "), false).is_none());

        // A Bash ERROR renders the plain ``` fence (Node:
        // `toAcpContentUpdate(content, true)`), not the console block — the
        // non-terminal client has no Bash `errorResult` (2.9 evidence).
        let bash_err = tool_result_content(
            "Bash",
            &json!({}),
            None,
            &json!("User refused permission to run tool"),
            true,
        )
        .unwrap();
        let ToolCallContent::Content(c) = &bash_err[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text block");
        };
        assert_eq!(
            t.text.as_str(),
            "```\nUser refused permission to run tool\n```",
            "a Bash deny/error uses a plain ``` fence, not ```console"
        );

        // Edit/Write/Skill results surface no content on success.
        assert!(tool_result_content("Edit", &json!({}), None, &json!("x"), false).is_none());
        assert!(tool_result_content("Write", &json!({}), None, &json!("x"), false).is_none());
        assert!(tool_result_content("Skill", &json!({}), None, &json!("x"), false).is_none());
        // An error result is the result to show (no reporter `errorResult`).
        assert_eq!(
            tool_result_content("Edit", &json!({}), None, &json!("rejected"), true)
                .unwrap()
                .len(),
            1
        );
    }

    /// PC-3 (T-58/T-59) — ExitPlanMode's reporter: "Approve Plan"/switch_mode
    /// with the plan as the display copy, and the accepted-plan completion as
    /// "Exited Plan Mode" with the unfenced raw output. A rejection unfences
    /// Claude's fenced reason: AIR gets it as raw output only, every other
    /// client also gets the fenced display copy (`renderer.ts:359-368`).
    #[test]
    fn inv_24_exit_plan_mode_shape_and_result() {
        let shape = tool_shape("ExitPlanMode", &json!({"plan": "do it"}), None);
        assert_eq!(shape.kind, ToolKind::SwitchMode);
        assert_eq!(shape.title, "Approve Plan");
        assert_eq!(shape.content.len(), 1);
        let ToolCallContent::Content(c) = &shape.content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(t.text.as_str(), "do it");
        // An absent or empty plan has no display copy.
        assert!(tool_shape("ExitPlanMode", &json!({}), None)
            .content
            .is_empty());
        assert!(tool_shape("ExitPlanMode", &json!({"plan": ""}), None)
            .content
            .is_empty());

        // Accepted plan, non-AIR: title + unfenced rawOutput, no content.
        let accepted = exit_plan_mode_result(Some(&json!("approved")), false, false);
        assert_eq!(accepted.title, Some("Exited Plan Mode"));
        assert!(accepted.content.is_none());
        assert_eq!(accepted.raw_output, Some(json!("approved")));

        // Accepted plan, AIR: the reporter sets `rawOutput` to undefined, so it
        // stays out.
        let air = exit_plan_mode_result(Some(&json!("approved")), false, true);
        assert_eq!(air.title, Some("Exited Plan Mode"));
        assert!(air.content.is_none());
        assert!(air.raw_output.is_none());

        // Rejection: Claude fences the reason; Node unfences it for rawOutput.
        let fenced = json!("```\nThe user wants to keep planning.\n```");
        let rejected = exit_plan_mode_result(Some(&fenced), true, false);
        assert!(rejected.title.is_none());
        assert_eq!(
            rejected.raw_output,
            Some(json!("The user wants to keep planning."))
        );
        let rejected_content = rejected
            .content
            .expect("non-AIR rejection has a display copy");
        let ToolCallContent::Content(c) = &rejected_content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        // `toAcpContentUpdate(content, true)` fences the already-fenced reason.
        assert_eq!(
            t.text.as_str(),
            "```\n```\nThe user wants to keep planning.\n```\n```"
        );

        // AIR rejection: raw output only, unfenced.
        let air_rejected = exit_plan_mode_result(Some(&fenced), true, true);
        assert!(air_rejected.content.is_none());
        assert_eq!(
            air_rejected.raw_output,
            Some(json!("The user wants to keep planning."))
        );

        // `is_error` without text takes the accepted-plan path
        // (`renderer.ts:359-368`).
        let empty_error = exit_plan_mode_result(Some(&json!("")), true, false);
        assert_eq!(empty_error.title, Some("Exited Plan Mode"));
        assert!(empty_error.content.is_none());
        assert_eq!(empty_error.raw_output, Some(json!("")));

        // Fence stripping: language tag, CRLF, surrounding whitespace, and
        // exactly one fence (a nested fence stays in the body).
        assert_eq!(
            exit_plan_mode_raw_output(Some(&json!("```markdown\nline\n```"))),
            Some(json!("line"))
        );
        assert_eq!(
            exit_plan_mode_raw_output(Some(&json!("  ```md\r\nx\r\n```  \n"))),
            Some(json!("x"))
        );
        assert_eq!(
            exit_plan_mode_raw_output(Some(&json!("```\n```\n```"))),
            Some(json!("```"))
        );
        assert_eq!(
            exit_plan_mode_raw_output(Some(&json!("plain text"))),
            Some(json!("plain text"))
        );
        assert_eq!(
            exit_plan_mode_raw_output(Some(&json!([{"type":"text","text":"x"}]))),
            Some(json!([{"type":"text","text":"x"}]))
        );
    }

    /// rust-air-gaps 1.4 (G11) — AIR `_meta`: `commandTitle` from the shell
    /// description, `subagent` for Agent/Task, `skill`, `mcp`, all stamped with
    /// the AIR `version`; nothing AIR-specific for other tools.
    #[test]
    fn inv_51_air_tool_meta_keys() {
        let bash = air_tool_meta(
            "Bash",
            &json!({"command": "ls", "description": "List"}),
            None,
        );
        assert_eq!(
            Value::Object(bash),
            json!({"claudeCode": {"toolName": "Bash"},
                   "jetbrains": {"air": {"version": 1, "commandTitle": "List"}}})
        );
        // `command_description` is the fallback; an empty description adds nothing.
        let fallback = air_tool_meta("PowerShell", &json!({"command_description": "Do"}), None);
        assert_eq!(fallback["jetbrains"]["air"]["commandTitle"], "Do");
        let empty = air_tool_meta("Bash", &json!({"description": ""}), None);
        assert!(empty.get("jetbrains").is_none());
        let agent = air_tool_meta("Agent", &json!({}), None);
        assert_eq!(
            agent["jetbrains"]["air"],
            json!({"version": 1, "subagent": true})
        );
        let task = air_tool_meta("Task", &json!({}), None);
        assert_eq!(task["jetbrains"]["air"]["subagent"], true);
        let skill = air_tool_meta("Skill", &json!({"skill": "demo"}), None);
        assert_eq!(skill["jetbrains"]["air"]["skill"], json!({"name": "demo"}));
        let mcp = air_tool_meta("mcp__srv__lookup__deep", &json!({}), None);
        assert_eq!(
            mcp["jetbrains"]["air"]["mcp"],
            json!({"server": "srv", "tool": "lookup__deep"})
        );
        // Other tools: claudeCode only.
        let read = air_tool_meta("Read", &json!({"file_path": "/a"}), None);
        assert_eq!(
            Value::Object(read),
            json!({"claudeCode": {"toolName": "Read"}})
        );
    }

    /// rust-air-gaps 1.4 — `mcp__<server>__<tool>` splitting.
    #[test]
    fn inv_51_mcp_tool_name_splits() {
        assert_eq!(mcp_tool_name("mcp__a__b"), Some(("a", "b")));
        assert_eq!(mcp_tool_name("mcp__a__b__c"), Some(("a", "b__c")));
        assert_eq!(mcp_tool_name("mcp____b"), None);
        assert_eq!(mcp_tool_name("mcp__a__"), None);
        assert_eq!(mcp_tool_name("mcp__a"), None);
        assert_eq!(mcp_tool_name("Bash"), None);
    }

    fn write_skill(dir: &Path, rel: &str, body: &str) -> PathBuf {
        let file = dir.join(rel);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, body).unwrap();
        file
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("skill-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// rust-air-gaps 1.4 — `resolveSkillPath` on a temp tree: project and home
    /// containers, frontmatter name mismatch, scoped / plugin layouts, commands
    /// shadowing, and rejection of path escapes.
    #[test]
    fn inv_51_resolve_skill_path_on_a_temp_tree() {
        let root = scratch("tree");
        let cwd = root.join("project");
        let home = root.join("home");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let cwd_ref = Some(cwd.as_path());
        let home_ref = Some(home.as_path());

        // Unknown skill, missing cwd.
        assert_eq!(resolve_skill_path_in("demo", cwd_ref, home_ref), None);
        let project_skill = write_skill(
            &cwd,
            ".claude/skills/demo/SKILL.md",
            "---\nname: demo\n---\nbody",
        );
        assert_eq!(
            resolve_skill_path_in("demo", cwd_ref, home_ref),
            Some(project_skill.clone())
        );
        assert_eq!(resolve_skill_path_in("demo", None, home_ref), None);

        // The project wins over the home container.
        write_skill(
            &home,
            ".claude/skills/demo/SKILL.md",
            "---\nname: demo\n---\n",
        );
        assert_eq!(
            resolve_skill_path_in("demo", cwd_ref, home_ref),
            Some(project_skill)
        );

        // Home only, no frontmatter name declared: accepted.
        let home_only = write_skill(&home, ".agents/skills/plain/SKILL.md", "no frontmatter");
        assert_eq!(
            resolve_skill_path_in("plain", cwd_ref, home_ref),
            Some(home_only)
        );

        // A directory declaring ANOTHER name is skipped; the declaring file wins.
        write_skill(
            &cwd,
            ".claude/skills/wrong/SKILL.md",
            "---\nname: other\n---\n",
        );
        let declared = write_skill(
            &cwd,
            ".claude/skills/dir-x/SKILL.md",
            "---\nname: \"wrong\" # c\n---\n",
        );
        assert_eq!(
            resolve_skill_path_in("wrong", cwd_ref, home_ref),
            Some(declared)
        );

        // `name:value` (no blank) declares nothing; `name:` nested is no name.
        write_skill(
            &cwd,
            ".claude/skills/nospace/SKILL.md",
            "---\nname:foo\n---\n",
        );
        assert_eq!(
            resolve_skill_path_in("nospace", cwd_ref, home_ref),
            Some(cwd.join(".claude/skills/nospace/SKILL.md"))
        );

        // Scoped: directory-scoped and plugin layouts.
        let scoped = write_skill(
            &cwd,
            "team/.claude/skills/build/SKILL.md",
            "---\nname: build\n---\n",
        );
        assert_eq!(
            resolve_skill_path_in("team:build", cwd_ref, home_ref),
            Some(scoped)
        );
        let plugin = write_skill(
            &cwd,
            ".claude/plugins/market/skills/lint/SKILL.md",
            "---\nname: lint\n---\n",
        );
        assert_eq!(
            resolve_skill_path_in("market:lint", cwd_ref, home_ref),
            Some(plugin)
        );
        let installed = write_skill(
            &home,
            ".claude/plugins/marketplaces/mp/plugins/acme/skills/fmt/SKILL.md",
            "---\nname: fmt\n---\n",
        );
        assert_eq!(
            resolve_skill_path_in("acme:fmt", cwd_ref, home_ref),
            Some(installed)
        );

        // A `.claude/commands` prompt command shadows the declared-name scan.
        write_skill(
            &cwd,
            ".claude/skills/some-dir/SKILL.md",
            "---\nname: cmd\n---\n",
        );
        write_skill(&cwd, ".claude/commands/cmd.md", "a command");
        assert_eq!(resolve_skill_path_in("cmd", cwd_ref, home_ref), None);

        // Escapes never reach the filesystem.
        for bad in ["../x", "a/b", "a\\b", "..", "", "../evil:x", "/abs:x", "s:"] {
            assert_eq!(resolve_skill_path_in(bad, cwd_ref, home_ref), None, "{bad}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// rust-air-gaps 1.4 — AIR shows a read or a search with a path as viewed
    /// files; other tools are not.
    #[test]
    fn inv_51_names_viewed_file() {
        assert!(names_viewed_file(
            "Read",
            &json!({"file_path": "/tmp/a.txt"})
        ));
        assert!(names_viewed_file(
            "Grep",
            &json!({"pattern": "x", "path": "/tmp"})
        ));
        assert!(names_viewed_file(
            "Grep",
            &json!({"pattern": "x", "file_path": "/tmp/a"})
        ));
        assert!(!names_viewed_file("Grep", &json!({"pattern": "x"})));
        assert!(!names_viewed_file(
            "Bash",
            &json!({"command": "ls", "path": "/tmp"})
        ));
    }

    /// D3 (T-73) — `toDisplayPath` goldens from
    /// `node -e require(dist/tool-calls/content.js).toDisplayPath`.
    #[test]
    fn inv_74_display_path_inside() {
        assert_eq!(
            display_path("/work/a.txt", Some(Path::new("/work"))),
            "a.txt"
        );
        assert_eq!(
            display_path("/work/sub/b.txt", Some(Path::new("/work"))),
            "sub/b.txt"
        );
    }

    #[test]
    fn inv_74_display_path_equal() {
        assert_eq!(display_path("/work", Some(Path::new("/work"))), "");
        // A trailing separator resolves to the same directory.
        assert_eq!(display_path("/work/", Some(Path::new("/work"))), "");
    }

    #[test]
    fn inv_74_display_path_outside() {
        assert_eq!(
            display_path("/tmp/outside.txt", Some(Path::new("/work"))),
            "/tmp/outside.txt"
        );
        // A sibling whose name merely starts with the cwd is not inside it.
        assert_eq!(
            display_path("/workspace/a.txt", Some(Path::new("/work"))),
            "/workspace/a.txt"
        );
    }

    #[test]
    fn inv_74_display_path_relative_input() {
        let cwd = std::env::current_dir().expect("cwd");
        // A relative input resolves against the process directory, so under
        // the same cwd it stays relative (Node `path.resolve`).
        assert_eq!(display_path("a.txt", Some(&cwd)), "a.txt");
        assert_eq!(display_path("sub/b.txt", Some(&cwd)), "sub/b.txt");
    }

    #[test]
    fn inv_74_display_path_no_cwd() {
        assert_eq!(display_path("/work/a.txt", None), "/work/a.txt");
    }

    /// D3 — the Read/Edit/Write titles use the cwd-relative path and keep the
    /// raw path in `locations` (`reporters/read.ts:11-24`, `file-edit.ts`).
    #[test]
    fn inv_74_tool_titles_are_cwd_relative() {
        let cwd = Some(Path::new("/work"));
        let read = tool_shape(
            "Read",
            &json!({"file_path": "/work/a.txt", "offset": 2, "limit": 3}),
            cwd,
        );
        assert_eq!(read.title, "Read a.txt (2 - 4)");
        assert_eq!(read.locations[0].path, PathBuf::from("/work/a.txt"));
        assert_eq!(read.locations[0].line, Some(2));

        let read_from = tool_shape(
            "Read",
            &json!({"file_path": "/work/a.txt", "offset": 7}),
            cwd,
        );
        assert_eq!(read_from.title, "Read a.txt (from line 7)");

        let edit = tool_shape(
            "Edit",
            &json!({"file_path": "/work/a.txt", "old_string": "a", "new_string": "b"}),
            cwd,
        );
        assert_eq!(edit.title, "Edit a.txt");

        let write = tool_shape(
            "Write",
            &json!({"file_path": "/work/a.txt", "content": "x"}),
            cwd,
        );
        assert_eq!(write.title, "Write a.txt");

        // Outside the cwd the absolute path is kept.
        let outside = tool_shape("Read", &json!({"file_path": "/tmp/outside.txt"}), cwd);
        assert_eq!(outside.title, "Read /tmp/outside.txt");
    }

    /// D4 (T-37) — goldens from `AcpToolCallRenderer.for({}).result(...)`
    /// (Node 0.94.0): the structured Read is numbered from `startLine`, the
    /// trailing newline is not a line, and the system-reminder in the raw text
    /// never leaks.
    #[test]
    fn inv_75_read_structured_numbered() {
        let structured = json!({
            "type": "text",
            "file": {"content": "one\ntwo\nthree", "startLine": 1, "numLines": 3, "totalLines": 3}
        });
        let raw = json!("     1\tone\n     2\ttwo\n<system-reminder>x</system-reminder>");
        let content = read_result_content(Some(&structured), &json!({}), &raw).unwrap();
        assert_eq!(content.len(), 1);
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(t.text.as_str(), "```\n1\tone\n2\ttwo\n3\tthree\n```");
    }

    #[test]
    fn inv_75_read_structured_start_line_and_trailing_newline() {
        let structured = json!({
            "type": "text",
            "file": {"content": "a\nb\n", "startLine": 5}
        });
        let content = read_result_content(Some(&structured), &json!({}), &json!("raw")).unwrap();
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(t.text.as_str(), "```\n5\ta\n6\tb\n```");
    }

    #[test]
    fn inv_75_read_structured_offset_fallback() {
        // No `startLine`: the tool input's `offset` is the first line.
        let structured = json!({"type": "text", "file": {"content": "a\nb"}});
        let content =
            read_result_content(Some(&structured), &json!({"offset": 9}), &json!("raw")).unwrap();
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(t.text.as_str(), "```\n9\ta\n10\tb\n```");
    }

    #[test]
    fn inv_75_read_structured_truncated_by_token_cap() {
        let structured = json!({
            "type": "text",
            "file": {"content": "a\nb", "startLine": 5, "numLines": 2, "totalLines": 9,
                     "truncatedByTokenCap": true}
        });
        let content = read_result_content(Some(&structured), &json!({}), &json!("raw")).unwrap();
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(
            t.text.as_str(),
            "```\n5\ta\n6\tb\n[File truncated: showing 2 of 9 lines]\n```"
        );

        // Without the line counts the banner keeps its short form.
        let structured = json!({
            "type": "text",
            "file": {"content": "a", "truncatedByTokenCap": true}
        });
        let content = read_result_content(Some(&structured), &json!({}), &json!("raw")).unwrap();
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(t.text.as_str(), "```\n1\ta\n[File truncated]\n```");
    }

    #[test]
    fn inv_75_read_empty_structured_keeps_raw_note() {
        // An empty file has nothing to number: the raw note stays.
        let structured = json!({"type": "text", "file": {"content": ""}});
        let content =
            read_result_content(Some(&structured), &json!({}), &json!("File is empty")).unwrap();
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(t.text.as_str(), "```\nFile is empty\n```");
    }

    #[test]
    fn inv_75_read_non_text_structured_falls_back() {
        // An image Read keeps the raw content blocks.
        let structured = json!({"type": "image"});
        let raw = json!([{"type": "image", "source": {"type": "base64", "data": "AAA",
                          "media_type": "image/png"}}]);
        let content = read_result_content(Some(&structured), &json!({}), &raw).unwrap();
        assert!(matches!(
            content[0],
            ToolCallContent::Content(ref c) if matches!(c.content, ContentBlock::Image(_))
        ));
    }

    #[test]
    fn inv_75_read_raw_array_escapes_each_text_block() {
        let raw = json!([{"type": "text", "text": "a```b"}, {"type": "text", "text": "c"}]);
        let content = read_result_content(None, &json!({}), &raw).unwrap();
        let texts: Vec<String> = content
            .iter()
            .map(|item| match item {
                ToolCallContent::Content(c) => match &c.content {
                    ContentBlock::Text(t) => t.text.to_string(),
                    _ => panic!("expected text"),
                },
                _ => panic!("expected content"),
            })
            .collect();
        assert_eq!(texts, vec!["```\na```b\n```", "```\nc\n```"]);
    }

    /// D4 — Bash goldens from the installed reporter/renderer.
    #[test]
    fn inv_75_bash_structured_stdout_stderr() {
        let structured = json!({"stdout": "out", "stderr": "err"});
        let content = bash_result_content(Some(&structured), &json!("raw"), false).unwrap();
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(t.text.as_str(), "```console\nout\nerr\n```");

        // Empty stderr drops out of the join; the raw text's extra suffix does
        // not survive the rebuild.
        let structured = json!({"stdout": "hi", "stderr": ""});
        let content = bash_result_content(Some(&structured), &json!("hi\nsuffix"), false).unwrap();
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(t.text.as_str(), "```console\nhi\n```");
    }

    #[test]
    fn inv_75_bash_structured_interrupted() {
        let structured = json!({"stdout": "out", "stderr": "", "interrupted": true});
        let content = bash_result_content(
            Some(&structured),
            &json!("[Command was aborted before completion]"),
            false,
        )
        .unwrap();
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(
            t.text.as_str(),
            "```console\nout\n[Command was aborted before completion]\n```"
        );

        // An interrupted command with no output still carries the note.
        let structured = json!({"stdout": "", "stderr": "", "interrupted": true});
        let content = bash_result_content(Some(&structured), &json!("raw"), false).unwrap();
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(
            t.text.as_str(),
            "```console\n[Command was aborted before completion]\n```"
        );
    }

    #[test]
    fn inv_75_bash_structured_persisted_output() {
        let structured = json!({
            "stdout": "out", "stderr": "",
            "persistedOutputPath": "/p/out", "persistedOutputSize": 1234
        });
        let content = bash_result_content(Some(&structured), &json!("raw"), false).unwrap();
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(
            t.text.as_str(),
            "```console\nout\n[Output truncated (1234 bytes total): full output saved to /p/out]\n```"
        );

        // Without a size the note keeps its short form.
        let structured = json!({"stdout": "out", "stderr": "", "persistedOutputPath": "/p/out"});
        let content = bash_result_content(Some(&structured), &json!("raw"), false).unwrap();
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(
            t.text.as_str(),
            "```console\nout\n[Output truncated: full output saved to /p/out]\n```"
        );
    }

    #[test]
    fn inv_75_bash_structured_image_and_background_skip() {
        // An image result keeps the raw image block, not a console fence.
        let structured = json!({"stdout": "", "stderr": "", "isImage": true});
        let raw = json!([{"type": "image", "source": {"type": "base64", "data": "AAA",
                          "media_type": "image/png"}}]);
        let content = bash_result_content(Some(&structured), &raw, false).unwrap();
        assert!(matches!(
            content[0],
            ToolCallContent::Content(ref c) if matches!(c.content, ContentBlock::Image(_))
        ));

        // A backgrounded command announces itself in the raw text; the
        // structured output is skipped (D16 / R11).
        let structured = json!({"stdout": "", "stderr": "", "backgroundTaskId": "b1"});
        let content = bash_result_content(
            Some(&structured),
            &json!("Command running in background with ID: b1"),
            false,
        )
        .unwrap();
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(
            t.text.as_str(),
            "```console\nCommand running in background with ID: b1\n```"
        );
    }

    #[test]
    fn inv_75_bash_error_uses_fenced_raw_text() {
        // No structured read on an error: the fenced raw text is the result.
        let structured = json!({"stdout": "", "stderr": "", "interrupted": true});
        let content = bash_result_content(
            Some(&structured),
            &json!("[Request interrupted by user for tool use]"),
            true,
        )
        .unwrap();
        let ToolCallContent::Content(c) = &content[0] else {
            panic!("expected content");
        };
        let ContentBlock::Text(t) = &c.content else {
            panic!("expected text");
        };
        assert_eq!(
            t.text.as_str(),
            "```\n[Request interrupted by user for tool use]\n```"
        );
    }

    #[test]
    fn inv_75_bash_empty_output_has_no_content() {
        assert!(bash_result_content(None, &json!(""), false).is_none());
        assert!(bash_result_content(None, &json!("   \n"), false).is_none());
        // A valid structured result with empty output also carries no content.
        let structured = json!({"stdout": "", "stderr": ""});
        assert!(bash_result_content(Some(&structured), &json!("raw"), false).is_none());
    }

    /// T-40 — the Write input aliases (`path`, `file_text`, `file_content`) and
    /// the canonical-key precedence (`normalizeWriteInput`).
    #[test]
    fn inv_75_write_input_aliases() {
        let write = tool_shape(
            "Write",
            &json!({"path": "/work/a.txt", "file_text": "hi"}),
            None,
        );
        assert_eq!(write.title, "Write /work/a.txt");
        let ToolCallContent::Diff(diff) = &write.content[0] else {
            panic!("expected diff");
        };
        assert_eq!(diff.path, PathBuf::from("/work/a.txt"));
        assert_eq!(diff.new_text, "hi");
        assert_eq!(diff.old_text, None);

        let write = tool_shape(
            "Write",
            &json!({"path": "/work/a.txt", "file_content": "ho"}),
            None,
        );
        let ToolCallContent::Diff(diff) = &write.content[0] else {
            panic!("expected diff");
        };
        assert_eq!(diff.new_text, "ho");

        // The canonical key wins when both spellings are present.
        let write = tool_shape(
            "Write",
            &json!({"file_path": "/work/a.txt", "content": "canonical",
                    "file_text": "alias"}),
            None,
        );
        let ToolCallContent::Diff(diff) = &write.content[0] else {
            panic!("expected diff");
        };
        assert_eq!(diff.new_text, "canonical");
    }

    /// T-42 — an empty `old_string` is no old text (`oldText: edit.old_string
    /// || null`), and the diff exists only when one side is non-empty.
    #[test]
    fn inv_75_edit_empty_old_string() {
        let edit = tool_shape(
            "Edit",
            &json!({"file_path": "/work/a.txt", "old_string": "", "new_string": "b"}),
            None,
        );
        let ToolCallContent::Diff(diff) = &edit.content[0] else {
            panic!("expected diff");
        };
        assert_eq!(diff.old_text, None, "an empty old_string is no old text");
        assert_eq!(diff.new_text, "b");

        // Both sides empty: no diff at all.
        let empty = tool_shape(
            "Edit",
            &json!({"file_path": "/work/a.txt", "old_string": "", "new_string": ""}),
            None,
        );
        assert!(empty.content.is_empty());

        // A missing new_string is an empty new text, the old text stays.
        let only_old = tool_shape(
            "Edit",
            &json!({"file_path": "/work/a.txt", "old_string": "a"}),
            None,
        );
        let ToolCallContent::Diff(diff) = &only_old.content[0] else {
            panic!("expected diff");
        };
        assert_eq!(diff.old_text.as_deref(), Some("a"));
        assert_eq!(diff.new_text, "");
    }

    /// The text of a single text content block.
    fn content_text(item: &ToolCallContent) -> &str {
        let ToolCallContent::Content(content) = item else {
            panic!("expected content, got {item:?}");
        };
        let ContentBlock::Text(text) = &content.content else {
            panic!("expected text, got {:?}", content.content);
        };
        text.text.as_str()
    }

    /// F6a / T-29 — Bash reads `description`, else `command_description`; an
    /// empty description adds no display copy, and an empty command is the
    /// "Terminal" title (`reporters/bash.ts:6-17,28`).
    #[test]
    fn f6a_bash_description_fallback_and_truthiness() {
        let fallback = tool_shape(
            "Bash",
            &json!({"command": "ls", "command_description": "List files"}),
            None,
        );
        assert_eq!(fallback.title, "ls");
        assert_eq!(fallback.content.len(), 1);
        assert_eq!(content_text(&fallback.content[0]), "List files");

        // The canonical key wins when both are present.
        let canonical = tool_shape(
            "Bash",
            &json!({"command": "ls", "description": "Canonical",
                    "command_description": "Alias"}),
            None,
        );
        assert_eq!(content_text(&canonical.content[0]), "Canonical");

        // An empty description is falsy: no display copy.
        let empty = tool_shape(
            "Bash",
            &json!({"command": "ls", "description": "", "command_description": ""}),
            None,
        );
        assert!(empty.content.is_empty());

        // An empty command falls back to the "Terminal" title.
        let no_command = tool_shape("Bash", &json!({"command": ""}), None);
        assert_eq!(no_command.title, "Terminal");
        assert_eq!(no_command.kind, ToolKind::Execute);
    }

    /// F6a / T-30 — PowerShell shares the Bash reporter (`reporters/index.ts:23`).
    #[test]
    fn f6a_powershell_shares_the_bash_reporter() {
        let shape = tool_shape(
            "PowerShell",
            &json!({"command": "Get-ChildItem", "command_description": "List files"}),
            None,
        );
        assert_eq!(shape.kind, ToolKind::Execute);
        assert_eq!(shape.title, "Get-ChildItem");
        assert_eq!(content_text(&shape.content[0]), "List files");

        let empty = tool_shape("PowerShell", &json!({}), None);
        assert_eq!(empty.title, "Terminal");
        assert!(empty.content.is_empty());
    }

    /// F6a / T-35 — the Read title is a truthy `limit > 0` range, else a truthy
    /// `offset` start line, else plain; an empty `file_path` is "File" with no
    /// locations (`reporters/read.ts:19-33`).
    #[test]
    fn f6a_read_title_truthiness() {
        let offset_only = tool_shape("Read", &json!({"file_path": "/w/a", "offset": 7}), None);
        assert_eq!(offset_only.title, "Read /w/a (from line 7)");

        let zero_limit = tool_shape(
            "Read",
            &json!({"file_path": "/w/a", "offset": 4, "limit": 0}),
            None,
        );
        assert_eq!(zero_limit.title, "Read /w/a (from line 4)");

        // `offset ?? 1`: an explicit 0 is kept (not nullish), so the range
        // starts at 0 and the location line is 0.
        let zero_offset = tool_shape(
            "Read",
            &json!({"file_path": "/w/a", "offset": 0, "limit": 2}),
            None,
        );
        assert_eq!(zero_offset.title, "Read /w/a (0 - 1)");
        assert_eq!(zero_offset.locations[0].line, Some(0));

        let no_path = tool_shape("Read", &json!({"file_path": ""}), None);
        assert_eq!(no_path.title, "Read File");
        assert!(no_path.locations.is_empty());
    }

    /// F6a / T-47 — an empty Glob path/pattern is absent: no label segment and
    /// no location (`reporters/search.ts:5-17`).
    #[test]
    fn f6a_glob_truthiness() {
        let empty = tool_shape("Glob", &json!({"path": "", "pattern": ""}), None);
        assert_eq!(empty.title, "Find");
        assert!(empty.locations.is_empty());

        let path_only = tool_shape("Glob", &json!({"path": "/w", "pattern": ""}), None);
        assert_eq!(path_only.title, "Find `/w`");
        assert_eq!(path_only.locations.len(), 1);
        assert_eq!(path_only.locations[0].path, PathBuf::from("/w"));

        // An undefined input (no fields) does not throw and gives bare "Find".
        assert_eq!(tool_shape("Glob", &json!({}), None).title, "Find");
    }

    /// F6a / T-48 — the Grep title is the equivalent grep command line: `-l` /
    /// `-c` flags name the output mode, `file_path` is a path alias, and the
    /// flags are tested for truthiness (`reporters/search.ts:20-67`).
    #[test]
    fn f6a_grep_command_line() {
        let files = tool_shape(
            "Grep",
            &json!({"pattern": "foo", "-l": true, "path": "/w"}),
            None,
        );
        assert_eq!(files.title, "grep -l \"foo\" /w");

        let count_string = tool_shape(
            "Grep",
            &json!({"pattern": "foo", "-c": "true", "file_path": "/w/src"}),
            None,
        );
        assert_eq!(count_string.title, "grep -c \"foo\" /w/src");

        // An explicit output_mode wins over the flags.
        let explicit = tool_shape(
            "Grep",
            &json!({"pattern": "foo", "-l": true, "output_mode": "count"}),
            None,
        );
        assert_eq!(explicit.title, "grep -c \"foo\"");

        // A non-`true` flag value is no flag.
        let other = tool_shape("Grep", &json!({"pattern": "foo", "-l": "yes"}), None);
        assert_eq!(other.title, "grep \"foo\"");

        // The flags are truthy values, not strict booleans.
        let truthy = tool_shape(
            "Grep",
            &json!({"pattern": "foo", "-i": "yes", "-n": 1, "multiline": "yes",
                    "-A": 2, "-B": null, "head_limit": 5, "glob": "*.rs",
                    "type": "rust"}),
            None,
        );
        assert_eq!(
            truthy.title,
            "grep -i -n -A 2 -B null | head -5 --include=\"*.rs\" --type=rust -P \"foo\""
        );

        // An empty path is absent; a non-string `path` falls back to `file_path`.
        let empty_path = tool_shape(
            "Grep",
            &json!({"pattern": "foo", "path": "", "file_path": "/w"}),
            None,
        );
        assert_eq!(empty_path.title, "grep \"foo\"");
        let alias = tool_shape(
            "Grep",
            &json!({"pattern": "foo", "path": null, "file_path": "/w"}),
            None,
        );
        assert_eq!(alias.title, "grep \"foo\" /w");

        // An undefined input (no fields) does not throw and gives bare "grep".
        assert_eq!(tool_shape("Grep", &json!({}), None).title, "grep");
    }

    /// F6a / T-49 — WebFetch appends "(from char N)" for a truthy offset and
    /// shows the prompt (`reporters/web.ts:9-21`).
    #[test]
    fn f6a_webfetch_offset_and_prompt() {
        let offset = tool_shape(
            "WebFetch",
            &json!({"url": "https://x.com/a", "offset": 120, "prompt": "summarize"}),
            None,
        );
        assert_eq!(offset.title, "Fetch https://x.com/a (from char 120)");
        assert_eq!(content_text(&offset.content[0]), "summarize");

        let zero = tool_shape(
            "WebFetch",
            &json!({"url": "https://x.com/a", "offset": 0, "prompt": ""}),
            None,
        );
        assert_eq!(zero.title, "Fetch https://x.com/a");
        assert!(zero.content.is_empty());

        let no_url = tool_shape("WebFetch", &json!({"url": ""}), None);
        assert_eq!(no_url.title, "Fetch");
    }

    /// F6a / T-50 — WebSearch is `Search "<query>"` or "Web search"; the domain
    /// filters are not part of the title (`reporters/web.ts:24-31`).
    #[test]
    fn f6a_websearch_title() {
        let query = tool_shape(
            "WebSearch",
            &json!({"query": "rust", "allowed_domains": ["a"], "blocked_domains": ["b"]}),
            None,
        );
        assert_eq!(query.title, "Search \"rust\"");
        assert_eq!(query.kind, ToolKind::Fetch);

        let empty = tool_shape("WebSearch", &json!({"query": ""}), None);
        assert_eq!(empty.title, "Web search");

        let missing = tool_shape("WebSearch", &json!({}), None);
        assert_eq!(missing.title, "Web search");
    }

    /// F6a / T-52 — Agent/Task appends " (<effort> effort)" unless the child is
    /// a fork (`reporters/agent.ts:8-19`).
    #[test]
    fn f6a_agent_effort_suffix() {
        let effort = tool_shape(
            "Agent",
            &json!({"description": "Review", "prompt": "go", "effort": "high"}),
            None,
        );
        assert_eq!(effort.title, "Review (high effort)");

        let fork = tool_shape(
            "Task",
            &json!({"description": "Review", "prompt": "go", "effort": "high",
                    "subagent_type": "fork"}),
            None,
        );
        assert_eq!(fork.title, "Review");

        let no_description = tool_shape("Agent", &json!({"effort": "low"}), None);
        assert_eq!(no_description.title, "Task (low effort)");

        let empty_effort = tool_shape(
            "Agent",
            &json!({"description": "Review", "effort": ""}),
            None,
        );
        assert_eq!(empty_effort.title, "Review");
    }

    /// F6a / T-61 — AIR gets the fixed AskUserQuestion title; every other
    /// client gets a single question (`reporters/interaction.ts:94-116`).
    #[test]
    fn f6a_ask_user_question_air_title() {
        let input = json!({"questions": [{"question": "ok?"}]});
        let plain = tool_shape_with("AskUserQuestion", &input, None, false);
        assert_eq!(plain.title, "ok?");

        let air = tool_shape_with("AskUserQuestion", &input, None, true);
        assert_eq!(air.title, "Asking for your input");
        assert_eq!(air.content.len(), 1);
        assert_eq!(content_text(&air.content[0]), "ok?");

        let empty_question = json!({"questions": [{"question": ""}, {"question": "second"}]});
        let plain = tool_shape("AskUserQuestion", &empty_question, None);
        assert_eq!(plain.title, "Asking for your input");
    }

    /// F6a / T-62 — an empty TodoWrite array still names the colon
    /// (`reporters/interaction.ts:139-146`).
    #[test]
    fn f6a_todowrite_empty_array() {
        let empty = tool_shape("TodoWrite", &json!({"todos": []}), None);
        assert_eq!(empty.title, "Update TODOs: ");

        let missing = tool_shape("TodoWrite", &json!({}), None);
        assert_eq!(missing.title, "Update TODOs");
    }

    /// F6a / T-64 — a finding line of 0 is falsy and no entry is dropped
    /// (`reporters/interaction.ts:165-185`).
    #[test]
    fn f6a_report_findings_line_truthiness() {
        let shape = tool_shape(
            "ReportFindings",
            &json!({"findings": [
                {"file": "a.rs", "line": 0, "summary": "zero"},
                {"file": "b.rs", "line": 3, "summary": "three"},
                {"summary": "no file"}
            ]}),
            None,
        );
        assert_eq!(shape.title, "Report 3 findings");
        let lines = shape.content.iter().map(content_text).collect::<Vec<_>>();
        assert_eq!(
            lines,
            vec![
                "**a.rs** — zero",
                "**b.rs:3** — three",
                "**undefined** — no file",
            ]
        );

        let none = tool_shape("ReportFindings", &json!({"findings": []}), None);
        assert_eq!(none.title, "Report findings: none found");
        assert!(none.content.is_empty());
    }

    /// F6a / T-66 — the "Other" input is a JSON fence with no newline before
    /// the closing fence (`reporters/interaction.ts:191-206`).
    #[test]
    fn f6a_other_json_fence() {
        let shape = tool_shape("Other", &json!({"a": 1}), None);
        assert_eq!(
            content_text(&shape.content[0]),
            "```json\n{\n  \"a\": 1\n}```"
        );

        let non_object = tool_shape("Other", &json!("text"), None);
        assert_eq!(content_text(&non_object.content[0]), "```json\n{}```");
    }

    /// F6a / T-67 — the agent-control tools title by name; AIR's error result is
    /// plain text, every other client's is fenced
    /// (`reporters/index.ts:36-40`, `reporters/interaction.ts:209-219`).
    #[test]
    fn f6a_agent_control_reporters() {
        for name in ["SendMessage", "TaskStop", "ListAgents", "Monitor"] {
            let shape = tool_shape(name, &json!({"anything": true}), None);
            assert_eq!(shape.title, name);
            assert_eq!(shape.kind, ToolKind::Other);
            assert!(shape.content.is_empty());
            assert!(is_agent_control_tool(name));
        }
        assert!(!is_agent_control_tool("Bash"));

        let air = agent_control_error_content(&json!("denied")).unwrap();
        assert_eq!(content_text(&air[0]), "denied");
    }

    /// F6a / T-38 / T-69 — content.ts block types: documents become compact
    /// placeholders, tool/web/code-execution/tool-reference blocks their text
    /// forms, and a bare object with a type is one block.
    #[test]
    fn f6a_content_block_types() {
        // Document placeholders, never the payload.
        let url_doc = generic_content(
            &json!({"type": "document",
                    "title": "report.pdf",
                    "source": {"type": "url", "url": "https://x.com/r.pdf"}}),
            false,
        )
        .unwrap();
        assert_eq!(
            content_text(&url_doc[0]),
            "[document \"report.pdf\": https://x.com/r.pdf]"
        );

        let base64_doc = generic_content(
            &json!({"type": "document",
                    "source": {"type": "base64", "media_type": "application/pdf",
                               "data": "A".repeat(4096)}}),
            false,
        )
        .unwrap();
        assert_eq!(
            content_text(&base64_doc[0]),
            "[document: application/pdf, 3.0 KB]"
        );

        let text_doc = generic_content(
            &json!({"type": "document",
                    "source": {"type": "text", "media_type": "text/plain", "data": "abcd"}}),
            false,
        )
        .unwrap();
        assert_eq!(content_text(&text_doc[0]), "[document: text/plain, 4 B]");

        let blocks = generic_content(
            &json!([
                {"type": "tool_reference", "tool_name": "Read"},
                {"type": "tool_search_tool_search_result",
                 "tool_references": [{"tool_name": "Read"}, {"tool_name": "Grep"}]},
                {"type": "tool_search_tool_result_error",
                 "error_code": "bad", "error_message": "nope"},
                {"type": "web_search_result", "title": "Rust", "url": "https://rust-lang.org"},
                {"type": "web_search_tool_result_error", "error_code": "unavailable"},
                {"type": "web_fetch_result", "url": "https://x.com"},
                {"type": "web_fetch_tool_result_error", "error_code": "denied"},
                {"type": "code_execution_result", "stdout": "out", "stderr": "err"},
                {"type": "bash_code_execution_result", "stdout": "", "stderr": "err2"},
                {"type": "code_execution_tool_result_error", "error_code": "timeout"},
                {"type": "bash_code_execution_tool_result_error", "error_code": "boom"},
                {"type": "text_editor_code_execution_view_result", "content": "body"},
                {"type": "text_editor_code_execution_create_result", "is_file_update": true},
                {"type": "text_editor_code_execution_create_result"},
                {"type": "text_editor_code_execution_str_replace_result", "lines": ["a", "b"]},
                {"type": "text_editor_code_execution_tool_result_error",
                 "error_code": "conflict", "error_message": "stale"}
            ]),
            false,
        )
        .unwrap();
        let texts = blocks.iter().map(content_text).collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec![
                "Tool: Read",
                "Tools found: Read, Grep",
                "Error: bad - nope",
                "Rust (https://rust-lang.org)",
                "Error: unavailable",
                "Fetched: https://x.com",
                "Error: denied",
                "Output: out",
                "Output: err2",
                "Error: timeout",
                "Error: boom",
                "body",
                "File updated",
                "File created",
                "a\nb",
                "Error: conflict - stale",
            ]
        );

        // A single object with a type is one block; an unknown type is JSON text.
        let object = generic_content(
            &json!({"type": "web_fetch_result", "url": "https://x.com"}),
            false,
        )
        .unwrap();
        assert_eq!(object.len(), 1);
        assert_eq!(content_text(&object[0]), "Fetched: https://x.com");

        let unknown = generic_content(&json!({"type": "mystery", "x": 1}), false).unwrap();
        assert_eq!(content_text(&unknown[0]), "{\"type\":\"mystery\",\"x\":1}");

        // A bare object without a type carries no content.
        assert!(generic_content(&json!({"x": 1}), false).is_none());

        // An error fences the text form.
        let error = generic_content(
            &json!({"type": "web_search_result", "title": "Rust", "url": "u"}),
            true,
        )
        .unwrap();
        assert_eq!(content_text(&error[0]), "```\nRust (u)\n```");
    }

    /// F6a / T-69 / T-70 — an image URL is a bracketed placeholder and is
    /// fenced on error; a base64 image becomes an ACP image.
    #[test]
    fn f6a_image_placeholders() {
        let url = generic_content(
            &json!({"type": "image", "source": {"type": "url", "url": "https://x.com/i.png"}}),
            false,
        )
        .unwrap();
        assert_eq!(content_text(&url[0]), "[image: https://x.com/i.png]");

        let error = generic_content(
            &json!({"type": "image", "source": {"type": "url", "url": "https://x.com/i.png"}}),
            true,
        )
        .unwrap();
        assert_eq!(
            content_text(&error[0]),
            "```\n[image: https://x.com/i.png]\n```"
        );

        let file =
            generic_content(&json!({"type": "image", "source": {"type": "file"}}), false).unwrap();
        assert_eq!(content_text(&file[0]), "[image: file reference]");

        let base64 = generic_content(
            &json!({"type": "image", "source": {"type": "base64",
                    "data": "aGk=", "media_type": "image/png"}}),
            false,
        )
        .unwrap();
        assert_eq!(kind_of(&base64[0]), "content");
        assert!(matches!(
            &base64[0],
            ToolCallContent::Content(c) if matches!(c.content, ContentBlock::Image(_))
        ));
    }

    /// F6a / T-74 — the fence run counts only backticks at the start of a line
    /// (`/^```+/gm`); an indented run does not widen the fence.
    #[test]
    fn f6a_markdown_escape_line_start_only() {
        assert_eq!(markdown_escape("plain"), "```\nplain\n```");
        assert_eq!(
            markdown_escape("```\ncode\n```"),
            "````\n```\ncode\n```\n````"
        );
        // An indented run is not at a line start.
        assert_eq!(markdown_escape("    ```\ncode"), "```\n    ```\ncode\n```");
        // A trailing newline is not doubled.
        assert_eq!(markdown_escape("a\n"), "```\na\n```");
    }

    /// F6a — Write and Edit treat an empty path/content as absent
    /// (`reporters/file-edit.ts:25-35,101-120`).
    #[test]
    fn f6a_write_edit_truthiness() {
        let write = tool_shape("Write", &json!({"file_path": "", "content": "x"}), None);
        assert_eq!(write.title, "Preparing file…");
        assert!(write.locations.is_empty());
        assert_eq!(write.content.len(), 1);
        assert_eq!(content_text(&write.content[0]), "x");

        let empty_content = tool_shape("Write", &json!({"file_path": "", "content": ""}), None);
        assert_eq!(empty_content.title, "Preparing file…");
        assert!(empty_content.content.is_empty());

        let edit = tool_shape("Edit", &json!({"file_path": ""}), None);
        assert_eq!(edit.title, "Edit");
        assert!(edit.locations.is_empty());
        assert!(edit.content.is_empty());
    }

    /// F6a / T-38 — a Read result with image and document blocks uses
    /// `toAcpContentBlock` for the non-text blocks
    /// (`reporters/read.ts:67-78`).
    #[test]
    fn f6a_read_result_document_and_image_blocks() {
        let raw = json!([
            {"type": "text", "text": "caption"},
            {"type": "document",
             "title": "report.pdf",
             "source": {"type": "base64", "media_type": "application/pdf",
                        "data": "A".repeat(1024)}},
            {"type": "image", "source": {"type": "url", "url": "https://x.com/i.png"}}
        ]);
        let content = read_result_content(None, &json!({}), &raw).unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content_text(&content[0]), "```\ncaption\n```");
        assert_eq!(
            content_text(&content[1]),
            "[document \"report.pdf\": application/pdf, 768 B]"
        );
        assert_eq!(content_text(&content[2]), "[image: https://x.com/i.png]");
    }

    /// F6a / T-30 — a PowerShell result is the shared Bash console block.
    #[test]
    fn f6a_powershell_result_console_block() {
        let content =
            tool_result_content("PowerShell", &json!({}), None, &json!("files"), false).unwrap();
        assert_eq!(content_text(&content[0]), "```console\nfiles\n```");
    }
}

/// The AIR permission presentation and the AIR `rawInput` (`presentation.ts`,
/// `renderer.ts:729`).
#[cfg(test)]
mod air_presentation_tests {
    use super::*;
    use serde_json::json;

    fn payload(meta: &Value) -> &Value {
        &meta["jetbrains"]["air"]["permission"]
    }

    #[test]
    fn permission_meta_title_description_and_default_to_no() {
        let meta = air_permission_meta(
            "Edit",
            &json!({}),
            None,
            "Edit  src/a.rs\n",
            Some("  rule\tmatched \u{7}here "),
            true,
        );
        assert_eq!(meta["jetbrains"]["air"]["version"], 1);
        assert_eq!(payload(&meta)["version"], 1);
        // Whitespace folded, as `humanText(..., singleLine)`.
        assert_eq!(payload(&meta)["title"], "Edit src/a.rs");
        // The description keeps its inner whitespace, drops control bytes.
        assert_eq!(payload(&meta)["description"], "Reason: rule\tmatched here");
        assert_eq!(payload(&meta)["defaultToNo"], true);

        let plain = air_permission_meta("Edit", &json!({}), None, "Edit a", None, false);
        assert!(payload(&plain).get("description").is_none());
        assert!(payload(&plain).get("defaultToNo").is_none());
    }

    #[test]
    fn permission_meta_special_titles() {
        let title = |name: &str, input: Value, display: Option<&str>, call: &str| {
            air_permission_meta(name, &input, display, call, None, false)["jetbrains"]["air"]
                ["permission"]["title"]
                .clone()
        };
        assert_eq!(
            title("ExitPlanMode", json!({}), None, "Ready?"),
            "Ready to code?"
        );
        // A shell command is executable text: not folded.
        assert_eq!(
            title("Bash", json!({}), None, "echo  'a  b'"),
            "echo  'a  b'"
        );
        assert_eq!(
            title(
                "SandboxNetworkAccess",
                json!({"host": "example.com"}),
                None,
                "x"
            ),
            "example.com"
        );
        assert_eq!(
            title(
                "mcp__computer-use__click",
                json!({}),
                Some("Click  here"),
                "x"
            ),
            "Click here"
        );
        // Blank or over-long titles fall back.
        assert_eq!(title("Read", json!({}), None, "   "), "Use tool?");
        assert_eq!(
            title("Read", json!({}), None, &"a".repeat(4_001)),
            "Use tool?"
        );
    }

    #[test]
    fn air_raw_input_drops_the_file_text_a_diff_holds() {
        let write = json!({"file_path": "/a", "file_text": "t", "content": null, "extra": 1});
        assert_eq!(
            air_raw_input("Write", &write),
            json!({"file_path": "/a", "content": null, "extra": 1})
        );
        // No path: no diff is built, so the text stays.
        let no_path = json!({"content": "t"});
        assert_eq!(air_raw_input("Write", &no_path), no_path);
        let edit =
            json!({"file_path": "/a", "old_string": "o", "new_string": "n", "replace_all": true});
        assert_eq!(
            air_raw_input("Edit", &edit),
            json!({"file_path": "/a", "replace_all": true})
        );
        let bash = json!({"command": "ls"});
        assert_eq!(air_raw_input("Bash", &bash), bash);
    }
}
