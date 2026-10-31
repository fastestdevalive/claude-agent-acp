//! `--hide-claude-auth`: this integration must never bill a claude.ai
//! subscription (port of upstream `src/hide-claude-auth.ts`). Under the flag the
//! agent hides the claude.ai login method from `initialize`, and refuses with
//! `authRequired` every session and every turn that a subscription would pay for.
//! The agent decides when the guard applies (flag on, no provider override); this
//! module decides what it refuses.
//!
//! The guard judges the `AccountInfo` the CLI reports at `initialize`; that cache
//! never changes during the life of a query. The agent therefore recreates the
//! query whenever the account may have changed (a sign-out, or an auth probe that
//! finds a different kind of identity), and the new `initialize` gives the guard a
//! current account. Session creation also refuses an account with no credential
//! this integration accepts.

use agent_client_protocol::Error;
use serde_json::{json, Value};

/// `data.reason` carried by the `authRequired` error when the flag blocks a turn.
pub const CLAUDE_SUBSCRIPTION_NOT_SUPPORTED_REASON: &str = "claude_subscription_not_supported";

pub const CLAUDE_SUBSCRIPTION_NOT_SUPPORTED_MESSAGE: &str =
    "This integration does not support using claude.ai subscriptions.";

/// The message of the plain sign-out refusal. It carries no `reason`: the account
/// holds nothing this integration can bill, which is the ordinary signed-out state
/// every ACP client already knows how to answer.
pub const CLAUDE_LOGIN_REQUIRED_MESSAGE: &str =
    "Sign in with an API key or a Console account to use this integration.";

/// `shouldHideClaudeAuth`: the adapter's own argv carries the flag.
pub fn should_hide_claude_auth(argv: &[String]) -> bool {
    argv.iter().any(|arg| arg == "--hide-claude-auth")
}

/// The `apiKeySource` values that outrank a stored subscription: the environment
/// variable, the configured helper command, and a key `/login` created with a
/// Console account. Each is an API key that pays instead of the subscription.
const API_KEY_SOURCES_ABOVE_SUBSCRIPTION: [&str; 3] =
    ["ANTHROPIC_API_KEY", "apiKeyHelper", "/login managed key"];

/// A non-empty string field (JS truthiness for the strings the account carries).
fn field<'a>(account: &'a Value, name: &str) -> Option<&'a str> {
    account
        .get(name)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

/// `apiProvider` set to something other than `firstParty`.
fn third_party_provider(account: &Value) -> bool {
    account
        .get("apiProvider")
        .is_some_and(|provider| provider.as_str() != Some("firstParty"))
}

fn key_above_subscription(account: &Value) -> bool {
    API_KEY_SOURCES_ABOVE_SUBSCRIPTION.contains(&field(account, "apiKeySource").unwrap_or(""))
}

/// `billsClaudeSubscription`: true when a turn on this account is paid by a
/// claude.ai subscription. An allowlist: an account bills the subscription only
/// when every field proves it, and an unknown value keeps the guard on.
///
/// - `subscriptionType` must be set; without it the CLI reports no subscription.
/// - `tokenSource` must be absent: the CLI sets it in place of the subscription
///   when a bearer or OAuth environment token pays for the turn.
/// - `apiProvider` must be absent or `firstParty`.
/// - `apiKeySource` must not outrank the subscription.
pub fn bills_claude_subscription(account: Option<&Value>) -> bool {
    let Some(account) = account else { return false };
    if field(account, "subscriptionType").is_none() {
        return false;
    }
    if field(account, "tokenSource").is_some() {
        return false;
    }
    if third_party_provider(account) {
        return false;
    }
    !key_above_subscription(account)
}

/// `holdsNonSubscriptionCredential`: true when the account already holds a
/// credential this integration accepts: a third-party backend, a bearer or OAuth
/// environment token, or one of the API key sources above the subscription.
///
/// False covers both the signed-out CLI and the account whose only credential is a
/// claude.ai subscription; [`bills_claude_subscription`] runs first and separates
/// the two.
pub fn holds_non_subscription_credential(account: Option<&Value>) -> bool {
    let Some(account) = account else { return false };
    if third_party_provider(account) {
        return true;
    }
    if field(account, "tokenSource").is_some() {
        return true;
    }
    key_above_subscription(account)
}

/// `claudeSubscriptionNotSupportedError`: `authRequired` with
/// `data.reason = claude_subscription_not_supported`.
pub fn claude_subscription_not_supported_error() -> Error {
    Error::new(
        -32000,
        format!("Authentication required: {CLAUDE_SUBSCRIPTION_NOT_SUPPORTED_MESSAGE}"),
    )
    .data(json!({ "reason": CLAUDE_SUBSCRIPTION_NOT_SUPPORTED_REASON }))
}

/// `claudeLoginRequiredError`: the signed-out refusal, deliberately without `data`,
/// so a client that reads `data.reason` sees the plain ACP sign-out it handles.
pub fn claude_login_required_error() -> Error {
    Error::new(
        -32000,
        format!("Authentication required: {CLAUDE_LOGIN_REQUIRED_MESSAGE}"),
    )
}

/// The state the guard keeps for one session, so each degraded-guard warning is
/// logged once (`ClaudeSubscriptionGuardState`). The shared in-flight run of
/// upstream has no counterpart: the cached account is read without waiting.
#[derive(Debug, Default, Clone)]
pub struct GuardState {
    /// True after the "no account source" warning was logged for this session.
    degraded_warning_logged: bool,
}

/// What the guard needs to report: a sink for the degraded-guard warning.
pub type GuardLog<'a> = &'a dyn Fn(&str);

/// `warnClaudeSubscriptionGuardDegraded`: report once per session that the guard
/// cannot read the account at all. The guard is then absent, not passing. The CLI
/// build decides this, not the user, so a message on every turn would only add
/// noise.
pub fn warn_guard_degraded(
    session_id: &str,
    state: &mut GuardState,
    cause: &str,
    warn: GuardLog<'_>,
) {
    if state.degraded_warning_logged {
        return;
    }
    state.degraded_warning_logged = true;
    warn(&format!(
        "Session {session_id}: the --hide-claude-auth subscription guard is degraded ({cause}). This session cannot detect a claude.ai subscription."
    ));
}

/// The decision on one account: the refusal, if any (`evaluateGuard` after the
/// account was read). A subscription account is refused with the subscription
/// reason; an account holding nothing this integration accepts, with the plain
/// sign-out error.
pub fn decide(account: &Value) -> Option<Error> {
    if bills_claude_subscription(Some(account)) {
        return Some(claude_subscription_not_supported_error());
    }
    if !holds_non_subscription_credential(Some(account)) {
        return Some(claude_login_required_error());
    }
    None
}

/// The per-turn guard for a live session (`refuseClaudeSubscriptionTurn`), on the
/// account the CLI cached at `initialize`.
///
/// `on_account` runs with the account before the decision, so a refused turn still
/// reports which account it was refused for (a display hook: it cannot change the
/// verdict). The guard fails open when no account can be read at all: session
/// creation already refused every account without a usable credential, and an
/// unavailable account must not block a session that passed that check.
pub fn refuse_claude_subscription_turn(
    session_id: &str,
    account: Option<&Value>,
    state: &mut GuardState,
    warn: GuardLog<'_>,
    on_account: impl FnOnce(&Value),
) -> Option<Error> {
    let Some(account) = account else {
        warn_guard_degraded(
            session_id,
            state,
            "the CLI reported no account at initialize",
            warn,
        );
        return None;
    };
    on_account(account);
    decide(account)
}

#[cfg(test)]
mod tests;
