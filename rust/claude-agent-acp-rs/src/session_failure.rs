//! JetBrains AIR session failure extension (rust-air-gaps G9, Decision 17, 26).
//!
//! Mirrors `src/session-failure-extension.ts` (0.94.0):
//! - `AIR_FAILURE_POLICY` table for the 13 failure kinds
//! - `provider_failure_category` mapping provider / SDK error kinds to [`ClaudeFailureKind`]
//! - `session_failure_meta` formatting for prompt responses and session updates
//! - `USAGE_LIMIT_ERROR_PREFIXES` and synthetic usage-limit detection (N-53)

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The 13 typed failure kinds defined by the AIR session-failure extension
/// (`src/session-failure-extension.ts:19-32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeFailureKind {
    AccessDenied,
    Advisory,
    AuthRequired,
    BadRequest,
    BudgetExhausted,
    ContextExhausted,
    InternalError,
    Overloaded,
    ProviderError,
    QuotaExhausted,
    RateLimited,
    TransportLost,
    WorkerShutdown,
}

impl ClaudeFailureKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AccessDenied => "access_denied",
            Self::Advisory => "advisory",
            Self::AuthRequired => "auth_required",
            Self::BadRequest => "bad_request",
            Self::BudgetExhausted => "budget_exhausted",
            Self::ContextExhausted => "context_exhausted",
            Self::InternalError => "internal_error",
            Self::Overloaded => "overloaded",
            Self::ProviderError => "provider_error",
            Self::QuotaExhausted => "quota_exhausted",
            Self::RateLimited => "rate_limited",
            Self::TransportLost => "transport_lost",
            Self::WorkerShutdown => "worker_shutdown",
        }
    }
}

/// The failure policy definition for one [`ClaudeFailureKind`]
/// (`src/session-failure-extension.ts:89-166`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailurePolicy {
    pub category: &'static str,
    pub actions: &'static [&'static str],
    pub fallback_title: &'static str,
}

/// Policy for each of the 13 failure kinds.
pub const fn failure_policy(kind: ClaudeFailureKind) -> FailurePolicy {
    match kind {
        ClaudeFailureKind::AccessDenied => FailurePolicy {
            category: "service",
            actions: &["retry"],
            fallback_title: "Claude could not access the account or provider credentials.",
        },
        ClaudeFailureKind::Advisory => FailurePolicy {
            category: "unknown",
            actions: &[],
            fallback_title: "Claude reported a notice.",
        },
        ClaudeFailureKind::AuthRequired => FailurePolicy {
            category: "access",
            actions: &["login"],
            fallback_title: "Sign in to continue using Claude.",
        },
        ClaudeFailureKind::BadRequest => FailurePolicy {
            category: "request",
            actions: &[],
            fallback_title: "Claude could not process this request.",
        },
        ClaudeFailureKind::BudgetExhausted => FailurePolicy {
            category: "limit",
            actions: &["new_session"],
            fallback_title: "This Claude session reached its configured budget.",
        },
        ClaudeFailureKind::ContextExhausted => FailurePolicy {
            category: "limit",
            actions: &["new_session"],
            fallback_title: "This Claude turn reached its configured limit.",
        },
        ClaudeFailureKind::InternalError => FailurePolicy {
            category: "service",
            actions: &["retry", "new_session"],
            fallback_title: "Claude Agent encountered an internal error.",
        },
        ClaudeFailureKind::Overloaded => FailurePolicy {
            category: "service",
            actions: &["retry"],
            fallback_title: "Claude is temporarily overloaded.",
        },
        ClaudeFailureKind::ProviderError => FailurePolicy {
            category: "service",
            actions: &["retry"],
            fallback_title: "The model provider reported an error.",
        },
        ClaudeFailureKind::QuotaExhausted => FailurePolicy {
            category: "limit",
            actions: &[],
            fallback_title: "The Claude account has no available quota.",
        },
        ClaudeFailureKind::RateLimited => FailurePolicy {
            category: "limit",
            actions: &["retry"],
            fallback_title: "Claude is temporarily rate limited.",
        },
        ClaudeFailureKind::TransportLost => FailurePolicy {
            category: "connection",
            actions: &["new_session"],
            fallback_title: "The connection to Claude was lost.",
        },
        ClaudeFailureKind::WorkerShutdown => FailurePolicy {
            category: "connection",
            actions: &["new_session"],
            fallback_title: "The Claude worker is shutting down.",
        },
    }
}

