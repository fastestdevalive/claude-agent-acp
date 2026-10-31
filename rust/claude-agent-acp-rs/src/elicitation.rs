//! ACP elicitation protocol bridging (F23a, F23b, F23c).
//!
//! Bridges between Claude Agent SDK elicitation callbacks and ACP's elicitation protocol
//! (`session/create_elicitation`, `session/complete_elicitation` / `elicitation/create`, `elicitation/complete`).

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Form-field key for question at index `index`.
#[must_use]
pub fn question_field_key(index: usize) -> String {
    format!("question_{index}")
}

/// Form-field key for custom answer at index `index`.
#[must_use]
pub fn question_custom_field_key(index: usize) -> String {
    format!("question_{index}_custom")
}

/// `_meta` key under which a bridged enum option carries its `preview`.
pub const OPTION_META_KEY: &str = "_claude/askUserQuestionOption";

/// The AIR custom answer keys (U-63).
pub const AIR_CUSTOM_ANSWER_KEY: &str = "customAnswer";
pub const LEGACY_AIR_CUSTOM_ANSWER_KEY: &str = "_askUserQuestionCustomAnswer";

/// The refusal fallback dialog kind (U-43).
pub const REFUSAL_FALLBACK_DIALOG_KIND: &str = "refusal_fallback_prompt";
/// Form-field key carrying the user's choice in the refusal-fallback form.
pub const REFUSAL_FALLBACK_CHOICE_KEY: &str = "choice";
/// Dialog wire result values for refusal fallback.
pub const RETRY_FALLBACK_RESULT: &str = "retry_fallback";
pub const KEEP_REFUSAL_RESULT: &str = "cancelled";

/// Modes the connected client advertised support for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ElicitationSupport {
    pub form: bool,
    pub url: bool,
}

// The capability decision itself lives in `client_caps` (`elicitation_form` /
// `elicitation_url`, JS truthiness); callers pass the resolved
// [`ElicitationSupport`] here, so no second, `is_some()`-based reader exists.

/// An elicitation request received from an MCP server via Claude.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpElicitationRequest {
    #[serde(default, alias = "serverName", alias = "mcp_server_name")]
    pub server_name: Option<String>,
    pub message: String,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default, alias = "elicitationId", alias = "elicitation_id")]
    pub elicitation_id: Option<String>,
    #[serde(default, alias = "requestedSchema", alias = "requested_schema")]
    pub requested_schema: Option<Value>,
}

/// Option for an AskUserQuestion question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskUserQuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub preview: Option<String>,
}

/// A single question as supplied by the AskUserQuestion tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskUserQuestion {
    pub question: String,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default, rename = "multiSelect")]
    pub multi_select: bool,
    #[serde(default)]
    pub options: Vec<AskUserQuestionOption>,
}

/// Outcome of an AskUserQuestion elicitation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskUserQuestionOutcome {
    Answered { updated_input: Value },
    Cancel,
}

/// Payload of the `refusal_fallback_prompt` dialog (U-43).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusalFallbackPrompt {
    pub original_model: String,
    pub fallback_model: String,
    pub api_refusal_category: Option<String>,
    pub guidance_text: Option<String>,
}

/// Extract and validate the opaque dialog payload into a [`RefusalFallbackPrompt`].
#[must_use]
pub fn extract_refusal_fallback_prompt(payload: &Value) -> Option<RefusalFallbackPrompt> {
    let original_model = payload.get("originalModel")?.as_str()?;
    let fallback_model = payload.get("fallbackModel")?.as_str()?;
    let api_refusal_category = payload
        .get("apiRefusalCategory")
        .and_then(Value::as_str)
        .map(String::from);
    let guidance_text = payload
        .get("guidanceText")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from);

    Some(RefusalFallbackPrompt {
        original_model: original_model.to_string(),
        fallback_model: fallback_model.to_string(),
        api_refusal_category,
        guidance_text,
    })
}

/// Render the refusal-fallback consent prompt as an ACP form elicitation (U-43).
#[must_use]
pub fn refusal_fallback_to_create_request(
    prompt: &RefusalFallbackPrompt,
    session_id: &str,
) -> Value {
    let category = prompt
        .api_refusal_category
        .as_ref()
        .filter(|c| !c.is_empty())
        .map(|c| format!(" ({c})"))
        .unwrap_or_default();
    let guidance = prompt
        .guidance_text
        .as_ref()
        .map(|g| format!("\n\n{g}"))
        .unwrap_or_default();
    let message = format!(
        "{} declined this request{}. Retry with {}?{}",
        prompt.original_model, category, prompt.fallback_model, guidance
    );

    let requested_schema = json!({
        "type": "object",
        "properties": {
            REFUSAL_FALLBACK_CHOICE_KEY: {
                "type": "string",
                "oneOf": [
                    {
                        "const": RETRY_FALLBACK_RESULT,
                        "title": format!("Retry with {}", prompt.fallback_model),
                        "description": format!("The session continues on {}.", prompt.fallback_model),
                    },
                    {
                        "const": KEEP_REFUSAL_RESULT,
                        "title": "Keep the refusal",
                        "description": "You can send a new message.",
                    }
                ]
            }
        }
    });

    json!({
        "mode": "form",
        "sessionId": session_id,
        "message": message,
        "requestedSchema": requested_schema,
    })
}

/// Map the elicitation response back to the dialog's result enum (U-43).
#[must_use]
pub fn refusal_fallback_result_from_response(response: &Value) -> &'static str {
    let content = accepted_elicitation_content(response);
    let choice = content
        .get(REFUSAL_FALLBACK_CHOICE_KEY)
        .and_then(Value::as_str);
    if choice == Some(RETRY_FALLBACK_RESULT) {
        RETRY_FALLBACK_RESULT
    } else {
        KEEP_REFUSAL_RESULT
    }
}

/// Resolve when the session's abort signal fires (upstream's
/// `signal.aborted`), or when its sender is dropped (the session is gone).
/// Never holds a guard across `.await`: each check copies the flag out first.
pub(crate) async fn wait_cancelled(cancel: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() {
            return;
        }
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

/// Handle an inbound `elicitation` control request (R-10).
///
/// `cancel` is the session's abort signal. Upstream settles an aborted MCP
/// elicitation as `{action:"cancel"}` — not `decline`, which would tell the
/// server the user refused (`acp-agent.ts:10284-10296`).
pub async fn handle_elicitation_control_request<F, Fut>(
    frame: &Value,
    session_id: &str,
    support: ElicitationSupport,
    send_elicitation: F,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Value
where
    F: FnOnce(Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    let request_id = frame
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let req = frame.get("request").cloned().unwrap_or(Value::Null);
    let mcp_req: Result<McpElicitationRequest, _> = serde_json::from_value(req);
    let Ok(mcp_req) = mcp_req else {
        return json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": { "action": "decline" },
            }
        });
    };

    let is_url = mcp_req.mode.as_deref() == Some("url");
    if (is_url && !support.url) || (!is_url && !support.form) {
        return json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": { "action": "decline" },
            }
        });
    }

    let create_request = mcp_elicitation_to_create_request(&mcp_req, session_id);
    let Some(create_request) = create_request else {
        return json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": { "action": "decline" },
            }
        });
    };

    let result = tokio::select! {
        biased;
        () = wait_cancelled(&mut cancel) => json!({ "action": "cancel" }),
        res = send_elicitation(create_request) => match res {
            Ok(resp) => {
                // Check the signal again after the response (`acp-agent.ts:10287`):
                // an abort that races it still settles as a cancel.
                if *cancel.borrow() {
                    json!({ "action": "cancel" })
                } else {
                    create_elicitation_response_to_elicit_result(&resp)
                }
            }
            Err(_) => {
                if *cancel.borrow() {
                    json!({ "action": "cancel" })
                } else {
                    json!({ "action": "decline" })
                }
            }
        },
    };

    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": result,
        }
    })
}

