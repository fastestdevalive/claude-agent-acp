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
    let shape = tool_shape(name, input);
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
            // No terminal support. Node's Bash reporter only renders the
            // console code block for a NON-error command output; an error (e.g.
            // a deny message) falls back to the generic error fence
            // (`toAcpContentUpdate(content, true)`) — a plain ``` fence,
            // because a non-terminal client has no Bash `errorResult`
            // (reporters/bash.js:29-30, content.js:89).
            if is_error {
                generic_content(result_content, true)
            } else {
                let text = result_text(result_content).unwrap_or_default();
                let trimmed = text.trim_end();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(vec![text_content(format!("```console\n{}\n```", trimmed))])
                }
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
        let bash = tool_result_content("Bash", &json!("echo hi"), false).unwrap();
        assert_eq!(kind_of(&bash[0]), "content");

        let read = tool_result_content("Read", &json!("line one\nline two"), false).unwrap();
        assert_eq!(kind_of(&read[0]), "content");

        // Empty Bash output surfaces nothing.
        assert!(tool_result_content("Bash", &json!("  "), false).is_none());

        // A Bash ERROR renders the plain ``` fence (Node:
        // `toAcpContentUpdate(content, true)`), not the console block — the
        // non-terminal client has no Bash `errorResult` (2.9 evidence).
        let bash_err =
            tool_result_content("Bash", &json!("User refused permission to run tool"), true)
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

        // Edit/Write/Skill results surface no content.
        assert!(tool_result_content("Edit", &json!("x"), false).is_none());
        assert!(tool_result_content("Write", &json!("x"), false).is_none());
        assert!(tool_result_content("Skill", &json!("x"), false).is_none());
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
}
