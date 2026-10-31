//! Turn 3 lane B: carried items and the provider, gateway and hide-claude-auth
//! packages (agent-level tests).

use super::*;

/// 0d / U-75: a failed write of a raw update is reported (the forwarder sends the
/// actor `UpdateSendFailed`); a good write is not.
#[test]
fn t3b_a_failed_raw_update_write_is_reported() {
    let update = json!({"sessionId": "s", "update": {"sessionUpdate": "async_task_state_update"}});
    assert!(write_raw_update(update.clone(), |_| Ok::<(), ()>(())));
    assert!(!write_raw_update(update, |_| Err::<(), _>(
        "connection closed"
    )));
}

/// M-103: setting the same gateway again (same type, URL and headers) reuses the
/// previous listing without asking the gateway; any difference lists again.
#[tokio::test]
async fn t3b_the_same_gateway_reuses_its_listing() {
    use super::providers::ProviderConfig;
    use super::recreate::list_provider_models;
    // A gateway nobody listens on: a listing request would fail.
    let url = "http://127.0.0.1:1";
    let headers = vec![("x-route".to_string(), "a".to_string())];
    let mut current = ProviderConfig::new("anthropic", url, headers.clone());
    current.listed_models = Some(vec!["claude-sonnet-5".to_string()]);
    current.bedrock_region_prefix = Some("eu".to_string());
    let mut again = ProviderConfig::new("anthropic", url, headers.clone());
    let reused = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        list_provider_models(Some(&current), &mut again, &None),
    )
    .await
    .expect("no network wait");
    assert!(reused.is_ok());
    assert_eq!(again.listed_models, current.listed_models);
    assert_eq!(again.bedrock_region_prefix, current.bedrock_region_prefix);
    // Another header: a rotated token lists again, and the dead gateway fails it.
    let mut rotated = ProviderConfig::new("anthropic", url, vec![("x-route".into(), "b".into())]);
    let listed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        list_provider_models(Some(&current), &mut rotated, &None),
    )
    .await
    .expect("the refused connection fails fast");
    assert!(listed.is_err());
    // A previous config that listed nothing is not reused either.
    let mut unlisted = current.clone();
    unlisted.listed_models = None;
    let mut next = ProviderConfig::new("anthropic", url, headers);
    let listed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        list_provider_models(Some(&unlisted), &mut next, &None),
    )
    .await
    .expect("fails fast");
    assert!(listed.is_err());
    // `vertex` gateways are not listed at all.
    let mut vertex = ProviderConfig::new("vertex", url, Vec::new());
    assert!(list_provider_models(None, &mut vertex, &None).await.is_ok());
    assert_eq!(vertex.listed_models, None);
}
