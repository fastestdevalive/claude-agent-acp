//! Client-managed LLM routing: the `main` provider of `providers/*` and the
//! legacy gateway `authenticate` (upstream `acp-agent.ts` `ProviderConfig`,
//! `createEnvForProvider`, `gatewayRequestToProviderConfig`, `isValidBaseUrl`,
//! `defaultProviderConfig`).

use agent_client_protocol::Error;
use serde_json::{json, Map, Value};

use crate::env_snapshot::EnvSnapshot;
use crate::gateway_models::PROVIDER_AUTH_TOKEN_PLACEHOLDER;

pub const PROVIDER_ID: &str = "main";
pub const SUPPORTED_PROTOCOLS: [&str; 3] = ["anthropic", "bedrock", "vertex"];
const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_VERTEX_BASE_URL: &str = "https://aiplatform.googleapis.com";
const DEFAULT_BEDROCK_BASE_URL: &str = "https://bedrock-runtime.amazonaws.com";

/// Project and region of a `vertex` provider (`_meta.claudeCode.vertex`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexConfig {
    pub project_id: String,
    pub region: String,
}

/// The routing config of the `main` provider, shared by `providers/set` and the
/// legacy gateway `authenticate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub api_type: String,
    pub base_url: String,
    /// In request order.
    pub headers: Vec<(String, String)>,
    pub vertex: Option<VertexConfig>,
    /// Model ids an `anthropic` or `bedrock` gateway listed when it was set via
    /// `providers/set`; the model picker offers only these. `None` when the listing
    /// failed or the config came from elsewhere, so nothing is narrowed.
    pub listed_models: Option<Vec<String>>,
    /// The cross-region prefix of the model ids a `bedrock` gateway listed, passed
    /// to the CLI as `ANTHROPIC_BEDROCK_REGION_PREFIX`.
    pub bedrock_region_prefix: Option<String>,
}

impl ProviderConfig {
    pub fn new(api_type: &str, base_url: &str, headers: Vec<(String, String)>) -> Self {
        Self {
            api_type: api_type.to_string(),
            base_url: base_url.to_string(),
            headers,
            vertex: None,
            listed_models: None,
            bedrock_region_prefix: None,
        }
    }
}

/// `invalidParams(data, message)`: `-32602` with the SDK's message prefix.
fn invalid_params(data: Option<Value>, message: &str) -> Error {
    let error = Error::new(-32602, format!("Invalid params: {message}"));
    match data {
        Some(data) => error.data(data),
        None => error,
    }
}

/// `isValidBaseUrl`: a non-empty absolute http(s) URL.
pub fn is_valid_base_url(base_url: Option<&Value>) -> bool {
    let Some(Value::String(text)) = base_url else {
        return false;
    };
    if text.trim().is_empty() {
        return false;
    }
    url::Url::parse(text).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

/// A JSON header value as JavaScript would template it into a string.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn headers_of(value: Option<&Value>) -> Vec<(String, String)> {
    value
        .and_then(Value::as_object)
        .map(|map| map.iter().map(|(k, v)| (k.clone(), js_string(v))).collect())
        .unwrap_or_default()
}

/// The `providers/set` request, validated (`unstable_setProvider`, before the
/// listing).
pub fn parse_set_provider(params: &Value) -> Result<ProviderConfig, Error> {
    let provider_id = params.get("providerId");
    if provider_id.and_then(Value::as_str) != Some(PROVIDER_ID) {
        let shown = provider_id.map_or_else(|| "undefined".to_string(), js_string);
        let data = provider_id.map_or_else(|| json!({}), |id| json!({ "providerId": id }));
        return Err(invalid_params(
            Some(data),
            &format!("Unknown provider ID \"{shown}\"; expected \"{PROVIDER_ID}\"."),
        ));
    }
    let api_type = params.get("apiType");
    let api_type_text = api_type.and_then(Value::as_str);
    if !api_type_text.is_some_and(|t| SUPPORTED_PROTOCOLS.contains(&t)) {
        let shown = api_type.map_or_else(|| "undefined".to_string(), js_string);
        let data = match api_type {
            Some(api_type) => json!({ "apiType": api_type, "supported": SUPPORTED_PROTOCOLS }),
            None => json!({ "supported": SUPPORTED_PROTOCOLS }),
        };
        return Err(invalid_params(
            Some(data),
            &format!("Unsupported apiType \"{shown}\" for provider \"{PROVIDER_ID}\"."),
        ));
    }
    let base_url = params.get("baseUrl");
    if !is_valid_base_url(base_url) {
        let data = base_url.map_or_else(|| json!({}), |url| json!({ "baseUrl": url }));
        return Err(invalid_params(
            Some(data),
            "baseUrl must be a non-empty absolute http(s) URL.",
        ));
    }
    let api_type = api_type_text.unwrap_or_default();
    let mut config = ProviderConfig::new(
        api_type,
        base_url.and_then(Value::as_str).unwrap_or_default(),
        headers_of(params.get("headers")),
    );
    // Vertex requires project + region, which the standard payload cannot carry,
    // so they arrive via `_meta.claudeCode.vertex`.
    if api_type == "vertex" {
        let vertex = params
            .get("_meta")
            .and_then(|m| m.get("claudeCode"))
            .and_then(|c| c.get("vertex"));
        let field = |name: &str| {
            vertex
                .and_then(|v| v.get(name))
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
        };
        match (field("projectId"), field("region")) {
            (Some(project_id), Some(region)) => {
                config.vertex = Some(VertexConfig {
                    project_id: project_id.to_string(),
                    region: region.to_string(),
                });
            }
            _ => {
                return Err(invalid_params(
                    None,
                    "vertex apiType requires non-empty `_meta.claudeCode.vertex.projectId` and `_meta.claudeCode.vertex.region`.",
                ));
            }
        }
    }
    Ok(config)
}

