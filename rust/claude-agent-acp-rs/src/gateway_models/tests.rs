//! Ports of upstream `gateway-models.test.ts` (the listing and region cases), run
//! against a stub gateway on 127.0.0.1 (never the real network).

use super::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// What the stub gateway answers.
enum Reply {
    Json(u16, Value),
    Raw(u16, &'static str),
    /// Accept and say nothing.
    Hang,
}

/// A request as the stub saw it: path with query, and the header lines.
struct Seen {
    target: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A gateway that serves `replies` in order (the last one repeats) and reports
/// each request.
async fn stub(replies: Vec<Reply>) -> (String, mpsc::UnboundedReceiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let (seen_tx, seen_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut served = 0usize;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match socket.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let text = String::from_utf8_lossy(&buf).into_owned();
            let mut lines = text.split("\r\n");
            let target = lines
                .next()
                .and_then(|line| line.split(' ').nth(1))
                .unwrap_or_default()
                .to_string();
            let headers = lines
                .take_while(|line| !line.is_empty())
                .filter_map(|line| line.split_once(':'))
                .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
                .collect();
            let _ = seen_tx.send(Seen { target, headers });
            let reply = &replies[served.min(replies.len() - 1)];
            served += 1;
            let (status, body) = match reply {
                Reply::Json(status, body) => (*status, body.to_string()),
                Reply::Raw(status, body) => (*status, (*body).to_string()),
                Reply::Hang => {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    continue;
                }
            };
            let response = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    (base, seen_rx)
}

fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect()
}

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), future)
        .await
        .expect("the listing finished")
}

/// listGatewayModels > lists /v1/models under the base URL with the chat request's auth
#[tokio::test]
async fn t3b_lists_v1_models_under_the_base_url_with_the_chat_auth() {
    let (base, mut seen) = stub(vec![Reply::Json(
        200,
        json!({ "data": [{ "id": "claude-sonnet-5" }] }),
    )])
    .await;
    let found = within(list_gateway_models(
        &format!("{base}/anthropic/"),
        &headers(&[("Authorization", "Bearer real"), ("X-Route", "a")]),
        &None,
    ))
    .await;
    assert_eq!(found, Ok(Some(ids(&["claude-sonnet-5"]))));
    let request = seen.recv().await.expect("one request");
    assert_eq!(request.target, "/anthropic/v1/models?limit=1000");
    assert_eq!(request.header("authorization"), Some("Bearer real"));
    assert_eq!(request.header("x-route"), Some("a"));
    assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
}

/// listGatewayModels > sends the placeholder bearer token when the provider headers carry no Authorization
#[tokio::test]
async fn t3b_sends_the_placeholder_bearer_without_an_authorization_header() {
    let (base, mut seen) = stub(vec![Reply::Json(200, json!({ "data": [] }))]).await;
    let found = within(list_gateway_models(
        &base,
        &headers(&[("x-api-key", "key")]),
        &None,
    ))
    .await;
    assert_eq!(found, Ok(Some(Vec::new())));
    let request = seen.recv().await.expect("one request");
    assert_eq!(request.header("authorization"), Some("Bearer acp-proxy"));
    assert_eq!(request.header("x-api-key"), Some("key"));
    assert_eq!(request.header("accept"), Some("application/json"));
}

/// listGatewayModels > follows has_more / last_id pagination
#[tokio::test]
async fn t3b_follows_has_more_and_last_id_pagination() {
    let (base, mut seen) = stub(vec![
        Reply::Json(
            200,
            json!({ "data": [{ "id": "a" }, { "id": "b" }], "has_more": true, "last_id": "b" }),
        ),
        Reply::Json(
            200,
            json!({ "data": [{ "id": "c" }], "has_more": false, "last_id": "c" }),
        ),
    ])
    .await;
    let found = within(list_gateway_models(&base, &[], &None)).await;
    assert_eq!(found, Ok(Some(ids(&["a", "b", "c"]))));
    let first = seen.recv().await.expect("first page");
    let second = seen.recv().await.expect("second page");
    assert_eq!(first.target, "/v1/models?limit=1000");
    assert_eq!(second.target, "/v1/models?limit=1000&after_id=b");
}

