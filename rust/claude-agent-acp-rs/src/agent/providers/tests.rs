//! Ports of the pure parts of upstream `providers.test.ts`.

use super::*;

fn env_of(config: &ProviderConfig) -> std::collections::HashMap<String, String> {
    create_env_for_provider(config).into_iter().collect()
}

fn wire(error: &Error) -> Value {
    serde_json::to_value(error).expect("error")
}

fn set_request(extra: Value) -> Value {
    let mut base = json!({ "providerId": "main", "apiType": "anthropic", "baseUrl": "https://gateway.example" });
    if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in extra {
            if v.is_null() {
                base.remove(k);
            } else {
                base.insert(k.clone(), v.clone());
            }
        }
    }
    base
}

/// lists one mutually exclusive provider slot with native routing
#[test]
fn t3b_lists_one_provider_slot_with_native_routing() {
    let config = default_provider_config(&EnvSnapshot::empty(), None);
    assert_eq!(
        list_providers_result(&config),
        json!({ "providers": [{
            "providerId": "main",
            "supported": ["anthropic", "bedrock", "vertex"],
            "required": false,
            "current": { "apiType": "anthropic", "baseUrl": "https://api.anthropic.com" },
        }] })
    );
}

/// O-09: the defaults follow the provider variables of the environment snapshot
#[test]
fn t3b_defaults_follow_the_provider_variables() {
    let snapshot = |pairs: &[(&str, &str)]| EnvSnapshot::from_pairs(pairs.iter().copied());
    let bedrock = default_provider_config(&snapshot(&[("CLAUDE_CODE_USE_BEDROCK", "1")]), None);
    assert_eq!(
        (bedrock.api_type.as_str(), bedrock.base_url.as_str()),
        ("bedrock", "https://bedrock-runtime.amazonaws.com")
    );
    let bedrock = default_provider_config(
        &snapshot(&[
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("ANTHROPIC_BEDROCK_BASE_URL", "https://b.example"),
        ]),
        None,
    );
    assert_eq!(bedrock.base_url, "https://b.example");
    let vertex = default_provider_config(&snapshot(&[("CLAUDE_CODE_USE_VERTEX", "1")]), None);
    assert_eq!(
        (vertex.api_type.as_str(), vertex.base_url.as_str()),
        ("vertex", "https://aiplatform.googleapis.com")
    );
    // Bedrock wins over Vertex, as in upstream's order.
    let both = default_provider_config(
        &snapshot(&[
            ("CLAUDE_CODE_USE_BEDROCK", "1"),
            ("CLAUDE_CODE_USE_VERTEX", "1"),
        ]),
        None,
    );
    assert_eq!(both.api_type, "bedrock");
    let based = default_provider_config(
        &snapshot(&[("ANTHROPIC_BASE_URL", "https://p.example")]),
        None,
    );
    assert_eq!(
        (based.api_type.as_str(), based.base_url.as_str()),
        ("anthropic", "https://p.example")
    );
    // An empty variable is falsy for the use-flags.
    let empty = default_provider_config(&snapshot(&[("CLAUDE_CODE_USE_BEDROCK", "")]), None);
    assert_eq!(empty.api_type, "anthropic");
    // The legacy gateway wins over all of them.
    let gateway = json!({ "methodId": "gateway-bedrock",
        "_meta": { "gateway": { "baseUrl": "https://g.example", "headers": { "x": "1" } } } });
    let from_gateway = default_provider_config(
        &snapshot(&[("CLAUDE_CODE_USE_VERTEX", "1")]),
        Some(&gateway),
    );
    assert_eq!(
        (
            from_gateway.api_type.as_str(),
            from_gateway.base_url.as_str()
        ),
        ("bedrock", "https://g.example")
    );
    assert_eq!(
        from_gateway.headers,
        vec![("x".to_string(), "1".to_string())]
    );
}