/// The legacy gateway `authenticate` payload (`_meta.gateway`), validated.
/// `Ok(())` for a call that carries no gateway at all.
pub fn validate_gateway_payload(params: &Value) -> Result<(), Error> {
    let gateway = params.get("_meta").and_then(|m| m.get("gateway"));
    let Some(gateway) = gateway.filter(|g| !g.is_null()) else {
        return Ok(());
    };
    let object = gateway.as_object();
    if !object.is_some_and(|g| is_valid_base_url(g.get("baseUrl"))) {
        let base_url = gateway.get("baseUrl");
        let data = base_url.map_or_else(|| json!({}), |url| json!({ "baseUrl": url }));
        return Err(invalid_params(
            Some(data),
            "`_meta.gateway.baseUrl` must be a non-empty absolute http(s) URL.",
        ));
    }
    let headers = object.and_then(|g| g.get("headers"));
    if headers.is_some_and(|h| !(h.is_object() || h.is_array() || h.is_null())) {
        return Err(invalid_params(
            None,
            "`_meta.gateway.headers` must be an object of header names to values.",
        ));
    }
    Ok(())
}

/// `gatewayRequestToProviderConfig`: the `authenticate` request as a provider
/// config; `None` when it carries no usable gateway.
pub fn gateway_request_to_provider_config(request: Option<&Value>) -> Option<ProviderConfig> {
    let request = request?;
    let gateway = request.get("_meta").and_then(|m| m.get("gateway"))?;
    let base_url = gateway.get("baseUrl");
    if !is_valid_base_url(base_url) {
        return None;
    }
    let api_type = if request.get("methodId").and_then(Value::as_str) == Some("gateway-bedrock") {
        "bedrock"
    } else {
        "anthropic"
    };
    Some(ProviderConfig::new(
        api_type,
        base_url.and_then(Value::as_str).unwrap_or_default(),
        headers_of(gateway.get("headers")),
    ))
}

/// `defaultProviderConfig`: what `providers/list` reports without an override,
/// from the legacy gateway, else the provider variables of the environment
/// (read through the snapshot only).
pub fn default_provider_config(env: &EnvSnapshot, gateway_auth: Option<&Value>) -> ProviderConfig {
    if let Some(config) = gateway_request_to_provider_config(gateway_auth) {
        return config;
    }
    let truthy = |name: &str| env.get(name).is_some_and(|value| !value.is_empty());
    if truthy("CLAUDE_CODE_USE_BEDROCK") {
        let base = env
            .get("ANTHROPIC_BEDROCK_BASE_URL")
            .unwrap_or(DEFAULT_BEDROCK_BASE_URL);
        return ProviderConfig::new("bedrock", base, Vec::new());
    }
    if truthy("CLAUDE_CODE_USE_VERTEX") {
        let base = env
            .get("ANTHROPIC_VERTEX_BASE_URL")
            .unwrap_or(DEFAULT_VERTEX_BASE_URL);
        return ProviderConfig::new("vertex", base, Vec::new());
    }
    let base = env
        .get("ANTHROPIC_BASE_URL")
        .unwrap_or(DEFAULT_ANTHROPIC_BASE_URL);
    ProviderConfig::new("anthropic", base, Vec::new())
}

/// What the session start narrows the model picker to: the ids the gateway listed
/// and the Bedrock region prefix (`None`: nothing is narrowed).
pub fn gateway_models_of(
    provider: Option<&ProviderConfig>,
) -> Option<(Vec<String>, Option<String>)> {
    let provider = provider?;
    provider
        .listed_models
        .clone()
        .map(|listed| (listed, provider.bedrock_region_prefix.clone()))
}

/// `providers/list` result.
pub fn list_providers_result(config: &ProviderConfig) -> Value {
    json!({
        "providers": [{
            "providerId": PROVIDER_ID,
            "supported": SUPPORTED_PROTOCOLS,
            "required": false,
            "current": { "apiType": config.api_type, "baseUrl": config.base_url },
        }]
    })
}

