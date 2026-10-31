//! Rust port of the Node adapter's permission option building and selection
//! effects (P7, Decision 7): `dist/permissions/options.js` +
//! `options/{filesystem,shell,tools,shared}.js` + `effects.js`.
//!
//! The Node `canUseTool` callback builds the ACP `session/request_permission`
//! `options` array and maps a client's selection back to the `control_response`
//! the CLI expects. This module ports both sides:
//!
//! - [`build_options`] produces the option set (ids, labels and `kind`) for a
//!   tool in Node's exact order, honouring the CLI's `defaultToNo` decline-first
//!   ordering.
//! - [`apply_selection`] maps a chosen option id to the selection effect: the
//!   `updatedPermissions` (the CLI's `permission_suggestions` verbatim where
//!   Node carries them, or a locally generated rule) plus the
//!   `decisionClassification`, or a deny with optional `interrupt`.
//!
//! Option ids stay Node's (`allow-once`, `allow-with-updates`,
//! `allow-skill-exact`, `allow-skill-prefix`, `reject`). ExitPlanMode's
//! context-reset variants are intentionally out of scope (they need a
//! fresh-query hand-off inside the turn machine — Decision 7 / Open question 4).

use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

/// Node's `PERMISSION_OPTION_ID` (`options/shared.js`).
pub const OPTION_ALLOW_ONCE: &str = "allow-once";
pub const OPTION_ALLOW_WITH_UPDATES: &str = "allow-with-updates";
pub const OPTION_ALLOW_SKILL_EXACT: &str = "allow-skill-exact";
pub const OPTION_ALLOW_SKILL_PREFIX: &str = "allow-skill-prefix";
pub const OPTION_REJECT: &str = "reject";
pub const OPTION_EXIT_PLAN_BYPASS: &str = "exit-plan-bypass";
pub const OPTION_EXIT_PLAN_AUTO: &str = "exit-plan-auto";
pub const OPTION_EXIT_PLAN_ACCEPT_EDITS: &str = "exit-plan-accept-edits";
pub const OPTION_EXIT_PLAN_DEFAULT: &str = "exit-plan-default";
pub const OPTION_EXIT_PLAN_CLEAR_AUTO: &str = "exit-plan-clear-auto";
pub const OPTION_EXIT_PLAN_CLEAR_BYPASS: &str = "exit-plan-clear-bypass";
pub const OPTION_EXIT_PLAN_CLEAR_ACCEPT_EDITS: &str = "exit-plan-clear-accept-edits";

/// A normalized durable permission change set (`{updates:[...]}`), Node's
/// `normalizeDurablePermissionChangeSet` snapshot of the CLI's
/// `permission_suggestions`.
#[derive(Debug, Clone)]
pub struct ChangeSet {
    pub updates: Vec<Value>,
}

/// The context Node passes to `buildClaudePermissionOptions` /
/// `applyClaudePermissionSelection`.
#[derive(Debug, Clone)]
pub struct OptionsContext {
    /// The programmatic tool name (`toolName`).
    pub tool_name: String,
    /// The optional display label (`displayName`).
    pub display_name: Option<String>,
    /// The tool input object.
    pub input: Value,
    /// The session working directory (`session.cwd`).
    pub cwd: PathBuf,
    /// The validated suggestion change set, if any.
    pub change_set: Option<ChangeSet>,
    /// `true` when `matchedAskRule` is present or `suppressAlwaysAllowRule`
    /// (`noPersistentRule`): no persistent "always allow" option may be
    /// offered.
    pub no_persistent_rule: bool,
    /// Node's `defaultToNo` hint: lead with the reject options.
    pub default_to_no: bool,
    /// The set of available mode ids (`sessionModes.availableModeIds`), used by
    /// ExitPlanMode to decide which elevated modes (auto / bypass) to offer.
    pub available_modes: Vec<String>,
    /// Whether the current model lacks Auto (`sessionModes.isAutoUnavailable`).
    /// Node's `applyPermissionFallback` (`session-mode.ts:196-215`) rewrites a
    /// `setMode auto` allow to Accept edits, and the caller publishes the
    /// fallback warning before the mode update.
    pub auto_unavailable: bool,
    /// The session's pre-plan mode (`session.prePlanMode`), used by
    /// ExitPlanMode to lead with bypass when the session was in bypass before
    /// entering plan mode.
    pub pre_plan_mode: Option<String>,
    /// ExitPlanMode's `(contextUsedPercent% used)` suffix, `None` when the
    /// context window is unknown.
    pub context_used_percent: Option<u32>,
    /// The user's home directory (Node `os.homedir()`), threaded from the
    /// hermetic environment snapshot (never `std::env` here).
    pub home_dir: Option<PathBuf>,
}

impl OptionsContext {
    /// Node's `allowPersistentOptions` (the negation of `noPersistentRule`).
    fn allow_persistent_options(&self) -> bool {
        !self.no_persistent_rule
    }
}

const MAX_UPDATES: usize = 100;
const MAX_RULES_PER_UPDATE: usize = 100;
const MAX_DIRECTORIES_PER_UPDATE: usize = 100;
const MAX_TOOL_NAME_LENGTH: usize = 512;
const MAX_RULE_CONTENT_LENGTH: usize = 10_000;
const MAX_DIRECTORY_LENGTH: usize = 4_096;

/// Normalize a raw `permission_suggestions` value into a [`ChangeSet`], or
/// `None` when Node would refuse it (invalid, empty, too many updates, or a
/// forced ask). Port of `normalizeDurablePermissionChangeSet`
/// (`permissions/normalization.js`).
pub fn normalize_change_set(suggestions: &Value, no_persistent_rule: bool) -> Option<ChangeSet> {
    if no_persistent_rule {
        return None;
    }
    let updates = suggestions.as_array()?;
    if updates.is_empty() || updates.len() > MAX_UPDATES || !updates.iter().all(is_known_update) {
        return None;
    }
    Some(ChangeSet {
        updates: updates.clone(),
    })
}

/// Node's `safePlainString`: non-blank, bounded, no control characters.
fn safe_plain_string(value: &Value, max_len: usize) -> bool {
    let Some(text) = value.as_str() else {
        return false;
    };
    !text.trim().is_empty()
        && text.encode_utf16().count() <= max_len
        && !text.trim().chars().any(|c| {
            let code = c as u32;
            code <= 0x08
                || code == 0x0b
                || code == 0x0c
                || (0x0e..=0x1f).contains(&code)
                || code == 0x7f
        })
}

/// Node's `isKnownUpdate` (`normalization.js`).
fn is_known_update(update: &Value) -> bool {
    let Some(obj) = update.as_object() else {
        return false;
    };
    let destination_ok = matches!(
        obj.get("destination").and_then(Value::as_str),
        Some("session" | "cliArg" | "userSettings" | "projectSettings" | "localSettings")
    );
    if !destination_ok {
        return false;
    }
    match obj.get("type").and_then(Value::as_str) {
        Some(kind @ ("addRules" | "replaceRules" | "removeRules")) => {
            let Some(rules) = obj.get("rules").and_then(Value::as_array) else {
                return false;
            };
            rules.len() <= MAX_RULES_PER_UPDATE
                && (kind == "replaceRules" || !rules.is_empty())
                && rules.iter().all(|rule| {
                    rule.as_object().is_some_and(|r| {
                        r.get("toolName")
                            .is_some_and(|t| safe_plain_string(t, MAX_TOOL_NAME_LENGTH))
                            && r.get("ruleContent")
                                .is_none_or(|c| safe_plain_string(c, MAX_RULE_CONTENT_LENGTH))
                    })
                })
                && matches!(
                    obj.get("behavior").and_then(Value::as_str),
                    Some("allow" | "deny" | "ask")
                )
        }
        Some("setMode") => matches!(
            obj.get("mode").and_then(Value::as_str),
            Some("default" | "acceptEdits" | "bypassPermissions" | "plan" | "dontAsk" | "auto")
        ),
        Some("addDirectories" | "removeDirectories") => obj
            .get("directories")
            .and_then(Value::as_array)
            .is_some_and(|dirs| {
                !dirs.is_empty()
                    && dirs.len() <= MAX_DIRECTORIES_PER_UPDATE
                    && dirs
                        .iter()
                        .all(|d| safe_plain_string(d, MAX_DIRECTORY_LENGTH))
            }),
        _ => false,
    }
}