/// rejects set for an unknown providerId / an unsupported apiType / an empty or non-http baseUrl (the four exact -32602 messages)
#[test]
fn t3b_set_rejects_with_the_exact_messages() {
    let error = parse_set_provider(&set_request(json!({ "providerId": "openai" }))).unwrap_err();
    assert_eq!(wire(&error)["code"], -32602);
    assert_eq!(
        wire(&error)["message"],
        "Invalid params: Unknown provider ID \"openai\"; expected \"main\"."
    );
    assert_eq!(wire(&error)["data"], json!({ "providerId": "openai" }));

    let error = parse_set_provider(&set_request(json!({ "apiType": "openai" }))).unwrap_err();
    assert_eq!(
        wire(&error)["message"],
        "Invalid params: Unsupported apiType \"openai\" for provider \"main\"."
    );
    assert_eq!(
        wire(&error)["data"],
        json!({ "apiType": "openai", "supported": ["anthropic", "bedrock", "vertex"] })
    );

    for bad in [
        json!(""),
        json!("ftp://nope.example"),
        json!("not a url"),
        json!("   "),
    ] {
        let error = parse_set_provider(&set_request(json!({ "baseUrl": bad }))).unwrap_err();
        assert_eq!(wire(&error)["code"], -32602);
        assert_eq!(
            wire(&error)["message"],
            "Invalid params: baseUrl must be a non-empty absolute http(s) URL."
        );
        assert_eq!(wire(&error)["data"], json!({ "baseUrl": bad }));
    }

    // rejects vertex set without _meta project/region, and with a partial _meta
    for meta in [
        None,
        Some(json!({ "claudeCode": { "vertex": { "projectId": "my-project" } } })),
        Some(json!({ "claudeCode": { "vertex": { "projectId": "  ", "region": "x" } } })),
    ] {
        let mut request = set_request(json!({ "apiType": "vertex" }));
        if let Some(meta) = meta {
            request["_meta"] = meta;
        }
        let error = parse_set_provider(&request).unwrap_err();
        assert_eq!(wire(&error)["code"], -32602);
        assert_eq!(
            wire(&error)["message"],
            "Invalid params: vertex apiType requires non-empty `_meta.claudeCode.vertex.projectId` and `_meta.claudeCode.vertex.region`."
        );
        assert!(wire(&error).get("data").is_none());
    }
}

/// reflects apiType/baseUrl after set, and never echoes headers
#[test]
fn t3b_list_never_echoes_headers() {
    let config = parse_set_provider(&set_request(json!({
        "baseUrl": "https://gateway.example/v1",
        "headers": { "authorization": "Bearer secret" },
    })))
    .expect("valid");
    assert_eq!(
        config.headers,
        vec![("authorization".to_string(), "Bearer secret".to_string())]
    );
    let listed = list_providers_result(&config);
    assert_eq!(
        listed["providers"][0]["current"],
        json!({ "apiType": "anthropic", "baseUrl": "https://gateway.example/v1" })
    );
    assert!(!listed.to_string().contains("secret"));
}

/// routes anthropic / bedrock / vertex provider config into session env
#[test]
fn t3b_routes_provider_config_into_the_env() {
    let anthropic = ProviderConfig::new(
        "anthropic",
        "https://gateway.example",
        vec![("x-api-key".into(), "test".into())],
    );
    let env = env_of(&anthropic);
    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "acp-proxy");
    assert_eq!(env["ANTHROPIC_BASE_URL"], "https://gateway.example");
    assert_eq!(env["ANTHROPIC_CUSTOM_HEADERS"], "x-api-key: test");
    // The routing it replaces is reset.
    assert_eq!(env["ANTHROPIC_API_KEY"], "");
    assert_eq!(env["CLAUDE_CODE_USE_BEDROCK"], "0");
    assert_eq!(env["CLAUDE_CODE_OAUTH_TOKEN"], "");

    let mut bedrock = ProviderConfig::new(
        "bedrock",
        "https://gateway.example",
        vec![("custom-header".into(), "test".into())],
    );
    let env = env_of(&bedrock);
    assert_eq!(env["CLAUDE_CODE_USE_BEDROCK"], "1");
    assert_eq!(env["AWS_BEARER_TOKEN_BEDROCK"], "acp-proxy");
    assert_eq!(env["ANTHROPIC_BEDROCK_BASE_URL"], "https://gateway.example");
    assert_eq!(env["ANTHROPIC_CUSTOM_HEADERS"], "custom-header: test");
    assert!(!env.contains_key("ANTHROPIC_BEDROCK_REGION_PREFIX"));
    // O-70: the region prefix of the listed models.
    bedrock.bedrock_region_prefix = Some("eu".into());
    assert_eq!(env_of(&bedrock)["ANTHROPIC_BEDROCK_REGION_PREFIX"], "eu");

    let mut vertex = ProviderConfig::new(
        "vertex",
        "https://vertex.example",
        vec![("custom-header".into(), "test".into())],
    );
    vertex.vertex = Some(VertexConfig {
        project_id: "my-project".into(),
        region: "us-east5".into(),
    });
    let env = env_of(&vertex);
    assert_eq!(env["CLAUDE_CODE_USE_VERTEX"], "1");
    assert_eq!(env["ANTHROPIC_VERTEX_BASE_URL"], "https://vertex.example");
    assert_eq!(env["ANTHROPIC_VERTEX_PROJECT_ID"], "my-project");
    assert_eq!(env["CLOUD_ML_REGION"], "us-east5");
    assert_eq!(env["ANTHROPIC_CUSTOM_HEADERS"], "custom-header: test");

    // leaves the native Vertex base URL unset for the default endpoint
    vertex.base_url = "https://aiplatform.googleapis.com".into();
    vertex.vertex = Some(VertexConfig {
        project_id: "my-project".into(),
        region: "global".into(),
    });
    let env = env_of(&vertex);
    assert_eq!(env["ANTHROPIC_VERTEX_BASE_URL"], "");
    assert_eq!(env["CLOUD_ML_REGION"], "global");
}

