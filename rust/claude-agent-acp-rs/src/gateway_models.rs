//! Model discovery for a gateway set via `providers/set` (port of upstream
//! `src/gateway-models.ts`).
//!
//! An organization can restrict which models its gateway serves, and the
//! gateway's listing returns only those: `GET /v1/models` for an `anthropic`
//! gateway, `GET /foundation-models` (Bedrock `ListFoundationModels`) for a
//! `bedrock` one. The adapter lists them once when the provider is set, and the
//! model picker then offers only the SDK models the gateway listed
//! ([`crate::session_model::narrow_to_gateway_models`]). A listing that fails fails
//! `providers/set` with a [`GatewayListingError`].

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use crate::logger::Logger;

pub use crate::session_model::bedrock_region_prefix;

const LISTING_TIMEOUT: Duration = Duration::from_secs(10);
const ANTHROPIC_PAGE_LIMIT: &str = "1000";
const MAX_LISTING_PAGES: usize = 20;
const MAX_LOGGED_BODY_CHARS: usize = 500;
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The bearer token `createEnvForProvider` sets as `ANTHROPIC_AUTH_TOKEN` and
/// `AWS_BEARER_TOKEN_BEDROCK`, so the CLI sends `Authorization: Bearer acp-proxy`
/// unless the provider headers replace it. The listing sends the same, so the
/// gateway sees the same auth as on chat requests.
pub const PROVIDER_AUTH_TOKEN_PLACEHOLDER: &str = "acp-proxy";

/// The adapter logger the listing writes to (`None`: diagnostics are off).
pub type ListingLogger = Option<Arc<Logger>>;

/// A gateway listing that failed: a network error, a status other than 2xx or
/// 4xx, a body that is not the expected listing, or the 10 s timeout. The message
/// names the request path and the reason, never a header value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayListingError(pub String);

impl std::fmt::Display for GatewayListingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GatewayListingError {}

/// One parsed page of a listing: its ids and the query of the next page.
struct ListingPage {
    ids: Vec<String>,
    next_query: Option<Vec<(String, String)>>,
}

fn log_line(logger: &ListingLogger, message: &str) {
    if let Some(logger) = logger {
        logger.log(message);
    }
}

fn log_error(logger: &ListingLogger, message: &str) {
    if let Some(logger) = logger {
        logger.error(message);
    }
}

/// `baseUrl.replace(/\/+$/, "")`.
fn trim_trailing_slashes(base_url: &str) -> &str {
    base_url.trim_end_matches('/')
}

/// Lists the model ids an `anthropic` gateway at `base_url` serves, following
/// `has_more` / `last_id` pagination. `Ok(None)` when the gateway answers 4xx: it
/// has no listing, and nothing is narrowed. Header values are never logged.
pub async fn list_gateway_models(
    base_url: &str,
    provider_headers: &[(String, String)],
    logger: &ListingLogger,
) -> Result<Option<Vec<String>>, GatewayListingError> {
    list_gateway_models_within(base_url, provider_headers, logger, LISTING_TIMEOUT).await
}

/// [`list_gateway_models`] with an explicit time limit (tests shorten it).
pub async fn list_gateway_models_within(
    base_url: &str,
    provider_headers: &[(String, String)],
    logger: &ListingLogger,
    timeout: Duration,
) -> Result<Option<Vec<String>>, GatewayListingError> {
    let endpoint = format!("{}/v1/models", trim_trailing_slashes(base_url));
    let headers = listing_headers(
        provider_headers,
        &[("anthropic-version", ANTHROPIC_VERSION)],
    )?;
    list_pages(
        &endpoint,
        vec![("limit".to_string(), ANTHROPIC_PAGE_LIMIT.to_string())],
        headers,
        logger,
        timeout,
        |body| {
            let data = body.get("data")?.as_array()?;
            let next_query = match (body.get("has_more"), body.get("last_id")) {
                (Some(Value::Bool(true)), Some(Value::String(last))) => Some(vec![
                    ("limit".to_string(), ANTHROPIC_PAGE_LIMIT.to_string()),
                    ("after_id".to_string(), last.clone()),
                ]),
                _ => None,
            };
            Some(ListingPage {
                ids: string_field(data, "id"),
                next_query,
            })
        },
    )
    .await
}

