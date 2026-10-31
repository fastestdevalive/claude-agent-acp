//! `_meta.claudeCode.options` -> typed spawn inputs (rust-parity-fixes
//! Decision 2, P1/P2).
//!
//! Node spreads `userProvidedOptions` (`_meta.claudeCode.options`) into the SDK
//! options and the SDK maps them to `claude` argv/env. This module is the pure
//! typed mapper: it parses the option keys listed in the System Boundaries table
//! into a [`ClaudeOptions`], which `agent.rs` folds into [`SpawnOptions`]; the
//! argv order and extraArgs/env encoding are exercised by the unit tests here.
//!
//! Unknown keys (`callbacks`, `hooks`, `mcpServers`, `plugins`, `agents`,
//! `sandbox`, `settings`, `thinking`) are ignored and documented as PARITY
//! `pending`.

use std::path::PathBuf;

use serde_json::Value;

/// Parsed `_meta.claudeCode.options`.
#[derive(Debug, Clone, Default)]
pub struct ClaudeOptions {
    pub model: Option<String>,
    pub betas: Vec<String>,
    /// `extraArgs` in insertion order; `None` value = a bare `--key`.
    pub extra_args: Vec<(String, Option<String>)>,
    /// User env overrides (merged over inherited; `CLAUDE_CODE_EMIT_...` last).
    pub env: Vec<(String, String)>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub tools: Option<Vec<String>>,
    pub effort: Option<String>,
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    pub fallback_model: Option<String>,
    pub agent: Option<String>,
    pub include_hook_events: bool,
    pub additional_directories: Vec<PathBuf>,
    pub persist_session: bool,
    pub strict_mcp_config: bool,
    pub prompt_suggestions: Option<bool>,
    pub allow_dangerously_skip_permissions: Option<bool>,
    pub setting_sources: Option<Vec<String>>,
    pub resume: Option<String>,
}

fn opt_str(opts: &Value, key: &str) -> Option<String> {
    opts.get(key).and_then(Value::as_str).map(String::from)
}

