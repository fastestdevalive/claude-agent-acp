//! A trivial in-repo ACP v1 echo agent used by the recorder integration test
//! (item 1.5 / 1.T4).
//!
//! It answers `initialize`, `session/new` and `session/prompt` over stdio,
//! echoing the prompt text back as an `agentMessageChunk` update before
//! settling the turn. Diagnostics go to stderr; stdout carries only JSON-RPC.
//!
//! When the client advertises `elicitation.form` in `initialize` and a prompt
//! asks to "elicit", the agent sends an `elicitation/create` request before it
//! settles the turn. That exercises the recorder's `elicit_reply` step (F1)
//! against a real ACP request/response round trip.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, CreateElicitationRequest,
    ElicitationFormMode, ElicitationSchema, ElicitationSessionScope, InitializeRequest,
    InitializeResponse, MessageId, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, ConnectionTo, Result, Stdio};

#[tokio::main]
async fn main() -> Result<()> {
    let session_counter = Arc::new(AtomicUsize::new(0));
    // Whether the client advertised form elicitation in `initialize`.
    let elicit_capable = Arc::new(AtomicBool::new(false));

    Agent
        .builder()
        .name("echo-agent")
        .on_receive_request(
            {
                let elicit_capable = elicit_capable.clone();
                async move |initialize: InitializeRequest, responder, _connection| {
                    elicit_capable.store(
                        initialize.client_capabilities.elicitation.is_some(),
                        Ordering::SeqCst,
                    );
                    responder.respond(
                        InitializeResponse::new(initialize.protocol_version)
                            .agent_capabilities(AgentCapabilities::new()),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let session_counter = session_counter.clone();
                async move |_request: NewSessionRequest,
                            responder,
                            connection: ConnectionTo<agent_client_protocol::Client>| {
                    let n = session_counter.fetch_add(1, Ordering::SeqCst) + 1;
                    let session_id = format!("echo-session-{n}");
                    responder.respond(NewSessionResponse::new(session_id.clone()))?;
                    // Report an initial idle state so the client has a session
                    // to which updates are delivered.
                    connection.send_notification(SessionNotification::new(
                        session_id,
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(
                            ContentBlock::Text(TextContent::new("echo session ready")),
                        )),
                    ))?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let elicit_capable = elicit_capable.clone();
                async move |request: PromptRequest,
                            responder,
                            connection: ConnectionTo<agent_client_protocol::Client>| {
                    let echoed = request
                        .prompt
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");

                    // F1: only a client that advertised the whole
                    // `elicitation.form` capability is asked; this proves the
                    // recorder carried the full object, not just `_meta`.
                    if elicit_capable.load(Ordering::SeqCst) && echoed.contains("elicit") {
                        let elicitation = CreateElicitationRequest::new(
                            ElicitationFormMode::new(
                                ElicitationSessionScope::new(request.session_id.clone()),
                                ElicitationSchema::new(),
                            ),
                            "What should the echo say?",
                        );
                        // A request handler runs on the dispatch loop, so
                        // `block_task()` here would deadlock: register the
                        // continuation instead and answer the prompt from it.
                        let session_id = request.session_id.clone();
                        let reply_connection = connection.clone();
                        connection
                            .send_request(elicitation)
                            .on_receiving_result(async move |result| {
                                let response = result?;
                                // Echo the action so the test can observe the
                                // round trip as well as the response frame.
                                let action = match response.action {
                                    agent_client_protocol::schema::v1::ElicitationAction::Accept(
                                        _,
                                    ) => "accept",
                                    agent_client_protocol::schema::v1::ElicitationAction::Decline => {
                                        "decline"
                                    }
                                    agent_client_protocol::schema::v1::ElicitationAction::Cancel => {
                                        "cancel"
                                    }
                                    _ => "other",
                                };
                                reply_connection.send_notification(SessionNotification::new(
                                    session_id.clone(),
                                    SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                        ContentBlock::Text(TextContent::new(format!(
                                            "elicitation: {action}"
                                        ))),
                                    )),
                                ))?;
                                reply_connection.send_notification(SessionNotification::new(
                                    session_id,
                                    SessionUpdate::AgentMessageChunk(
                                        ContentChunk::new(ContentBlock::Text(TextContent::new(
                                            format!("Echo: {echoed}"),
                                        )))
                                        .message_id(MessageId::new("echo-message-1")),
                                    ),
                                ))?;
                                responder.respond(PromptResponse::new(StopReason::EndTurn))
                            })?;
                        return Ok(());
                    }

                    let message_id = MessageId::new("echo-message-1");
                    connection.send_notification(SessionNotification::new(
                        request.session_id.clone(),
                        SessionUpdate::AgentMessageChunk(
                            ContentChunk::new(ContentBlock::Text(TextContent::new(format!(
                                "Echo: {echoed}"
                            ))))
                            .message_id(message_id),
                        ),
                    ))?;

                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_cancel: CancelNotification, _connection| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}
