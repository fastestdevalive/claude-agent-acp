//! T9B — the `auth-status.test.ts` mapper cases and the capability marker.
//! The upstream fixtures are the CLI/SDK outputs verified against the real
//! binary; the expected payloads are the same `{kind,label,detail,account}`
//! objects the Rust mapper produces.

use claude_agent_acp_rs::auth_status::{
    from_account_info, from_cli_status, merge, same_status, AuthAccount, AuthStatus,
};
use claude_agent_acp_rs::client_caps::ClientCapabilities;
use claude_agent_acp_rs::env_snapshot::EnvSnapshot;
use serde_json::json;

const CLI_SUBSCRIPTION: &str = r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"user@example.com","orgId":"org-1","orgName":"ACME","subscriptionType":"max"}"#;
const CLI_API_KEY: &str = r#"{"loggedIn":true,"authMethod":"none","apiProvider":"firstParty","apiKeySource":"apiKeyHelper"}"#;
const CLI_KEY_HELPER_AND_SUBSCRIPTION: &str = r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","apiKeySource":"apiKeyHelper","email":"user@example.com","orgName":"ACME","subscriptionType":"max"}"#;
const CLI_LOGGED_OUT: &str = r#"{"loggedIn":false,"authMethod":"none","apiProvider":"firstParty"}"#;
const CLI_SUBSCRIPTION_NO_ORG: &str = r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"user@example.com","subscriptionType":"max"}"#;
const CLI_OTHER_ACCOUNT: &str = r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"other@example.com","subscriptionType":"pro"}"#;
const CLI_BEDROCK: &str = r#"{"loggedIn":false,"authMethod":"none","apiProvider":"bedrock"}"#;

fn status(kind: &str, label: &str, detail: Option<&str>) -> AuthStatus {
    AuthStatus {
        kind: kind.into(),
        label: label.into(),
        detail: detail.map(Into::into),
        account: None,
        vendor: None,
    }
}

fn account(plan: &str, email: Option<&str>, organization: Option<&str>) -> AuthStatus {
    AuthStatus {
        kind: "account".into(),
        label: claude_agent_acp_rs::auth_status::plan_label(plan),
        detail: None,
        account: Some(AuthAccount {
            plan: Some(plan.into()),
            email: email.map(Into::into),
            organization: organization.map(Into::into),
        }),
        vendor: None,
    }
}

/// The AIR gateway helper: `/\bcentral\b/i` and `/\bproxy\s+start\b/i`, in every
/// spelling the CLI accepts, and never a command in the payload.
#[test]
fn t9b_central_helper_spellings_name_the_gateway() {
    let expected = status("gateway", "JetBrains Air Gateway", None);
    for helper in [
        "central proxy start",
        "/opt/tools/central proxy start --arbitrary-flag",
        "CeNtRaL ProXy   StArT --return-key",
        "central.exe proxy\tstart",
    ] {
        assert_eq!(
            from_cli_status(CLI_KEY_HELPER_AND_SUBSCRIPTION, Some(helper)),
            Some(expected.clone()),
            "{helper}"
        );
        assert_eq!(
            from_account_info(
                &json!({"apiProvider": "firstParty", "apiKeySource": "apiKeyHelper",
                    "subscriptionType": "max"}),
                Some(helper),
            ),
            Some(expected.clone()),
            "{helper}"
        );
    }
}

/// A non-Central helper stays the human-readable API-key identity.
#[test]
fn t9b_non_central_helper_stays_an_api_key() {
    let expected = status("api_key", "Anthropic API key", Some("API key helper"));
    for helper in [
        None,
        Some(""),
        Some("decentralized proxy start"),
        Some("centralized proxy start"),
        Some("central proxy stop"),
        Some("another proxy start"),
        Some("central proxy starter"),
    ] {
        assert_eq!(
            from_cli_status(CLI_API_KEY, helper),
            Some(expected.clone()),
            "{helper:?}"
        );
    }
}

