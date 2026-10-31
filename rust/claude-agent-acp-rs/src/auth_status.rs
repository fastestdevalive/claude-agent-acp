//! `authStatus` — the `_auth/status_update` push of the connection's auth
//! identity (rust-air-gaps G2, Decision 11). A port of `dist/auth-status.js`
//! (`fromCliStatus`, `fromAccountInfo`, `mergeAuthStatus`, `sameAuthStatus`,
//! `planLabel`) plus an [`AuthActor`] that owns the last-reported status.
//!
//! Push only. The status is read (a) after the `initialize` response from
//! `claude auth status --json`, (b) from each created session's account and
//! (c) again when a session's cwd differs from the last probe's. A payload is
//! pushed only when it differs from the last one sent; an unreadable identity
//! is silence, never an error.
//!
//! Not ported (no Rust counterpart, PARITY): ACP gateway authentication and
//! provider overrides, the auth epoch, the `--hide-claude-auth` guard, the
//! `vendor` bag, the per-prompt re-probe.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;

/// Upper bound on one `claude auth status --json` run.
pub const AUTH_STATUS_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The ext-notification method carrying a changed status.
pub const AUTH_STATUS_UPDATE_METHOD: &str = "_auth/status_update";

/// The account half of an `account`-kind status.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthAccount {
    pub plan: Option<String>,
    pub email: Option<String>,
    pub organization: Option<String>,
}

/// One `authStatus` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthStatus {
    pub kind: String,
    pub label: String,
    pub detail: Option<String>,
    pub account: Option<AuthAccount>,
}

impl AuthStatus {
    fn new(kind: &str, label: &str) -> Self {
        Self {
            kind: kind.to_string(),
            label: label.to_string(),
            detail: None,
            account: None,
        }
    }

    /// The wire object (`{kind,label,detail?,account?}`).
    pub fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("kind".into(), json!(self.kind));
        map.insert("label".into(), json!(self.label));
        if let Some(detail) = &self.detail {
            map.insert("detail".into(), json!(detail));
        }
        if let Some(account) = &self.account {
            let mut a = serde_json::Map::new();
            if let Some(plan) = &account.plan {
                a.insert("plan".into(), json!(plan));
            }
            if let Some(email) = &account.email {
                a.insert("email".into(), json!(email));
            }
            if let Some(org) = &account.organization {
                a.insert("organization".into(), json!(org));
            }
            map.insert("account".into(), Value::Object(a));
        }
        Value::Object(map)
    }
}

/// `notLoggedInAuthStatus`.
pub fn not_logged_in() -> AuthStatus {
    AuthStatus::new("none", "Not logged in")
}

/// `gatewayAuthStatus()` without a base URL.
fn gateway() -> AuthStatus {
    AuthStatus::new("gateway", "Custom model gateway")
}

fn external_label(provider: &str) -> String {
    match provider {
        "bedrock" => "AWS Bedrock",
        "vertex" => "Google Vertex AI",
        "foundry" => "Azure AI Foundry",
        "anthropicAws" => "Anthropic on AWS",
        "anthropicGoogleCloud" => "Anthropic on Google Cloud",
        "mantle" => "Mantle",
        other => other,
    }
    .to_string()
}

fn api_key_source_detail(source: &str) -> Option<&'static str> {
    match source {
        "apiKeyHelper" => Some("API key helper"),
        "env" => Some("env"),
        "ANTHROPIC_API_KEY" => Some("ANTHROPIC_API_KEY"),
        "ANTHROPIC_AUTH_TOKEN" => Some("ANTHROPIC_AUTH_TOKEN"),
        _ => None,
    }
}

/// `planLabel` — exactly one "Claude", each word title-cased (a word that
/// already carries capitals is kept).
pub fn plan_label(plan: &str) -> String {
    let mut titled = String::with_capacity(plan.len());
    let mut at_word_start = true;
    for ch in plan.chars() {
        if ch.is_whitespace() {
            at_word_start = true;
            titled.push(ch);
        } else if at_word_start {
            at_word_start = false;
            titled.extend(ch.to_uppercase());
        } else {
            titled.push(ch);
        }
    }
    let lower = plan.to_lowercase();
    let starts_with_claude = lower.starts_with("claude")
        && lower["claude".len()..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace);
    if starts_with_claude {
        titled
    } else {
        format!("Claude {titled}")
    }
}

