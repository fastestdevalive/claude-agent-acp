//! A trivial in-repo ACP v1 echo agent used by the recorder integration test
//! (item 1.5 / 1.T4).
//!
//! It answers `initialize`, `session/new` and `session/prompt` over stdio,
//! echoing the prompt text back as an `agentMessageChunk` update before
//! settling the turn. Diagnostics go to stderr; stdout carries only JSON-RPC.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
    InitializeResponse, MessageId, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, ConnectionTo, Result, Stdio};

#[tokio::main]
async fn main() -> Result<()> {
    let session_counter = Arc::new(AtomicUsize::new(0));

    Agent
        .builder()
        .name("echo-agent")
        .on_receive_request(
            async move |initialize: InitializeRequest, responder, _connection| {
                responder.respond(
                    InitializeResponse::new(initialize.protocol_version)
                        .agent_capabilities(AgentCapabilities::new()),
                )
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