fn opt_str_ary(opts: &Value, key: &str) -> Vec<String> {
    opts.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Parse `_meta.claudeCode.options` (or `&Value::Null` when absent).
pub fn claude_options(params: &Value) -> ClaudeOptions {
    let opts = params
        .get("_meta")
        .and_then(|m| m.get("claudeCode"))
        .and_then(|c| c.get("options"))
        .cloned()
        .unwrap_or(Value::Null);

    let extra_args = opts
        .get("extraArgs")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(k, v)| {
                    let value = match v {
                        Value::Null => None,
                        Value::String(s) => Some(s.clone()),
                        Value::Bool(b) => Some(b.to_string()),
                        Value::Number(n) => Some(n.to_string()),
                        other => Some(other.to_string()),
                    };
                    (k.clone(), value)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let env = opts
        .get("env")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let additional_directories = {
        let mut dirs = opt_str_ary(&opts, "additionalDirectories")
            .into_iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        // Prefer official ACP params.additionalDirectories, falling back to legacy _meta.additionalRoots.
        let acp_dirs = params
            .get("additionalDirectories")
            .and_then(Value::as_array)
            .or_else(|| {
                params
                    .get("_meta")
                    .and_then(|m| m.get("additionalRoots"))
                    .and_then(Value::as_array)
            });
        if let Some(roots) = acp_dirs {
            for r in roots.iter().filter_map(Value::as_str) {
                dirs.push(PathBuf::from(r));
            }
        }
        dirs
    };

    ClaudeOptions {
        model: opt_str(&opts, "model"),
        betas: opt_str_ary(&opts, "betas"),
        extra_args,
        env,
        allowed_tools: opt_str_ary(&opts, "allowedTools"),
        disallowed_tools: opt_str_ary(&opts, "disallowedTools"),
        tools: {
            let t = opt_str_ary(&opts, "tools");
            if t.is_empty() && opts.get("tools").is_none() {
                None
            } else {
                Some(t)
            }
        },
        effort: opt_str(&opts, "effort"),
        max_turns: opts
            .get("maxTurns")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        max_budget_usd: opts.get("maxBudgetUsd").and_then(Value::as_f64),
        fallback_model: opt_str(&opts, "fallbackModel"),
        agent: opt_str(&opts, "agent"),
        include_hook_events: opts
            .get("includeHookEvents")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        additional_directories,
        persist_session: opts
            .get("persistSession")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        strict_mcp_config: opts
            .get("strictMcpConfig")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        prompt_suggestions: opts.get("promptSuggestions").and_then(Value::as_bool),
        allow_dangerously_skip_permissions: opts
            .get("allowDangerouslySkipPermissions")
            .and_then(Value::as_bool),
        setting_sources: {
            let s = opt_str_ary(&opts, "settingSources");
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        },
        resume: opt_str(&opts, "resume"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{build_argv, SpawnOptions};
    use serde_json::json;

    fn base_spawn() -> SpawnOptions {
        SpawnOptions {
            session_id: Some("sess".into()),
            permission_prompt_tool: true,
            include_partial_messages: true,
            replay_user_messages: true,
            ..SpawnOptions::default()
        }
    }

    #[test]
    fn inv_57_options_to_argv_order() {
        let mut opts = base_spawn();
        opts.model = Some("haiku".into());
        opts.betas = vec!["b1".into(), "b2".into()];
        opts.allowed_tools = vec!["Read".into()];
        opts.disallowed_tools = vec!["Write".into()];
        opts.tools = Some(vec!["Glob".into()]);
        opts.effort = Some("medium".into());
        opts.max_turns = Some(5);
        opts.max_budget_usd = Some(1.5);
        opts.fallback_model = Some("so".into());
        opts.agent = Some("ag".into());
        opts.include_hook_events = true;
        opts.strict_mcp_config = true;
        opts.permission_mode = Some("plan".into());
        opts.allow_dangerously_skip_permissions = true;
        opts.additional_directories = vec![std::path::PathBuf::from("/d1")];

        let argv = build_argv(&opts);
        assert_eq!(
            argv,
            vec![
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json",
                "--effort=medium",
                "--max-turns=5",
                "--max-budget-usd=1.5",
                "--model=haiku",
                "--agent=ag",
                "--betas=b1,b2",
                "--permission-prompt-tool=stdio",
                "--allowedTools=Read",
                "--disallowedTools=Write",
                "--tools=Glob",
                "--setting-sources=user,project,local",
                "--strict-mcp-config",
                "--permission-mode=plan",
                "--allow-dangerously-skip-permissions",
                "--fallback-model=so",
                "--include-hook-events",
                "--include-partial-messages",
                "--add-dir=/d1",
                "--session-id=sess",
                "--replay-user-messages",
                "",
            ]
        );
    }

    #[test]
    fn inv_57_extra_args_encoding() {
        // null -> one `--key` token; plain -> `--key`,`value`; dash (len>1) -> `--key=value`;
        // single dash '-' (len==1) -> `--key`,`-`; empty string -> `--key`, `""`;
        // numbers and booleans -> `--key`,`str`.
        let mut opts = base_spawn();
        opts.extra_args = vec![
            ("chrome".into(), None),
            ("flag".into(), Some("v".into())),
            ("dash".into(), Some("-v".into())),
            ("single_dash".into(), Some("-".into())),
            ("empty".into(), Some(String::new())),
            ("num".into(), Some("42".into())),
            ("bool".into(), Some("true".into())),
        ];
        let argv = build_argv(&opts);
        // Find the extraArgs region (after --session-id=sess).
        let idx = argv.iter().position(|a| a == "--session-id=sess").unwrap();
        assert_eq!(
            &argv[idx + 1..],
            &[
                "--chrome".to_string(),
                "--flag".to_string(),
                "v".to_string(),
                "--dash=-v".to_string(),
                "--single_dash".to_string(),
                "-".to_string(),
                "--empty".to_string(),
                String::new(),
                "--num".to_string(),
                "42".to_string(),
                "--bool".to_string(),
                "true".to_string(),
                "--replay-user-messages".to_string(),
                String::new(),
            ]
        );
    }

    #[test]
    fn inv_57_extra_args_types_parsing() {
        let params = json!({
            "_meta": {
                "claudeCode": {
                    "options": {
                        "extraArgs": {
                            "null_key": null,
                            "str_key": "val",
                            "bool_key": true,
                            "num_key": 123
                        }
                    }
                }
            }
        });
        let opts = claude_options(&params);
        let mut map: std::collections::BTreeMap<String, Option<String>> =
            opts.extra_args.into_iter().collect();
        assert_eq!(map.remove("null_key"), Some(None));
        assert_eq!(map.remove("str_key"), Some(Some("val".into())));
        assert_eq!(map.remove("bool_key"), Some(Some("true".into())));
        assert_eq!(map.remove("num_key"), Some(Some("123".into())));
    }

    #[test]
    fn inv_57_env_precedence() {
        // User env is merged by `apply_claude_env` (process.rs) AFTER inherited;
        // the parse side simply surfaces option env verbatim.
        let params = serde_json::json!({
            "_meta": {"claudeCode": {"options": {
                "env": {"CLAUDE_CODE_ARTIFACT": "1"}
            }}}
        });
        let opts = claude_options(&params);
        assert_eq!(
            opts.env,
            vec![("CLAUDE_CODE_ARTIFACT".to_string(), "1".to_string())]
        );
    }

    #[test]
    fn inv_57_options_parser_fields() {
        let params = serde_json::json!({
            "_meta": {"claudeCode": {"options": {
                "model": "haiku",
                "betas": ["ctx-1m"],
                "extraArgs": {"chrome": null},
                "allowedTools": ["Read"],
                "disallowedTools": ["Bash"],
                "tools": ["Glob", "Grep"],
                "effort": "high",
                "maxTurns": 9,
                "maxBudgetUsd": 2.0,
                "fallbackModel": "sonnet",
                "agent": "builder",
                "includeHookEvents": true,
                "additionalDirectories": ["/x"],
                "persistSession": true,
                "strictMcpConfig": true,
                "promptSuggestions": false,
                "allowDangerouslySkipPermissions": false,
                "settingSources": ["user", "project"],
                "resume": "abc"
            }}}
        });
        let o = claude_options(&params);
        assert_eq!(o.model.as_deref(), Some("haiku"));
        assert_eq!(o.betas, vec!["ctx-1m"]);
        assert_eq!(o.extra_args, vec![("chrome".to_string(), None)]);
        assert_eq!(o.allowed_tools, vec!["Read"]);
        assert_eq!(o.disallowed_tools, vec!["Bash"]);
        assert_eq!(o.tools, Some(vec!["Glob".to_string(), "Grep".to_string()]));
        assert_eq!(o.effort.as_deref(), Some("high"));
        assert_eq!(o.max_turns, Some(9));
        assert_eq!(o.max_budget_usd, Some(2.0));
        assert_eq!(o.fallback_model.as_deref(), Some("sonnet"));
        assert_eq!(o.agent.as_deref(), Some("builder"));
        assert!(o.include_hook_events);
        assert_eq!(o.additional_directories, vec![PathBuf::from("/x")]);
        assert!(o.persist_session);
        assert!(o.strict_mcp_config);
        assert_eq!(o.prompt_suggestions, Some(false));
        assert_eq!(o.allow_dangerously_skip_permissions, Some(false));
        assert_eq!(
            o.setting_sources,
            Some(vec!["user".to_string(), "project".to_string()])
        );
        assert_eq!(o.resume.as_deref(), Some("abc"));
    }

    #[test]
    fn inv_57_additional_directories_sources() {
        // (a) params.additionalDirectories merged after options.additionalDirectories
        let params = json!({
            "additionalDirectories": ["/p1", "/p2"],
            "_meta": {
                "claudeCode": {
                    "options": {
                        "additionalDirectories": ["/opt1"]
                    }
                }
            }
        });
        let o = claude_options(&params);
        assert_eq!(
            o.additional_directories,
            vec![
                PathBuf::from("/opt1"),
                PathBuf::from("/p1"),
                PathBuf::from("/p2")
            ]
        );

        // (b) fallback to _meta.additionalRoots when params.additionalDirectories is absent
        let params_roots = json!({
            "_meta": {
                "additionalRoots": ["/root1"],
                "claudeCode": {
                    "options": {
                        "additionalDirectories": ["/opt1"]
                    }
                }
            }
        });
        let o = claude_options(&params_roots);
        assert_eq!(
            o.additional_directories,
            vec![PathBuf::from("/opt1"), PathBuf::from("/root1")]
        );
    }
}