/// `sameHeaders`.
pub fn same_headers(a: &[(String, String)], b: &[(String, String)]) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|(key, value)| b.iter().any(|(k, v)| k == key && v == value))
}

/// `createEnvForProvider`: the Claude Code env vars that redirect API traffic and
/// inject headers. The token placeholders are required so the CLI skips its normal
/// login/credential checks when a gateway is in use.
pub fn create_env_for_provider(config: &ProviderConfig) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = [
        ("ANTHROPIC_BASE_URL", ""),
        ("ANTHROPIC_BEDROCK_BASE_URL", ""),
        ("ANTHROPIC_VERTEX_BASE_URL", ""),
        ("CLAUDE_CODE_USE_BEDROCK", "0"),
        ("CLAUDE_CODE_USE_VERTEX", "0"),
        ("ANTHROPIC_VERTEX_PROJECT_ID", ""),
        ("CLOUD_ML_REGION", ""),
        ("AWS_REGION", ""),
        ("ANTHROPIC_API_KEY", ""),
        ("ANTHROPIC_AUTH_TOKEN", ""),
        ("CLAUDE_CODE_OAUTH_TOKEN", ""),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let mut set = |key: &str, value: String| match env.iter_mut().find(|(k, _)| k == key) {
        Some(entry) => entry.1 = value,
        None => env.push((key.to_string(), value)),
    };
    let custom_headers = config
        .headers
        .iter()
        .map(|(key, value)| format!("{key}: {value}"))
        .collect::<Vec<_>>()
        .join("\n");
    match config.api_type.as_str() {
        "bedrock" => {
            set("CLAUDE_CODE_USE_BEDROCK", "1".into());
            // Bypass local AWS credential checks.
            set(
                "AWS_BEARER_TOKEN_BEDROCK",
                PROVIDER_AUTH_TOKEN_PLACEHOLDER.into(),
            );
            set("ANTHROPIC_BEDROCK_BASE_URL", config.base_url.clone());
            set("ANTHROPIC_CUSTOM_HEADERS", custom_headers);
            if let Some(prefix) = &config.bedrock_region_prefix {
                set("ANTHROPIC_BEDROCK_REGION_PREFIX", prefix.clone());
            }
        }
        "vertex" => {
            set("CLAUDE_CODE_USE_VERTEX", "1".into());
            // The default endpoint stays unset: supplying even the default host
            // marks the session as a custom Vertex deployment and can hide
            // otherwise-valid models.
            if config.base_url != DEFAULT_VERTEX_BASE_URL {
                set("ANTHROPIC_VERTEX_BASE_URL", config.base_url.clone());
            }
            set(
                "ANTHROPIC_VERTEX_PROJECT_ID",
                config
                    .vertex
                    .as_ref()
                    .map(|v| v.project_id.clone())
                    .unwrap_or_default(),
            );
            set(
                "CLOUD_ML_REGION",
                config
                    .vertex
                    .as_ref()
                    .map(|v| v.region.clone())
                    .unwrap_or_default(),
            );
            set("ANTHROPIC_CUSTOM_HEADERS", custom_headers);
        }
        _ => {
            set("ANTHROPIC_BASE_URL", config.base_url.clone());
            set("ANTHROPIC_CUSTOM_HEADERS", custom_headers);
            // Bypass local Claude login checks.
            set(
                "ANTHROPIC_AUTH_TOKEN",
                PROVIDER_AUTH_TOKEN_PLACEHOLDER.into(),
            );
        }
    }
    env
}

/// The programmatic settings tier under an active route: Claude Code applies env
/// from settings.json after the subprocess env, so the route goes in there too and
/// user/project settings cannot restore a different base URL
/// (`{...settings, apiKeyHelper: "", env: {...settings.env, ...providerEnv}}`).
pub fn settings_with_provider(settings: Option<Value>, provider_env: &[(String, String)]) -> Value {
    let mut object: Map<String, Value> = match settings {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    let mut env: Map<String, Value> = object
        .get("env")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (key, value) in provider_env {
        env.insert(key.clone(), Value::String(value.clone()));
    }
    object.insert("apiKeyHelper".into(), Value::String(String::new()));
    object.insert("env".into(), Value::Object(env));
    Value::Object(object)
}

/// Applies `provider` to the spawn options of a session about to start: the env
/// goes after every other source, and the settings tier carries the route too.
pub fn apply_provider_to_spawn(
    spawn: &mut crate::process::SpawnOptions,
    provider: Option<&ProviderConfig>,
) {
    let Some(provider) = provider else { return };
    let provider_env = create_env_for_provider(provider);
    let settings = spawn
        .settings_arg
        .as_deref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok());
    spawn.settings_arg = Some(settings_with_provider(settings, &provider_env).to_string());
    spawn.extra_env.extend(provider_env);
}

#[cfg(test)]
mod tests;