/// A prepared or published failure payload (`src/session-failure-extension.ts:43-58`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedSessionFailure {
    pub id: String,
    pub revision: u64,
    pub kind: ClaudeFailureKind,
    pub category: &'static str,
    pub severity: &'static str,
    pub title: String,
    pub details: Option<String>,
    pub reason: Option<String>,
    pub actions: Vec<String>,
}

/// Format the `jetbrains.air` meta object carrying `sessionFailure`
/// (`src/session-failure-extension.ts:168-178`).
pub fn session_failure_meta(failure: &PublishedSessionFailure) -> Value {
    let mut payload = serde_json::Map::new();
    payload.insert("id".to_string(), json!(failure.id));
    payload.insert("revision".to_string(), json!(failure.revision));
    payload.insert("category".to_string(), json!(failure.category));
    payload.insert("severity".to_string(), json!(failure.severity));
    payload.insert("title".to_string(), json!(failure.title));
    if let Some(ref details) = failure.details {
        payload.insert("details".to_string(), json!(details));
    }
    if let Some(ref reason) = failure.reason {
        payload.insert("reason".to_string(), json!(reason));
    }
    payload.insert("actions".to_string(), json!(failure.actions));

    json!({
        "jetbrains": {
            "air": {
                "version": 1,
                "sessionFailure": Value::Object(payload),
            }
        }
    })
}

/// Map an SDK assistant error kind or usage-limit flag to [`ClaudeFailureKind`]
/// (`src/session-failure-extension.ts:449-486`).
pub fn provider_failure_category(
    error_kind: Option<&str>,
    is_usage_limit: bool,
) -> ClaudeFailureKind {
    if is_usage_limit {
        return ClaudeFailureKind::QuotaExhausted;
    }
    match error_kind {
        Some("authentication_failed" | "oauth_org_not_allowed") => ClaudeFailureKind::AuthRequired,
        Some("billing_error" | "account_on_hold") => ClaudeFailureKind::QuotaExhausted,
        Some("verification_required" | "cloud_credential_error") => ClaudeFailureKind::AccessDenied,
        Some("rate_limit") => ClaudeFailureKind::RateLimited,
        Some("overloaded") => ClaudeFailureKind::Overloaded,
        Some("invalid_request" | "model_not_found") => ClaudeFailureKind::BadRequest,
        Some("max_output_tokens") => ClaudeFailureKind::ContextExhausted,
        Some("server_error" | "unknown") | None => ClaudeFailureKind::ProviderError,
        _ => ClaudeFailureKind::ProviderError,
    }
}

/// The SDK's stable synthetic usage-limit error prefixes copied from
/// `@anthropic-ai/claude-agent-sdk` (`sdk.mjs`).
pub const USAGE_LIMIT_ERROR_PREFIXES: &[&str] = &[
    "You've hit your",
    "You've reached your",
    "You're out of usage credits",
    "Your org is out of usage · add funds to continue",
    "Your org is out of usage · contact your admin",
    "Your seat type doesn't include usage credits",
    "Your seat type doesn't include usage",
    "Your usage allocation has been disabled by your admin",
    "Your group's usage limit is set to $0",
    "Fable 5 requires usage credits",
    "You're out of extra usage",
    "Your seat type doesn't include extra usage",
];