/// An inactive helper does not override an external backend or the logged-out
/// verdict; a future key-source identifier stays an API key without a detail.
#[test]
fn t9b_inactive_helper_and_future_key_source() {
    let helper = Some("central proxy start");
    assert_eq!(
        from_cli_status(CLI_BEDROCK, helper),
        Some(status("external", "AWS Bedrock", None))
    );
    assert_eq!(
        from_cli_status(CLI_LOGGED_OUT, helper),
        Some(status("none", "Not logged in", None))
    );
    assert_eq!(
        from_account_info(
            &json!({"apiProvider": "firstParty", "apiKeySource": "env"}),
            helper
        ),
        Some(status("api_key", "Anthropic API key", Some("env")))
    );
    assert_eq!(
        from_account_info(&json!({"apiKeySource": "futureInternalIdentifier"}), helper),
        Some(status("api_key", "Anthropic API key", None))
    );
}

/// A first-party subscription maps its plan, email and organization.
#[test]
fn t9b_first_party_subscription_account() {
    assert_eq!(
        from_account_info(
            &json!({"apiProvider": "firstParty", "subscriptionType": "max",
                "email": "user@example.com", "organization": "ACME"}),
            None
        ),
        Some(account("max", Some("user@example.com"), Some("ACME")))
    );
}

/// The plan label carries exactly one "Claude" however the vendor spells it;
/// the raw plan stays in the account object.
#[test]
fn t9b_plan_labels_once() {
    for (plan, label) in [
        ("max", "Claude Max"),
        ("Claude Max", "Claude Max"),
        ("claude max", "Claude Max"),
        ("Team Premium", "Claude Team Premium"),
    ] {
        let got = from_account_info(
            &json!({"apiProvider": "firstParty", "subscriptionType": plan}),
            None,
        )
        .expect("a subscription account");
        assert_eq!(got.label, label, "{plan}");
        assert_eq!(
            got.account.and_then(|a| a.plan).as_deref(),
            Some(plan),
            "the plan stays the raw vendor string"
        );
    }
}

/// An API key source maps its detail, and an `apiKeyHelper` key outranks a
/// stored subscription when both are reported.
#[test]
fn t9b_api_key_source_and_precedence() {
    assert_eq!(
        from_account_info(
            &json!({"apiProvider": "firstParty", "apiKeySource": "ANTHROPIC_API_KEY"}),
            None
        ),
        Some(status(
            "api_key",
            "Anthropic API key",
            Some("ANTHROPIC_API_KEY")
        ))
    );
    assert_eq!(
        from_cli_status(CLI_KEY_HELPER_AND_SUBSCRIPTION, None),
        Some(status(
            "api_key",
            "Anthropic API key",
            Some("API key helper")
        ))
    );
    assert_eq!(
        from_account_info(
            &json!({"apiProvider": "firstParty", "apiKeySource": "apiKeyHelper",
                "subscriptionType": "max", "email": "user@example.com",
                "organization": "ACME"}),
            None
        ),
        Some(status(
            "api_key",
            "Anthropic API key",
            Some("API key helper")
        ))
    );
}

/// Third-party backends are external auth; the gateway backend is its own kind.
#[test]
fn t9b_external_and_gateway_backends() {
    assert_eq!(
        from_account_info(&json!({"apiProvider": "bedrock"}), None),
        Some(status("external", "AWS Bedrock", None))
    );
    assert_eq!(
        from_account_info(&json!({"apiProvider": "vertex"}), None),
        Some(status("external", "Google Vertex AI", None))
    );
    assert_eq!(
        from_account_info(&json!({"apiProvider": "gateway"}), None),
        Some(status("gateway", "Custom model gateway", None))
    );
}

/// An account with no identity signal is "no information", not logged out;
/// `tokenSource` names the OAuth token's origin, never a key source.
#[test]
fn t9b_account_without_identity_is_no_information() {
    assert_eq!(
        from_account_info(&json!({"apiProvider": "firstParty"}), None),
        None
    );
    assert_eq!(from_account_info(&json!({}), None), None);
    assert_eq!(from_account_info(&json!({}), None), None);
    assert_eq!(
        from_account_info(
            &json!({"apiProvider": "firstParty", "tokenSource": "CLAUDE_CODE_OAUTH_TOKEN"}),
            None
        ),
        None
    );
}

