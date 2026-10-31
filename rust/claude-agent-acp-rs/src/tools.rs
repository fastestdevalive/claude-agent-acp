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
    Content, ContentBlock, Diff, TextContent, ToolCallContent, ToolCallLocation, ToolKind,
};
use serde_json::Value;
use std::path::{Path, PathBuf};

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

/// Whether the tool is one of the Task* tools whose tool_use is suppressed.
pub fn is_task_tool(name: &str) -> bool {
    matches!(name, "TaskCreate" | "TaskUpdate" | "TaskList" | "TaskGet")
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

/// Per-tool ACP shape from a tool name + input (port of `toolInfoFromToolUse`,
/// `src/tools.ts:131-502`). `cwd` relativises the displayed file paths
/// (`toDisplayPath`, Decision 6); `supportsTerminalOutput` is always `false`.
pub fn tool_shape(name: &str, input: &Value, cwd: Option<&Path>) -> ToolShape {
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
            let content = if let Some(path) = write.file_path {
                // A client that negotiated diff patches gets the exact patch
                // from the approval preview / PostToolUse hook; the input does
                // not tell whether the file exists, so its live tool call shows
                // no diff (`reporters/file-edit.ts:33-55`). This crate serves
                // no v2 client and `diffPatch` only reaches AIR through the
                // session wiring; a non-AIR client always gets the input diff.
                vec![diff_content(path, None, write.content.unwrap_or(""))]
            } else if let Some(content) = write.content {
                vec![text_content(content)]
            } else {
                Vec::new()
            };
            let title = match write.file_path {
                Some(p) => format!("Write {}", display_path(p, cwd)),
                None => "Preparing file…".to_string(),
            };
            let locations = write
                .file_path
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
                Some(p) => format!("Edit {}", display_path(p, cwd)),
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
            // `ExitPlanModeReporter.toolUse` (`reporters/interaction.ts:37-48`):
            // the plan is input that the user approves, so its text is the
            // display copy for a non-AIR client (`renderer.ts:750-757`). An
            // AIR `planFile` client gets the path in `rawInput` instead; that
            // capability is not threaded into the renderer yet (T-60/T-04).
            let mut shape = ToolShape::new(ToolKind::SwitchMode, "Approve Plan");
            if let Some(plan) = input.get("plan").and_then(Value::as_str) {
                if !plan.is_empty() {
                    shape = shape.content(vec![text_content(plan)]);
                }
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
        "Bash" => bash_result_content(structured, result_content, is_error),
        "Agent" | "Task" => generic_content(result_content, is_error),
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
    if is_error {
        return generic_content(raw, true);
    }
    let mut output = String::new();
    let structured_used = match structured.filter(|value| value.is_object()) {
        Some(structured) => {
            let stdout = structured.get("stdout").and_then(Value::as_str);
            let stderr = structured.get("stderr").and_then(Value::as_str);
            let is_image = structured
                .get("isImage")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let background = structured
                .get("backgroundTaskId")
                .is_some_and(|value| !value.is_null());
            match (stdout, stderr) {
                (Some(stdout), Some(stderr)) if !is_image && !background => {
                    output = join_non_empty([stdout, stderr]);
                    if structured.get("interrupted").and_then(Value::as_bool) == Some(true) {
                        output = join_non_empty([
                            output.as_str(),
                            "[Command was aborted before completion]",
                        ]);
                    }
                    if let Some(path) = structured
                        .get("persistedOutputPath")
                        .and_then(Value::as_str)
                    {
                        let size = structured
                            .get("persistedOutputSize")
                            .and_then(Value::as_u64)
                            .map(|bytes| format!(" ({bytes} bytes total)"))
                            .unwrap_or_default();
                        output = join_non_empty([
                            output.as_str(),
                            &format!("[Output truncated{size}: full output saved to {path}]"),
                        ]);
                    }
                    true
                }
                _ => false,
            }
        }
        None => false,
    };
    if !structured_used {
        if let Some(block) = raw.as_object().filter(|object| {
            object.get("type").and_then(Value::as_str) == Some("bash_code_execution_result")
        }) {
            let stdout = block.get("stdout").and_then(Value::as_str).unwrap_or("");
            let stderr = block.get("stderr").and_then(Value::as_str).unwrap_or("");
            output = join_non_empty([stdout, stderr]);
        } else if let Some(text) = raw.as_str() {
            output = text.to_string();
        } else if let Some(blocks) = raw.as_array().filter(|blocks| !blocks.is_empty()) {
            let text_only = blocks
                .iter()
                .all(|block| block.get("text").and_then(Value::as_str).is_some());
            if !text_only {
                // Binary payloads cannot travel in the terminal channel.
                return to_acp_content_update(raw, false);
            }
            output = blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
    let trimmed = output.trim_end();
    if trimmed.trim().is_empty() {
        return None;
    }
    Some(vec![text_content(format!("```console\n{trimmed}\n```"))])
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
/// `content.ts:110-240`). Images are converted; a shape this crate does not
/// model falls back to its JSON text, like the upstream `default` arm.
fn acp_content_block(block: &Value, is_error: bool) -> ToolCallContent {
    if block.get("type").and_then(Value::as_str) == Some("image") {
        return image_content(block);
    }
    let text = serde_json::to_string(block).unwrap_or_default();
    text_content(text_or_error(text, is_error))
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
        Some("url") => format!(
            "[image: {}]",
            source
                .and_then(|s| s.get("url"))
                .and_then(Value::as_str)
                .unwrap_or("")
        ),
        _ => "[image: file reference]".to_string(),
    };
    text_content(text)
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
}