/// Lists the Anthropic models a `bedrock` gateway at `base_url` serves
/// (`GET /foundation-models`, Bedrock `ListFoundationModels`, one page), the
/// listing AI Router answers on its Bedrock route. Returns the listed ids, such as
/// `global.anthropic.claude-sonnet-5-5`; `None` and the errors are those of
/// [`list_gateway_models`].
pub async fn list_bedrock_models(
    base_url: &str,
    provider_headers: &[(String, String)],
    logger: &ListingLogger,
) -> Result<Option<Vec<String>>, GatewayListingError> {
    list_bedrock_models_within(base_url, provider_headers, logger, LISTING_TIMEOUT).await
}

/// [`list_bedrock_models`] with an explicit time limit (tests shorten it).
pub async fn list_bedrock_models_within(
    base_url: &str,
    provider_headers: &[(String, String)],
    logger: &ListingLogger,
    timeout: Duration,
) -> Result<Option<Vec<String>>, GatewayListingError> {
    let endpoint = format!("{}/foundation-models", trim_trailing_slashes(base_url));
    let headers = listing_headers(provider_headers, &[])?;
    let ids = list_pages(&endpoint, Vec::new(), headers, logger, timeout, |body| {
        let summaries = body.get("modelSummaries")?.as_array()?;
        Some(ListingPage {
            ids: string_field(summaries, "modelId"),
            next_query: None,
        })
    })
    .await?;
    Ok(ids.map(|ids| {
        ids.into_iter()
            .filter(|id| id.contains("anthropic."))
            .collect()
    }))
}

/// The region of the listed Bedrock models and the ids that remain
/// ([`select_bedrock_region`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BedrockRegion {
    pub region_prefix: Option<String>,
    pub model_ids: Vec<String>,
}

/// Picks the region of the listed Bedrock models: `global` when the gateway lists
/// it, else the prefix of the first id that has one. A gateway that lists `eu.*`
/// and `us.*` ids gets the prefix that comes first, and the ids of the other
/// prefixes are dropped, so the picker offers only models the CLI can call under
/// that prefix. Ids without a prefix (`anthropic.claude-...`) are dropped too once a
/// prefix is picked; without any prefix every id is kept.
pub fn select_bedrock_region(model_ids: &[String]) -> BedrockRegion {
    let prefixes: Vec<Option<&'static str>> = model_ids
        .iter()
        .map(|id| bedrock_region_prefix(id))
        .collect();
    let region_prefix = if prefixes.contains(&Some("global")) {
        Some("global")
    } else {
        prefixes.iter().find_map(|prefix| *prefix)
    };
    match region_prefix {
        None => BedrockRegion {
            region_prefix: None,
            model_ids: model_ids.to_vec(),
        },
        Some(prefix) => BedrockRegion {
            region_prefix: Some(prefix.to_string()),
            model_ids: model_ids
                .iter()
                .filter(|id| bedrock_region_prefix(id) == Some(prefix))
                .cloned()
                .collect(),
        },
    }
}