/// Handle an inbound `request_user_dialog` control request (R-08, U-43).
///
/// `cancel` is the session's abort signal: an aborted dialog answers
/// `{behavior:"cancelled"}` (the dialog's default), including when the abort
/// races the client response (`acp-agent.ts:10392-10394`).
pub async fn handle_user_dialog_control_request<F, Fut>(
    frame: &Value,
    session_id: &str,
    support: ElicitationSupport,
    send_elicitation: F,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Value
where
    F: FnOnce(Value) -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    let request_id = frame
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let dialog_kind = frame
        .pointer("/request/dialog_kind")
        .and_then(Value::as_str)
        .unwrap_or("");

    if dialog_kind != REFUSAL_FALLBACK_DIALOG_KIND || !support.form {
        return json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": { "behavior": "cancelled" },
            }
        });
    }

    let payload = frame
        .pointer("/request/payload")
        .cloned()
        .unwrap_or(Value::Null);
    let prompt = extract_refusal_fallback_prompt(&payload);
    let Some(prompt) = prompt else {
        return json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": { "behavior": "cancelled" },
            }
        });
    };

    let create_request = refusal_fallback_to_create_request(&prompt, session_id);
    let completed = tokio::select! {
        biased;
        () = wait_cancelled(&mut cancel) => None,
        res = send_elicitation(create_request) => match res {
            Ok(resp) => {
                if *cancel.borrow() {
                    None
                } else {
                    Some(refusal_fallback_result_from_response(&resp))
                }
            }
            Err(_) => None,
        },
    };

    match completed {
        Some(result) => json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": {
                    "behavior": "completed",
                    "result": result,
                },
            }
        }),
        None => json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": { "behavior": "cancelled" },
            }
        }),
    }
}