/// The ASCII word runs (`\w+`) of `text` with the text between them.
fn word_runs(text: &str) -> Vec<(&str, &str)> {
    let mut runs = Vec::new();
    let mut gap_start = 0usize;
    let mut word_start: Option<usize> = None;
    for (i, ch) in text.char_indices() {
        let is_word = ch.is_ascii_alphanumeric() || ch == '_';
        match (word_start, is_word) {
            (None, true) => word_start = Some(i),
            (Some(start), false) => {
                runs.push((&text[gap_start..start], &text[start..i]));
                gap_start = i;
                word_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = word_start {
        runs.push((&text[gap_start..start], &text[start..]));
    }
    runs
}

/// `/\bcentral\b/i` and `/\bproxy\s+start\b/i` both match: the AIR gateway's
/// `apiKeyHelper` command.
fn is_air_gateway_helper(helper: &str) -> bool {
    let runs = word_runs(helper);
    let has_central = runs.iter().any(|(_, w)| w.eq_ignore_ascii_case("central"));
    let has_proxy_start = runs.windows(2).any(|pair| {
        pair[0].1.eq_ignore_ascii_case("proxy")
            && pair[1].1.eq_ignore_ascii_case("start")
            && !pair[1].0.is_empty()
            && pair[1].0.chars().all(char::is_whitespace)
    });
    has_central && has_proxy_start
}

struct Fields<'a> {
    api_provider: Option<&'a str>,
    subscription_type: Option<&'a str>,
    api_key_source: Option<&'a str>,
    email: Option<&'a str>,
    organization: Option<&'a str>,
    api_key_helper: Option<&'a str>,
}

/// A non-empty string field (JS truthiness for the strings we read).
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// `mapAuthFields` — the single mapping table.
fn map_auth_fields(f: &Fields<'_>) -> AuthStatus {
    if f.api_provider == Some("gateway") {
        return gateway();
    }
    if let Some(provider) = f.api_provider.filter(|p| *p != "firstParty") {
        return AuthStatus::new("external", &external_label(provider));
    }
    if let Some(source) = f.api_key_source {
        if source == "apiKeyHelper" && f.api_key_helper.is_some_and(is_air_gateway_helper) {
            return AuthStatus::new("gateway", "JetBrains Air Gateway");
        }
        let mut status = AuthStatus::new("api_key", "Anthropic API key");
        status.detail = api_key_source_detail(source).map(str::to_string);
        return status;
    }
    if let Some(plan) = f.subscription_type {
        let mut status = AuthStatus::new("account", &plan_label(plan));
        status.account = Some(AuthAccount {
            plan: Some(plan.to_string()),
            email: f.email.map(str::to_string),
            organization: f.organization.map(str::to_string),
        });
        return status;
    }
    not_logged_in()
}

/// `accountInfoHasIdentitySignal`.
fn account_has_identity_signal(account: &Value) -> bool {
    text(account, "apiKeySource").is_some()
        || text(account, "subscriptionType").is_some()
        || text(account, "apiProvider").is_some_and(|p| p != "firstParty")
}

/// `fromAccountInfo` — the session account mapping; `None` when it carries no
/// identity signal (callers keep what they know).
pub fn from_account_info(account: &Value, api_key_helper: Option<&str>) -> Option<AuthStatus> {
    if !account.is_object() || !account_has_identity_signal(account) {
        return None;
    }
    Some(map_auth_fields(&Fields {
        api_provider: text(account, "apiProvider"),
        subscription_type: text(account, "subscriptionType"),
        api_key_source: text(account, "apiKeySource"),
        email: text(account, "email"),
        organization: text(account, "organization"),
        api_key_helper,
    }))
}

/// `fromCliStatus` — the `claude auth status --json` stdout mapping; `None`
/// when the output is not the expected JSON object.
pub fn from_cli_status(stdout: &str, api_key_helper: Option<&str>) -> Option<AuthStatus> {
    let parsed: Value = serde_json::from_str(stdout).ok()?;
    if !parsed.is_object() {
        return None;
    }
    let logged_in = parsed.get("loggedIn")?.as_bool()?;
    let api_provider = text(&parsed, "apiProvider");
    let api_key_source = text(&parsed, "apiKeySource");
    let external_backend = api_provider.is_some_and(|p| p != "firstParty");
    if !external_backend && !logged_in && api_key_source.is_none() {
        return Some(not_logged_in());
    }
    Some(map_auth_fields(&Fields {
        api_provider,
        subscription_type: text(&parsed, "subscriptionType"),
        api_key_source,
        email: text(&parsed, "email"),
        organization: text(&parsed, "orgName"),
        api_key_helper,
    }))
}

/// `sameIdentity` — the field identifying each kind.
fn same_identity(a: &AuthStatus, b: &AuthStatus) -> bool {
    if a.kind != b.kind {
        return false;
    }
    match a.kind.as_str() {
        "api_key" => a.detail == b.detail,
        "account" => {
            let email = |s: &AuthStatus| {
                s.account
                    .as_ref()
                    .and_then(|x| x.email.clone())
                    .unwrap_or_default()
            };
            email(a) == email(b)
        }
        _ => true,
    }
}

/// `mergeAuthStatus` — a richer read of the same identity keeps the fields it
/// lacks; a different identity replaces the old one wholesale.
pub fn merge(previous: Option<&AuthStatus>, next: &AuthStatus) -> AuthStatus {
    let Some(previous) = previous.filter(|p| same_identity(p, next)) else {
        return next.clone();
    };
    let mut merged = next.clone();
    if merged.detail.is_none() {
        merged.detail = previous.detail.clone();
    }
    if previous.account.is_some() || next.account.is_some() {
        let p = previous.account.clone().unwrap_or_default();
        let n = next.account.clone().unwrap_or_default();
        merged.account = Some(AuthAccount {
            plan: n.plan.or(p.plan),
            email: n.email.or(p.email),
            organization: n.organization.or(p.organization),
        });
    }
    merged
}

/// `sameAuthStatus` — every rendered field equal; `None` never equals a payload.
pub fn same_status(a: Option<&AuthStatus>, b: &AuthStatus) -> bool {
    a.is_some_and(|a| a == b)
}

/// `apiKeyHelper` from the user / project / local `settings.json` files (last
/// one wins), the `SettingsManager.getApiKeyHelper` sources.
pub async fn read_api_key_helper(cwd: &Path) -> Option<String> {
    read_api_key_helper_with_env(cwd, &crate::env_snapshot::EnvSnapshot::from_process()).await
}

/// `apiKeyHelper` read using an explicit environment snapshot.
pub async fn read_api_key_helper_with_env(
    cwd: &Path,
    env: &crate::env_snapshot::EnvSnapshot,
) -> Option<String> {
    let user_dir = env
        .get_path("CLAUDE_CONFIG_DIR")
        .or_else(|| env.get_path("HOME").map(|h| h.join(".claude")));
    let mut files = Vec::new();
    if let Some(dir) = user_dir {
        files.push(dir.join("settings.json"));
    }
    files.push(cwd.join(".claude").join("settings.json"));
    files.push(cwd.join(".claude").join("settings.local.json"));
    let mut helper = None;
    for file in files {
        let Ok(raw) = tokio::fs::read_to_string(&file).await else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if let Some(h) = parsed.get("apiKeyHelper").and_then(Value::as_str) {
            helper = Some(h.to_string());
        }
    }
    helper
}

/// The outcome of one CLI auth status probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    Status(AuthStatus),
    LoggedOut,
    TimedOut,
}

/// Run `claude auth status --json` once (5 s bound, stdin null) and map it.
/// A non-zero exit still counts when stdout holds the JSON (logged out exits 1);
/// a timeout returns `TimedOut`, and an unreadable answer is `LoggedOut`.
pub async fn probe_cli(
    claude_path: Option<&Path>,
    cwd: &Path,
    extra_env: &[(String, String)],
) -> ProbeOutcome {
    let Ok(binary) = crate::process::resolve_claude(claude_path) else {
        return ProbeOutcome::LoggedOut;
    };
    let run = match crate::process::run_cli(
        &binary,
        &["auth", "status", "--json"],
        Some(cwd),
        extra_env,
        AUTH_STATUS_PROBE_TIMEOUT,
    )
    .await
    {
        Ok(r) => r,
        Err(_) => return ProbeOutcome::LoggedOut,
    };
    if run.timed_out {
        return ProbeOutcome::TimedOut;
    }
    if run.stdout.trim().is_empty() {
        return ProbeOutcome::LoggedOut;
    }
    let helper = read_api_key_helper(cwd).await;
    match from_cli_status(&run.stdout, helper.as_deref()) {
        Some(st) => ProbeOutcome::Status(st),
        None => ProbeOutcome::LoggedOut,
    }
}

/// Why `claude auth logout` failed: the Node `RequestError.internalError`
/// message and `data.stderr` (`acp-agent.js:2051-2056`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogoutError {
    pub message: String,
    pub stderr: String,
}