/// O-68: at most 20 pages are read, and what was read is used.
#[tokio::test]
async fn t3b_stops_after_twenty_pages() {
    let (base, mut seen) = stub(vec![Reply::Json(
        200,
        json!({ "data": [{ "id": "m" }], "has_more": true, "last_id": "m" }),
    )])
    .await;
    let found = within(list_gateway_models(&base, &[], &None)).await;
    assert_eq!(found.map(|ids| ids.map(|ids| ids.len())), Ok(Some(20)));
    let mut count = 0;
    while seen.try_recv().is_ok() {
        count += 1;
    }
    assert_eq!(count, 20);
}

/// listGatewayModels > throws a GatewayListingError on a 5xx status / a body without data / a non-JSON body
#[tokio::test]
async fn t3b_fails_on_a_5xx_an_unexpected_body_and_non_json() {
    let (base, _seen) = stub(vec![Reply::Json(503, json!({ "error": "unavailable" }))]).await;
    let error = within(list_gateway_models(&base, &[], &None))
        .await
        .expect_err("a 503 fails");
    assert_eq!(
        error.0,
        "Model listing GET /v1/models returned 503: {\"error\":\"unavailable\"}"
    );
    let (base, _seen) = stub(vec![Reply::Json(200, json!({ "models": [] }))]).await;
    let error = within(list_gateway_models(&base, &[], &None))
        .await
        .expect_err("no data fails");
    assert_eq!(
        error.0,
        "Model listing GET /v1/models returned an unexpected body"
    );
    let (base, _seen) = stub(vec![Reply::Raw(200, "<html>")]).await;
    assert!(within(list_gateway_models(&base, &[], &None))
        .await
        .is_err());
}

/// listGatewayModels > throws a GatewayListingError on a network error
#[tokio::test]
async fn t3b_fails_on_a_network_error_without_naming_the_host() {
    // A port nobody listens on.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    drop(listener);
    let error = within(list_gateway_models(&base, &[], &None))
        .await
        .expect_err("refused");
    assert_eq!(error.0, "Model listing GET /v1/models failed: fetch failed");
}

/// O-72: the 10 s limit (shortened here) fails the listing.
#[tokio::test]
async fn t3b_a_gateway_that_never_answers_times_out() {
    let (base, _seen) = stub(vec![Reply::Hang]).await;
    let error = within(list_gateway_models_within(
        &base,
        &[],
        &None,
        Duration::from_millis(200),
    ))
    .await
    .expect_err("times out");
    assert_eq!(
        error.0,
        "Model listing GET /v1/models failed: The operation was aborted due to timeout"
    );
}

/// gateways without a listing > returns undefined for a 400/401/403/404/405/429 answer
#[tokio::test]
async fn t3b_a_4xx_answer_means_no_listing() {
    for status in [400, 401, 403, 404, 405, 429] {
        let (base, _seen) = stub(vec![Reply::Raw(status, "no")]).await;
        assert_eq!(
            within(list_gateway_models(&base, &[], &None)).await,
            Ok(None)
        );
        assert_eq!(
            within(list_bedrock_models(&base, &[], &None)).await,
            Ok(None)
        );
    }
}

/// listing request guards > names an invalid provider header without its value
#[tokio::test]
async fn t3b_names_an_invalid_header_without_its_value_and_sends_nothing() {
    let (base, mut seen) = stub(vec![Reply::Json(200, json!({ "data": [] }))]).await;
    let error = within(list_gateway_models(
        &base,
        &headers(&[("authorization", "Bearer sk-SECRET\r\nx: y")]),
        &None,
    ))
    .await
    .expect_err("invalid header");
    assert_eq!(
        error.0,
        "Model listing cannot send the invalid header \"authorization\""
    );
    assert!(!error.0.contains("sk-SECRET"));
    assert!(seen.try_recv().is_err(), "nothing was sent");
}

/// listing request guards > refuses a base URL with credentials without repeating them
#[tokio::test]
async fn t3b_refuses_a_base_url_with_credentials() {
    let error = within(list_gateway_models(
        "https://user:secret@gateway.example",
        &[],
        &None,
    ))
    .await
    .expect_err("credentials");
    assert_eq!(
        error.0,
        "Model listing GET /v1/models cannot be sent: the base URL carries credentials"
    );
    assert!(!error.0.contains("secret"));
}