/// Node's `plainString` (`options/shared.js`).
fn plain_string(value: Option<&Value>) -> Option<String> {
    value?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Node's option constructors (`options/shared.js`).
fn allow_once(name: &str) -> Value {
    json!({ "optionId": OPTION_ALLOW_ONCE, "name": name, "kind": "allow_once" })
}

fn allow_with_updates(name: &str) -> Value {
    json!({ "optionId": OPTION_ALLOW_WITH_UPDATES, "name": name, "kind": "allow_always" })
}

fn reject(name: &str) -> Value {
    json!({ "optionId": OPTION_REJECT, "name": name, "kind": "reject_once" })
}

/// Node's `withOptionalUpdate` (`options/shared.js`): a `[Yes, (allow-with-updates,) No]`
/// set where the middle option is only present when a change set + label is
/// offered.
fn with_optional_update(change_set: Option<&ChangeSet>, update_name: Option<&str>) -> Vec<Value> {
    let mut options = vec![allow_once("Yes")];
    if let (Some(_cs), Some(name)) = (change_set, update_name) {
        options.push(allow_with_updates(name));
    }
    options.push(reject("No"));
    options
}

/// Node's `withGeneratedUpdate` (`options/shared.js`): a persistent option is
/// always offered with a locally generated rule.
fn with_generated_update(name: &str) -> Vec<Value> {
    vec![allow_once("Yes"), allow_with_updates(name), reject("No")]
}

/// Normalize a rule path for label display (Node `permissionPath`).
fn permission_path(rule_content: Option<&Value>) -> Option<String> {
    let value = plain_string(rule_content)?;
    Some(value.replace("/**", "").replace("\\**", ""))
}

/// Node's `isInside` (`options/filesystem.js`).
fn is_inside(directory: &Path, candidate: &Path) -> bool {
    let Ok(rel) = candidate.strip_prefix(directory) else {
        return false;
    };
    match rel.components().next() {
        None => true, // equal
        Some(Component::ParentDir) => false,
        Some(_) => true,
    }
}

/// Build the ACP `options` array for a `can_use_tool` request, port of
/// `buildClaudePermissionOptions` + `buildUnsortedClaudePermissionOptions`
/// (`dist/permissions/options.js`). Node sorts on every call: approvals
/// before "other" before the reject options, and the decline-first order when
/// `defaultToNo` is set (`options.ts:25-33`).
pub fn build_options(ctx: &OptionsContext) -> Vec<Value> {
    let mut options = build_unsorted_options(ctx);
    options.sort_by_key(|opt| option_order(opt, ctx.default_to_no));
    options
}

fn build_unsorted_options(ctx: &OptionsContext) -> Vec<Value> {
    // Node checks the computer-use MCP prefix BEFORE the tool-name switch.
    if is_computer_use_mcp_tool(&ctx.tool_name) {
        return build_computer_use_mcp_options(ctx);
    }
    match ctx.tool_name.as_str() {
        "Bash" => build_shell_options(ctx, "Bash"),
        "PowerShell" => build_shell_options(ctx, "PowerShell"),
        "Read" => build_file_options(ctx, "file_path", "read"),
        "Glob" => build_file_options(ctx, "path", "read"),
        "Grep" => build_file_options(ctx, "path", "read"),
        "Edit" => build_file_options(ctx, "file_path", "write"),
        "Write" => build_file_options(ctx, "file_path", "write"),
        "NotebookEdit" => build_file_options(ctx, "notebook_path", "write"),
        "WebFetch" => build_webfetch_options(ctx),
        "Skill" => build_skill_options(ctx),
        "EnterPlanMode" => build_enter_plan_mode_options(),
        "ExitPlanMode" => build_exit_plan_mode_options(ctx),
        "SandboxNetworkAccess" => build_sandbox_network_options(ctx),
        _ => build_fallback_options(ctx),
    }
}

fn build_file_options(ctx: &OptionsContext, input_key: &str, operation: &str) -> Vec<Value> {
    let label = file_session_label(ctx, input_key, operation);
    with_optional_update(ctx.change_set.as_ref(), label.as_deref())
}

fn build_shell_options(ctx: &OptionsContext, tool_name: &str) -> Vec<Value> {
    let change_set = ctx.change_set.as_ref();
    let label = change_set.and_then(|cs| shell_suggestions_label(tool_name, cs));
    with_optional_update(change_set, label.as_deref())
}

fn build_webfetch_options(ctx: &OptionsContext) -> Vec<Value> {
    if !ctx.no_persistent_rule {
        if let Some(url) = plain_string(ctx.input.get("url")) {
            if let Some(hostname) = url_hostname(&url) {
                return with_generated_update(&format!("Yes, and don't ask again for {hostname}"));
            }
        }
    }
    vec![allow_once("Yes"), reject("No")]
}

fn build_skill_options(ctx: &OptionsContext) -> Vec<Value> {
    let skill = plain_string(ctx.input.get("skill"));
    let mut options = vec![allow_once("Yes")];
    if let Some(skill) = skill {
        if !ctx.no_persistent_rule {
            options.push(json!({
                "optionId": OPTION_ALLOW_SKILL_EXACT,
                "name": format!("Yes, and don't ask again for {skill}"),
                "kind": "allow_always",
            }));
            if let Some(space) = skill.find(' ') {
                if space > 0 {
                    let prefix = format!("{}:*", &skill[..space]);
                    options.push(json!({
                        "optionId": OPTION_ALLOW_SKILL_PREFIX,
                        "name": format!("Yes, and don't ask again for {prefix} commands"),
                        "kind": "allow_always",
                    }));
                }
            }
        }
    }
    options.push(reject("No"));
    options
}

fn build_enter_plan_mode_options() -> Vec<Value> {
    vec![
        allow_once("Yes, enter plan mode"),
        reject("No, start implementing now"),
    ]
}

/// Node's `isComputerUseMcpTool` (`options/tools.js`).
fn is_computer_use_mcp_tool(tool_name: &str) -> bool {
    tool_name.starts_with("mcp__computer-use__")
}

/// Node `buildComputerUseMcpPermissionOptions` (`options/tools.js`): the
/// computer-use MCP tools get the MCP "don't ask again" option (label without
/// the `commands` suffix), gated on an unrestricted allow change set. The
/// selection effect is `applyCommonSelection` (Node `effects.js` `mcp__` arm).
fn build_computer_use_mcp_options(ctx: &OptionsContext) -> Vec<Value> {
    let tool_label = ctx
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| ctx.tool_name.clone());
    let change_set = if ctx.allow_persistent_options() {
        ctx.change_set
            .clone()
            .filter(|cs| is_mcp_allow_change_set(cs, &ctx.tool_name))
    } else {
        None
    };
    let label = change_set
        .as_ref()
        .map(|_| format!("Yes, and don't ask again for {tool_label}"));
    with_optional_update(change_set.as_ref(), label.as_deref())
}