/// Run `claude auth logout` (non-interactive, idempotent). The credentials of
/// the native CLI store can only be cleared by the binary itself.
pub async fn run_logout(
    claude_path: Option<&Path>,
    extra_env: &[(String, String)],
) -> Result<(), LogoutError> {
    let binary = crate::process::resolve_claude(claude_path).map_err(|e| LogoutError {
        message: format!("claude auth logout failed: {e}"),
        stderr: String::new(),
    })?;
    let run = crate::process::run_cli(
        &binary,
        &["auth", "logout"],
        None,
        extra_env,
        Duration::from_secs(60),
    )
    .await
    .map_err(|e| LogoutError {
        message: format!("claude auth logout failed: {e}"),
        stderr: String::new(),
    })?;
    if run.success {
        return Ok(());
    }
    let stderr = run.stderr.trim().to_string();
    let detail = if run.timed_out {
        "timed out".to_string()
    } else {
        format!(
            "Command failed: {} auth logout\n{}",
            binary.display(),
            run.stderr
        )
    };
    Err(LogoutError {
        message: format!("claude auth logout failed: {detail}"),
        stderr,
    })
}

/// A boxed probe: the cwd in, the mapped status out.
pub type ProbeFn =
    Box<dyn Fn(PathBuf) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send>> + Send + Sync>;