/// CLI probe output maps the three identities; a third-party backend outranks
/// the logged-out flag; unparseable output is not known.
#[test]
fn t9b_cli_probe_output_and_unparseable() {
    assert_eq!(
        from_cli_status(CLI_SUBSCRIPTION, None),
        Some(account("max", Some("user@example.com"), Some("ACME")))
    );
    assert_eq!(
        from_cli_status(CLI_API_KEY, None),
        Some(status(
            "api_key",
            "Anthropic API key",
            Some("API key helper")
        ))
    );
    assert_eq!(
        from_cli_status(CLI_LOGGED_OUT, None),
        Some(status("none", "Not logged in", None))
    );
    assert_eq!(
        from_cli_status(CLI_BEDROCK, None),
        Some(status("external", "AWS Bedrock", None)),
        "loggedIn tracks claude.ai only"
    );
    for stdout in ["", "Usage: claude auth status", "[]", r#"{"foo": 1}"#] {
        assert_eq!(from_cli_status(stdout, None), None, "{stdout}");
    }
}

/// A poorer read of the same identity keeps the richer stored fields; a
/// different identity replaces everything; key sources identify api_key
/// payloads.
#[test]
fn t9b_merge_same_and_different_identity() {
    let stored = account("max", Some("user@example.com"), Some("ACME"));
    assert_eq!(
        merge(
            Some(&stored),
            &account("max", Some("user@example.com"), None)
        ),
        stored,
        "the organization survives a poorer read"
    );
    // The same login read from the CLI without the organization.
    assert_eq!(
        merge(
            Some(&stored),
            &from_cli_status(CLI_SUBSCRIPTION_NO_ORG, None).expect("a subscription")
        ),
        stored
    );
    assert_eq!(
        merge(
            Some(&stored),
            &account("pro", Some("other@example.com"), None)
        ),
        account("pro", Some("other@example.com"), None),
        "another login replaces everything"
    );
    assert_eq!(
        merge(
            Some(&stored),
            &from_cli_status(CLI_OTHER_ACCOUNT, None).expect("another account")
        ),
        account("pro", Some("other@example.com"), None)
    );
    assert_eq!(
        merge(
            Some(&stored),
            &status("api_key", "Anthropic API key", Some("env"))
        ),
        status("api_key", "Anthropic API key", Some("env"))
    );
    let helper = status("api_key", "Anthropic API key", Some("API key helper"));
    let env = status("api_key", "Anthropic API key", Some("env"));
    assert!(!same_status(Some(&helper), &env));
    assert!(same_status(Some(&stored), &stored.clone()));
}

/// The initialize response advertises the extension in
/// `agentCapabilities._meta.authStatus`, never at the top level.
#[test]
fn t9b_initialize_advertises_the_auth_status_marker() {
    let response = claude_agent_acp_rs::agent::initialize_response(
        None,
        &ClientCapabilities::default(),
        &EnvSnapshot::empty(),
        "/bin/adapter",
        &[],
        &[],
        false,
    );
    assert_eq!(
        response.pointer("/agentCapabilities/_meta/authStatus"),
        Some(&json!({})),
        "{response}"
    );
    assert!(
        response.pointer("/_meta/authStatus").is_none(),
        "the marker lives in agentCapabilities only"
    );
}

/// `auth-status.test.ts` > "pushes the logged-out CLI verdict even though the
/// probe exits non-zero": the CLI exits 1 with the logged-out JSON on stdout;
/// `run_cli` keeps the stdout with `success = false` and the mapper still
/// reports "not logged in".
#[tokio::test]
async fn t9b_non_zero_probe_exit_still_maps_the_logged_out_json() {
    use claude_agent_acp_rs::auth_status::{
        not_logged_in, probe_cli_in, ProbeOutcome, ProbeRequest,
    };

    let tmp = std::env::temp_dir().join(format!("t9b-auth-exit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).expect("temp dir");
    // `sh <cwd>/auth status --json`: the script prints the JSON and exits 1.
    std::fs::write(
        tmp.join("auth"),
        "#!/bin/sh\nprintf '%s\\n' '{\"loggedIn\":false,\"authMethod\":\"none\",\"apiProvider\":\"firstParty\"}'\nexit 1\n",
    )
    .expect("script");
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        probe_cli_in(
            Some(std::path::Path::new("/bin/sh")),
            &ProbeRequest::in_dir(&tmp),
            &[],
            &EnvSnapshot::empty(),
        ),
    )
    .await
    .expect("the probe finished");
    assert_eq!(outcome, ProbeOutcome::Status(not_logged_in()));
    let _ = std::fs::remove_dir_all(&tmp);
}