/// Node `buildSandboxNetworkPermissionOptions` (`options/tools.js`): the
/// durable option is offered only when the change set is exactly one
/// `localSettings` allow rule matching the requested host.
fn build_sandbox_network_options(ctx: &OptionsContext) -> Vec<Value> {
    let host = plain_string(ctx.input.get("host"));
    let label = host.as_deref().and_then(|h| {
        exact_local_allow_rule(ctx.change_set.as_ref(), &ctx.tool_name, h)
            .then(|| format!("Yes, and don't ask again for {h}"))
    });
    with_optional_update(ctx.change_set.as_ref(), label.as_deref())
}

/// Node `exactLocalAllowRule` (`options/shared.js`): `true` exactly when the
/// change set is a single `localSettings` allow rule for `tool_name` whose
/// trimmed `ruleContent` equals `rule_content`.
fn exact_local_allow_rule(
    change_set: Option<&ChangeSet>,
    tool_name: &str,
    rule_content: &str,
) -> bool {
    let Some(change_set) = change_set else {
        return false;
    };
    if change_set.updates.len() != 1 {
        return false;
    }
    let update = &change_set.updates[0];
    if update.get("type").and_then(Value::as_str) != Some("addRules")
        || update.get("behavior").and_then(Value::as_str) != Some("allow")
        || update.get("destination").and_then(Value::as_str) != Some("localSettings")
    {
        return false;
    }
    let Some(rules) = update.get("rules").and_then(Value::as_array) else {
        return false;
    };
    rules.len() == 1
        && rules[0].get("toolName").and_then(Value::as_str) == Some(tool_name)
        && plain_string(rules[0].get("ruleContent")).as_deref() == Some(rule_content)
}

/// Node `buildExitPlanModePermissionOptions` (`options/tools.js`): the
/// elevated (auto / bypass) modes lead, then the default manual-approve
/// option, then "No, keep planning". When `input.plan` is present an
/// additional clear-context option leads the set (Decision 7 / open-question 4
/// kept the clear-context hand-off itself out of scope, but the option is
/// still offered exactly as Node does and its selection maps to the fresh-query
/// interrupt).
fn build_exit_plan_mode_options(ctx: &OptionsContext) -> Vec<Value> {
    let modes: std::collections::HashSet<&str> =
        ctx.available_modes.iter().map(|s| s.as_str()).collect();
    let mut options: Vec<Value> = Vec::new();
    // Claude Code offers bypass only when launched with it; otherwise auto
    // leads. Order flips when the session was in bypass before plan mode.
    let elevated: Vec<&str> = if ctx.pre_plan_mode.as_deref() == Some("bypassPermissions") {
        ["bypassPermissions", "auto"]
    } else {
        ["auto", "bypassPermissions"]
    }
    .into_iter()
    .filter(|mode| modes.contains(mode))
    .collect();
    let preferred = elevated.first().copied().unwrap_or("acceptEdits");
    if plain_string(ctx.input.get("plan")).is_some() {
        let usage = ctx
            .context_used_percent
            .map(|pct| format!(" ({pct}% used)"))
            .unwrap_or_default();
        let (id, name) = match preferred {
            "auto" => (
                OPTION_EXIT_PLAN_CLEAR_AUTO,
                format!("Yes, clear context{usage} and use auto mode"),
            ),
            "bypassPermissions" => (
                OPTION_EXIT_PLAN_CLEAR_BYPASS,
                format!("Yes, clear context{usage} and bypass permissions"),
            ),
            _ => (
                OPTION_EXIT_PLAN_CLEAR_ACCEPT_EDITS,
                format!("Yes, clear context{usage} and auto-accept edits"),
            ),
        };
        options.push(json!({ "optionId": id, "name": name, "kind": "allow_always" }));
    }
    for mode in &elevated {
        let (id, name) = match *mode {
            "auto" => (OPTION_EXIT_PLAN_AUTO, "Yes, and use auto mode".to_string()),
            _ => (
                OPTION_EXIT_PLAN_BYPASS,
                "Yes, and bypass permissions".to_string(),
            ),
        };
        options.push(json!({ "optionId": id, "name": name, "kind": "allow_always" }));
    }
    if elevated.is_empty() {
        options.push(json!({
            "optionId": OPTION_EXIT_PLAN_ACCEPT_EDITS,
            "name": "Yes, auto-accept edits",
            "kind": "allow_always",
        }));
    }
    options.push(json!({
        "optionId": OPTION_EXIT_PLAN_DEFAULT,
        "name": "Yes, manually approve edits",
        "kind": "allow_once",
    }));
    options.push(reject("No, keep planning"));
    options
}

fn build_fallback_options(ctx: &OptionsContext) -> Vec<Value> {
    let tool_label = ctx
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| ctx.tool_name.clone());
    if ctx.tool_name.starts_with("mcp__") {
        let change_set = if ctx.no_persistent_rule {
            None
        } else {
            ctx.change_set
                .clone()
                .filter(|cs| is_mcp_allow_change_set(cs, &ctx.tool_name))
        };
        let label = change_set
            .as_ref()
            .map(|_| format!("Yes, and don't ask again for {tool_label} commands"));
        with_optional_update(change_set.as_ref(), label.as_deref())
    } else if ctx.no_persistent_rule {
        vec![allow_once("Yes"), reject("No")]
    } else {
        with_generated_update(&format!(
            "Yes, and don't ask again for {tool_label} commands"
        ))
    }
}