/// Messages of the auth-status actor ([`spawn_auth_actor`]).
#[derive(Debug)]
pub enum AuthCmd {
    /// Re-read the identity from the CLI. `fresh` forces a new run even when
    /// one is in flight.
    Probe { cwd: PathBuf, fresh: bool },
    /// Bump epoch to invalidate in-flight probes (e.g. before logout subprocess).
    BumpEpoch,
    /// A session was created: publish its account (if informative) and re-probe
    /// when its cwd differs from the last probe's.
    SessionCreated {
        cwd: PathBuf,
        account: Option<Value>,
    },
    /// `logout` cleared the credentials: drop every probe that started before
    /// (its answer is stale), re-read the store with a fresh probe and, when it
    /// cannot be read, report "not logged in". `reply` resolves once the new
    /// status (if any) was pushed.
    Logout {
        cwd: PathBuf,
        reply: tokio::sync::oneshot::Sender<()>,
    },
    /// A spawned probe finished (internal). `epoch` is the actor epoch the
    /// probe started in; `reply` is set for the post-logout read.
    ProbeDone {
        outcome: ProbeOutcome,
        epoch: u64,
        reply: Option<tokio::sync::oneshot::Sender<()>>,
    },
}

/// The auth-probe context Node compares in `useAuthStatusSettings`
/// (`acp-agent.ts:3324-3346`): the settings cwd, the api-key helper, and the
/// launch env serialized with `JSON.stringify` (`None` for `undefined`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthContext {
    pub cwd: PathBuf,
    pub api_key_helper: Option<String>,
    pub env: Option<String>,
}

impl Default for AuthContext {
    /// Node's initial context: the process cwd, no helper, no env
    /// (`this.authStatusSettings?.manager.getCwd() ?? process.cwd()`, `helper
    /// !== undefined`, `JSON.stringify(undefined) !== undefined`).
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_default(),
            api_key_helper: None,
            env: None,
        }
    }
}

/// Whether a new auth-probe context differs from the previous one — cwd OR
/// api-key helper OR serialized `options.env` (D10 / Decision 12). An equal
/// context does not re-probe.
pub fn auth_context_changed(prev: &AuthContext, next: &AuthContext) -> bool {
    prev.cwd != next.cwd || prev.api_key_helper != next.api_key_helper || prev.env != next.env
}

/// Cheap handle to the actor.
#[derive(Debug, Clone)]
pub struct AuthHandle {
    tx: mpsc::UnboundedSender<AuthCmd>,
}

impl AuthHandle {
    /// Fire-and-forget; a stopped actor is ignored.
    pub fn send(&self, cmd: AuthCmd) {
        let _ = self.tx.send(cmd);
    }

    /// Bump epoch before running logout (in-flight probes dropped).
    pub fn bump_epoch(&self) {
        self.send(AuthCmd::BumpEpoch);
    }

    /// Run a fresh probe in `cwd`.
    pub fn probe(&self, cwd: PathBuf, fresh: bool) {
        self.send(AuthCmd::Probe { cwd, fresh });
    }
}

/// What the actor hands its forwarder, in wire order.
#[derive(Debug)]
pub enum AuthPush {
    /// The `authStatus` object of a changed status.
    Status(Value),
    /// Resolved by the forwarder once every earlier push was sent, so a
    /// response can be ordered behind its notifications.
    Barrier(tokio::sync::oneshot::Sender<()>),
}