/// Extract the concatenated plain text from an API assistant message
/// (`src/session-failure-extension.ts:185-203`).
pub fn assistant_message_text(api_message: &Value) -> Option<String> {
    if !api_message.is_object() {
        return None;
    }
    let content = api_message.get("content")?;
    if let Some(text) = content.as_str() {
        if !text.is_empty() {
            return Some(text.to_string());
        }
        return None;
    }
    let blocks = content.as_array()?;
    let mut combined = String::new();
    for block in blocks {
        if let Some(obj) = block.as_object() {
            if obj.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(text) = obj.get("text").and_then(Value::as_str) {
                    combined.push_str(text);
                }
            }
        }
    }
    if combined.is_empty() {
        None
    } else {
        Some(combined)
    }
}

/// Test whether an assistant message is a synthetic usage-limit error
/// (`src/session-failure-extension.ts:205-212`).
pub fn is_synthetic_usage_limit_message(api_message: &Value) -> bool {
    if !api_message.is_object() {
        return false;
    }
    if api_message.get("model").and_then(Value::as_str) != Some("<synthetic>") {
        return false;
    }
    let Some(text) = assistant_message_text(api_message) else {
        return false;
    };
    USAGE_LIMIT_ERROR_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

/// Extract the active usage limit from transcript history entries (N-53,
/// `src/session-failure-extension.ts:214-235`).
///
/// A later real model answer proves the account recovered; synthetic
/// local-command/interruption messages do not.
///
/// Note: Unused outside tests in this turn; package F25 (session restore / replay)
/// is its consumer.
pub fn active_usage_limit_message<'a, I>(entries: I) -> Option<(String, String)>
where
    I: IntoIterator<Item = &'a Value>,
{
    let mut active: Option<(String, String)> = None;
    for entry in entries {
        if entry.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if !entry.get("parent_tool_use_id").is_none_or(Value::is_null) {
            continue;
        }
        let Some(inner_msg) = entry.get("message") else {
            continue;
        };
        if is_synthetic_usage_limit_message(inner_msg) {
            if let Some(title) = assistant_message_text(inner_msg) {
                let uuid = entry
                    .get("uuid")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                active = Some((uuid, title));
            }
            continue;
        }
        let model = inner_msg.get("model").and_then(Value::as_str);
        if model.is_some_and(|m| m != "<synthetic>") {
            active = None;
        }
    }
    active
}

/// Failure context recorded from assistant frames for the active turn
/// (`AssistantFailureCtx`, rust-air-gaps Decision 26).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssistantFailureCtx {
    pub error: Option<String>,
    pub was_usage_limit: bool,
    pub title: Option<String>,
}

impl AssistantFailureCtx {
    pub fn reset(&mut self) {
        self.error = None;
        self.was_usage_limit = false;
        self.title = None;
    }