/// Extract a URL's hostname, mirroring Node `new URL(input).hostname`.
/// Requires an absolute URL with a `scheme://host` authority; returns `None`
/// when Node would throw (no scheme / invalid).
fn url_hostname(url: &str) -> Option<String> {
    // Find the authority after `scheme://`.
    let after_scheme = url.find("://")?;
    let authority = &url[after_scheme + 3..];
    // Cut at the first path/query/fragment delimiter.
    let authority = authority.split(['/', '?', '#']).next().unwrap_or(authority);
    if authority.is_empty() {
        return None;
    }
    // Strip userinfo (`user:pass@`).
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    // Strip port (`:port`). Keep a bracketed IPv6 host intact (`[::1]:8080` ->
    // `[::1]`), matching Node's `new URL(url).hostname` which keeps the brackets.
    let host = if let Some(rest) = hostport.strip_prefix('[') {
        let end = rest.find(']')?;
        format!("[{}]", &rest[..end])
    } else {
        hostport.split(':').next().unwrap_or(hostport).to_string()
    };
    let host = host.to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Node `option_order` / `declineFirstOptionOrder` (`options.js`).
fn option_order(option: &Value, decline_first: bool) -> usize {
    let kind = option.get("kind").and_then(Value::as_str).unwrap_or("");
    if decline_first && matches!(kind, "reject_once" | "reject_always") {
        return 0;
    }
    match kind {
        "allow_once" => 1,
        "allow_always" => 2,
        "reject_once" | "reject_always" => 4,
        _ => 3,
    }
}

/// Node `isMcpAllowChangeSet` (`options/shared.js`): every update adds an
/// unrestricted allow rule for `tool_name`.
fn is_mcp_allow_change_set(change_set: &ChangeSet, tool_name: &str) -> bool {
    !change_set.updates.is_empty()
        && change_set.updates.iter().all(|update| {
            update.get("type").and_then(Value::as_str) == Some("addRules")
                && update.get("behavior").and_then(Value::as_str) == Some("allow")
                && update
                    .get("rules")
                    .and_then(Value::as_array)
                    .is_some_and(|rules| {
                        !rules.is_empty()
                            && rules.iter().all(|rule| {
                                rule.get("toolName").and_then(Value::as_str) == Some(tool_name)
                                    && rule.get("ruleContent").and_then(Value::as_str).is_none()
                            })
                    })
        })
}

// --- filesystem.js: `fileSessionLabel` ---

fn is_file_session_change_set(change_set: &ChangeSet, operation: &str) -> bool {
    change_set.updates.iter().all(|update| {
        if update.get("destination").and_then(Value::as_str) != Some("session") {
            return false;
        }
        match update.get("type").and_then(Value::as_str) {
            Some("setMode") => {
                operation == "write"
                    && update.get("mode").and_then(Value::as_str) == Some("acceptEdits")
            }
            Some("addDirectories") => operation == "write",
            Some("addRules") if update.get("behavior").and_then(Value::as_str) == Some("allow") => {
                update
                    .get("rules")
                    .and_then(Value::as_array)
                    .is_some_and(|rules| {
                        rules.iter().all(|rule| {
                            let wanted = if operation == "read" { "Read" } else { "Edit" };
                            rule.get("toolName").and_then(Value::as_str) == Some(wanted)
                        })
                    })
            }
            _ => false,
        }
    })
}

fn input_path(ctx: &OptionsContext, key: &str) -> Option<String> {
    plain_string(ctx.input.get(key))
}

fn has_session_allow_rule_for(change_set: &ChangeSet, tool_name: &str) -> bool {
    change_set.updates.iter().any(|update| {
        update.get("type").and_then(Value::as_str) == Some("addRules")
            && update.get("destination").and_then(Value::as_str) == Some("session")
            && update.get("behavior").and_then(Value::as_str) == Some("allow")
            && update
                .get("rules")
                .and_then(Value::as_array)
                .is_some_and(|rules| {
                    rules
                        .iter()
                        .any(|rule| rule.get("toolName").and_then(Value::as_str) == Some(tool_name))
                })
    })
}

fn has_broad_session_allow_rule_for(change_set: &ChangeSet, tool_name: &str) -> bool {
    change_set.updates.iter().any(|update| {
        update.get("type").and_then(Value::as_str) == Some("addRules")
            && update.get("destination").and_then(Value::as_str) == Some("session")
            && update.get("behavior").and_then(Value::as_str) == Some("allow")
            && update
                .get("rules")
                .and_then(Value::as_array)
                .is_some_and(|rules| {
                    rules.iter().any(|rule| {
                        rule.get("toolName").and_then(Value::as_str) == Some(tool_name)
                            && rule.get("ruleContent").and_then(Value::as_str).is_none()
                    })
                })
    })
}

fn has_accept_edits_mode(change_set: &ChangeSet) -> bool {
    change_set.updates.iter().any(|update| {
        update.get("type").and_then(Value::as_str) == Some("setMode")
            && update.get("destination").and_then(Value::as_str) == Some("session")
            && update.get("mode").and_then(Value::as_str) == Some("acceptEdits")
    })
}

/// Node `effectCoversFilePath` (`options/filesystem.js`).
fn effect_covers_file_path(ctx: &OptionsContext, file_path: &Path, operation: &str) -> bool {
    let directory = file_path.parent().unwrap_or(Path::new(""));
    let comparable = |candidate: &str| -> bool {
        let resolved = ctx.cwd.join(candidate);
        resolved == directory || is_inside(&resolved, file_path)
    };
    let Some(change_set) = ctx.change_set.as_ref() else {
        return false;
    };
    change_set
        .updates
        .iter()
        .flat_map(|update| {
            if update.get("type").and_then(Value::as_str) == Some("addDirectories") {
                let dirs: Vec<String> = update
                    .get("directories")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                return dirs;
            }
            if update.get("type").and_then(Value::as_str) != Some("addRules")
                || update.get("behavior").and_then(Value::as_str) != Some("allow")
            {
                return Vec::new();
            }
            update
                .get("rules")
                .and_then(Value::as_array)
                .map(|rules| {
                    rules
                        .iter()
                        .filter(|rule| {
                            let wanted = if operation == "read" { "Read" } else { "Edit" };
                            rule.get("toolName").and_then(Value::as_str) == Some(wanted)
                        })
                        .filter_map(|rule| permission_path(rule.get("ruleContent")))
                        .collect()
                })
                .unwrap_or_default()
        })
        .any(|p| comparable(&p))
}

/// Node `fileSessionLabel` (`options/filesystem.js`): the durable label for
/// read/write tools, or `None` when the change set is not a file session set.
fn file_session_label(ctx: &OptionsContext, input_key: &str, operation: &str) -> Option<String> {
    let change_set = ctx.change_set.as_ref()?;
    if !is_file_session_change_set(change_set, operation) {
        return None;
    }
    let file_path = input_path(ctx, input_key).map(|p| ctx.cwd.join(&p));
    let resolved_file_path = file_path.as_deref();
    let is_claude_path = operation == "write"
        && resolved_file_path
            .map(|p| {
                let cwd_claude = ctx.cwd.join(".claude");
                let home_claude = ctx.home_dir.clone().unwrap_or_default().join(".claude");
                is_inside(&cwd_claude, p) || is_inside(&home_claude, p)
            })
            .unwrap_or(false);
    let broad_rule = has_broad_session_allow_rule_for(
        change_set,
        if operation == "read" { "Read" } else { "Edit" },
    );
    if let Some(p) = resolved_file_path {
        let covers = effect_covers_file_path(ctx, p, operation)
            || broad_rule
            || (operation == "write" && !is_claude_path && has_accept_edits_mode(change_set));
        if !covers {
            return None;
        }
    } else if !broad_rule && !(operation == "write" && has_accept_edits_mode(change_set)) {
        return None;
    }
    if operation == "write"
        && resolved_file_path.is_some()
        && is_claude_path
        && has_session_allow_rule_for(change_set, "Edit")
    {
        return Some("Yes, and allow Claude to edit its own settings for this session".to_string());
    }
    let in_cwd = resolved_file_path
        .map(|p| is_inside(&ctx.cwd, p))
        .unwrap_or(false);
    if resolved_file_path.is_none() || in_cwd {
        return Some(if operation == "read" {
            "Yes, during this session".to_string()
        } else {
            "Yes, allow all edits during this session".to_string()
        });
    }
    let directory = resolved_file_path
        .and_then(|p| p.parent())
        .unwrap_or(Path::new(""));
    let directory_name = directory
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| "this directory".to_string());
    Some(if operation == "read" {
        format!("Yes, allow reading from {directory_name}/ during this session")
    } else {
        format!("Yes, allow all edits in {directory_name}/ during this session")
    })
}

// --- shell.js: `shellSuggestionsLabel` + display helpers ---

fn permission_rule_prefix(value: &str) -> &str {
    value.strip_suffix(":*").unwrap_or(value)
}

fn display_list(values: &[&str]) -> String {
    if values.join(", ").encode_utf16().count() > 50 {
        return "similar".to_string();
    }
    match values {
        [one] => (*one).to_string(),
        [a, b] => format!("{a} and {b}"),
        [init @ .., last] => format!("{}, and {last}", init.join(", ")),
        [] => String::new(),
    }
}