/// Provider headers win over the defaults, matched case-insensitively, the way
/// `ANTHROPIC_CUSTOM_HEADERS` overrides the CLI's own headers.
fn listing_headers(
    provider_headers: &[(String, String)],
    protocol_headers: &[(&str, &str)],
) -> Result<HeaderMap, GatewayListingError> {
    let mut headers = HeaderMap::new();
    let defaults = [
        (
            "authorization",
            format!("Bearer {PROVIDER_AUTH_TOKEN_PLACEHOLDER}"),
        ),
        ("accept", "application/json".to_string()),
    ];
    for (name, value) in defaults
        .iter()
        .map(|(n, v)| (*n, v.as_str()))
        .chain(protocol_headers.iter().map(|(name, value)| (*name, *value)))
    {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for (name, value) in provider_headers {
        // Name only the header: the value may be a secret.
        let invalid = || {
            GatewayListingError(format!(
                "Model listing cannot send the invalid header \"{name}\""
            ))
        };
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?;
        // `Headers.set` strips leading and trailing HTTP whitespace.
        let header_value = HeaderValue::from_str(value.trim_matches([' ', '\t', '\r', '\n']))
            .map_err(|_| invalid())?;
        headers.insert(header_name, header_value);
    }
    Ok(headers)
}

fn string_field(entries: &[Value], field: &str) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| {
            let value = entry.get(field)?.as_str()?;
            (!value.trim().is_empty()).then(|| value.to_string())
        })
        .collect()
}

/// Reads `endpoint` page by page; `parse` returns `None` for a body it does not
/// understand. A 4xx answer on any page means the gateway has no listing: the
/// result is `Ok(None)`, and nothing is narrowed.
async fn list_pages(
    endpoint: &str,
    first_query: Vec<(String, String)>,
    headers: HeaderMap,
    logger: &ListingLogger,
    timeout: Duration,
    parse: impl Fn(&Value) -> Option<ListingPage>,
) -> Result<Option<Vec<String>>, GatewayListingError> {
    let parsed_endpoint = url::Url::parse(endpoint);
    let path = parsed_endpoint
        .as_ref()
        .map(|url| url.path().to_string())
        .unwrap_or_else(|_| endpoint.to_string());
    let failure = |reason: &str| {
        let error = GatewayListingError(format!("Model listing GET {path} {reason}"));
        log_error(logger, &format!("[providers/set] {}", error.0));
        error
    };
    let endpoint_url = parsed_endpoint.map_err(|e| {
        failure(&format!(
            "failed: Failed to parse URL from {endpoint} ({e})"
        ))
    })?;
    // fetch refuses such a URL and repeats it, credentials included, in its error.
    if !endpoint_url.username().is_empty() || endpoint_url.password().is_some() {
        return Err(failure("cannot be sent: the base URL carries credentials"));
    }
    // Node's `fetch` does not read the proxy variables.
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| failure(&format!("failed: {e}")))?;
    let read = async {
        let mut ids: Vec<String> = Vec::new();
        let mut query = Some(first_query);
        let mut page = 0;
        while page < MAX_LISTING_PAGES {
            let Some(current) = query.take() else { break };
            page += 1;
            let mut url = endpoint_url.clone();
            if !current.is_empty() {
                let mut pairs = url.query_pairs_mut();
                for (name, value) in &current {
                    pairs.append_pair(name, value);
                }
            }
            let response = client
                .get(url)
                .headers(headers.clone())
                .send()
                .await
                .map_err(|_| failure("failed: fetch failed"))?;
            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|_| failure("failed: fetch failed"))?;
            if status.is_client_error() {
                log_line(
                    logger,
                    &format!(
                        "[providers/set] Model listing GET {path} returned {}: the gateway has no listing, models are not narrowed",
                        status.as_u16()
                    ),
                );
                return Ok(None);
            }
            if !status.is_success() {
                let excerpt: String = body.chars().take(MAX_LOGGED_BODY_CHARS).collect();
                return Err(failure(&format!("returned {}: {excerpt}", status.as_u16())));
            }
            let parsed = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|value| parse(&value));
            let Some(parsed) = parsed else {
                return Err(failure("returned an unexpected body"));
            };
            ids.extend(parsed.ids);
            query = parsed.next_query;
        }
        if query.is_some() {
            log_error(
                logger,
                &format!(
                    "[providers/set] Model listing GET {path} still had more pages after {MAX_LISTING_PAGES}; using what was read"
                ),
            );
        }
        Ok(Some(ids))
    };
    match tokio::time::timeout(timeout, read).await {
        Ok(result) => result,
        Err(_) => Err(failure("failed: The operation was aborted due to timeout")),
    }
}

#[cfg(test)]
mod tests;