    pub fn has_error(&self) -> bool {
        self.error.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inv_54_air_failure_policy_table() {
        let kinds = [
            (ClaudeFailureKind::AccessDenied, "service", vec!["retry"]),
            (ClaudeFailureKind::Advisory, "unknown", vec![]),
            (ClaudeFailureKind::AuthRequired, "access", vec!["login"]),
            (ClaudeFailureKind::BadRequest, "request", vec![]),
            (
                ClaudeFailureKind::BudgetExhausted,
                "limit",
                vec!["new_session"],
            ),
            (
                ClaudeFailureKind::ContextExhausted,
                "limit",
                vec!["new_session"],
            ),
            (
                ClaudeFailureKind::InternalError,
                "service",
                vec!["retry", "new_session"],
            ),
            (ClaudeFailureKind::Overloaded, "service", vec!["retry"]),
            (ClaudeFailureKind::ProviderError, "service", vec!["retry"]),
            (ClaudeFailureKind::QuotaExhausted, "limit", vec![]),
            (ClaudeFailureKind::RateLimited, "limit", vec!["retry"]),
            (
                ClaudeFailureKind::TransportLost,
                "connection",
                vec!["new_session"],
            ),
            (
                ClaudeFailureKind::WorkerShutdown,
                "connection",
                vec!["new_session"],
            ),
        ];

        for (kind, expected_cat, expected_actions) in kinds {
            let policy = failure_policy(kind);
            assert_eq!(policy.category, expected_cat, "category for {:?}", kind);
            assert_eq!(
                policy.actions,
                expected_actions.as_slice(),
                "actions for {:?}",
                kind
            );
            assert!(
                !policy.fallback_title.is_empty(),
                "fallback title for {:?}",
                kind
            );
        }
    }

    #[test]
    fn inv_54_provider_failure_category() {
        assert_eq!(
            provider_failure_category(None, true),
            ClaudeFailureKind::QuotaExhausted
        );
        assert_eq!(
            provider_failure_category(Some("billing_error"), true),
            ClaudeFailureKind::QuotaExhausted
        );
        assert_eq!(
            provider_failure_category(Some("authentication_failed"), false),
            ClaudeFailureKind::AuthRequired
        );
        assert_eq!(
            provider_failure_category(Some("oauth_org_not_allowed"), false),
            ClaudeFailureKind::AuthRequired
        );
        assert_eq!(
            provider_failure_category(Some("billing_error"), false),
            ClaudeFailureKind::QuotaExhausted
        );
        assert_eq!(
            provider_failure_category(Some("account_on_hold"), false),
            ClaudeFailureKind::QuotaExhausted
        );
        assert_eq!(
            provider_failure_category(Some("verification_required"), false),
            ClaudeFailureKind::AccessDenied
        );
        assert_eq!(
            provider_failure_category(Some("cloud_credential_error"), false),
            ClaudeFailureKind::AccessDenied
        );
        assert_eq!(
            provider_failure_category(Some("rate_limit"), false),
            ClaudeFailureKind::RateLimited
        );
        assert_eq!(
            provider_failure_category(Some("overloaded"), false),
            ClaudeFailureKind::Overloaded
        );
        assert_eq!(
            provider_failure_category(Some("invalid_request"), false),
            ClaudeFailureKind::BadRequest
        );
        assert_eq!(
            provider_failure_category(Some("model_not_found"), false),
            ClaudeFailureKind::BadRequest
        );
        assert_eq!(
            provider_failure_category(Some("max_output_tokens"), false),
            ClaudeFailureKind::ContextExhausted
        );
        assert_eq!(
            provider_failure_category(Some("server_error"), false),
            ClaudeFailureKind::ProviderError
        );
        assert_eq!(
            provider_failure_category(Some("unknown"), false),
            ClaudeFailureKind::ProviderError
        );
        assert_eq!(
            provider_failure_category(None, false),
            ClaudeFailureKind::ProviderError
        );
        assert_eq!(
            provider_failure_category(Some("something_unrecognized"), false),
            ClaudeFailureKind::ProviderError
        );
    }

    #[test]
    fn inv_54_usage_limit_detection() {
        for prefix in USAGE_LIMIT_ERROR_PREFIXES {
            let msg = json!({
                "model": "<synthetic>",
                "content": format!("{prefix} for today. Resets at midnight.")
            });
            assert!(
                is_synthetic_usage_limit_message(&msg),
                "must match prefix {prefix}"
            );

            let msg_blocks = json!({
                "model": "<synthetic>",
                "content": [
                    {"type": "text", "text": prefix.to_string()}
                ]
            });
            assert!(
                is_synthetic_usage_limit_message(&msg_blocks),
                "must match blocks for {prefix}"
            );
        }

        // Non-synthetic model is not detected even with limit text
        let non_synth = json!({
            "model": "claude-sonnet-4-5",
            "content": "You've hit your limit"
        });
        assert!(!is_synthetic_usage_limit_message(&non_synth));

        // Synthetic with normal text is not limit
        let normal_synth = json!({
            "model": "<synthetic>",
            "content": "Task completed successfully"
        });
        assert!(!is_synthetic_usage_limit_message(&normal_synth));
    }

    #[test]
    fn inv_54_session_failure_meta_shape() {
        let failure = PublishedSessionFailure {
            id: "prompt-1:error".to_string(),
            revision: 1,
            kind: ClaudeFailureKind::InternalError,
            category: "service",
            severity: "error",
            title: "Claude ended the turn without returning results for tool calls: toolu_1"
                .to_string(),
            details: None,
            reason: None,
            actions: vec!["retry".to_string(), "new_session".to_string()],
        };

        let meta = session_failure_meta(&failure);
        let sf = &meta["jetbrains"]["air"]["sessionFailure"];
        assert_eq!(meta["jetbrains"]["air"]["version"], 1);
        assert_eq!(sf["id"], "prompt-1:error");
        assert_eq!(sf["revision"], 1);
        assert_eq!(sf["category"], "service");
        assert_eq!(sf["severity"], "error");
        assert_eq!(
            sf["title"],
            "Claude ended the turn without returning results for tool calls: toolu_1"
        );
        assert_eq!(sf["actions"], json!(["retry", "new_session"]));
        assert!(sf.get("details").is_none());
        assert!(sf.get("reason").is_none());
    }

    #[test]
    fn inv_54_active_usage_limit_message_recovery() {
        let limit_entry = json!({
            "type": "assistant",
            "parent_tool_use_id": null,
            "uuid": "entry-uuid-limit",
            "message": {
                "model": "<synthetic>",
                "content": "You've hit your usage limit"
            }
        });
        let real_entry = json!({
            "type": "assistant",
            "parent_tool_use_id": null,
            "uuid": "entry-uuid-real",
            "message": {
                "model": "claude-3-5-sonnet-20241022",
                "content": "Hello!"
            }
        });
        let tool_entry = json!({
            "type": "assistant",
            "parent_tool_use_id": null,
            "uuid": "entry-uuid-tool",
            "message": {
                "model": "<synthetic>",
                "content": "Local command output"
            }
        });
        let subagent_limit_entry = json!({
            "type": "assistant",
            "parent_tool_use_id": "toolu_subagent",
            "uuid": "entry-uuid-subagent",
            "message": {
                "model": "<synthetic>",
                "content": "You've hit your usage limit"
            }
        });

        // 1. Single top-level limit entry recovers transcript entry uuid
        let entries = vec![&limit_entry];
        let active = active_usage_limit_message(entries);
        assert!(active.is_some());
        let (uuid, title) = active.unwrap();
        assert_eq!(uuid, "entry-uuid-limit");
        assert_eq!(title, "You've hit your usage limit");

        // 2. Subagent limit entry is ignored
        let entries = vec![&subagent_limit_entry];
        assert!(active_usage_limit_message(entries).is_none());

        // 3. Limit followed by real message clears active limit
        let entries = vec![&limit_entry, &real_entry];
        assert!(active_usage_limit_message(entries).is_none());

        // 4. Limit followed by non-limit synthetic message does NOT clear active limit
        let entries = vec![&limit_entry, &tool_entry];
        let active = active_usage_limit_message(entries);
        assert!(active.is_some());
        assert_eq!(active.unwrap().0, "entry-uuid-limit");
    }

    #[test]
    fn inv_54_assistant_failure_ctx_has_error() {
        let mut ctx = AssistantFailureCtx::default();
        assert!(!ctx.has_error());

        // Synthetic usage limit does NOT set has_error (Finding 9)
        ctx.was_usage_limit = true;
        assert!(!ctx.has_error());

        // Assistant error sets has_error
        ctx.error = Some("rate_limit".to_string());
        assert!(ctx.has_error());

        ctx.reset();
        assert!(!ctx.has_error());
        assert!(!ctx.was_usage_limit);
        assert!(ctx.error.is_none());
        assert!(ctx.title.is_none());
    }
}