/// Plain-text port of Claude Code's `generateShellSuggestionsLabel`
/// (`options/shell.js`). Only the commands-only form (no read paths /
/// directories) is reproduced; the path-shortening forms fall back to `None`
/// (Yes/No) as the existing `durable_bash` port documented (PARITY C).
fn shell_suggestions_label(tool_name: &str, change_set: &ChangeSet) -> Option<String> {
    let representable = change_set.updates.iter().all(|update| {
        if update.get("type").and_then(Value::as_str) == Some("addDirectories") {
            return update.get("destination").and_then(Value::as_str) == Some("session");
        }
        if update.get("type").and_then(Value::as_str) != Some("addRules")
            || update.get("behavior").and_then(Value::as_str) != Some("allow")
        {
            return false;
        }
        update
            .get("rules")
            .and_then(Value::as_array)
            .is_some_and(|rules| {
                rules.iter().all(|rule| {
                    let name = rule.get("toolName").and_then(Value::as_str);
                    (name == Some(tool_name) || name == Some("Read"))
                        && plain_string(rule.get("ruleContent")).is_some()
                })
            })
    });
    if !representable {
        return None;
    }
    let rules: Vec<Value> = change_set
        .updates
        .iter()
        .filter(|u| u.get("type").and_then(Value::as_str) == Some("addRules"))
        .flat_map(|u| {
            u.get("rules")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    let has_read_paths = rules
        .iter()
        .any(|rule| rule.get("toolName").and_then(Value::as_str) == Some("Read"));
    let has_directories = change_set
        .updates
        .iter()
        .any(|u| u.get("type").and_then(Value::as_str) == Some("addDirectories"));
    if has_read_paths || has_directories {
        // Path forms are not reproduced (PARITY C): Yes/No fallback.
        return None;
    }
    let mut commands: Vec<&str> = Vec::new();
    for rule in rules
        .iter()
        .filter(|rule| rule.get("toolName").and_then(Value::as_str) == Some(tool_name))
    {
        if let Some(content) = rule.get("ruleContent").and_then(Value::as_str) {
            let prefix = permission_rule_prefix(content);
            if !prefix.is_empty() && !commands.contains(&prefix) {
                commands.push(prefix);
            }
        }
    }
    if commands.is_empty() {
        return None;
    }
    Some(format!(
        "Yes, and don't ask again for {} commands",
        display_list(&commands)
    ))
}

// --- selection effects (`dist/permissions/effects.js`) ---

/// The result of applying a permission selection (the `control_response`
/// `response.response` body, minus the framing).
#[derive(Debug, Clone, PartialEq)]
pub enum SelectionOutcome {
    Allow {
        updated_permissions: Option<Vec<Value>>,
        permanent: bool,
    },
    Deny {
        message: String,
        interrupt: bool,
    },
}

/// Apply a chosen option id to produce the selection effect, port of
/// `applyClaudePermissionSelection` + helpers (`dist/permissions/effects.js`).
/// Returns `Err` with a tool-independent reason for an unknown option id or an
/// invalid input.
pub fn apply_selection(option_id: &str, ctx: &OptionsContext) -> Result<SelectionOutcome, String> {
    match ctx.tool_name.as_str() {
        "Skill" => apply_skill_selection(option_id, ctx),
        "WebFetch" => {
            apply_generated_durable_selection(option_id, ctx, || web_fetch_permission_update(ctx))
        }
        "Read"
        | "Bash"
        | "PowerShell"
        | "Glob"
        | "Grep"
        | "Edit"
        | "Write"
        | "NotebookEdit"
        | "EnterPlanMode"
        | "SandboxNetworkAccess" => apply_common_selection(option_id, ctx),
        "ExitPlanMode" => apply_exit_plan_mode_selection(option_id, ctx),
        _ => {
            if ctx.tool_name.starts_with("mcp__") {
                apply_common_selection(option_id, ctx)
            } else {
                apply_generated_durable_selection(option_id, ctx, || {
                    Ok(local_allow_rule_update(&ctx.tool_name, None))
                })
            }
        }
    }
}

fn apply_skill_selection(
    option_id: &str,
    ctx: &OptionsContext,
) -> Result<SelectionOutcome, String> {
    let skill = skill_name(ctx)?;
    match option_id {
        OPTION_ALLOW_SKILL_EXACT => Ok(SelectionOutcome::Allow {
            updated_permissions: Some(local_allow_rule_update("Skill", Some(skill))),
            permanent: true,
        }),
        OPTION_ALLOW_SKILL_PREFIX => {
            let space_index = skill
                .find(' ')
                .ok_or_else(|| "Skill prefix permission requires arguments".to_string())?;
            if space_index == 0 {
                return Err("Skill prefix permission requires arguments".to_string());
            }
            Ok(SelectionOutcome::Allow {
                updated_permissions: Some(local_allow_rule_update(
                    "Skill",
                    Some(format!("{}:*", &skill[..space_index])),
                )),
                permanent: true,
            })
        }
        _ => apply_common_selection(option_id, ctx),
    }
}

fn skill_name(ctx: &OptionsContext) -> Result<String, String> {
    let value = ctx
        .input
        .get("skill")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if value.is_empty() || value.encode_utf16().count() > 1_000 {
        return Err("Invalid Skill permission input".to_string());
    }
    Ok(value)
}

fn local_allow_rule_update(tool_name: &str, rule_content: Option<String>) -> Vec<Value> {
    let rule = match rule_content {
        Some(content) => json!({ "toolName": tool_name, "ruleContent": content }),
        None => json!({ "toolName": tool_name }),
    };
    vec![json!({
        "type": "addRules",
        "rules": [rule],
        "behavior": "allow",
        "destination": "localSettings",
    })]
}

fn web_fetch_permission_update(ctx: &OptionsContext) -> Result<Vec<Value>, String> {
    let url = ctx
        .input
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "Invalid WebFetch permission input".to_string())?;
    let hostname =
        url_hostname(url).ok_or_else(|| "Invalid WebFetch permission input".to_string())?;
    Ok(local_allow_rule_update(
        "WebFetch",
        Some(format!("domain:{hostname}")),
    ))
}

fn apply_common_selection(
    option_id: &str,
    ctx: &OptionsContext,
) -> Result<SelectionOutcome, String> {
    match option_id {
        OPTION_ALLOW_ONCE => Ok(SelectionOutcome::Allow {
            updated_permissions: None,
            permanent: false,
        }),
        OPTION_ALLOW_WITH_UPDATES => {
            let updates = ctx
                .change_set
                .as_ref()
                .map(|cs| cs.updates.clone())
                .ok_or_else(|| "Invalid durable permission selection".to_string())?;
            Ok(SelectionOutcome::Allow {
                updated_permissions: Some(updates),
                permanent: true,
            })
        }
        OPTION_REJECT => Ok(SelectionOutcome::Deny {
            message: "User refused permission to run tool".to_string(),
            interrupt: false,
        }),
        _ => Err(format!("Unknown permission option: {option_id}")),
    }
}

/// Node `exitPlanMode` (`effects.js`): map an ExitPlanMode option id to the
/// session mode to activate, or `None` for ids without a mode.
pub fn exit_plan_mode(option_id: &str) -> Option<&'static str> {
    match option_id {
        OPTION_EXIT_PLAN_BYPASS => Some("bypassPermissions"),
        OPTION_EXIT_PLAN_AUTO => Some("auto"),
        OPTION_EXIT_PLAN_ACCEPT_EDITS => Some("acceptEdits"),
        OPTION_EXIT_PLAN_DEFAULT => Some("default"),
        _ => None,
    }
}

/// Node `exitPlanClearContextMode` (`effects.js`): the context-reset variants
/// map to the mode the fresh query should resume in.
fn exit_plan_clear_context_mode(option_id: &str) -> Option<&'static str> {
    match option_id {
        OPTION_EXIT_PLAN_CLEAR_AUTO => Some("auto"),
        OPTION_EXIT_PLAN_CLEAR_BYPASS => Some("bypassPermissions"),
        OPTION_EXIT_PLAN_CLEAR_ACCEPT_EDITS => Some("acceptEdits"),
        _ => None,
    }
}