/// Handle a system message `elicitation_complete` (N-37).
///
/// When the client supports url elicitation, calls `complete_elicitation` with the notification.
pub async fn handle_elicitation_complete_system_message<F, Fut>(
    message: &Value,
    support: ElicitationSupport,
    complete_elicitation: F,
) -> bool
where
    F: FnOnce(Value) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    if !support.url {
        return false;
    }
    let elicitation_id = message
        .get("elicitation_id")
        .or_else(|| message.get("elicitationId"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if elicitation_id.is_empty() {
        return false;
    }
    let notif = json!({ "elicitationId": elicitation_id });
    let _ = complete_elicitation(notif).await;
    true
}

/// MCP OAuth polling interval (A-01, S-02).
pub const MCP_OAUTH_STATUS_POLL: std::time::Duration = std::time::Duration::from_millis(1_000);
/// MCP OAuth timeout (10 minutes, A-01, S-02).
pub const MCP_OAUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Check if an MCP server needs authentication based on its status (A-01).
#[must_use]
pub fn server_needs_auth(status: &Value) -> bool {
    status.get("status").and_then(Value::as_str) == Some("needs-auth")
}

/// Perform MCP OAuth authentication for a single server (A-01, S-02).
pub async fn authenticate_single_mcp_server<
    AuthFn,
    AuthFut,
    StatusFn,
    StatusFut,
    ElicitFn,
    ElicitFut,
    CompleteFn,
    CompleteFut,
>(
    session_id: &str,
    server_name: &str,
    mcp_authenticate: AuthFn,
    poll_status: StatusFn,
    create_elicitation: ElicitFn,
    complete_elicitation: CompleteFn,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<bool, String>
where
    AuthFn: FnOnce() -> AuthFut,
    AuthFut: std::future::Future<Output = Result<Value, String>>,
    StatusFn: Fn() -> StatusFut,
    // `None` is upstream's "the server is not in `mcpServerStatus()`", which
    // fails the wait (`acp-agent.ts:2664-2670`).
    StatusFut: std::future::Future<Output = Result<Option<String>, String>>,
    ElicitFn: FnOnce(Value) -> ElicitFut,
    ElicitFut: std::future::Future<Output = Result<Value, String>>,
    CompleteFn: FnOnce(Value) -> CompleteFut,
    CompleteFut: std::future::Future<Output = Result<(), String>>,
{
    let auth_res = mcp_authenticate().await?;
    // A missing `requiresUserAction` is falsy upstream (`if (!login.requiresUserAction)
    // return`, `acp-agent.ts:2611`), i.e. no dialog and no polling.
    let requires_action = auth_res
        .get("requiresUserAction")
        .or_else(|| auth_res.get("requires_user_action"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !requires_action {
        return Ok(true);
    }

    let auth_url = auth_res
        .get("authUrl")
        .or_else(|| auth_res.get("auth_url"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if auth_url.is_empty() {
        return Err(
            "Claude Code requested user action without returning an authorization URL".to_string(),
        );
    }

    let elicitation_id = format!("mcp-oauth-{}", crate::agent::uuid_v4());
    let create_request = json!({
        "mode": "url",
        "sessionId": session_id,
        "message": format!("Authenticate with MCP server {server_name}"),
        "url": auth_url,
        "elicitationId": elicitation_id,
    });

    let polling = async {
        let deadline = tokio::time::Instant::now() + MCP_OAUTH_TIMEOUT;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            match poll_status().await {
                Ok(Some(status)) => {
                    if status == "connected" {
                        return true;
                    }
                    if status == "failed" || status == "disabled" {
                        return false;
                    }
                }
                Ok(None) => return false,
                Err(_) => return false,
            }
            tokio::time::sleep(MCP_OAUTH_STATUS_POLL).await;
        }
    };

    tokio::pin!(polling);

    // Upstream races the status wait against the client's answer
    // (`acp-agent.ts:2634-2654`); the session abort tears the whole flow down.
    enum First {
        Completed(bool),
        Elicitation(Result<Value, String>),
    }

    // Pinned outside the race: when the poll wins, the URL elicitation is
    // dropped only AFTER `elicitation/complete`, so the completion precedes the
    // `$/cancel_request` the dropped request sends (`acp-agent.ts:2645-2658`).
    let mut elicitation = Box::pin(create_elicitation(create_request));

    let first = tokio::select! {
        biased;
        () = wait_cancelled(&mut cancel) => return Ok(false),
        elicit_res = &mut elicitation => First::Elicitation(elicit_res),
        completed = &mut polling => First::Completed(completed),
    };

    match first {
        // The poll settled first: the URL dialog is dismissed whether the server
        // connected or failed/timed out (upstream completes unconditionally after
        // the accept check).
        First::Completed(completed) => {
            let _ = complete_elicitation(json!({ "elicitationId": elicitation_id })).await;
            drop(elicitation);
            Ok(completed)
        }
        First::Elicitation(Err(error)) => Err(error),
        First::Elicitation(Ok(resp)) => {
            if resp.get("action").and_then(Value::as_str) != Some("accept") {
                return Ok(false);
            }
            let completed = tokio::select! {
                biased;
                () = wait_cancelled(&mut cancel) => return Ok(false),
                completed = &mut polling => completed,
            };
            // Accept-then-poll: complete even when the poll failed or timed out,
            // so the client's URL dialog closes (`acp-agent.ts:2639-2654`).
            let _ = complete_elicitation(json!({ "elicitationId": elicitation_id })).await;
            Ok(completed)
        }
    }
}

/// Pull well-formed questions out of an AskUserQuestion tool input.
///
/// The filter is upstream's (`elicitation.ts:122-132`): `q` truthy, `question`
/// a string (empty is allowed), and `options` an array with at least one entry.
/// The SDK validates the options themselves before the tool call reaches us, so
/// an option without a label still surfaces (with an empty label) rather than
/// silently dropping the whole question.
#[must_use]
pub fn extract_ask_user_questions(input: &Value) -> Option<Vec<AskUserQuestion>> {
    let questions_array = input.get("questions")?.as_array()?;
    let valid: Vec<AskUserQuestion> = questions_array
        .iter()
        .filter_map(|q| {
            let question = q.get("question")?.as_str()?;
            let options_arr = q.get("options")?.as_array()?;
            if options_arr.is_empty() {
                return None;
            }
            let options: Vec<AskUserQuestionOption> = options_arr
                .iter()
                .map(|opt| {
                    let label = opt
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let description = opt
                        .get("description")
                        .and_then(Value::as_str)
                        .map(String::from);
                    let preview = opt.get("preview").and_then(Value::as_str).map(String::from);
                    AskUserQuestionOption {
                        label,
                        description,
                        preview,
                    }
                })
                .collect();
            let header = q.get("header").and_then(Value::as_str).map(String::from);
            let multi_select = q
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(AskUserQuestion {
                question: question.to_string(),
                header,
                multi_select,
                options,
            })
        })
        .collect();

    if valid.is_empty() {
        None
    } else {
        Some(valid)
    }
}

/// Render the AskUserQuestion tool's questions as an ACP form elicitation (U-42, U-63).
#[must_use]
pub fn ask_user_questions_to_create_request(
    questions: &[AskUserQuestion],
    session_id: &str,
    tool_call_id: Option<&str>,
    air_client: bool,
) -> Value {
    let single = questions.len() == 1;
    let mut properties = Map::new();

    for (index, question) in questions.iter().enumerate() {
        let mut enum_options = Vec::new();
        for option in &question.options {
            let mut enum_opt = Map::new();
            enum_opt.insert("const".to_string(), Value::String(option.label.clone()));
            enum_opt.insert("title".to_string(), Value::String(option.label.clone()));
            if let Some(desc) = &option.description {
                if !desc.is_empty() {
                    enum_opt.insert("description".to_string(), Value::String(desc.clone()));
                }
            }
            // `if (option.preview)` (`elicitation.ts:195`): an empty string is
            // falsy in JS and gets no `_meta`.
            if let Some(preview) = option.preview.as_ref().filter(|p| !p.is_empty()) {
                enum_opt.insert(
                    "_meta".to_string(),
                    json!({
                        OPTION_META_KEY: {
                            "preview": preview
                        }
                    }),
                );
            }
            enum_options.push(Value::Object(enum_opt));
        }

        let mut prop = Map::new();
        let title = question.header.as_deref().filter(|h| !h.is_empty());
        if question.multi_select {
            prop.insert("type".to_string(), Value::String("array".to_string()));
            if let Some(t) = title {
                prop.insert("title".to_string(), Value::String(t.to_string()));
            }
            if !single {
                prop.insert(
                    "description".to_string(),
                    Value::String(question.question.clone()),
                );
            }
            prop.insert("items".to_string(), json!({ "anyOf": enum_options }));
        } else {
            prop.insert("type".to_string(), Value::String("string".to_string()));
            if let Some(t) = title {
                prop.insert("title".to_string(), Value::String(t.to_string()));
            }
            if !single {
                prop.insert(
                    "description".to_string(),
                    Value::String(question.question.clone()),
                );
            }
            prop.insert("oneOf".to_string(), Value::Array(enum_options));
        }
        properties.insert(question_field_key(index), Value::Object(prop));

        let mut custom_prop = Map::new();
        custom_prop.insert("type".to_string(), Value::String("string".to_string()));
        custom_prop.insert("title".to_string(), Value::String("Other".to_string()));
        let custom_desc = if question.multi_select {
            "Type your own answer to add to your selection above (optional)."
        } else {
            "Type your own answer, or add a note to the option you chose above (optional)."
        };
        custom_prop.insert(
            "description".to_string(),
            Value::String(custom_desc.to_string()),
        );
        if air_client {
            let custom_answer = json!({
                "questionId": question_field_key(index),
                "isCustomAnswer": true,
            });
            let meta = crate::air::with_air_meta(
                Some(&json!({
                    LEGACY_AIR_CUSTOM_ANSWER_KEY: custom_answer,
                })),
                AIR_CUSTOM_ANSWER_KEY,
                custom_answer,
            );
            custom_prop.insert("_meta".to_string(), meta);
        }
        properties.insert(question_custom_field_key(index), Value::Object(custom_prop));
    }

    let message = if single {
        questions
            .first()
            .map(|q| q.question.clone())
            .unwrap_or_default()
    } else {
        "Please answer the following questions.".to_string()
    };

    let mut request = Map::new();
    request.insert("mode".to_string(), Value::String("form".to_string()));
    request.insert(
        "sessionId".to_string(),
        Value::String(session_id.to_string()),
    );
    if let Some(tool_id) = tool_call_id {
        request.insert("toolCallId".to_string(), Value::String(tool_id.to_string()));
    }
    request.insert("message".to_string(), Value::String(message));
    request.insert(
        "requestedSchema".to_string(),
        json!({
            "type": "object",
            "properties": properties,
        }),
    );
    Value::Object(request)
}

/// Serialize a multi-select answer as comma-joined with JSON quoting when needed.
#[must_use]
pub fn join_multi_select_answer(items: &[String]) -> String {
    items
        .iter()
        .map(|item| {
            if item.contains(", ") || item.contains('"') {
                serde_json::to_string(item).unwrap_or_else(|_| item.clone())
            } else {
                item.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Fold an ACP elicitation response into the AskUserQuestion tool's input (U-42).
#[must_use]
pub fn apply_ask_elicitation_response(
    response: &Value,
    tool_input: &Value,
    questions: &[AskUserQuestion],
) -> AskUserQuestionOutcome {
    let action = response.get("action").and_then(Value::as_str).unwrap_or("");
    if action == "decline" {
        let mut updated = tool_input.as_object().cloned().unwrap_or_default();
        updated.insert("answers".to_string(), json!({}));
        return AskUserQuestionOutcome::Answered {
            updated_input: Value::Object(updated),
        };
    }

    if action != "accept" {
        return AskUserQuestionOutcome::Cancel;
    }

    let content = accepted_elicitation_content(response);
    let content_obj = content.as_object();
    let mut answers = Map::new();
    let mut annotations = Map::new();

    for (index, question) in questions.iter().enumerate() {
        let custom = content_obj
            .and_then(|c| c.get(&question_custom_field_key(index)))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();

        let val = content_obj.and_then(|c| c.get(&question_field_key(index)));
        let picks: Vec<String> = match val {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(arr)) => arr
                .iter()
                .filter(|item| !item.is_null() && item.as_str() != Some(""))
                .map(|item| {
                    item.as_str()
                        .map(String::from)
                        .unwrap_or_else(|| item.to_string())
                })
                .collect(),
            Some(Value::String(s)) => {
                if s.is_empty() {
                    Vec::new()
                } else {
                    vec![s.clone()]
                }
            }
            Some(other) => vec![other.to_string()],
        };

        if question.multi_select {
            let all_picks = if custom.is_empty() {
                picks
            } else {
                let mut p = picks;
                p.push(custom.to_string());
                p
            };
            let text = join_multi_select_answer(&all_picks);
            if !text.is_empty() {
                answers.insert(question.question.clone(), Value::String(text));
            }
        } else {
            let picked = picks.join(", ");
            if picked.is_empty() {
                if !custom.is_empty() {
                    answers.insert(question.question.clone(), Value::String(custom.to_string()));
                }
            } else {
                answers.insert(question.question.clone(), Value::String(picked));
                if !custom.is_empty() {
                    annotations.insert(question.question.clone(), json!({ "notes": custom }));
                }
            }
        }
    }

    let mut updated = tool_input.as_object().cloned().unwrap_or_default();
    updated.insert("answers".to_string(), Value::Object(answers));
    if !annotations.is_empty() {
        updated.insert("annotations".to_string(), Value::Object(annotations));
    }

    AskUserQuestionOutcome::Answered {
        updated_input: Value::Object(updated),
    }
}

/// Coerce an arbitrary MCP `requestedSchema` into an ACP `ElicitationSchema`.
#[must_use]
pub fn normalize_elicitation_schema(schema: Option<&Value>) -> Value {
    match schema {
        // `{...schema, type: "object"}` (`elicitation.ts:359-366`): the object
        // arm only guarantees the discriminator; it never adds `properties`.
        Some(Value::Object(map)) => {
            let mut obj = map.clone();
            obj.insert("type".to_string(), Value::String("object".to_string()));
            Value::Object(obj)
        }
        _ => json!({
            "type": "object",
            "properties": {}
        }),
    }
}

/// Convert an MCP elicitation request into an ACP `CreateElicitationRequest` payload (U-40).
#[must_use]
pub fn mcp_elicitation_to_create_request(
    request: &McpElicitationRequest,
    session_id: &str,
) -> Option<Value> {
    if request.mode.as_deref() == Some("url") {
        let url = request.url.as_deref().unwrap_or("");
        if url.is_empty() {
            return None;
        }
        let elicitation_id = request
            .elicitation_id
            .clone()
            .unwrap_or_else(crate::agent::uuid_v4);
        Some(json!({
            "mode": "url",
            "sessionId": session_id,
            "message": request.message,
            "url": url,
            "elicitationId": elicitation_id,
        }))
    } else {
        Some(json!({
            "mode": "form",
            "sessionId": session_id,
            "message": request.message,
            "requestedSchema": normalize_elicitation_schema(request.requested_schema.as_ref()),
        }))
    }
}

/// Content of an accepted elicitation response.
#[must_use]
pub fn accepted_elicitation_content(response: &Value) -> Value {
    if response.get("action").and_then(Value::as_str) == Some("accept") {
        match response.get("content") {
            Some(Value::Object(map)) => Value::Object(map.clone()),
            _ => Value::Object(Map::new()),
        }
    } else {
        Value::Object(Map::new())
    }
}

/// Map an ACP elicitation response back to the MCP `ElicitResult` (U-41).
#[must_use]
pub fn create_elicitation_response_to_elicit_result(response: &Value) -> Value {
    let action = response.get("action").and_then(Value::as_str).unwrap_or("");
    match action {
        "accept" => json!({
            "action": "accept",
            "content": accepted_elicitation_content(response),
        }),
        "decline" => json!({
            "action": "decline",
        }),
        _ => json!({
            "action": "cancel",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "session-1";

    fn mk_question(
        question: &str,
        options: Vec<AskUserQuestionOption>,
        header: Option<&str>,
        multi_select: bool,
    ) -> AskUserQuestion {
        AskUserQuestion {
            question: question.to_string(),
            header: header.map(String::from),
            multi_select,
            options,
        }
    }

    #[test]
    fn mcp_elicitation_maps_form_mode_request() {
        let request: McpElicitationRequest = serde_json::from_value(json!({
            "serverName": "server",
            "message": "Need your name",
            "mode": "form",
            "requestedSchema": { "properties": { "name": { "type": "string" } } }
        }))
        .expect("valid json");

        let result =
            mcp_elicitation_to_create_request(&request, SESSION_ID).expect("request built");
        assert_eq!(
            result,
            json!({
                "mode": "form",
                "sessionId": SESSION_ID,
                "message": "Need your name",
                "requestedSchema": {
                    "type": "object",
                    "properties": { "name": { "type": "string" } }
                }
            })
        );
    }

    #[test]
    fn mcp_elicitation_maps_url_mode_request_and_preserves_id() {
        let request: McpElicitationRequest = serde_json::from_value(json!({
            "serverName": "server",
            "message": "Authorize",
            "mode": "url",
            "url": "https://example.com/auth",
            "elicitationId": "el-123"
        }))
        .expect("valid json");

        let result =
            mcp_elicitation_to_create_request(&request, SESSION_ID).expect("request built");
        assert_eq!(
            result,
            json!({
                "mode": "url",
                "sessionId": SESSION_ID,
                "message": "Authorize",
                "url": "https://example.com/auth",
                "elicitationId": "el-123"
            })
        );
    }

    #[test]
    fn mcp_elicitation_generates_id_when_url_omits_one() {
        let request: McpElicitationRequest = serde_json::from_value(json!({
            "serverName": "server",
            "message": "Authorize",
            "mode": "url",
            "url": "https://example.com/auth"
        }))
        .expect("valid json");

        let result =
            mcp_elicitation_to_create_request(&request, SESSION_ID).expect("request built");
        assert_eq!(result["mode"], "url");
        assert!(result["elicitationId"].as_str().is_some());
    }

    #[test]
    fn mcp_elicitation_returns_none_for_url_with_no_url() {
        let request: McpElicitationRequest = serde_json::from_value(json!({
            "serverName": "server",
            "message": "Authorize",
            "mode": "url"
        }))
        .expect("valid json");

        assert!(mcp_elicitation_to_create_request(&request, SESSION_ID).is_none());
    }

    #[test]
    fn create_elicitation_response_maps_accept_with_content() {
        let response = json!({ "action": "accept", "content": { "name": "Ada" } });
        assert_eq!(
            create_elicitation_response_to_elicit_result(&response),
            json!({
                "action": "accept",
                "content": { "name": "Ada" }
            })
        );
    }

    #[test]
    fn create_elicitation_response_defaults_accept_content_to_empty_object() {
        let response = json!({ "action": "accept" });
        assert_eq!(
            create_elicitation_response_to_elicit_result(&response),
            json!({
                "action": "accept",
                "content": {}
            })
        );
    }

    #[test]
    fn create_elicitation_response_maps_decline_and_cancel() {
        assert_eq!(
            create_elicitation_response_to_elicit_result(&json!({ "action": "decline" })),
            json!({ "action": "decline" })
        );
        assert_eq!(
            create_elicitation_response_to_elicit_result(&json!({ "action": "cancel" })),
            json!({ "action": "cancel" })
        );
    }

    #[test]
    fn create_elicitation_response_maps_custom_action_to_cancel() {
        assert_eq!(
            create_elicitation_response_to_elicit_result(&json!({ "action": "_zed/snooze" })),
            json!({ "action": "cancel" })
        );
    }

    #[test]
    fn extract_ask_user_questions_reads_from_tool_input() {
        let question = mk_question(
            "Which?",
            vec![
                AskUserQuestionOption {
                    label: "A".into(),
                    description: None,
                    preview: None,
                },
                AskUserQuestionOption {
                    label: "B".into(),
                    description: None,
                    preview: None,
                },
            ],
            None,
            false,
        );
        let input = json!({ "questions": [question] });
        let extracted = extract_ask_user_questions(&input).expect("extracted");
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].question, "Which?");
    }

    #[test]
    fn extract_ask_user_questions_returns_none_when_empty_or_invalid() {
        assert!(extract_ask_user_questions(&json!({})).is_none());
        assert!(extract_ask_user_questions(&json!({ "questions": [] })).is_none());
        assert!(extract_ask_user_questions(&json!({
            "questions": [{ "question": "no options" }, { "options": [] }]
        }))
        .is_none());
    }

    #[test]
    fn ask_user_questions_to_create_request_single_select() {
        let questions = vec![mk_question(
            "Which library?",
            vec![
                AskUserQuestionOption {
                    label: "date-fns".into(),
                    description: Some("Lightweight".into()),
                    preview: None,
                },
                AskUserQuestionOption {
                    label: "luxon".into(),
                    description: None,
                    preview: None,
                },
            ],
            Some("Library"),
            false,
        )];

        let result =
            ask_user_questions_to_create_request(&questions, SESSION_ID, Some("tool-1"), false);
        assert_eq!(result["mode"], "form");
        assert_eq!(result["sessionId"], SESSION_ID);
        assert_eq!(result["toolCallId"], "tool-1");
        assert_eq!(result["message"], "Which library?");

        let q0 = &result["requestedSchema"]["properties"]["question_0"];
        assert_eq!(q0["type"], "string");
        assert_eq!(q0["title"], "Library");
        assert!(q0.get("description").is_none());
        assert_eq!(
            q0["oneOf"],
            json!([
                { "const": "date-fns", "title": "date-fns", "description": "Lightweight" },
                { "const": "luxon", "title": "luxon" }
            ])
        );
    }

    #[test]
    fn ask_user_questions_to_create_request_preview_under_meta() {
        let questions = vec![mk_question(
            "Which layout?",
            vec![
                AskUserQuestionOption {
                    label: "Grid".into(),
                    description: Some("Cards".into()),
                    preview: Some("```\n[ ]\n```".into()),
                },
                AskUserQuestionOption {
                    label: "Plain".into(),
                    description: None,
                    preview: None,
                },
            ],
            None,
            false,
        )];

        let result = ask_user_questions_to_create_request(&questions, SESSION_ID, None, false);
        let q0 = &result["requestedSchema"]["properties"]["question_0"];
        assert_eq!(
            q0["oneOf"][0],
            json!({
                "const": "Grid",
                "title": "Grid",
                "description": "Cards",
                "_meta": {
                    "_claude/askUserQuestionOption": {
                        "preview": "```\n[ ]\n```"
                    }
                }
            })
        );
        assert_eq!(
            q0["oneOf"][1],
            json!({ "const": "Plain", "title": "Plain" })
        );
    }

    #[test]
    fn ask_user_questions_to_create_request_air_custom_field() {
        let questions = vec![mk_question(
            "Which?",
            vec![AskUserQuestionOption {
                label: "A".into(),
                description: None,
                preview: None,
            }],
            None,
            false,
        )];

        let air_result = ask_user_questions_to_create_request(&questions, SESSION_ID, None, true);
        let custom = &air_result["requestedSchema"]["properties"]["question_0_custom"];
        assert_eq!(custom["title"], "Other");
        assert_eq!(
            custom["_meta"]["_askUserQuestionCustomAnswer"],
            json!({ "questionId": "question_0", "isCustomAnswer": true })
        );
        assert_eq!(
            custom["_meta"]["jetbrains"]["air"]["customAnswer"],
            json!({ "questionId": "question_0", "isCustomAnswer": true })
        );

        let plain_result =
            ask_user_questions_to_create_request(&questions, SESSION_ID, None, false);
        assert!(
            plain_result["requestedSchema"]["properties"]["question_0_custom"]
                .get("_meta")
                .is_none()
        );
    }

    #[test]
    fn apply_ask_elicitation_response_accept_answers() {
        let questions = vec![
            mk_question(
                "Single?",
                vec![
                    AskUserQuestionOption {
                        label: "A".into(),
                        description: None,
                        preview: None,
                    },
                    AskUserQuestionOption {
                        label: "B".into(),
                        description: None,
                        preview: None,
                    },
                ],
                None,
                false,
            ),
            mk_question(
                "Multi?",
                vec![
                    AskUserQuestionOption {
                        label: "X".into(),
                        description: None,
                        preview: None,
                    },
                    AskUserQuestionOption {
                        label: "Y".into(),
                        description: None,
                        preview: None,
                    },
                ],
                None,
                true,
            ),
        ];
        let tool_input = json!({ "questions": [], "metadata": { "source": "test" } });

        let response = json!({
            "action": "accept",
            "content": { "question_0": "A", "question_1": ["X", "Y"] }
        });

        let outcome = apply_ask_elicitation_response(&response, &tool_input, &questions);
        match outcome {
            AskUserQuestionOutcome::Answered { updated_input } => {
                assert_eq!(
                    updated_input["answers"],
                    json!({ "Single?": "A", "Multi?": "X, Y" })
                );
            }
            AskUserQuestionOutcome::Cancel => panic!("expected answered"),
        }
    }

    #[test]
    fn apply_ask_elicitation_response_custom_answer_alone_and_combined() {
        let questions = vec![
            mk_question(
                "Single?",
                vec![AskUserQuestionOption {
                    label: "A".into(),
                    description: None,
                    preview: None,
                }],
                None,
                false,
            ),
            mk_question(
                "Multi?",
                vec![AskUserQuestionOption {
                    label: "X".into(),
                    description: None,
                    preview: None,
                }],
                None,
                true,
            ),
        ];
        let tool_input = json!({ "questions": [] });

        // Single with custom alone
        let r1 = json!({
            "action": "accept",
            "content": { "question_0_custom": "  my own take  " }
        });
        if let AskUserQuestionOutcome::Answered { updated_input } =
            apply_ask_elicitation_response(&r1, &tool_input, &questions)
        {
            assert_eq!(updated_input["answers"]["Single?"], "my own take");
            assert!(updated_input.get("annotations").is_none());
        } else {
            panic!("expected answered");
        }

        // Single with choice + notes
        let r2 = json!({
            "action": "accept",
            "content": { "question_0": "A", "question_0_custom": "  note text  " }
        });
        if let AskUserQuestionOutcome::Answered { updated_input } =
            apply_ask_elicitation_response(&r2, &tool_input, &questions)
        {
            assert_eq!(updated_input["answers"]["Single?"], "A");
            assert_eq!(
                updated_input["annotations"]["Single?"],
                json!({ "notes": "note text" })
            );
        } else {
            panic!("expected answered");
        }

        // Multi with choice + custom (appended)
        let r3 = json!({
            "action": "accept",
            "content": { "question_1": ["X"], "question_1_custom": "Use Redis, not Memcached" }
        });
        if let AskUserQuestionOutcome::Answered { updated_input } =
            apply_ask_elicitation_response(&r3, &tool_input, &questions)
        {
            assert_eq!(
                updated_input["answers"]["Multi?"],
                "X, \"Use Redis, not Memcached\""
            );
        } else {
            panic!("expected answered");
        }
    }

    /// U-42 (`elicitation.ts:295-321`): a multi-select question is an array
    /// property with a titled `anyOf` item enum, and with more than one question
    /// each field carries its own question text.
    #[test]
    fn ask_user_questions_to_create_request_multi_select() {
        let questions = vec![
            mk_question(
                "Which?",
                vec![AskUserQuestionOption {
                    label: "A".into(),
                    description: None,
                    preview: None,
                }],
                Some("One"),
                false,
            ),
            mk_question(
                "Pick many",
                vec![
                    AskUserQuestionOption {
                        label: "X".into(),
                        description: Some("first".into()),
                        preview: None,
                    },
                    AskUserQuestionOption {
                        label: "Y".into(),
                        description: None,
                        preview: None,
                    },
                ],
                Some("Many"),
                true,
            ),
        ];
        let req = ask_user_questions_to_create_request(&questions, SESSION_ID, None, false);
        assert_eq!(req["message"], "Please answer the following questions.");
        let one = &req["requestedSchema"]["properties"]["question_0"];
        assert_eq!(one["type"], "string");
        assert_eq!(one["description"], "Which?");
        let many = &req["requestedSchema"]["properties"]["question_1"];
        assert_eq!(many["type"], "array");
        assert_eq!(many["title"], "Many");
        assert_eq!(many["description"], "Pick many");
        assert_eq!(many["items"]["anyOf"][0]["const"], "X");
        assert_eq!(many["items"]["anyOf"][0]["description"], "first");
        assert_eq!(many["items"]["anyOf"][1]["const"], "Y");
        assert!(many["items"]["anyOf"][1].get("description").is_none());
    }

    /// U-42 (`elicitation.ts:427-508`): a blank custom answer keeps the pick,
    /// an empty multi-select with a custom answer uses it alone, empty picks and
    /// blank text are ignored, and unanswered questions contribute no answer.
    #[test]
    fn apply_ask_elicitation_response_blank_and_unanswered() {
        let questions = vec![
            mk_question(
                "Single?",
                vec![AskUserQuestionOption {
                    label: "A".into(),
                    description: None,
                    preview: None,
                }],
                None,
                false,
            ),
            mk_question(
                "Multi?",
                vec![AskUserQuestionOption {
                    label: "X".into(),
                    description: None,
                    preview: None,
                }],
                None,
                true,
            ),
            mk_question(
                "Unanswered?",
                vec![AskUserQuestionOption {
                    label: "Z".into(),
                    description: None,
                    preview: None,
                }],
                None,
                false,
            ),
        ];
        let tool_input = json!({ "questions": [] });

        let blank = json!({
            "action": "accept",
            "content": { "question_0": "A", "question_0_custom": "   " }
        });
        let AskUserQuestionOutcome::Answered { updated_input } =
            apply_ask_elicitation_response(&blank, &tool_input, &questions)
        else {
            panic!("expected answered");
        };
        assert_eq!(updated_input["answers"], json!({ "Single?": "A" }));
        assert!(updated_input.get("annotations").is_none());
        assert!(updated_input["answers"].get("Unanswered?").is_none());

        let multi_alone = json!({
            "action": "accept",
            "content": { "question_1": [], "question_1_custom": "only mine" }
        });
        let AskUserQuestionOutcome::Answered { updated_input } =
            apply_ask_elicitation_response(&multi_alone, &tool_input, &questions)
        else {
            panic!("expected answered");
        };
        assert_eq!(updated_input["answers"], json!({ "Multi?": "only mine" }));

        let empties = json!({
            "action": "accept",
            "content": { "question_0": null, "question_1": ["", null] }
        });
        let AskUserQuestionOutcome::Answered { updated_input } =
            apply_ask_elicitation_response(&empties, &tool_input, &questions)
        else {
            panic!("expected answered");
        };
        assert_eq!(updated_input["answers"], json!({}));
    }

    #[test]
    fn apply_ask_elicitation_response_decline_and_cancel() {
        let questions = vec![mk_question("Single?", vec![], None, false)];
        let tool_input = json!({ "questions": [] });

        let dec = json!({ "action": "decline" });
        if let AskUserQuestionOutcome::Answered { updated_input } =
            apply_ask_elicitation_response(&dec, &tool_input, &questions)
        {
            assert_eq!(updated_input["answers"], json!({}));
        } else {
            panic!("expected answered on decline");
        }

        let canc = json!({ "action": "cancel" });
        assert_eq!(
            apply_ask_elicitation_response(&canc, &tool_input, &questions),
            AskUserQuestionOutcome::Cancel
        );

        let cust = json!({ "action": "_zed/snooze" });
        assert_eq!(
            apply_ask_elicitation_response(&cust, &tool_input, &questions),
            AskUserQuestionOutcome::Cancel
        );
    }

    #[test]
    fn extract_refusal_fallback_prompt_extracts_fields() {
        let payload = json!({
            "originalModel": "claude-fable-5",
            "fallbackModel": "claude-opus-4-8",
            "apiRefusalCategory": "cyber",
            "guidanceText": "You can retry with the fallback model.",
            "retractedMessageUuids": ["u1"]
        });
        let prompt = extract_refusal_fallback_prompt(&payload).expect("extracted");
        assert_eq!(prompt.original_model, "claude-fable-5");
        assert_eq!(prompt.fallback_model, "claude-opus-4-8");
        assert_eq!(prompt.api_refusal_category.as_deref(), Some("cyber"));
        assert_eq!(
            prompt.guidance_text.as_deref(),
            Some("You can retry with the fallback model.")
        );
    }

    /// U-43 (`elicitation.ts:537-547`): a non-string category normalizes to
    /// `None` and an empty guidance text is dropped.
    #[test]
    fn extract_refusal_fallback_prompt_normalizes_category_and_guidance() {
        let prompt = extract_refusal_fallback_prompt(&json!({
            "originalModel": "claude-fable-5",
            "fallbackModel": "claude-opus-4-8",
            "apiRefusalCategory": 7,
            "guidanceText": ""
        }))
        .expect("extracted");
        assert!(prompt.api_refusal_category.is_none());
        assert!(prompt.guidance_text.is_none());
    }

    #[test]
    fn extract_refusal_fallback_prompt_returns_none_when_model_missing() {
        assert!(extract_refusal_fallback_prompt(&json!({ "fallbackModel": "opus" })).is_none());
        assert!(extract_refusal_fallback_prompt(
            &json!({ "originalModel": 5, "fallbackModel": "opus" })
        )
        .is_none());
    }

    #[test]
    fn refusal_fallback_to_create_request_builds_single_choice_form() {
        let prompt = RefusalFallbackPrompt {
            original_model: "claude-fable-5".into(),
            fallback_model: "claude-opus-4-8".into(),
            api_refusal_category: Some("cyber".into()),
            guidance_text: None,
        };
        let req = refusal_fallback_to_create_request(&prompt, SESSION_ID);
        assert_eq!(req["mode"], "form");
        assert_eq!(req["sessionId"], SESSION_ID);
        assert!(req["message"].as_str().unwrap().contains("claude-fable-5"));
        assert!(req["message"].as_str().unwrap().contains("(cyber)"));
        assert!(req["message"].as_str().unwrap().contains("claude-opus-4-8"));

        let choice = &req["requestedSchema"]["properties"][REFUSAL_FALLBACK_CHOICE_KEY];
        assert_eq!(choice["type"], "string");
        assert_eq!(choice["oneOf"][0]["const"], RETRY_FALLBACK_RESULT);
        assert_eq!(choice["oneOf"][1]["const"], KEEP_REFUSAL_RESULT);
    }

    #[test]
    fn refusal_fallback_result_from_response_maps_results() {
        let retry = json!({
            "action": "accept",
            "content": { "choice": "retry_fallback" }
        });
        assert_eq!(
            refusal_fallback_result_from_response(&retry),
            "retry_fallback"
        );

        for keep in [
            json!({ "action": "accept", "content": { "choice": "cancelled" } }),
            json!({ "action": "accept", "content": {} }),
            json!({ "action": "accept" }),
            json!({ "action": "decline" }),
            json!({ "action": "cancel" }),
        ] {
            assert_eq!(refusal_fallback_result_from_response(&keep), "cancelled");
        }
    }

    #[tokio::test]
    async fn handle_elicitation_control_request_form_mode() {
        let frame = json!({
            "type": "control_request",
            "request_id": "req-1",
            "request": {
                "subtype": "elicitation",
                "mcp_server_name": "srv",
                "message": "Enter value",
                "mode": "form",
                "requested_schema": { "properties": { "val": { "type": "string" } } }
            }
        });

        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let resp = handle_elicitation_control_request(
            &frame,
            SESSION_ID,
            ElicitationSupport {
                form: true,
                url: false,
            },
            |req| async move {
                assert_eq!(req["mode"], "form");
                Ok(json!({ "action": "accept", "content": { "val": "123" } }))
            },
            cancel_rx,
        )
        .await;

        assert_eq!(resp["type"], "control_response");
        assert_eq!(resp["response"]["subtype"], "success");
        assert_eq!(resp["response"]["request_id"], "req-1");
        assert_eq!(resp["response"]["response"]["action"], "accept");
        assert_eq!(resp["response"]["response"]["content"]["val"], "123");
    }

    #[tokio::test]
    async fn handle_elicitation_control_request_declines_without_capability() {
        let frame = json!({
            "type": "control_request",
            "request_id": "req-1",
            "request": {
                "subtype": "elicitation",
                "message": "Enter value",
                "mode": "form"
            }
        });

        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let resp = handle_elicitation_control_request(
            &frame,
            SESSION_ID,
            ElicitationSupport {
                form: false,
                url: false,
            },
            |_| async { panic!("should not be called") },
            cancel_rx,
        )
        .await;

        assert_eq!(resp["response"]["response"]["action"], "decline");
    }

    #[tokio::test]
    async fn handle_user_dialog_control_request_accept_and_cancel() {
        let frame = json!({
            "type": "control_request",
            "request_id": "req-2",
            "request": {
                "subtype": "request_user_dialog",
                "dialog_kind": "refusal_fallback_prompt",
                "payload": {
                    "originalModel": "m1",
                    "fallbackModel": "m2"
                }
            }
        });

        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let resp = handle_user_dialog_control_request(
            &frame,
            SESSION_ID,
            ElicitationSupport {
                form: true,
                url: false,
            },
            |_| async {
                Ok(json!({ "action": "accept", "content": { "choice": "retry_fallback" } }))
            },
            cancel_rx,
        )
        .await;

        assert_eq!(resp["response"]["subtype"], "success");
        assert_eq!(resp["response"]["response"]["behavior"], "completed");
        assert_eq!(resp["response"]["response"]["result"], "retry_fallback");

        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let cancel_resp = handle_user_dialog_control_request(
            &frame,
            SESSION_ID,
            ElicitationSupport {
                form: true,
                url: false,
            },
            |_| async { Ok(json!({ "action": "cancel" })) },
            cancel_rx,
        )
        .await;

        assert_eq!(cancel_resp["response"]["response"]["behavior"], "completed");
        assert_eq!(cancel_resp["response"]["response"]["result"], "cancelled");
    }

    #[tokio::test]
    async fn handle_elicitation_complete_system_message_forwards_when_url_supported() {
        let msg = json!({
            "type": "system",
            "subtype": "elicitation_complete",
            "elicitation_id": "elic-999"
        });

        let mut completed_id = String::new();
        let handled = handle_elicitation_complete_system_message(
            &msg,
            ElicitationSupport {
                form: false,
                url: true,
            },
            |notif| {
                completed_id = notif["elicitationId"].as_str().unwrap().to_string();
                async { Ok(()) }
            },
        )
        .await;

        assert!(handled);
        assert_eq!(completed_id, "elic-999");

        // When URL elicitation is not supported, it does nothing and returns false.
        let handled_unsupported = handle_elicitation_complete_system_message(
            &msg,
            ElicitationSupport {
                form: true,
                url: false,
            },
            |_| async { panic!("should not be called") },
        )
        .await;

        assert!(!handled_unsupported);
    }

    #[tokio::test(start_paused = true)]
    async fn authenticate_mcp_server_success_via_polling() {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let res = authenticate_single_mcp_server(
            SESSION_ID,
            "github",
            || async {
                Ok(json!({
                    "requiresUserAction": true,
                    "authUrl": "https://auth.github.com/oauth"
                }))
            },
            move || {
                let c = counter_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    if c >= 2 {
                        Ok(Some("connected".to_string()))
                    } else {
                        Ok(Some("needs-auth".to_string()))
                    }
                }
            },
            |req| async move {
                assert_eq!(req["mode"], "url");
                assert_eq!(req["url"], "https://auth.github.com/oauth");
                // Simulate client keeping dialog open
                futures::future::pending::<Result<Value, String>>().await
            },
            move |notif| {
                let id = notif["elicitationId"].as_str().unwrap().to_string();
                let _ = done_tx.send(id);
                async { Ok(()) }
            },
            cancel_rx,
        );

        tokio::pin!(res);

        // Let the flow poll once, then advance past the polling interval.
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(3)).await;

        let outcome = res.await.expect("success");
        assert!(outcome);
        let completed_id = done_rx.try_recv().expect("completed id sent");
        assert!(completed_id.starts_with("mcp-oauth-"));
    }

    #[tokio::test]
    async fn authenticate_mcp_server_declined_by_user() {
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let res = authenticate_single_mcp_server(
            SESSION_ID,
            "github",
            || async {
                Ok(json!({
                    "requiresUserAction": true,
                    "authUrl": "https://auth.github.com/oauth"
                }))
            },
            || async { Ok(Some("needs-auth".to_string())) },
            |_| async { Ok(json!({ "action": "decline" })) },
            |_| async { panic!("should not complete") },
            cancel_rx,
        )
        .await
        .expect("finished");

        assert!(!res);
    }

    #[tokio::test(start_paused = true)]
    async fn authenticate_mcp_server_times_out_after_10_minutes() {
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed_in = completed.clone();
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

        let res = authenticate_single_mcp_server(
            SESSION_ID,
            "github",
            || async {
                Ok(json!({
                    "requiresUserAction": true,
                    "authUrl": "https://auth.github.com/oauth"
                }))
            },
            || async { Ok(Some("needs-auth".to_string())) },
            |_| async { futures::future::pending::<Result<Value, String>>().await },
            move |notif| {
                assert!(notif["elicitationId"]
                    .as_str()
                    .unwrap()
                    .starts_with("mcp-oauth-"));
                completed_in.store(true, std::sync::atomic::Ordering::SeqCst);
                async { Ok(()) }
            },
            cancel_rx,
        );

        tokio::pin!(res);

        // Start the flow, then advance past the 10-minute deadline.
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(601)).await;

        let outcome = res.await.expect("finished");
        assert!(!outcome, "the poll deadline reports not authenticated");
        assert!(
            completed.load(std::sync::atomic::Ordering::SeqCst),
            "the client's URL dialog is dismissed even on timeout"
        );
    }

    #[tokio::test]
    async fn authenticate_mcp_server_no_user_action_required() {
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let res = authenticate_single_mcp_server(
            SESSION_ID,
            "github",
            || async { Ok(json!({ "requiresUserAction": false })) },
            || async { panic!("should not poll") },
            |_| async { panic!("should not elicit") },
            |_| async { panic!("should not complete") },
            cancel_rx,
        )
        .await
        .expect("finished");

        assert!(res);
    }

    /// A missing `requiresUserAction` is falsy upstream: no dialog, no polling.
    #[tokio::test]
    async fn authenticate_mcp_server_missing_user_action_is_false() {
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let res = authenticate_single_mcp_server(
            SESSION_ID,
            "github",
            || async { Ok(json!({ "authUrl": "https://auth.github.com/oauth" })) },
            || async { panic!("should not poll") },
            |_| async { panic!("should not elicit") },
            |_| async { panic!("should not complete") },
            cancel_rx,
        )
        .await
        .expect("finished");

        assert!(res);
    }

    /// The server vanishing from `mcpServerStatus()` fails the wait.
    #[tokio::test]
    async fn authenticate_mcp_server_missing_status_is_false() {
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let res = authenticate_single_mcp_server(
            SESSION_ID,
            "github",
            || async {
                Ok(json!({
                    "requiresUserAction": true,
                    "authUrl": "https://auth.github.com/oauth"
                }))
            },
            || async { Ok(None) },
            |_| async { futures::future::pending::<Result<Value, String>>().await },
            |_| async { Ok(()) },
            cancel_rx,
        )
        .await
        .expect("finished");

        assert!(!res);
    }

    /// The session abort stops the OAuth flow instead of waiting out the poll.
    #[tokio::test(start_paused = true)]
    async fn authenticate_mcp_server_cancel_stops_the_flow() {
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let res = authenticate_single_mcp_server(
            SESSION_ID,
            "github",
            || async {
                Ok(json!({
                    "requiresUserAction": true,
                    "authUrl": "https://auth.github.com/oauth"
                }))
            },
            || async { Ok(Some("needs-auth".to_string())) },
            |_| async { futures::future::pending::<Result<Value, String>>().await },
            |_| async { panic!("should not complete on abort") },
            cancel_rx,
        );

        tokio::pin!(res);
        tokio::task::yield_now().await;
        cancel_tx.send(true).expect("cancel signal sent");

        let outcome = res.await.expect("finished");
        assert!(!outcome, "an aborted flow is not authenticated");
    }

    /// An aborted MCP elicitation is `cancel`, not `decline`
    /// (`acp-agent.ts:10284-10296`).
    #[tokio::test(start_paused = true)]
    async fn handle_elicitation_control_request_cancel_is_cancel() {
        let frame = json!({
            "type": "control_request",
            "request_id": "req-cancel",
            "request": {
                "subtype": "elicitation",
                "message": "Enter value",
                "mode": "form"
            }
        });
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            handle_elicitation_control_request(
                &frame,
                SESSION_ID,
                ElicitationSupport {
                    form: true,
                    url: false,
                },
                |_| async { futures::future::pending::<Result<Value, String>>().await },
                cancel_rx,
            )
            .await
        });

        tokio::task::yield_now().await;
        cancel_tx.send(true).expect("cancel signal sent");
        let resp = handle.await.expect("handler finished");

        assert_eq!(resp["response"]["response"]["action"], "cancel");
        assert_ne!(resp["response"]["response"]["action"], "decline");
    }

    /// An abort that races the client answer still answers `cancelled`.
    #[tokio::test]
    async fn handle_user_dialog_control_request_cancel_is_cancelled() {
        let frame = json!({
            "type": "control_request",
            "request_id": "req-dialog-cancel",
            "request": {
                "subtype": "request_user_dialog",
                "dialog_kind": "refusal_fallback_prompt",
                "payload": { "originalModel": "m1", "fallbackModel": "m2" }
            }
        });
        let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(true);
        let resp = handle_user_dialog_control_request(
            &frame,
            SESSION_ID,
            ElicitationSupport {
                form: true,
                url: false,
            },
            |_| async {
                Ok(json!({ "action": "accept", "content": { "choice": "retry_fallback" } }))
            },
            cancel_rx,
        )
        .await;

        assert_eq!(resp["response"]["response"]["behavior"], "cancelled");
        assert!(resp["response"]["response"].get("result").is_none());
    }

    #[test]
    fn normalize_elicitation_schema_keeps_absent_properties_absent() {
        let normalized = normalize_elicitation_schema(Some(&json!({
            "type": "string",
            "properties": { "kept": { "type": "number" } }
        })));
        assert_eq!(
            normalized,
            json!({
                "type": "object",
                "properties": { "kept": { "type": "number" } }
            })
        );

        // No `properties` key in the input: none is added (`{...schema, type}`).
        let normalized = normalize_elicitation_schema(Some(&json!({ "description": "x" })));
        assert_eq!(normalized, json!({ "description": "x", "type": "object" }));
        assert!(normalized.get("properties").is_none());

        // A missing/non-object schema still gets the empty object shape.
        assert_eq!(
            normalize_elicitation_schema(None),
            json!({ "type": "object", "properties": {} })
        );
    }

    #[test]
    fn ask_user_questions_to_create_request_filters_empty_preview() {
        let questions = vec![mk_question(
            "Which?",
            vec![AskUserQuestionOption {
                label: "A".into(),
                description: Some("".into()),
                preview: Some("".into()),
            }],
            None,
            false,
        )];
        let request = ask_user_questions_to_create_request(&questions, SESSION_ID, None, false);
        let option = &request["requestedSchema"]["properties"]["question_0"]["oneOf"][0];
        assert_eq!(option, &json!({ "const": "A", "title": "A" }));
    }

    #[test]
    fn refusal_fallback_to_create_request_filters_empty_category() {
        let prompt = RefusalFallbackPrompt {
            original_model: "m1".into(),
            fallback_model: "m2".into(),
            api_refusal_category: Some(String::new()),
            guidance_text: None,
        };
        let request = refusal_fallback_to_create_request(&prompt, SESSION_ID);
        assert_eq!(
            request["message"],
            "m1 declined this request. Retry with m2?"
        );
        assert!(!request["message"].as_str().unwrap().contains("()"));
    }

    #[test]
    fn extract_ask_user_questions_keeps_unlabelled_options_and_empty_question() {
        let input = json!({
            "questions": [
                { "question": "", "options": [{ "description": "no label" }] }
            ]
        });
        let questions = extract_ask_user_questions(&input).expect("the question surfaces");
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].question, "");
        assert_eq!(questions[0].options.len(), 1);
        assert_eq!(questions[0].options[0].label, "");
    }
}
