//! Ports of the upstream `authorization.test.ts` guard cases that need no
//! session: the predicates, the creation verdicts and the refusal errors.

use super::*;
use std::cell::RefCell;

const OUTRANKS_SUBSCRIPTION: [&str; 3] =
    ["ANTHROPIC_API_KEY", "apiKeyHelper", "/login managed key"];

/// billsClaudeSubscription > classifies apiKeySource %s
#[test]
fn t3b_classifies_every_api_key_source() {
    for source in [
        "ANTHROPIC_API_KEY",
        "apiKeyHelper",
        "/login managed key",
        "none",
        "user",
        "project",
        "org",
        "temporary",
        "oauth",
        "a-source-only-a-future-cli-emits",
    ] {
        let account = json!({ "subscriptionType": "pro", "apiKeySource": source });
        assert_eq!(
            bills_claude_subscription(Some(&account)),
            !OUTRANKS_SUBSCRIPTION.contains(&source),
            "{source}"
        );
    }
}

/// billsClaudeSubscription > classifies apiProvider %s
#[test]
fn t3b_classifies_every_api_provider() {
    for provider in [
        "firstParty",
        "bedrock",
        "vertex",
        "foundry",
        "anthropicAws",
        "anthropicGoogleCloud",
        "mantle",
        "gateway",
    ] {
        let account = json!({ "subscriptionType": "pro", "apiProvider": provider });
        assert_eq!(
            bills_claude_subscription(Some(&account)),
            provider == "firstParty",
            "{provider}"
        );
    }
}

/// billsClaudeSubscription > bills the subscription when apiProvider and apiKeySource are absent / does not bill when a bearer token pays / is false without a subscription
#[test]
fn t3b_bills_only_a_subscription_that_nothing_outranks() {
    assert!(bills_claude_subscription(Some(
        &json!({ "subscriptionType": "max" })
    )));
    assert!(!bills_claude_subscription(Some(&json!({
        "subscriptionType": "pro", "tokenSource": "ANTHROPIC_AUTH_TOKEN", "apiKeySource": "none"
    }))));
    assert!(!bills_claude_subscription(Some(&json!({}))));
    assert!(!bills_claude_subscription(None));
    assert!(!bills_claude_subscription(Some(
        &json!({ "apiKeySource": "ANTHROPIC_API_KEY" })
    )));
}

/// fails closed at session creation > refuses or allows $name (the verdict table)
#[test]
fn t3b_the_creation_verdict_table() {
    // None: allowed. Some(true): the subscription refusal. Some(false): signed out.
    let cases: [(&str, Value, Option<bool>); 9] = [
        ("a logged-out CLI", json!({}), Some(false)),
        (
            "an account with no key in use",
            json!({ "apiKeySource": "none" }),
            Some(false),
        ),
        (
            "a claude.ai subscription",
            json!({ "subscriptionType": "pro" }),
            Some(true),
        ),
        (
            "ANTHROPIC_API_KEY",
            json!({ "apiKeySource": "ANTHROPIC_API_KEY" }),
            None,
        ),
        (
            "apiKeyHelper",
            json!({ "apiKeySource": "apiKeyHelper" }),
            None,
        ),
        (
            "a Console login",
            json!({ "apiKeySource": "/login managed key" }),
            None,
        ),
        (
            "a bearer token",
            json!({ "tokenSource": "ANTHROPIC_AUTH_TOKEN" }),
            None,
        ),
        ("bedrock", json!({ "apiProvider": "bedrock" }), None),
        ("vertex", json!({ "apiProvider": "vertex" }), None),
    ];
    for (name, account, verdict) in cases {
        let refusal = decide(&account);
        match verdict {
            None => assert!(refusal.is_none(), "{name}"),
            Some(subscription) => {
                let error = refusal.unwrap_or_else(|| panic!("{name} is refused"));
                let wire = serde_json::to_value(&error).expect("error");
                assert_eq!(wire["code"], -32000, "{name}");
                if subscription {
                    assert_eq!(
                        wire["message"],
                        "Authentication required: This integration does not support using claude.ai subscriptions."
                    );
                    assert_eq!(
                        wire["data"],
                        json!({ "reason": "claude_subscription_not_supported" })
                    );
                } else {
                    assert_eq!(
                        wire["message"],
                        "Authentication required: Sign in with an API key or a Console account to use this integration."
                    );
                    assert!(wire.get("data").is_none(), "no reason on a plain sign-out");
                }
            }
        }
    }
}

/// --hide-claude-auth argv flag
#[test]
fn t3b_the_flag_is_an_argv_entry() {
    let argv = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(should_hide_claude_auth(&argv(&["--hide-claude-auth"])));
    assert!(should_hide_claude_auth(&argv(&[
        "--x",
        "--hide-claude-auth"
    ])));
    assert!(!should_hide_claude_auth(&argv(&[
        "--hide-claude-auth=1",
        "--cli"
    ])));
    assert!(!should_hide_claude_auth(&[]));
}

/// per-turn guard: the account hook runs before the decision, a missing account fails open and warns once per session
#[test]
fn t3b_the_turn_guard_reports_the_account_first_and_fails_open() {
    let order = RefCell::new(Vec::<String>::new());
    let warn = |message: &str| order.borrow_mut().push(format!("warn:{message}"));
    let mut state = GuardState::default();
    let subscription = json!({ "subscriptionType": "pro" });
    let refusal =
        refuse_claude_subscription_turn("s1", Some(&subscription), &mut state, &warn, |_| {
            order.borrow_mut().push("account".into());
        });
    assert!(refusal.is_some(), "a subscription turn is refused");
    assert_eq!(*order.borrow(), vec!["account".to_string()]);
    // No account: fails open; the degraded warning goes out once.
    for _ in 0..3 {
        assert!(refuse_claude_subscription_turn("s1", None, &mut state, &warn, |_| {}).is_none());
    }
    let warnings: Vec<String> = order
        .borrow()
        .iter()
        .filter(|entry| entry.starts_with("warn:"))
        .cloned()
        .collect();
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        warnings[0],
        "warn:Session s1: the --hide-claude-auth subscription guard is degraded (the CLI reported no account at initialize). This session cannot detect a claude.ai subscription."
    );
    // Another session warns again.
    let mut other = GuardState::default();
    refuse_claude_subscription_turn("s2", None, &mut other, &warn, |_| {});
    assert_eq!(
        order
            .borrow()
            .iter()
            .filter(|e| e.starts_with("warn:"))
            .count(),
        2
    );
}