/// Node `applyExitPlanModeSelection` (`effects.js`): selecting an elevated
/// mode or the default runs a session `setMode` (classified permanent except
/// for the default), "No, keep planning" interrupts the turn, and the
/// clear-context variants deny with an interrupt so the adapter can hand off
/// to a fresh query.
fn apply_exit_plan_mode_selection(
    option_id: &str,
    ctx: &OptionsContext,
) -> Result<SelectionOutcome, String> {
    if let Some(mode) = exit_plan_clear_context_mode(option_id) {
        // The adapter consumes this interrupt and continues the same ACP turn
        // on a fresh Claude query (Node `effects.js:101-106`).
        let _ = mode;
        return Ok(SelectionOutcome::Deny {
            message: "User accepted the plan and requested a fresh context".to_string(),
            interrupt: true,
        });
    }
    if let Some(mode) = exit_plan_mode(option_id) {
        let permanent = mode != "default";
        return Ok(SelectionOutcome::Allow {
            updated_permissions: Some(vec![json!({
                "type": "setMode",
                "mode": mode,
                "destination": "session",
            })]),
            permanent,
        });
    }
    if option_id == OPTION_REJECT {
        // A regular deny would let Claude keep planning; interrupt stops this
        // ACP turn (Node `effects.js:111-116`).
        return Ok(SelectionOutcome::Deny {
            message: "User chose to keep planning".to_string(),
            interrupt: true,
        });
    }
    apply_common_selection(option_id, ctx)
}

/// Node's `decodeClaudePermissionResponse` "was this option offered" guard
/// (`permissions/response.js:5-8`): a selected id that was not among the
/// offered options is rejected the way Node throws (`Permission option was not
/// offered: <id>`).
pub fn option_was_offered(ctx: &OptionsContext, option_id: &str) -> bool {
    build_options(ctx)
        .iter()
        .any(|option| option.get("optionId").and_then(Value::as_str) == Some(option_id))
}