/// overrides user routing settings while preserving unrelated settings
#[test]
fn t3b_the_route_overrides_user_routing_settings() {
    let provider = ProviderConfig::new("anthropic", "https://gateway.example", Vec::new());
    let user = json!({
        "apiKeyHelper": "risky-helper",
        "env": {
            "ANTHROPIC_BASE_URL": "https://user-gateway.example",
            "ANTHROPIC_API_KEY": "user-secret",
            "UNRELATED_SETTING": "preserved",
        },
        "model": "claude-sonnet-4-6",
    });
    let merged = settings_with_provider(Some(user), &create_env_for_provider(&provider));
    assert_eq!(merged["apiKeyHelper"], "");
    assert_eq!(merged["model"], "claude-sonnet-4-6");
    assert_eq!(
        merged["env"]["ANTHROPIC_BASE_URL"],
        "https://gateway.example"
    );
    assert_eq!(merged["env"]["ANTHROPIC_API_KEY"], "");
    assert_eq!(merged["env"]["ANTHROPIC_AUTH_TOKEN"], "acp-proxy");
    assert_eq!(merged["env"]["UNRELATED_SETTING"], "preserved");
    // Without user settings the tier still carries the route.
    let bare = settings_with_provider(None, &create_env_for_provider(&provider));
    assert_eq!(bare["apiKeyHelper"], "");
    assert_eq!(bare["env"]["ANTHROPIC_BASE_URL"], "https://gateway.example");
}