/// Spawn the actor. `push` receives every changed status (and barriers) for
/// the forwarder; `probe` reads the CLI. State (`current`, in-flight count,
/// last probe cwd) lives only inside the task — no lock.
pub fn spawn_auth_actor(
    push: mpsc::UnboundedSender<AuthPush>,
    probe: ProbeFn,
    initial_cwd: PathBuf,
) -> AuthHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<AuthCmd>();
    let self_tx = tx.clone();
    tokio::spawn(async move {
        let mut current: Option<AuthStatus> = None;
        let mut in_flight = 0usize;
        let mut last_cwd = initial_cwd;
        // Bumped by a logout: a probe that started earlier read a world that no
        // longer exists, so its answer is dropped (Node's `authEpoch`).
        let mut epoch = 0u64;
        let set = |current: &mut Option<AuthStatus>, next: AuthStatus| {
            if same_status(current.as_ref(), &next) {
                return;
            }
            let _ = push.send(AuthPush::Status(next.to_json()));
            *current = Some(next);
        };
        while let Some(cmd) = rx.recv().await {
            match cmd {
                AuthCmd::BumpEpoch => {
                    epoch += 1;
                }
                AuthCmd::Probe { cwd, fresh } => {
                    if in_flight > 0 && !fresh {
                        continue;
                    }
                    in_flight += 1;
                    let fut = probe(cwd);
                    let done = self_tx.clone();
                    tokio::spawn(async move {
                        let _ = done.send(AuthCmd::ProbeDone {
                            outcome: fut.await,
                            epoch,
                            reply: None,
                        });
                    });
                }
                AuthCmd::Logout { cwd, reply } => {
                    epoch += 1;
                    in_flight += 1;
                    let fut = probe(cwd);
                    let done = self_tx.clone();
                    tokio::spawn(async move {
                        let _ = done.send(AuthCmd::ProbeDone {
                            outcome: fut.await,
                            epoch,
                            reply: Some(reply),
                        });
                    });
                }
                AuthCmd::ProbeDone {
                    outcome,
                    epoch: started,
                    reply,
                } => {
                    in_flight = in_flight.saturating_sub(1);
                    if reply.is_some() {
                        // The post-logout read: an unreadable store means the
                        // old identity is stale, so report "not logged in".
                        // On probe timeout, keep current status instead of pushing notLoggedIn.
                        match outcome {
                            ProbeOutcome::Status(st) => {
                                let next = merge(current.as_ref(), &st);
                                set(&mut current, next);
                            }
                            ProbeOutcome::LoggedOut => {
                                set(&mut current, not_logged_in());
                            }
                            ProbeOutcome::TimedOut => {
                                if current.is_none() {
                                    set(&mut current, not_logged_in());
                                }
                            }
                        }
                    } else if started == epoch {
                        if let ProbeOutcome::Status(st) = outcome {
                            let merged = merge(current.as_ref(), &st);
                            set(&mut current, merged);
                        }
                    }
                    if let Some(reply) = reply {
                        // Resolve behind the forwarder so the logout response
                        // cannot overtake the status it caused.
                        let _ = push.send(AuthPush::Barrier(reply));
                    }
                }
                AuthCmd::SessionCreated { cwd, account } => {
                    if let Some(account) = account {
                        let helper = read_api_key_helper(&cwd).await;
                        if let Some(status) = from_account_info(&account, helper.as_deref()) {
                            set(&mut current, status);
                        }
                    }
                    if cwd != last_cwd {
                        last_cwd = cwd.clone();
                        let _ = self_tx.send(AuthCmd::Probe { cwd, fresh: true });
                    }
                }
            }
        }
    });
    AuthHandle { tx }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(plan: &str, email: Option<&str>, org: Option<&str>) -> AuthStatus {
        AuthStatus {
            kind: "account".into(),
            label: plan_label(plan),
            detail: None,
            account: Some(AuthAccount {
                plan: Some(plan.into()),
                email: email.map(Into::into),
                organization: org.map(Into::into),
            }),
        }
    }

    fn st(kind: &str, label: &str, detail: Option<&str>) -> AuthStatus {
        AuthStatus {
            kind: kind.into(),
            label: label.into(),
            detail: detail.map(Into::into),
            account: None,
        }
    }

    /// Expected values come from the Node 0.94.0 `dist/auth-status.js`
    /// (`fromCliStatus`) run via `node` against the installed package.
    #[test]
    fn inv_50_from_cli_status_table() {
        let none = || Some(not_logged_in());
        let cases: Vec<(&str, Option<&str>, Option<AuthStatus>)> = vec![
            (
                r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"pro","email":"a@b.c","orgName":"Org"}"#,
                None,
                Some(acct("pro", Some("a@b.c"), Some("Org"))),
            ),
            (r#"{"loggedIn":false}"#, None, none()),
            (
                r#"{"loggedIn":false,"apiKeySource":"ANTHROPIC_API_KEY"}"#,
                None,
                Some(st(
                    "api_key",
                    "Anthropic API key",
                    Some("ANTHROPIC_API_KEY"),
                )),
            ),
            (
                r#"{"loggedIn":false,"apiKeySource":"apiKeyHelper"}"#,
                None,
                Some(st("api_key", "Anthropic API key", Some("API key helper"))),
            ),
            (
                r#"{"loggedIn":true,"apiKeySource":"apiKeyHelper","subscriptionType":"max"}"#,
                Some("bash ./central proxy start"),
                Some(st("gateway", "JetBrains Air Gateway", None)),
            ),
            (
                r#"{"loggedIn":true,"apiKeySource":"apiKeyHelper"}"#,
                Some("central-proxy start"),
                Some(st("gateway", "JetBrains Air Gateway", None)),
            ),
            (
                r#"{"loggedIn":false,"apiProvider":"bedrock"}"#,
                None,
                Some(st("external", "AWS Bedrock", None)),
            ),
            (
                r#"{"loggedIn":false,"apiProvider":"weird"}"#,
                None,
                Some(st("external", "weird", None)),
            ),
            (
                r#"{"loggedIn":true,"apiProvider":"gateway"}"#,
                None,
                Some(st("gateway", "Custom model gateway", None)),
            ),
            (
                r#"{"loggedIn":true,"subscriptionType":"Claude Max","email":"x@y.z"}"#,
                None,
                Some(acct("Claude Max", Some("x@y.z"), None)),
            ),
            (
                r#"{"loggedIn":true,"subscriptionType":"max 5x"}"#,
                None,
                Some(acct("max 5x", None, None)),
            ),
            (
                r#"{"loggedIn":true,"subscriptionType":"claude"}"#,
                None,
                Some(acct("claude", None, None)),
            ),
            (r#"{"loggedIn":true}"#, None, none()),
            (
                r#"{"loggedIn":true,"apiKeySource":"other"}"#,
                None,
                Some(st("api_key", "Anthropic API key", None)),
            ),
            ("not json", None, None),
            ("[]", None, None),
            (r#"{"loggedIn":"yes"}"#, None, None),
        ];
        for (stdout, helper, expected) in cases {
            assert_eq!(from_cli_status(stdout, helper), expected, "{stdout}");
        }
    }

    #[test]
    fn inv_50_plan_labels_and_wire_json() {
        assert_eq!(plan_label("max 5x"), "Claude Max 5x");
        assert_eq!(plan_label("Claude Max"), "Claude Max");
        assert_eq!(plan_label("claude"), "Claude");
        assert_eq!(plan_label("team"), "Claude Team");
        assert_eq!(
            acct("pro", Some("a@b.c"), Some("Org")).to_json(),
            json!({"kind":"account","label":"Claude Pro",
                "account":{"plan":"pro","email":"a@b.c","organization":"Org"}})
        );
    }

    #[test]
    fn inv_50_from_account_info_table() {
        let cases = [
            (json!({"apiProvider": "firstParty"}), None),
            (json!({}), None),
            (
                json!({"apiProvider":"firstParty","subscriptionType":"pro","email":"a@b.c","organization":"Org"}),
                Some(acct("pro", Some("a@b.c"), Some("Org"))),
            ),
            (
                json!({"apiProvider":"firstParty","apiKeySource":"env"}),
                Some(st("api_key", "Anthropic API key", Some("env"))),
            ),
            (
                json!({"apiProvider":"vertex"}),
                Some(st("external", "Google Vertex AI", None)),
            ),
            (
                json!({"subscriptionType":"team"}),
                Some(acct("team", None, None)),
            ),
        ];
        for (account, expected) in cases {
            assert_eq!(from_account_info(&account, None), expected, "{account}");
        }
    }

    #[test]
    fn inv_50_merge_and_same_tables() {
        let a = acct("pro", Some("a@b.c"), None);
        let b = acct("pro", Some("a@b.c"), Some("Org"));
        let c = acct("max", Some("z@b.c"), None);
        let k = st("api_key", "Anthropic API key", Some("env"));
        let k2 = st("api_key", "Anthropic API key", None);
        let n = not_logged_in();
        assert_eq!(merge(None, &a), a);
        assert_eq!(merge(Some(&a), &b), b);
        assert_eq!(
            merge(Some(&b), &a),
            b,
            "a poorer read keeps the organization"
        );
        assert_eq!(merge(Some(&a), &c), c);
        assert_eq!(merge(Some(&k2), &k), k);
        assert_eq!(merge(Some(&k), &k2), k2);
        assert_eq!(merge(Some(&n), &a), a);

        assert!(!same_status(None, &a));
        assert!(same_status(Some(&a), &a));
        assert!(!same_status(Some(&a), &b));
        assert!(same_status(Some(&k), &k));
        assert!(!same_status(Some(&k), &k2));
        assert!(same_status(Some(&n), &n));
    }

    #[test]
    fn inv_50_air_gateway_helper_regexes() {
        assert!(is_air_gateway_helper("bash ./central proxy start"));
        assert!(is_air_gateway_helper("CENTRAL-Proxy   START --x"));
        assert!(!is_air_gateway_helper("central proxystart"));
        assert!(!is_air_gateway_helper("central proxy-start"));
        assert!(!is_air_gateway_helper("proxy start"));
        assert!(!is_air_gateway_helper("centrals proxy start"));
    }

    fn fixed_probe(status: Option<AuthStatus>) -> ProbeFn {
        Box::new(move |_cwd| {
            let status = status.clone();
            Box::pin(async move {
                match status {
                    Some(s) => ProbeOutcome::Status(s),
                    None => ProbeOutcome::LoggedOut,
                }
            })
        })
    }

    /// An actor plus a test forwarder: `Status` pushes arrive on the returned
    /// receiver, barriers are acknowledged like the agent's forwarder does.
    fn actor(probe: ProbeFn, cwd: &str) -> (AuthHandle, mpsc::UnboundedReceiver<Value>) {
        let (push_tx, mut push_rx) = mpsc::unbounded_channel::<AuthPush>();
        let (out_tx, out_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            while let Some(item) = push_rx.recv().await {
                match item {
                    AuthPush::Status(v) => {
                        let _ = out_tx.send(v);
                    }
                    AuthPush::Barrier(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        (spawn_auth_actor(push_tx, probe, PathBuf::from(cwd)), out_rx)
    }

    async fn next_push(rx: &mut mpsc::UnboundedReceiver<Value>) -> Option<Value> {
        tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test]
    async fn inv_50_actor_pushes_only_on_change() {
        let status = acct("pro", Some("a@b.c"), Some("Org"));
        let (handle, mut push_rx) = actor(fixed_probe(Some(status.clone())), "/a");
        handle.probe(PathBuf::from("/a"), false);
        assert_eq!(next_push(&mut push_rx).await, Some(status.to_json()));
        handle.probe(PathBuf::from("/a"), true);
        assert_eq!(
            next_push(&mut push_rx).await,
            None,
            "same status is not re-pushed"
        );
    }

    #[tokio::test]
    async fn inv_50_actor_unreadable_probe_is_silent() {
        let (handle, mut push_rx) = actor(fixed_probe(None), "/a");
        handle.probe(PathBuf::from("/a"), false);
        assert_eq!(next_push(&mut push_rx).await, None);
    }

    #[tokio::test]
    async fn inv_50_actor_logout_reads_store_or_reports_none() {
        let known = acct("pro", Some("a@b.c"), None);
        // First probe sees the account; the post-logout read is unreadable.
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();
        let probe: ProbeFn = Box::new(move |_cwd| {
            let first = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            let known = known.clone();
            Box::pin(async move {
                if first {
                    ProbeOutcome::Status(known)
                } else {
                    ProbeOutcome::LoggedOut
                }
            })
        });
        let (handle, mut push_rx) = actor(probe, "/a");
        handle.probe(PathBuf::from("/a"), false);
        assert!(next_push(&mut push_rx).await.is_some());
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        handle.send(AuthCmd::Logout {
            cwd: PathBuf::from("/a"),
            reply: reply_tx,
        });
        reply_rx.await.expect("logout read finished");
        assert_eq!(
            next_push(&mut push_rx).await,
            Some(not_logged_in().to_json()),
            "an unreadable store after logout reports not logged in"
        );
    }

    #[tokio::test]
    async fn inv_50_actor_logout_probe_timeout_keeps_current() {
        let known = acct("pro", Some("a@b.c"), None);
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();
        let known_clone = known.clone();
        let probe: ProbeFn = Box::new(move |_cwd| {
            let first = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            let known = known_clone.clone();
            Box::pin(async move {
                if first {
                    ProbeOutcome::Status(known)
                } else {
                    ProbeOutcome::TimedOut
                }
            })
        });
        let (handle, mut push_rx) = actor(probe, "/a");
        handle.probe(PathBuf::from("/a"), false);
        assert_eq!(next_push(&mut push_rx).await, Some(known.to_json()));
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        handle.send(AuthCmd::Logout {
            cwd: PathBuf::from("/a"),
            reply: reply_tx,
        });
        reply_rx.await.expect("logout read finished");
        assert_eq!(
            next_push(&mut push_rx).await,
            None,
            "probe timeout after logout must keep current status without pushing notLoggedIn"
        );
    }

    #[tokio::test]
    async fn inv_50_actor_logout_with_no_known_status_pushes_not_logged_in() {
        let probe: ProbeFn = Box::new(move |_cwd| Box::pin(async move { ProbeOutcome::TimedOut }));
        let (handle, mut push_rx) = actor(probe, "/a");
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        handle.send(AuthCmd::Logout {
            cwd: PathBuf::from("/a"),
            reply: reply_tx,
        });
        reply_rx.await.expect("logout read finished");
        assert_eq!(
            next_push(&mut push_rx).await,
            Some(not_logged_in().to_json()),
            "logout with no known status must push notLoggedIn"
        );
    }

    #[tokio::test]
    async fn inv_50_actor_bump_epoch_drops_in_flight() {
        let known = acct("pro", Some("a@b.c"), None);
        let probe: ProbeFn = Box::new(move |_cwd| {
            let known = known.clone();
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                ProbeOutcome::Status(known)
            })
        });
        let (handle, mut push_rx) = actor(probe, "/a");
        handle.probe(PathBuf::from("/a"), false);
        handle.bump_epoch();
        assert_eq!(
            next_push(&mut push_rx).await,
            None,
            "bump_epoch before in-flight probe completes drops stale probe outcome"
        );
    }

    #[tokio::test]
    async fn inv_50_actor_session_account_and_cwd_reprobe() {
        let probed = acct("pro", Some("a@b.c"), None);
        let (handle, mut push_rx) = actor(fixed_probe(Some(probed.clone())), "/a");
        // An uninformative account pushes nothing; the cwd change re-probes.
        handle.send(AuthCmd::SessionCreated {
            cwd: PathBuf::from("/b"),
            account: Some(json!({"organizationId": "org-1", "subscriptionType": null})),
        });
        assert_eq!(next_push(&mut push_rx).await, Some(probed.to_json()));
        // The richer session account for the same identity is a new payload.
        handle.send(AuthCmd::SessionCreated {
            cwd: PathBuf::from("/b"),
            account: Some(json!({"apiProvider":"firstParty","subscriptionType":"pro",
                "email":"a@b.c","organization":"Org"})),
        });
        assert_eq!(
            next_push(&mut push_rx).await,
            Some(acct("pro", Some("a@b.c"), Some("Org")).to_json())
        );
    }

    /// D10 (Decision 12) — the `useAuthStatusSettings` change rule: cwd, the
    /// api-key helper and the serialized launch env each trigger a re-probe;
    /// an equal context does not.
    #[test]
    fn inv_78_context_change_rules() {
        let base = AuthContext {
            cwd: PathBuf::from("/repo"),
            api_key_helper: None,
            env: None,
        };
        assert!(!auth_context_changed(&base, &base.clone()));

        let other_cwd = AuthContext {
            cwd: PathBuf::from("/elsewhere"),
            ..base.clone()
        };
        assert!(auth_context_changed(&base, &other_cwd));

        let helper = AuthContext {
            api_key_helper: Some("/bin/helper".into()),
            ..base.clone()
        };
        assert!(auth_context_changed(&base, &helper));

        let env = AuthContext {
            env: Some("{\"CLAUDE_CODE_ARTIFACT\":\"1\"}".into()),
            ..base.clone()
        };
        assert!(auth_context_changed(&base, &env));
        // Equal serialized env does not re-probe.
        let same_env = AuthContext {
            env: Some("{\"CLAUDE_CODE_ARTIFACT\":\"1\"}".into()),
            ..base.clone()
        };
        assert!(!auth_context_changed(&env, &same_env));
    }
}