fn apply_generated_durable_selection(
    option_id: &str,
    ctx: &OptionsContext,
    updates: impl FnOnce() -> Result<Vec<Value>, String>,
) -> Result<SelectionOutcome, String> {
    if option_id != OPTION_ALLOW_WITH_UPDATES {
        return apply_common_selection(option_id, ctx);
    }
    Ok(SelectionOutcome::Allow {
        updated_permissions: Some(updates()?),
        permanent: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx(tool_name: &str, input: Value) -> OptionsContext {
        OptionsContext {
            tool_name: tool_name.to_string(),
            display_name: None,
            input,
            cwd: PathBuf::from("/repo"),
            change_set: None,
            no_persistent_rule: false,
            default_to_no: false,
            available_modes: Vec::new(),
            auto_unavailable: false,
            pre_plan_mode: None,
            context_used_percent: None,
            home_dir: None,
        }
    }

    /// An ExitPlanMode-capable context with the given available modes and
    /// pre-plan mode.
    fn exit_ctx(
        input: Value,
        available_modes: &[&str],
        pre_plan_mode: Option<&str>,
    ) -> OptionsContext {
        OptionsContext {
            available_modes: available_modes.iter().map(|s| s.to_string()).collect(),
            pre_plan_mode: pre_plan_mode.map(String::from),
            ..ctx("ExitPlanMode", input)
        }
    }

    fn with_changeset(mut c: OptionsContext, updates: Vec<Value>) -> OptionsContext {
        c.change_set = Some(ChangeSet { updates });
        c
    }

    fn ids(options: &[Value]) -> Vec<String> {
        options
            .iter()
            .map(|o| o["optionId"].as_str().unwrap_or("").to_string())
            .collect()
    }

    /// P7 — Edit/Write offer a session-scoped "allow all edits during this
    /// session" `allow-with-updates` option when the CLI sent an acceptEdits
    /// setMode change set (Node `fileSessionLabel`), in Yes / allow-with-updates /
    /// No order.
    #[test]
    fn inv_60_options_edit_write_session_changeset() {
        let edits = ctx("Edit", json!({ "file_path": "src/a.rs" }));
        let edits = with_changeset(
            edits,
            vec![json!({"type":"setMode","mode":"acceptEdits","destination":"session"})],
        );
        let options = build_options(&edits);
        assert_eq!(
            json!(options),
            json!([
                { "optionId": "allow-once", "name": "Yes", "kind": "allow_once" },
                { "optionId": "allow-with-updates", "name": "Yes, allow all edits during this session", "kind": "allow_always" },
                { "optionId": "reject", "name": "No", "kind": "reject_once" },
            ])
        );

        // Same change set on Write carries the same durable label.
        let write = with_changeset(
            ctx("Write", json!({ "file_path": "out.txt" })),
            vec![json!({"type":"setMode","mode":"acceptEdits","destination":"session"})],
        );
        let options = build_options(&write);
        assert_eq!(
            options[1],
            json!({ "optionId": "allow-with-updates", "name": "Yes, allow all edits during this session", "kind": "allow_always" })
        );

        // Without a change set, Edit offers Yes/No only.
        let plain_edit = build_options(&ctx("Edit", json!({ "file_path": "a.rs" })));
        assert_eq!(ids(&plain_edit), vec!["allow-once", "reject"]);
    }

    /// P7 — Read/Glob/Grep offer the "Yes, during this session" durable option
    /// when a session Read allow rule covers the current path, and Yes/No only
    /// otherwise.
    #[test]
    fn inv_60_options_read_glob_grep() {
        let read = with_changeset(
            ctx("Read", json!({ "file_path": "src/a.rs" })),
            vec![json!({
                "type":"addRules",
                "rules":[{"toolName":"Read","ruleContent":"/repo/**"}],
                "behavior":"allow",
                "destination":"session"
            })],
        );
        let options = build_options(&read);
        assert_eq!(
            json!(options),
            json!([
                { "optionId": "allow-once", "name": "Yes", "kind": "allow_once" },
                { "optionId": "allow-with-updates", "name": "Yes, during this session", "kind": "allow_always" },
                { "optionId": "reject", "name": "No", "kind": "reject_once" },
            ])
        );

        // Glob and Grep share the read file-option path.
        for tool in ["Glob", "Grep"] {
            let g = with_changeset(
                ctx(tool, json!({ "path": "src" })),
                vec![json!({
                    "type":"addRules",
                    "rules":[{"toolName":"Read","ruleContent":"/repo/**"}],
                    "behavior":"allow",
                    "destination":"session"
                })],
            );
            let options = build_options(&g);
            assert_eq!(options[1]["name"], "Yes, during this session");
        }

        // Without a session change set, Read offers Yes/No only.
        let plain = build_options(&ctx("Read", json!({ "file_path": "a.rs" })));
        assert_eq!(ids(&plain), vec!["allow-once", "reject"]);
    }

    /// P7 — WebFetch offers a generated domain rule, Skill offers exact/prefix
    /// persistent options, and a generic tool offers a generated localSettings
    /// rule; all keep the Yes/No tails.
    #[test]
    fn inv_60_options_webfetch_skill_generic() {
        let webfetch = build_options(&ctx(
            "WebFetch",
            json!({ "url": "https://example.com/doc" }),
        ));
        assert_eq!(
            json!(webfetch),
            json!([
                { "optionId": "allow-once", "name": "Yes", "kind": "allow_once" },
                { "optionId": "allow-with-updates", "name": "Yes, and don't ask again for example.com", "kind": "allow_always" },
                { "optionId": "reject", "name": "No", "kind": "reject_once" },
            ])
        );

        let skill = build_options(&ctx("Skill", json!({ "skill": "github-cli auth" })));
        assert_eq!(
            json!(skill),
            json!([
                { "optionId": "allow-once", "name": "Yes", "kind": "allow_once" },
                { "optionId": "allow-skill-exact", "name": "Yes, and don't ask again for github-cli auth", "kind": "allow_always" },
                { "optionId": "allow-skill-prefix", "name": "Yes, and don't ask again for github-cli:* commands", "kind": "allow_always" },
                { "optionId": "reject", "name": "No", "kind": "reject_once" },
            ])
        );

        let generic = build_options(&ctx("SomeTool", json!({})));
        assert_eq!(
            json!(generic),
            json!([
                { "optionId": "allow-once", "name": "Yes", "kind": "allow_once" },
                { "optionId": "allow-with-updates", "name": "Yes, and don't ask again for SomeTool commands", "kind": "allow_always" },
                { "optionId": "reject", "name": "No", "kind": "reject_once" },
            ])
        );
    }

    /// P7 — an MCP tool offers `allow-with-updates` only when every provider
    /// update adds an unrestricted allow rule for that tool (`isMcpAllowChangeSet`);
    /// otherwise (or with a non-allow change set) it is Yes/No only.
    #[test]
    fn inv_60_options_mcp_changeset_only() {
        let mut c = ctx("mcp__server__read", json!({}));
        c.display_name = Some("Read tool".to_string());
        let mcp = with_changeset(
            c,
            vec![json!({
                "type":"addRules",
                "rules":[{"toolName":"mcp__server__read"}],
                "behavior":"allow",
                "destination":"localSettings"
            })],
        );
        let options = build_options(&mcp);
        assert_eq!(
            json!(options),
            json!([
                { "optionId": "allow-once", "name": "Yes", "kind": "allow_once" },
                { "optionId": "allow-with-updates", "name": "Yes, and don't ask again for Read tool commands", "kind": "allow_always" },
                { "optionId": "reject", "name": "No", "kind": "reject_once" },
            ])
        );

        // The same tool with only Yes/No available (no durable change set).
        let mut c = ctx("mcp__server__write", json!({}));
        c.display_name = Some("Write tool".to_string());
        let plain = build_options(&c);
        assert_eq!(ids(&plain), vec!["allow-once", "reject"]);
    }

    /// P7 — `defaultToNo` leads with the reject option (decline-first ordering);
    /// the normal order leads with the approvals.
    #[test]
    fn inv_60_options_default_to_no_order() {
        let base = with_changeset(
            ctx("Edit", json!({ "file_path": "src/a.rs" })),
            vec![json!({"type":"setMode","mode":"acceptEdits","destination":"session"})],
        );
        let normal = build_options(&base);
        assert_eq!(
            ids(&normal),
            vec!["allow-once", "allow-with-updates", "reject"]
        );

        let mut declined = base;
        declined.default_to_no = true;
        let options = build_options(&declined);
        assert_eq!(
            ids(&options),
            vec!["reject", "allow-once", "allow-with-updates"]
        );
    }

    /// P7 — selection effects: Skill exact/prefix and WebFetch/generic
    /// `allow-with-updates` produce localSettings rules classified permanent;
    /// allow-once is temporary; a session `allow-with-updates` carries the CLI's
    /// suggestions verbatim; reject denies.
    #[test]
    fn inv_60_selection_effects() {
        // Skill exact.
        let skill = ctx("Skill", json!({ "skill": "github-cli auth" }));
        let eff = apply_selection(OPTION_ALLOW_SKILL_EXACT, &skill).unwrap();
        assert_eq!(
            eff,
            SelectionOutcome::Allow {
                updated_permissions: Some(vec![json!({
                    "type":"addRules",
                    "rules":[{"toolName":"Skill","ruleContent":"github-cli auth"}],
                    "behavior":"allow",
                    "destination":"localSettings"
                })]),
                permanent: true,
            }
        );

        // Skill prefix derives `name:*`.
        let eff = apply_selection(OPTION_ALLOW_SKILL_PREFIX, &skill).unwrap();
        match eff {
            SelectionOutcome::Allow {
                updated_permissions,
                permanent: true,
            } => {
                let rules = updated_permissions.unwrap();
                assert_eq!(rules[0]["rules"][0]["ruleContent"], "github-cli:*");
            }
            other => panic!("expected permanent prefix allow, got {other:?}"),
        }

        // WebFetch domain rule.
        let webfetch = ctx("WebFetch", json!({ "url": "https://example.com/doc" }));
        let eff = apply_selection(OPTION_ALLOW_WITH_UPDATES, &webfetch).unwrap();
        assert_eq!(
            eff,
            SelectionOutcome::Allow {
                updated_permissions: Some(vec![json!({
                    "type":"addRules",
                    "rules":[{"toolName":"WebFetch","ruleContent":"domain:example.com"}],
                    "behavior":"allow",
                    "destination":"localSettings"
                })]),
                permanent: true,
            }
        );

        // Generic tool generated rule.
        let generic = ctx("SomeTool", json!({}));
        let eff = apply_selection(OPTION_ALLOW_WITH_UPDATES, &generic).unwrap();
        assert_eq!(
            eff,
            SelectionOutcome::Allow {
                updated_permissions: Some(vec![json!({
                    "type":"addRules",
                    "rules":[{"toolName":"SomeTool"}],
                    "behavior":"allow",
                    "destination":"localSettings"
                })]),
                permanent: true,
            }
        );

        // allow-once: temporary, no updatedPermissions.
        let eff = apply_selection(OPTION_ALLOW_ONCE, &generic).unwrap();
        assert_eq!(
            eff,
            SelectionOutcome::Allow {
                updated_permissions: None,
                permanent: false,
            }
        );

        // Session `allow-with-updates` carries the CLI's suggestions verbatim.
        let edit = with_changeset(
            ctx("Edit", json!({ "file_path": "src/a.rs" })),
            vec![json!({"type":"setMode","mode":"acceptEdits","destination":"session"})],
        );
        let eff = apply_selection(OPTION_ALLOW_WITH_UPDATES, &edit).unwrap();
        match eff {
            SelectionOutcome::Allow {
                updated_permissions,
                permanent: true,
            } => assert_eq!(
                updated_permissions.unwrap(),
                vec![json!({"type":"setMode","mode":"acceptEdits","destination":"session"})]
            ),
            other => panic!("expected permanent updates allow, got {other:?}"),
        }

        // reject denies with the standard message.
        let eff = apply_selection(OPTION_REJECT, &generic).unwrap();
        assert_eq!(
            eff,
            SelectionOutcome::Deny {
                message: "User refused permission to run tool".to_string(),
                interrupt: false,
            }
        );

        // Unknown option id errors.
        assert!(apply_selection("bogus", &generic).is_err());
    }

    /// Fix 1 (BLOCKER) — ExitPlanMode builds Node's option set exactly. Node
    /// sorts every call (`options.ts:29-32`), so the manual-approve
    /// `allow_once` leads, then the elevated auto/bypass modes and the
    /// clear-context form, then "No, keep planning". With `input.plan` an
    /// additional clear-context option is offered (Decision 7).
    #[test]
    fn inv_60_options_exit_plan_mode() {
        let modes = [
            "default",
            "acceptEdits",
            "plan",
            "auto",
            "bypassPermissions",
        ];
        let options = build_options(&exit_ctx(json!({}), &modes, None));
        assert_eq!(
            ids(&options),
            vec![
                "exit-plan-default",
                "exit-plan-auto",
                "exit-plan-bypass",
                "reject",
            ]
        );
        assert_eq!(options[0]["name"], "Yes, manually approve edits");
        assert_eq!(options[1]["name"], "Yes, and use auto mode");
        assert_eq!(options[2]["name"], "Yes, and bypass permissions");
        assert_eq!(options[3]["name"], "No, keep planning");

        // With a plan, the clear-context option (preferred auto) follows the
        // `allow_once` default in build order.
        let options = build_options(&exit_ctx(json!({"plan": "ship it"}), &modes, None));
        assert_eq!(
            ids(&options),
            vec![
                "exit-plan-default",
                "exit-plan-clear-auto",
                "exit-plan-auto",
                "exit-plan-bypass",
                "reject",
            ]
        );
        assert_eq!(
            options[1],
            json!({
                "optionId": "exit-plan-clear-auto",
                "name": "Yes, clear context and use auto mode",
                "kind": "allow_always",
            })
        );

        // Pre-plan bypass builds bypassPermissions and its clear-context form
        // first, then auto; the sort keeps the `allow_once` default leading.
        let options = build_options(&exit_ctx(
            json!({"plan": "x"}),
            &modes,
            Some("bypassPermissions"),
        ));
        assert_eq!(options[0]["optionId"], "exit-plan-default");
        assert_eq!(options[1]["optionId"], "exit-plan-clear-bypass");
        assert_eq!(options[2]["optionId"], "exit-plan-bypass");
        assert_eq!(options[3]["optionId"], "exit-plan-auto");
        assert_eq!(options[4]["optionId"], "reject");

        // Context-used percent is rendered in the clear-context label only.
        let mut c = exit_ctx(json!({"plan": "x"}), &modes, None);
        c.context_used_percent = Some(35);
        let options = build_options(&c);
        assert_eq!(
            options[1]["name"],
            "Yes, clear context (35% used) and use auto mode"
        );

        // Without elevated modes the accept-edits option is offered.
        let options = build_options(&exit_ctx(json!({}), &["default", "acceptEdits"], None));
        assert_eq!(
            ids(&options),
            vec!["exit-plan-default", "exit-plan-accept-edits", "reject"]
        );
    }

    /// Fix 1 (BLOCKER) — the ExitPlanMode `default` option must not be denied:
    /// selecting it runs a session `setMode {mode:"default"}` classified
    /// temporary; elevated modes are permanent; "keep planning" interrupts.
    #[test]
    fn inv_60_exit_plan_mode_selection_effects() {
        let modes = [
            "default",
            "acceptEdits",
            "plan",
            "auto",
            "bypassPermissions",
        ];
        let ctx = exit_ctx(json!({}), &modes, None);
        // exit-plan-default -> allow + setMode default, temporary (regression:
        // it used to be treated as an unknown id and DENIED).
        let eff = apply_selection(OPTION_EXIT_PLAN_DEFAULT, &ctx).unwrap();
        assert_eq!(
            eff,
            SelectionOutcome::Allow {
                updated_permissions: Some(vec![json!({
                    "type": "setMode",
                    "mode": "default",
                    "destination": "session",
                })]),
                permanent: false,
            }
        );
        // auto / bypass / accept-edits are permanent.
        let eff = apply_selection(OPTION_EXIT_PLAN_AUTO, &ctx).unwrap();
        assert_eq!(
            eff,
            SelectionOutcome::Allow {
                updated_permissions: Some(vec![json!({
                    "type": "setMode",
                    "mode": "auto",
                    "destination": "session",
                })]),
                permanent: true,
            }
        );
        let eff = apply_selection(OPTION_EXIT_PLAN_BYPASS, &ctx).unwrap();
        assert!(matches!(
            eff,
            SelectionOutcome::Allow {
                permanent: true,
                ..
            }
        ));
        // "keep planning" interrupts the turn.
        let eff = apply_selection(OPTION_REJECT, &ctx).unwrap();
        assert_eq!(
            eff,
            SelectionOutcome::Deny {
                message: "User chose to keep planning".to_string(),
                interrupt: true,
            }
        );
        // The clear-context variants deny with an interrupt (fresh-query hand-off).
        let eff = apply_selection(OPTION_EXIT_PLAN_CLEAR_AUTO, &ctx).unwrap();
        assert_eq!(
            eff,
            SelectionOutcome::Deny {
                message: "User accepted the plan and requested a fresh context".to_string(),
                interrupt: true,
            }
        );
    }

    /// Fix 2 (MAJOR) — an option id that was never offered is rejected the way
    /// Node's `response.js` throws (`Permission option was not offered`), so a
    /// durable selection is only honoured if the durable option was offered.
    #[test]
    fn inv_60_offered_option_guard() {
        // Edit with no change set offers Yes/No only.
        let edit = ctx("Edit", json!({ "file_path": "a.rs" }));
        assert!(option_was_offered(&edit, OPTION_ALLOW_ONCE));
        assert!(option_was_offered(&edit, OPTION_REJECT));
        assert!(
            !option_was_offered(&edit, OPTION_ALLOW_WITH_UPDATES),
            "allow-with-updates is not offered for a plain Edit"
        );
        assert!(
            !option_was_offered(&edit, "exit-plan-default"),
            "an unrelated id is not offered"
        );

        // A generic tool ALWAYS offers the generated allow-with-updates option.
        let generic = ctx("SomeTool", json!({}));
        assert!(option_was_offered(&generic, OPTION_ALLOW_WITH_UPDATES));

        // The dedicated entry point also surfaces on ExitPlanMode.
        let modes = ["default", "acceptEdits", "auto", "bypassPermissions"];
        let exit = exit_ctx(json!({}), &modes, None);
        assert!(option_was_offered(&exit, OPTION_EXIT_PLAN_DEFAULT));
    }

    /// Fix 4 (MINOR) — SandboxNetworkAccess offers the durable option only when
    /// the change set is exactly one matching `localSettings` allow rule.
    #[test]
    fn inv_60_options_sandbox_network_access() {
        let host_rule = |host: &str| {
            with_changeset(
                ctx("SandboxNetworkAccess", json!({ "host": host })),
                vec![json!({
                    "type": "addRules",
                    "rules": [{ "toolName": "SandboxNetworkAccess", "ruleContent": host }],
                    "behavior": "allow",
                    "destination": "localSettings",
                })],
            )
        };
        let mut c = host_rule("example.com");
        c.input = json!({ "host": "example.com" });
        let options = build_options(&c);
        assert_eq!(
            json!(options),
            json!([
                { "optionId": "allow-once", "name": "Yes", "kind": "allow_once" },
                { "optionId": "allow-with-updates", "name": "Yes, and don't ask again for example.com", "kind": "allow_always" },
                { "optionId": "reject", "name": "No", "kind": "reject_once" },
            ])
        );
        // A different host / no matching rule -> Yes/No only.
        let plain = build_options(&ctx("SandboxNetworkAccess", json!({ "host": "other.com" })));
        assert_eq!(ids(&plain), vec!["allow-once", "reject"]);
    }

    /// Fix 4 (MINOR) — computer-use MCP tools get the MCP durable option with a
    /// label carrying no `commands` suffix.
    #[test]
    fn inv_60_options_computer_use_mcp() {
        let mut c = ctx("mcp__computer-use__server__click", json!({}));
        c.display_name = Some("Click".to_string());
        c = with_changeset(
            c,
            vec![json!({
                "type": "addRules",
                "rules": [{ "toolName": "mcp__computer-use__server__click" }],
                "behavior": "allow",
                "destination": "localSettings",
            })],
        );
        let options = build_options(&c);
        assert_eq!(
            json!(options),
            json!([
                { "optionId": "allow-once", "name": "Yes", "kind": "allow_once" },
                { "optionId": "allow-with-updates", "name": "Yes, and don't ask again for Click", "kind": "allow_always" },
                { "optionId": "reject", "name": "No", "kind": "reject_once" },
            ])
        );
        // Without a matching change set: Yes/No only.
        let plain = build_options(&ctx("mcp__computer-use__server__type", json!({})));
        assert_eq!(ids(&plain), vec!["allow-once", "reject"]);
    }

    /// Fix 4 (MINOR) — the WebFetch domain rule renders a bracketed IPv6 host
    /// exactly as Node's `new URL(url).hostname` does.
    #[test]
    fn inv_60_url_hostname_ipv6() {
        assert_eq!(
            url_hostname("http://[::1]:8080/x").as_deref(),
            Some("[::1]")
        );
        assert_eq!(
            url_hostname("https://[2001:db8::1]/").as_deref(),
            Some("[2001:db8::1]")
        );
        assert_eq!(
            url_hostname("http://example.com/doc").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            url_hostname("http://user:pass@example.com:80/x").as_deref(),
            Some("example.com")
        );
    }
}