/// O-73: the spawn of a session carries the route after every other env source, and the settings tier too
#[test]
fn t3b_the_spawn_carries_the_route_last() {
    let mut spawn = crate::process::SpawnOptions {
        extra_env: vec![("ANTHROPIC_BASE_URL".into(), "https://user.example".into())],
        settings_arg: Some(r#"{"model":"haiku"}"#.into()),
        ..Default::default()
    };
    let provider = ProviderConfig::new("anthropic", "https://gateway.example", Vec::new());
    apply_provider_to_spawn(&mut spawn, Some(&provider));
    let last = spawn
        .extra_env
        .iter()
        .rev()
        .find(|(k, _)| k == "ANTHROPIC_BASE_URL")
        .expect("base url");
    assert_eq!(last.1, "https://gateway.example");
    let settings: Value =
        serde_json::from_str(spawn.settings_arg.as_deref().expect("settings")).expect("json");
    assert_eq!(settings["model"], "haiku");
    assert_eq!(settings["apiKeyHelper"], "");
    // Without a provider nothing changes.
    let mut native = crate::process::SpawnOptions::default();
    apply_provider_to_spawn(&mut native, None);
    assert!(native.extra_env.is_empty());
    assert!(native.settings_arg.is_none());
}

/// authenticate validation: a payload that IS present has to be usable; none at all is fine
#[test]
fn t3b_gateway_payload_validation() {
    let request =
        |gateway: Value| json!({ "methodId": "gateway", "_meta": { "gateway": gateway } });
    assert!(validate_gateway_payload(&json!({ "methodId": "gateway" })).is_ok());
    assert!(validate_gateway_payload(&json!({ "methodId": "gateway", "_meta": {} })).is_ok());
    assert!(validate_gateway_payload(&request(Value::Null)).is_ok());
    assert!(validate_gateway_payload(&request(json!({ "baseUrl": "https://g.example" }))).is_ok());
    assert!(validate_gateway_payload(&request(json!({
        "baseUrl": "https://g.example", "headers": { "x": "y" }
    })))
    .is_ok());
    for bad in [
        json!({}),
        json!({ "baseUrl": "" }),
        json!({ "baseUrl": "ftp://g.example" }),
        json!({ "baseUrl": 5 }),
        json!("nope"),
    ] {
        let error = validate_gateway_payload(&request(bad)).unwrap_err();
        assert_eq!(wire(&error)["code"], -32602);
        assert_eq!(
            wire(&error)["message"],
            "Invalid params: `_meta.gateway.baseUrl` must be a non-empty absolute http(s) URL."
        );
    }
    let error = validate_gateway_payload(&request(json!({
        "baseUrl": "https://g.example", "headers": "x"
    })))
    .unwrap_err();
    assert_eq!(
        wire(&error)["message"],
        "Invalid params: `_meta.gateway.headers` must be an object of header names to values."
    );
    assert!(wire(&error).get("data").is_none());
    // The stored request becomes a routing config.
    let config = gateway_request_to_provider_config(Some(&json!({
        "methodId": "gateway", "_meta": { "gateway": { "baseUrl": "https://g.example" } }
    })))
    .expect("config");
    assert_eq!(config.api_type, "anthropic");
    assert!(gateway_request_to_provider_config(Some(&json!({ "methodId": "gateway" }))).is_none());
    assert!(gateway_request_to_provider_config(None).is_none());
}

/// same-gateway listing reuse needs the very same headers
#[test]
fn t3b_same_headers_ignores_order_but_not_values() {
    let a = vec![
        ("a".to_string(), "1".to_string()),
        ("b".to_string(), "2".to_string()),
    ];
    let b = vec![
        ("b".to_string(), "2".to_string()),
        ("a".to_string(), "1".to_string()),
    ];
    assert!(same_headers(&a, &b));
    assert!(!same_headers(&a, &[("a".to_string(), "1".to_string())]));
    assert!(!same_headers(
        &a,
        &[
            ("a".to_string(), "9".to_string()),
            ("b".to_string(), "2".to_string())
        ]
    ));
}

/// O-74: only a listing narrows the picker
#[test]
fn t3b_a_listing_narrows_the_picker() {
    assert_eq!(gateway_models_of(None), None);
    let mut provider = ProviderConfig::new("anthropic", "https://g.example", Vec::new());
    assert_eq!(gateway_models_of(Some(&provider)), None);
    provider.listed_models = Some(vec!["claude-sonnet-5".into()]);
    provider.bedrock_region_prefix = Some("eu".into());
    assert_eq!(
        gateway_models_of(Some(&provider)),
        Some((vec!["claude-sonnet-5".to_string()], Some("eu".to_string())))
    );
}

/// `authorization.test.ts` > "uses gateway env after anthropic gateway auth"
/// and "uses gateway env after gateway auth": `authenticate` stores the
/// gateway as the routing config, and a later session spawn carries its env
/// after every other source (the user's env included).
#[test]
fn t8c_gateway_auth_env_reaches_the_spawn() {
    let mut spawn = crate::process::SpawnOptions {
        extra_env: vec![("userEnv".into(), "userEnv".into())],
        ..Default::default()
    };
    let config = gateway_request_to_provider_config(Some(&json!({
        "methodId": "gateway",
        "_meta": {"gateway": {"baseUrl": "https://gateway.example",
            "headers": {"x-api-key": "test"}}}
    })))
    .expect("anthropic gateway");
    apply_provider_to_spawn(&mut spawn, Some(&config));
    let env: std::collections::HashMap<String, String> = spawn.extra_env.into_iter().collect();
    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "acp-proxy");
    assert_eq!(env["ANTHROPIC_BASE_URL"], "https://gateway.example");
    assert_eq!(env["ANTHROPIC_CUSTOM_HEADERS"], "x-api-key: test");
    assert_eq!(env["userEnv"], "userEnv");

    let mut spawn = crate::process::SpawnOptions::default();
    let config = gateway_request_to_provider_config(Some(&json!({
        "methodId": "gateway-bedrock",
        "_meta": {"gateway": {"baseUrl": "https://gateway.example",
            "headers": {"custom-header": "test"}}}
    })))
    .expect("bedrock gateway");
    apply_provider_to_spawn(&mut spawn, Some(&config));
    let env: std::collections::HashMap<String, String> = spawn.extra_env.into_iter().collect();
    assert_eq!(env["CLAUDE_CODE_USE_BEDROCK"], "1");
    assert_eq!(env["AWS_BEARER_TOKEN_BEDROCK"], "acp-proxy");
    assert_eq!(env["ANTHROPIC_BEDROCK_BASE_URL"], "https://gateway.example");
    assert_eq!(env["ANTHROPIC_CUSTOM_HEADERS"], "custom-header: test");
}