/// listBedrockModels > lists the Anthropic ids of /foundation-models under the base URL
#[tokio::test]
async fn t3b_lists_the_anthropic_ids_of_foundation_models() {
    let (base, mut seen) = stub(vec![Reply::Json(
        200,
        json!({ "modelSummaries": [
            { "modelId": "global.anthropic.claude-sonnet-5-5" },
            { "modelId": "global.meta.llama3-3-70b-instruct-v1:0" },
            { "modelId": "global.anthropic.claude-opus-5-5" },
        ] }),
    )])
    .await;
    let found = within(list_bedrock_models(&format!("{base}/bedrock/"), &[], &None)).await;
    assert_eq!(
        found,
        Ok(Some(ids(&[
            "global.anthropic.claude-sonnet-5-5",
            "global.anthropic.claude-opus-5-5"
        ])))
    );
    assert_eq!(
        seen.recv().await.expect("request").target,
        "/bedrock/foundation-models"
    );
}

/// listBedrockModels > throws a GatewayListingError on a body without modelSummaries
#[tokio::test]
async fn t3b_a_bedrock_body_without_summaries_is_unexpected() {
    let (base, _seen) = stub(vec![Reply::Json(200, json!({ "data": [] }))]).await;
    let error = within(list_bedrock_models(&base, &[], &None))
        .await
        .expect_err("unexpected");
    assert_eq!(
        error.0,
        "Model listing GET /foundation-models returned an unexpected body"
    );
}

/// selectBedrockRegion > reads the prefix of an id or an inference-profile ARN
#[test]
fn t3b_reads_the_region_prefix_of_an_id_or_an_arn() {
    assert_eq!(
        bedrock_region_prefix("eu.anthropic.claude-sonnet-5"),
        Some("eu")
    );
    assert_eq!(
        bedrock_region_prefix("us-gov.anthropic.claude-sonnet-5"),
        Some("us-gov")
    );
    assert_eq!(
        bedrock_region_prefix(
            "arn:aws:bedrock:eu-west-1:123456789012:inference-profile/global.anthropic.claude-opus-5"
        ),
        Some("global")
    );
    assert_eq!(bedrock_region_prefix("anthropic.claude-sonnet-5"), None);
    assert_eq!(bedrock_region_prefix("claude-sonnet-5"), None);
}

/// selectBedrockRegion > prefers global over a prefix listed before it / keeps the ids of the first listed prefix without global / keeps the ids of global when listed / keeps every id when none has a prefix
#[test]
fn t3b_selects_the_bedrock_region() {
    let region = |list: &[&str]| select_bedrock_region(&ids(list));
    assert_eq!(
        region(&[
            "eu.anthropic.claude-sonnet-5",
            "global.anthropic.claude-sonnet-5",
            "us.anthropic.claude-opus-5"
        ]),
        BedrockRegion {
            region_prefix: Some("global".into()),
            model_ids: ids(&["global.anthropic.claude-sonnet-5"])
        }
    );
    assert_eq!(
        region(&["eu.anthropic.claude-sonnet-5", "us.anthropic.claude-opus-5"]),
        BedrockRegion {
            region_prefix: Some("eu".into()),
            model_ids: ids(&["eu.anthropic.claude-sonnet-5"])
        }
    );
    assert_eq!(
        region(&[
            "anthropic.claude-haiku-4-5",
            "global.anthropic.claude-opus-5",
            "eu.anthropic.claude-sonnet-5",
            "global.anthropic.claude-sonnet-5"
        ]),
        BedrockRegion {
            region_prefix: Some("global".into()),
            model_ids: ids(&[
                "global.anthropic.claude-opus-5",
                "global.anthropic.claude-sonnet-5"
            ])
        }
    );
    assert_eq!(
        region(&["anthropic.claude-sonnet-5"]),
        BedrockRegion {
            region_prefix: None,
            model_ids: ids(&["anthropic.claude-sonnet-5"])
        }
    );
}

/// O-71: provider headers override the defaults case-insensitively.
#[test]
fn t3b_provider_headers_override_defaults_case_insensitively() {
    let built = listing_headers(
        &headers(&[("AUTHORIZATION", "Bearer mine"), ("Accept", "text/plain")]),
        &[("anthropic-version", "2023-06-01")],
    )
    .expect("valid");
    assert_eq!(built.get("authorization").unwrap(), "Bearer mine");
    assert_eq!(built.get("accept").unwrap(), "text/plain");
    assert_eq!(built.get("anthropic-version").unwrap(), "2023-06-01");
    assert_eq!(built.len(), 3);
}
